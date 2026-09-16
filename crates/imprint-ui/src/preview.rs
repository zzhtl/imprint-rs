//! 预览画布：双纹理叠加 + 拖拽定位。
//!
//! **不要**每帧重新合成整张预览图再上传。3840×2160 一帧就是 33 MB 的转换加上传，
//! 60fps 下等于每秒 2 GB 的无用功。这里分成两张纹理：
//!
//! 1. **底图纹理** —— 打开文件时按预览尺寸上传一次，之后不动；
//! 2. **水印图层纹理** —— 只有水印**内容**（文字/字体/颜色/尺寸）变化时才重传。
//!
//! 位置、旋转、透明度、平铺全是绘制期参数，拖拽过程中零上传。
//! 几何定位调用 `imprint_core::compose` 导出的同一份公式，保证所见即所得。

use egui::{Color32, Id, Mesh, Pos2, Rect, Sense, Shape, TextureHandle, Vec2, emath::Rot2};
use imprint_core::{
    FieldContext, Renderer,
    compose::{layer_origin, tile_step},
    spec::{Content, Placement, SizeMode, WatermarkSpec},
};

/// 水印拖拽热区的控件 id。
///
/// 用固定 id 而不是 `ui.id().with(...)`：后者会随布局嵌套变化，
/// 换个面板结构就变了，测试也没法稳定引用它。
pub const WATERMARK_DRAG_ID: &str = "imprint_watermark_drag";

/// 标识一张水印纹理是由哪些参数生成的。
///
/// 只包含**影响图层像素**的字段：位置、旋转、透明度变化时不必重新渲染图层。
#[derive(Clone, PartialEq)]
struct LayerKey {
    content: Content,
    size: SizeMode,
    canvas: (u32, u32),
    /// 模板求值后的文本，动态字段（`{filename}` 等）换文件时必须重算。
    resolved_text: String,
}

struct LayerTexture {
    texture: TextureHandle,
    size: (u32, u32),
    key: LayerKey,
}

#[derive(Default)]
pub struct PreviewState {
    layer: Option<LayerTexture>,
    /// 上一次渲染图层失败的原因，显示给用户而不是静默留空。
    pub last_error: Option<String>,
}

impl PreviewState {
    /// 内容变了就丢弃缓存，下一帧重建。
    pub fn invalidate(&mut self) {
        self.layer = None;
    }
}

/// 预览画布的一次绘制结果。
pub struct PreviewOutput {
    /// 水印在画布上被拖动了，调用方据此更新 spec。
    pub dragged_to: Option<(f32, f32)>,
}

/// 画出预览，并处理拖拽。
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    state: &mut PreviewState,
    renderer: &mut Renderer,
    spec: &WatermarkSpec,
    base_texture: &TextureHandle,
    canvas_size: (u32, u32),
    ctx: &FieldContext,
) -> PreviewOutput {
    let mut out = PreviewOutput { dragged_to: None };

    let avail = ui.available_size();
    let (rect, _) = ui.allocate_exact_size(avail, Sense::hover());
    // 按原图宽高比把图片摆进可用区域，居中。
    let image_rect = fit_rect(rect, canvas_size);

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::from_gray(28));
    painter.image(
        base_texture.id(),
        image_rect,
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
        Color32::WHITE,
    );

    // 图层按**预览尺寸**渲染，与底图纹理对齐；导出时再按原图尺寸重渲一次。
    let preview_canvas = (
        image_rect.width().round().max(1.0) as u32,
        image_rect.height().round().max(1.0) as u32,
    );
    ensure_layer(ui.ctx(), state, renderer, spec, preview_canvas, ctx);

    let Some(layer) = &state.layer else {
        return out;
    };

    let scale = image_rect.width() / preview_canvas.0 as f32;
    let tint = Color32::from_white_alpha((spec.opacity.clamp(0.0, 1.0) * 255.0).round() as u8);
    let layer_px = Vec2::new(layer.size.0 as f32 * scale, layer.size.1 as f32 * scale);

    match &spec.placement {
        Placement::Tile {
            spacing_ratio,
            stagger,
        } => {
            draw_tiled(
                &painter,
                layer,
                image_rect,
                layer_px,
                *spacing_ratio,
                *stagger,
                spec.rotation_deg,
                tint,
                scale,
            );
        }
        placement => {
            let Some((ox, oy)) = layer_origin(placement, layer.size, preview_canvas) else {
                return out;
            };
            let top_left = image_rect.min + Vec2::new(ox * scale, oy * scale);
            let wm_rect = Rect::from_min_size(top_left, layer_px);
            draw_quad(
                &painter,
                layer.texture.id(),
                wm_rect,
                spec.rotation_deg,
                tint,
            );

            // 拖拽：命中区是水印自身的矩形（未旋转的包围盒，够用且好点）。
            let id = Id::new(WATERMARK_DRAG_ID);
            // 用裸 Sense::DRAG 而不是 Sense::drag()：后者带 FOCUSABLE，会抢走键盘焦点。
            let response = ui.interact(wm_rect.intersect(rect), id, Sense::DRAG);
            if response.hovered() || response.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                painter.rect_stroke(
                    wm_rect,
                    2.0,
                    egui::Stroke::new(1.0, Color32::from_white_alpha(140)),
                    egui::StrokeKind::Middle,
                );
            }
            if response.dragged() {
                // drag_delta 已按 layer transform 的 scaling 折算过，这里只需换算
                // 预览像素 → 归一化坐标，不要再除一次缩放。
                let center = wm_rect.center() + response.drag_delta();
                let nx = (center.x - image_rect.min.x) / image_rect.width();
                let ny = (center.y - image_rect.min.y) / image_rect.height();
                out.dragged_to = Some((nx.clamp(0.0, 1.0), ny.clamp(0.0, 1.0)));
            }
        }
    }

    out
}

/// 画一个（可旋转的）带纹理四边形。
fn draw_quad(
    painter: &egui::Painter,
    id: egui::TextureId,
    rect: Rect,
    rotation_deg: f32,
    tint: Color32,
) {
    let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    if rotation_deg.abs() < f32::EPSILON {
        painter.image(id, rect, uv, tint);
        return;
    }
    let mut mesh = Mesh::with_texture(id);
    mesh.add_rect_with_uv(rect, uv, tint);
    // 绕自身中心转，与 core 的 Transform::from_rotate_at 语义一致。
    mesh.rotate(Rot2::from_angle(rotation_deg.to_radians()), rect.center());
    painter.add(Shape::mesh(mesh));
}

/// 平铺预览。
///
/// 步长取自 `imprint_core::compose::tile_step`，与导出共用同一公式。
#[allow(clippy::too_many_arguments)]
fn draw_tiled(
    painter: &egui::Painter,
    layer: &LayerTexture,
    image_rect: Rect,
    layer_px: Vec2,
    spacing_ratio: (f32, f32),
    stagger: bool,
    rotation_deg: f32,
    tint: Color32,
    scale: f32,
) {
    let (step_x, step_y) = tile_step(layer.size, spacing_ratio);
    let (step_x, step_y) = (step_x * scale, step_y * scale);
    if step_x <= 0.5 || step_y <= 0.5 {
        return;
    }

    // 旋转后要覆盖整个画布，得从更大的范围铺起 —— 对角线长度是安全上界。
    let diag = image_rect.size().length();
    let center = image_rect.center();
    let half = Vec2::splat(diag / 2.0 + step_x.max(step_y));

    let cols = ((half.x * 2.0) / step_x).ceil() as i32 + 1;
    let rows = ((half.y * 2.0) / step_y).ceil() as i32 + 1;
    // 平铺 quad 数量在极端参数下会爆，设个上限保住帧率。
    const MAX_TILES: i32 = 4096;
    if cols * rows > MAX_TILES {
        return;
    }

    let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    let mut mesh = Mesh::with_texture(layer.texture.id());
    let origin = center - half;

    for row in 0..rows {
        for col in 0..cols {
            let mut x = origin.x + col as f32 * step_x;
            let y = origin.y + row as f32 * step_y;
            // 奇数行错半格，和 core 把错位烘进 tile 的做法等效。
            if stagger && row % 2 != 0 {
                x += step_x / 2.0;
            }
            mesh.add_rect_with_uv(Rect::from_min_size(Pos2::new(x, y), layer_px), uv, tint);
        }
    }
    if rotation_deg.abs() > f32::EPSILON {
        mesh.rotate(Rot2::from_angle(rotation_deg.to_radians()), center);
    }

    // 平铺会铺到画布外，裁掉溢出部分。
    painter.with_clip_rect(image_rect).add(Shape::mesh(mesh));
}

/// 按需重建水印图层纹理。
fn ensure_layer(
    egui_ctx: &egui::Context,
    state: &mut PreviewState,
    renderer: &mut Renderer,
    spec: &WatermarkSpec,
    canvas: (u32, u32),
    ctx: &FieldContext,
) {
    let resolved_text = match &spec.content {
        Content::Text(t) => imprint_core::fields::render(&t.template, ctx),
        Content::Image(_) => String::new(),
    };
    let key = LayerKey {
        content: spec.content.clone(),
        size: spec.size,
        canvas,
        resolved_text,
    };

    if state.layer.as_ref().is_some_and(|l| l.key == key) {
        return;
    }

    match renderer.render_layer(spec, canvas, ctx) {
        Ok(pixmap) => {
            let size = (pixmap.width(), pixmap.height());
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [size.0 as usize, size.1 as usize],
                // tiny-skia 是预乘的，egui 这里要非预乘，交给 take_demultiplied 换算。
                &pixmap.take_demultiplied(),
            );
            let texture =
                egui_ctx.load_texture("imprint_layer", image, egui::TextureOptions::LINEAR);
            state.layer = Some(LayerTexture { texture, size, key });
            state.last_error = None;
        }
        Err(e) => {
            state.layer = None;
            state.last_error = Some(e.to_string());
        }
    }
}

/// 把 `size` 的宽高比套进 `outer`，居中。
fn fit_rect(outer: Rect, size: (u32, u32)) -> Rect {
    if size.0 == 0 || size.1 == 0 || outer.width() <= 0.0 || outer.height() <= 0.0 {
        return outer;
    }
    let aspect = size.0 as f32 / size.1 as f32;
    let outer_aspect = outer.width() / outer.height();
    let inner = if aspect > outer_aspect {
        Vec2::new(outer.width(), outer.width() / aspect)
    } else {
        Vec2::new(outer.height() * aspect, outer.height())
    };
    Rect::from_center_size(outer.center(), inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use imprint_core::{
        FontLibrary,
        spec::{Anchor9, Rgba8, SizeMode, TextAlign, TextSpec},
    };
    use std::sync::Arc;

    const SCREEN: egui::Vec2 = egui::vec2(800.0, 600.0);
    /// 素材是正方形，在 800×600 的区域里会被摆成居中的正方形。
    const CANVAS: (u32, u32) = (1000, 1000);

    fn spec_at(x: f32, y: f32) -> WatermarkSpec {
        WatermarkSpec {
            content: Content::Text(TextSpec {
                template: "DRAG".to_owned(),
                color: Rgba8::WHITE,
                align: TextAlign::Left,
                ..TextSpec::default()
            }),
            size: SizeMode::RelativeFontSize(0.1),
            placement: Placement::Normalized {
                x,
                y,
                anchor: Anchor9::Center,
            },
            opacity: 1.0,
            rotation_deg: 0.0,
        }
    }

    fn raw_input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
            events,
            ..Default::default()
        }
    }

    fn press(pos: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
        ]
    }

    struct Env {
        ctx: egui::Context,
        state: PreviewState,
        renderer: Renderer,
        texture: TextureHandle,
    }

    impl Env {
        fn new() -> Self {
            let ctx = egui::Context::default();
            let texture = ctx.load_texture(
                "base",
                egui::ColorImage::from_rgba_unmultiplied([4, 4], &[0u8; 4 * 4 * 4]),
                egui::TextureOptions::LINEAR,
            );
            Self {
                ctx,
                state: PreviewState::default(),
                renderer: Renderer::new(Arc::new(FontLibrary::with_system_fonts())),
                texture,
            }
        }

        /// 跑一帧预览。
        fn frame(&mut self, spec: &WatermarkSpec, events: Vec<egui::Event>) -> PreviewOutput {
            let mut out = PreviewOutput { dragged_to: None };
            let (state, renderer, texture) = (&mut self.state, &mut self.renderer, &self.texture);
            // 0.36 把 Context::run 改名成 run_ui，闭包直接拿到 &mut Ui —— 与 App::ui 一致。
            let full = self.ctx.run_ui(raw_input(events), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    out = show(
                        ui,
                        state,
                        renderer,
                        spec,
                        texture,
                        CANVAS,
                        &FieldContext::new(),
                    );
                });
            });
            // 测试不真正上屏，纹理增量直接丢弃。
            full.drop_without_applying_deltas();
            out
        }

        /// 水印热区在屏幕上的位置。
        ///
        /// 不硬编码坐标：CentralPanel 自带 margin，可用区域并不等于窗口大小。
        fn watermark_rect(&self) -> Option<egui::Rect> {
            self.ctx
                .read_response(egui::Id::new(WATERMARK_DRAG_ID))
                .map(|r| r.rect)
        }

        /// 从水印中心拖动 `delta`，返回上报的归一化坐标。
        fn drag(&mut self, spec: &WatermarkSpec, delta: egui::Vec2) -> Option<(f32, f32)> {
            self.frame(spec, Vec::new());
            let center = self.watermark_rect()?.center();
            self.frame(spec, press(center));
            self.frame(spec, vec![egui::Event::PointerMoved(center + delta)])
                .dragged_to
        }
    }

    #[test]
    fn watermark_has_a_hit_area() {
        let mut e = Env::new();
        e.frame(&spec_at(0.5, 0.5), Vec::new());
        let rect = e.watermark_rect().expect("水印没有可交互的热区");
        assert!(
            rect.width() > 1.0 && rect.height() > 1.0,
            "热区尺寸异常: {rect:?}"
        );
    }

    #[test]
    fn dragging_moves_watermark_toward_the_pointer() {
        let mut e = Env::new();
        let spec = spec_at(0.5, 0.5);
        let (x, y) = e
            .drag(&spec, egui::vec2(120.0, 90.0))
            .expect("拖拽未被识别");

        // 往右下拖，两个方向的归一化坐标都该变大。
        assert!(x > 0.5, "水平方向没跟着指针走: {x}");
        assert!(y > 0.5, "垂直方向没跟着指针走: {y}");
        assert!((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y));
    }

    #[test]
    fn drag_round_trip_returns_to_start() {
        // 不去假设图片矩形的确切像素尺寸，而是验证换算自洽：
        // 拖过去再拖回来，必须回到原点。这正是"预览拖动 → 导出位置"不漂移的前提。
        let mut e = Env::new();
        let delta = egui::vec2(100.0, 75.0);

        let start = spec_at(0.5, 0.5);
        let (x1, y1) = e.drag(&start, delta).expect("首次拖拽未被识别");

        let moved = spec_at(x1, y1);
        let (x2, y2) = e.drag(&moved, -delta).expect("回程拖拽未被识别");

        assert!((x2 - 0.5).abs() < 0.01, "往返后水平坐标漂移: {x2}");
        assert!((y2 - 0.5).abs() < 0.01, "往返后垂直坐标漂移: {y2}");
    }

    #[test]
    fn dragging_is_clamped_to_canvas() {
        let mut e = Env::new();
        let spec = spec_at(0.9, 0.9);
        let (x, y) = e
            .drag(&spec, egui::vec2(5000.0, 5000.0))
            .expect("拖拽未被识别");

        // 越界必须钳制，否则水印会被丢到画布外，导出后彻底看不见。
        assert!((x - 1.0).abs() < 1e-6, "x 未被钳制: {x}");
        assert!((y - 1.0).abs() < 1e-6, "y 未被钳制: {y}");
    }

    #[test]
    fn pointer_outside_watermark_does_not_drag() {
        let mut e = Env::new();
        let spec = spec_at(0.5, 0.5);
        e.frame(&spec, Vec::new());
        let rect = e.watermark_rect().expect("水印没有热区");

        // 在水印热区之外按下并拖动。
        let far = rect.center() - egui::vec2(rect.width() + 60.0, rect.height() + 60.0);
        e.frame(&spec, press(far));
        let out = e.frame(
            &spec,
            vec![egui::Event::PointerMoved(far + egui::vec2(40.0, 40.0))],
        );

        assert!(out.dragged_to.is_none(), "不该把水印之外的拖动当成移动水印");
    }

    #[test]
    fn tiled_placement_is_not_draggable() {
        let mut e = Env::new();
        let spec = WatermarkSpec {
            placement: Placement::Tile {
                spacing_ratio: (0.5, 0.5),
                stagger: false,
            },
            ..spec_at(0.5, 0.5)
        };
        e.frame(&spec, Vec::new());
        // 平铺没有单一位置可拖，连热区都不该存在。
        assert!(e.watermark_rect().is_none());
    }
}
