//! 把水印图层合成到画布：定位、旋转、平铺、整体透明度。
//!
//! 全部走 `fill_path`/`fill_rect` + `Pattern` shader，不用 `draw_pixmap`。原因有二：
//! `draw_pixmap` 内部把 `anti_alias` 硬编码成 `false`（旋转后的水印会有锯齿边），
//! 且它固定使用 `ColorSpace::default()`。若按"旋转才走 path"分两条路，同一个水印
//! 在 0° 和 1° 下的边缘与混色会不一致 —— 统一走一条路才能保证预览与输出一致。

use tiny_skia::{
    ColorSpace, FillRule, FilterQuality, Paint, PathBuilder, Pattern, Pixmap, PixmapPaint,
    PixmapRef, Rect, SpreadMode, Transform,
};

use crate::{
    error::{Error, Result},
    spec::{Anchor9, Margin, Placement},
};

/// 平铺 tile 的边长上限，防止离谱的 spacing 参数撑爆内存。
const MAX_TILE_SIDE: u32 = 32_768;

/// 把水印图层合成到画布上。
///
/// `canvas` 原地修改。`layer` 是 [`crate::layer::render_layer`] 的产物 ——
/// 未定位、未旋转、未施加透明度的裸图层。
pub fn compose(
    canvas: &mut Pixmap,
    layer: &Pixmap,
    placement: &Placement,
    opacity: f32,
    rotation_deg: f32,
) -> Result<()> {
    if layer.width() == 0 || layer.height() == 0 {
        return Err(Error::InvalidSize {
            width: layer.width(),
            height: layer.height(),
        });
    }
    let opacity = opacity.clamp(0.0, 1.0);
    if opacity == 0.0 {
        return Ok(());
    }
    let rotation = if rotation_deg.is_finite() {
        rotation_deg
    } else {
        0.0
    };

    match placement {
        Placement::Anchor { anchor, margin } => {
            let (x, y) = anchor_origin(*anchor, *margin, layer, canvas);
            draw_single(canvas, layer, x, y, opacity, rotation)
        }
        Placement::Normalized { x, y, anchor } => {
            let (px, py) = normalized_origin(*x, *y, *anchor, layer, canvas);
            draw_single(canvas, layer, px, py, opacity, rotation)
        }
        Placement::Tile {
            spacing_ratio,
            stagger,
        } => draw_tiled(canvas, layer, *spacing_ratio, *stagger, opacity, rotation),
    }
}

/// 水印左上角在画布上的像素坐标。
///
/// 公开给 UI 预览复用：预览与导出必须用**同一份**定位公式，
/// 各算各的迟早会漂移。平铺模式没有单一位置，返回 `None`。
pub fn layer_origin(
    placement: &Placement,
    layer_size: (u32, u32),
    canvas_size: (u32, u32),
) -> Option<(f32, f32)> {
    let (lw, lh) = (layer_size.0 as f32, layer_size.1 as f32);
    let (cw, ch) = (canvas_size.0 as f32, canvas_size.1 as f32);
    match placement {
        Placement::Anchor { anchor, margin } => {
            Some(anchor_origin_f(*anchor, *margin, lw, lh, cw, ch))
        }
        Placement::Normalized { x, y, anchor } => {
            let (ax, ay) = anchor.unit_offset();
            Some((cw * x - lw * ax, ch * y - lh * ay))
        }
        Placement::Tile { .. } => None,
    }
}

/// 平铺的步长（相邻水印左上角的间隔，像素）。
///
/// 同样公开给预览复用，保证预览铺出来的间距与导出一致。
pub fn tile_step(layer_size: (u32, u32), spacing_ratio: (f32, f32)) -> (f32, f32) {
    let (lw, lh) = (layer_size.0 as f32, layer_size.1 as f32);
    (
        (lw * (1.0 + spacing_ratio.0)).round().max(1.0),
        (lh * (1.0 + spacing_ratio.1)).round().max(1.0),
    )
}

fn anchor_origin_f(
    anchor: Anchor9,
    margin: Margin,
    lw: f32,
    lh: f32,
    cw: f32,
    ch: f32,
) -> (f32, f32) {
    let (ax, ay) = anchor.unit_offset();
    let mx = cw * margin.x_ratio;
    let my = ch * margin.y_ratio;
    let anchor_x = mx + (cw - 2.0 * mx) * ax;
    let anchor_y = my + (ch - 2.0 * my) * ay;
    (anchor_x - lw * ax, anchor_y - lh * ay)
}

/// 九宫格锚点下水印左上角的画布坐标。
///
/// 边距先把可用区域向内收，锚点再在收缩后的矩形上取位，最后按锚点在水印自身
/// 包围盒中的相对位置回退出左上角 —— 这样 `BottomRight` 是"右下角贴着边距"，
/// 而 `Center` 是"中心对中心"，两者语义统一。
fn anchor_origin(anchor: Anchor9, margin: Margin, layer: &Pixmap, canvas: &Pixmap) -> (f32, f32) {
    anchor_origin_f(
        anchor,
        margin,
        layer.width() as f32,
        layer.height() as f32,
        canvas.width() as f32,
        canvas.height() as f32,
    )
}

/// 归一化坐标下水印左上角的画布坐标。
fn normalized_origin(
    x: f32,
    y: f32,
    anchor: Anchor9,
    layer: &Pixmap,
    canvas: &Pixmap,
) -> (f32, f32) {
    let (ax, ay) = anchor.unit_offset();
    (
        canvas.width() as f32 * x - layer.width() as f32 * ax,
        canvas.height() as f32 * y - layer.height() as f32 * ay,
    )
}

fn draw_single(
    canvas: &mut Pixmap,
    layer: &Pixmap,
    x: f32,
    y: f32,
    opacity: f32,
    rotation_deg: f32,
) -> Result<()> {
    let (w, h) = (layer.width() as f32, layer.height() as f32);
    let Some(rect) = Rect::from_xywh(0.0, 0.0, w, h) else {
        return Err(Error::InvalidSize {
            width: layer.width(),
            height: layer.height(),
        });
    };
    let path = PathBuilder::from_rect(rect);

    // 几何与 shader 会被 fill_path 的 transform 一起变换，所以 pattern 自身用 identity，
    // 由外层 transform 统一完成"绕水印中心旋转、再平移到目标位置"。
    let paint = pattern_paint(layer.as_ref(), SpreadMode::Pad, opacity);
    let transform = Transform::from_translate(x, y).pre_concat(Transform::from_rotate_at(
        rotation_deg,
        w / 2.0,
        h / 2.0,
    ));

    canvas.fill_path(&path, &paint, FillRule::Winding, transform, None);
    Ok(())
}

fn draw_tiled(
    canvas: &mut Pixmap,
    layer: &Pixmap,
    spacing_ratio: (f32, f32),
    stagger: bool,
    opacity: f32,
    rotation_deg: f32,
) -> Result<()> {
    let tile = build_tile(layer, spacing_ratio, stagger)?;

    let (cw, ch) = (canvas.width() as f32, canvas.height() as f32);
    let Some(full) = Rect::from_xywh(0.0, 0.0, cw, ch) else {
        return Err(Error::InvalidSize {
            width: canvas.width(),
            height: canvas.height(),
        });
    };

    // 旋转必须放进 pattern 自己的 transform：若交给 fill_rect 的 transform，
    // 填充矩形本身也会被转走，画布四角就会露白。
    // `SpreadMode::Repeat` 保证 pattern 旋转后依然铺满整个平面。
    let pattern_ts = Transform::from_rotate_at(rotation_deg, cw / 2.0, ch / 2.0);
    let paint = Paint {
        shader: Pattern::new(
            tile.as_ref(),
            SpreadMode::Repeat,
            FilterQuality::Bilinear,
            opacity,
            pattern_ts,
        ),
        anti_alias: true,
        ..default_paint()
    };

    canvas.fill_rect(full, &paint, Transform::identity(), None);
    Ok(())
}

/// 造一块可无缝重复的 tile。
///
/// 间距被烘进 tile 本身，之后 `SpreadMode::Repeat` 一次 `fill_rect` 就能铺满 ——
/// 比循环 N×M 次 `draw_pixmap` 少了成百上千次光栅化调用，且任意旋转角下都无缝。
fn build_tile(layer: &Pixmap, spacing_ratio: (f32, f32), stagger: bool) -> Result<Pixmap> {
    let (step_x, step_y) = tile_step((layer.width(), layer.height()), spacing_ratio);

    let tile_w = step_x as u32;
    // 错行靠"一块 tile 里放两行、第二行横移半格"实现 —— Pattern 只会做规则重复，
    // 把错位关系烘进 tile 是让它产生错行效果的唯一办法。
    let tile_h = if stagger {
        (step_y * 2.0) as u32
    } else {
        step_y as u32
    };

    if tile_w == 0 || tile_h == 0 || tile_w > MAX_TILE_SIDE || tile_h > MAX_TILE_SIDE {
        return Err(Error::InvalidSize {
            width: tile_w,
            height: tile_h,
        });
    }

    let mut tile = Pixmap::new(tile_w, tile_h).ok_or(Error::InvalidSize {
        width: tile_w,
        height: tile_h,
    })?;
    let paint = PixmapPaint::default();
    tile.draw_pixmap(0, 0, layer.as_ref(), &paint, Transform::identity(), None);

    if stagger {
        let half = (step_x / 2.0).round() as i32;
        let row2 = step_y as i32;
        tile.draw_pixmap(
            half,
            row2,
            layer.as_ref(),
            &paint,
            Transform::identity(),
            None,
        );
        // 半格横移会把第二行推出 tile 右边界，补画环绕的那一半，否则重复时会出现断裂。
        tile.draw_pixmap(
            half - tile_w as i32,
            row2,
            layer.as_ref(),
            &paint,
            Transform::identity(),
            None,
        );
    }

    Ok(tile)
}

fn pattern_paint<'a>(pixmap: PixmapRef<'a>, spread: SpreadMode, opacity: f32) -> Paint<'a> {
    Paint {
        shader: Pattern::new(
            pixmap,
            spread,
            FilterQuality::Bilinear,
            opacity,
            Transform::identity(),
        ),
        anti_alias: true,
        ..default_paint()
    }
}

fn default_paint<'a>() -> Paint<'a> {
    Paint {
        // 显式写出而非依赖默认值：`Linear` 是"把 sRGB 字节当线性量混合"的传统行为，
        // 与 Photoshop / GIMP 的默认一致，用户对水印透明度的直觉来自那里。
        // 换成 `SimpleSRGB` 在深色背景上观感更好，但会偏离用户预期，是个产品选择。
        colorspace: ColorSpace::Linear,
        ..Paint::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::PremultipliedColorU8;

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Pixmap {
        let mut pm = Pixmap::new(w, h).unwrap();
        let px = PremultipliedColorU8::from_rgba(rgba[0], rgba[1], rgba[2], rgba[3]).unwrap();
        for p in pm.pixels_mut() {
            *p = px;
        }
        pm
    }

    fn opaque_count(pm: &Pixmap) -> usize {
        pm.pixels().iter().filter(|p| p.alpha() > 0).count()
    }

    /// 图层不透明像素的重心（归一化到 0..1）。
    fn centroid(pm: &Pixmap) -> (f32, f32) {
        let (mut sx, mut sy, mut n) = (0.0f64, 0.0f64, 0u64);
        for y in 0..pm.height() {
            for x in 0..pm.width() {
                if pm.pixels()[(y * pm.width() + x) as usize].alpha() > 0 {
                    sx += x as f64;
                    sy += y as f64;
                    n += 1;
                }
            }
        }
        assert!(n > 0, "画布上没有任何水印像素");
        (
            (sx / n as f64) as f32 / pm.width() as f32,
            (sy / n as f64) as f32 / pm.height() as f32,
        )
    }

    #[test]
    fn anchor_bottom_right_lands_in_bottom_right() {
        let mut canvas = Pixmap::new(400, 400).unwrap();
        let layer = solid(40, 40, [255, 0, 0, 255]);
        compose(
            &mut canvas,
            &layer,
            &Placement::Anchor {
                anchor: Anchor9::BottomRight,
                margin: Margin {
                    x_ratio: 0.05,
                    y_ratio: 0.05,
                },
            },
            1.0,
            0.0,
        )
        .unwrap();

        let (cx, cy) = centroid(&canvas);
        assert!(cx > 0.8 && cy > 0.8, "重心应在右下角，实际 ({cx}, {cy})");
    }

    #[test]
    fn anchor_center_lands_in_center() {
        let mut canvas = Pixmap::new(400, 400).unwrap();
        let layer = solid(40, 40, [0, 255, 0, 255]);
        compose(
            &mut canvas,
            &layer,
            &Placement::Anchor {
                anchor: Anchor9::Center,
                margin: Margin::default(),
            },
            1.0,
            0.0,
        )
        .unwrap();

        let (cx, cy) = centroid(&canvas);
        assert!((cx - 0.5).abs() < 0.02, "水平未居中: {cx}");
        assert!((cy - 0.5).abs() < 0.02, "垂直未居中: {cy}");
    }

    #[test]
    fn normalized_placement_is_resolution_independent() {
        // 同一组归一化坐标，在不同分辨率画布上必须落到相同的相对位置。
        // 这是"预览拖拽 → 全尺寸导出"一致性的核心保证。
        let layer_small = solid(20, 20, [255, 255, 255, 255]);
        let layer_large = solid(100, 100, [255, 255, 255, 255]);
        let placement = Placement::Normalized {
            x: 0.25,
            y: 0.75,
            anchor: Anchor9::Center,
        };

        let mut small = Pixmap::new(200, 200).unwrap();
        compose(&mut small, &layer_small, &placement, 1.0, 0.0).unwrap();
        let mut large = Pixmap::new(1000, 1000).unwrap();
        compose(&mut large, &layer_large, &placement, 1.0, 0.0).unwrap();

        let (sx, sy) = centroid(&small);
        let (lx, ly) = centroid(&large);
        assert!((sx - lx).abs() < 0.01, "跨分辨率水平偏移: {sx} vs {lx}");
        assert!((sy - ly).abs() < 0.01, "跨分辨率垂直偏移: {sy} vs {ly}");
        assert!((sx - 0.25).abs() < 0.02 && (sy - 0.75).abs() < 0.02);
    }

    #[test]
    fn opacity_scales_coverage() {
        let layer = solid(50, 50, [255, 255, 255, 255]);
        let placement = Placement::Anchor {
            anchor: Anchor9::Center,
            margin: Margin::default(),
        };

        let mut full = Pixmap::new(200, 200).unwrap();
        compose(&mut full, &layer, &placement, 1.0, 0.0).unwrap();
        let mut half = Pixmap::new(200, 200).unwrap();
        compose(&mut half, &layer, &placement, 0.5, 0.0).unwrap();

        let full_a = full.pixels().iter().map(|p| p.alpha() as u32).sum::<u32>();
        let half_a = half.pixels().iter().map(|p| p.alpha() as u32).sum::<u32>();
        let ratio = half_a as f32 / full_a as f32;
        assert!((ratio - 0.5).abs() < 0.05, "透明度未线性生效: {ratio}");
    }

    #[test]
    fn zero_opacity_is_a_noop() {
        let mut canvas = Pixmap::new(100, 100).unwrap();
        let layer = solid(50, 50, [255, 0, 0, 255]);
        compose(
            &mut canvas,
            &layer,
            &Placement::Anchor {
                anchor: Anchor9::Center,
                margin: Margin::default(),
            },
            0.0,
            0.0,
        )
        .unwrap();
        assert_eq!(opaque_count(&canvas), 0);
    }

    #[test]
    fn rotation_changes_coverage_but_keeps_center() {
        let mut canvas = Pixmap::new(400, 400).unwrap();
        let layer = solid(100, 40, [0, 0, 255, 255]);
        let placement = Placement::Anchor {
            anchor: Anchor9::Center,
            margin: Margin::default(),
        };
        compose(&mut canvas, &layer, &placement, 1.0, 45.0).unwrap();

        // 绕自身中心旋转，重心应当仍在画布中心。
        let (cx, cy) = centroid(&canvas);
        assert!((cx - 0.5).abs() < 0.03 && (cy - 0.5).abs() < 0.03);

        // 旋转 45° 后包围盒变大，但像素总量应与原图层接近（抗锯齿边缘会有少量出入）。
        let n = opaque_count(&canvas);
        let expect = (100 * 40) as f32;
        assert!(
            (n as f32 - expect).abs() / expect < 0.25,
            "旋转后像素量异常: {n}"
        );
    }

    #[test]
    fn tiling_covers_whole_canvas() {
        let mut canvas = Pixmap::new(400, 400).unwrap();
        let layer = solid(20, 20, [255, 255, 255, 255]);
        compose(
            &mut canvas,
            &layer,
            &Placement::Tile {
                spacing_ratio: (0.5, 0.5),
                stagger: false,
            },
            1.0,
            0.0,
        )
        .unwrap();

        // step = 20 * 1.5 = 30，每 30×30 里有 20×20 是水印，覆盖率约 (20/30)^2 ≈ 0.44。
        let ratio = opaque_count(&canvas) as f32 / (400.0 * 400.0);
        assert!((ratio - 0.444).abs() < 0.08, "平铺覆盖率异常: {ratio}");

        // 四个象限都应当有水印，而不是挤在一角。
        for (qx, qy) in [(0u32, 0u32), (200, 0), (0, 200), (200, 200)] {
            let has = (qy..qy + 200).any(|y| {
                (qx..qx + 200).any(|x| canvas.pixels()[(y * 400 + x) as usize].alpha() > 0)
            });
            assert!(has, "象限 ({qx},{qy}) 没有水印");
        }
    }

    #[test]
    fn staggered_tiling_offsets_alternate_rows() {
        let layer = solid(20, 20, [255, 255, 255, 255]);
        let plain = build_tile(&layer, (0.5, 0.5), false).unwrap();
        let staggered = build_tile(&layer, (0.5, 0.5), true).unwrap();

        // 错行 tile 高度翻倍，容纳两行。
        assert_eq!(staggered.height(), plain.height() * 2);
        assert_eq!(staggered.width(), plain.width());

        let w = staggered.width();
        let row2_y = plain.height();
        let row_mask = |y: u32| -> Vec<bool> {
            (0..w)
                .map(|x| staggered.pixels()[(y * w + x) as usize].alpha() > 0)
                .collect()
        };
        let row1 = row_mask(0);
        let row2 = row_mask(row2_y);

        // 两行的占位图案必须不同，否则就不是错行而是规则重复。
        assert_ne!(row1, row2, "第二行未错开");

        // 像素总量必须守恒：半格横移会把第二行推出右边界，环绕部分若没补回来，
        // 平铺时就会出现断裂；若补重了，则会比第一行多。
        let n1 = row1.iter().filter(|b| **b).count();
        let n2 = row2.iter().filter(|b| **b).count();
        assert_eq!(n1, n2, "错行后像素量不守恒，环绕补画有误");
        assert_eq!(n1, layer.width() as usize);
    }

    #[test]
    fn public_layer_origin_matches_internal_placement() {
        // UI 预览走 layer_origin，导出走 compose 内部路径；两者必须逐像素一致，
        // 否则"预览调好的位置"到导出就会漂。
        let canvas = Pixmap::new(800, 600).unwrap();
        let layer = solid(120, 40, [255, 255, 255, 255]);
        for anchor in [
            Anchor9::TopLeft,
            Anchor9::Center,
            Anchor9::BottomRight,
            Anchor9::CenterLeft,
        ] {
            let margin = Margin {
                x_ratio: 0.03,
                y_ratio: 0.07,
            };
            let internal = anchor_origin(anchor, margin, &layer, &canvas);
            let public = layer_origin(
                &Placement::Anchor { anchor, margin },
                (layer.width(), layer.height()),
                (canvas.width(), canvas.height()),
            )
            .unwrap();
            assert_eq!(internal, public, "锚点 {anchor:?} 的公开/内部定位不一致");
        }
    }

    #[test]
    fn tile_placement_has_no_single_origin() {
        assert!(
            layer_origin(
                &Placement::Tile {
                    spacing_ratio: (0.5, 0.5),
                    stagger: false
                },
                (10, 10),
                (100, 100)
            )
            .is_none()
        );
    }

    #[test]
    fn rejects_empty_layer() {
        let mut canvas = Pixmap::new(100, 100).unwrap();
        let layer = Pixmap::new(1, 1).unwrap();
        // 1x1 是合法的；这里验证的是 compose 不会对合法输入误报。
        assert!(
            compose(
                &mut canvas,
                &layer,
                &Placement::Anchor {
                    anchor: Anchor9::Center,
                    margin: Margin::default()
                },
                1.0,
                0.0
            )
            .is_ok()
        );
    }
}
