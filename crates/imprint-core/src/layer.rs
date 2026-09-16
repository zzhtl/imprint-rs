//! 水印图层渲染：`spec` + 目标画布尺寸 → 一张独立的 RGBA 图层。
//!
//! 产出的图层是**未定位、未旋转、未施加整体透明度**的裸图层，那三件事属于
//! [`crate::compose`]。这样切分是因为同一张图层要同时服务三处消费者：
//! 预览显示、图片合成、视频 overlay 的 PNG 输入 —— 它们的定位方式相同，
//! 但只有图层本身值得缓存。

use cosmic_text::{FontSystem, SwashCache};
use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};

use crate::{
    error::{Error, Result},
    fields::{self, FieldContext},
    fonts::FontLibrary,
    spec::{Content, ImageSource, SizeMode, TextSpec, WatermarkSpec},
    text::{self, TextStyle},
};

/// 换算 `RelativeWidth` 时使用的参考字号。
///
/// 先在这个字号下量一次文本宽度，再按目标宽度求比例 —— cosmic-text 没有
/// "按宽度排版"的入口，只能这样反解。取 100 是为了让比例运算落在
/// f32 精度舒适区，太小会放大测量误差。
const REF_FONT_SIZE: f32 = 100.0;

/// 图层任一边的硬上限。
///
/// 越过这个尺寸基本可以断定是参数写错了（绝对像素模式没有相对量的天然约束），
/// 与其让进程 OOM，不如明确报错。
const MAX_LAYER_SIDE: u32 = 32_768;

/// 字号硬上限。
///
/// 必须在**进入光栅化之前**挡住：swash 的光栅化后端 zeno 在
/// `Format::buffer_size` 里做的是 `width * height` 的 **u32** 乘法
/// （`zeno/src/mask.rs:34`）。字号大到让单个字形的位图超过 u32 时，
/// debug 构建 panic，release 构建静默回绕成一个过小的 buffer size —— 后者更危险。
/// 4096 远超任何合理水印用途，又离溢出阈值有几个数量级的余量。
const MAX_FONT_SIZE: f32 = 4096.0;

/// 渲染一个水印图层。
///
/// `target` 是水印将要贴上去的画布尺寸，相对尺寸模式据此换算。
pub fn render_layer(
    fonts: &FontLibrary,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    spec: &WatermarkSpec,
    target: (u32, u32),
    ctx: &FieldContext,
) -> Result<Pixmap> {
    if target.0 == 0 || target.1 == 0 {
        return Err(Error::InvalidSize {
            width: target.0,
            height: target.1,
        });
    }

    match &spec.content {
        Content::Text(t) => text_layer(fonts, font_system, swash_cache, spec, t, target, ctx),
        Content::Image(i) => image_layer(&i.source, spec.size, target),
    }
}

fn text_layer(
    fonts: &FontLibrary,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    spec: &WatermarkSpec,
    text_spec: &TextSpec,
    target: (u32, u32),
    ctx: &FieldContext,
) -> Result<Pixmap> {
    let resolved = fields::render(&text_spec.template, ctx);
    if resolved.trim().is_empty() {
        return Err(Error::EmptyContent);
    }

    let family = fonts.resolve(&text_spec.family);
    let style_at = |font_size: f32| TextStyle {
        family,
        weight: text_spec.weight,
        italic: text_spec.italic,
        font_size,
        line_height: font_size * text_spec.line_height,
        color: text_spec.color,
        align: text_spec.align,
    };

    let font_size = match spec.size {
        // 字号本就是高度量，按画布高度取比例最符合直觉。
        SizeMode::RelativeFontSize(r) => target.1 as f32 * r,
        SizeMode::AbsoluteFontSize { px } => px,
        SizeMode::RelativeWidth(r) => {
            fit_font_to_width(font_system, &resolved, &style_at, target.0 as f32 * r)?
        }
        SizeMode::AbsoluteWidth { px } => {
            fit_font_to_width(font_system, &resolved, &style_at, px as f32)?
        }
    };

    if !font_size.is_finite() || font_size <= 0.0 || font_size > MAX_FONT_SIZE {
        return Err(Error::InvalidSize {
            width: font_size.max(0.0) as u32,
            height: font_size.max(0.0) as u32,
        });
    }

    let layer = text::rasterize(font_system, swash_cache, &resolved, &style_at(font_size))?;
    check_layer_size(layer.width(), layer.height())?;
    Ok(layer)
}

/// 反解出能让文本排版宽度达到 `want_width` 的字号。
fn fit_font_to_width<'a, F>(
    font_system: &mut FontSystem,
    text: &str,
    style_at: &F,
    want_width: f32,
) -> Result<f32>
where
    F: Fn(f32) -> TextStyle<'a>,
{
    if !want_width.is_finite() || want_width <= 0.0 {
        return Err(Error::EmptyContent);
    }
    let measured = text::measure(font_system, text, &style_at(REF_FONT_SIZE))?;
    if measured.width <= f32::EPSILON {
        return Err(Error::EmptyContent);
    }
    Ok(REF_FONT_SIZE * (want_width / measured.width))
}

fn image_layer(source: &ImageSource, size: SizeMode, target: (u32, u32)) -> Result<Pixmap> {
    match source {
        ImageSource::Raster(bytes) => {
            let decoded = image::load_from_memory(bytes)?.to_rgba8();
            let src = crate::convert::rgba_image_to_pixmap(&decoded)?;
            let (w, h) = fit_size(src.width(), src.height(), size, target)?;
            scale_pixmap(&src, w, h)
        }
        #[cfg(feature = "svg")]
        ImageSource::Svg(svg) => render_svg(svg, size, target),
    }
}

/// 按 [`SizeMode`] 求出图片水印的目标像素尺寸，保持原始宽高比。
///
/// `*FontSize` 两个变体对图片按**高度**解释 —— 字号本质是高度量，
/// 这样"FontSize 管高、Width 管宽"的语义在文字和图片之间是一致的。
fn fit_size(src_w: u32, src_h: u32, size: SizeMode, target: (u32, u32)) -> Result<(u32, u32)> {
    if src_w == 0 || src_h == 0 {
        return Err(Error::InvalidSize {
            width: src_w,
            height: src_h,
        });
    }
    let aspect = src_w as f32 / src_h as f32;

    let (w, h) = match size {
        SizeMode::RelativeWidth(r) => {
            let w = target.0 as f32 * r;
            (w, w / aspect)
        }
        SizeMode::AbsoluteWidth { px } => {
            let w = px as f32;
            (w, w / aspect)
        }
        SizeMode::RelativeFontSize(r) => {
            let h = target.1 as f32 * r;
            (h * aspect, h)
        }
        SizeMode::AbsoluteFontSize { px } => (px * aspect, px),
    };

    let w = w.round().max(1.0) as u32;
    let h = h.round().max(1.0) as u32;
    check_layer_size(w, h)?;
    Ok((w, h))
}

fn scale_pixmap(src: &Pixmap, w: u32, h: u32) -> Result<Pixmap> {
    if src.width() == w && src.height() == h {
        return Ok(src.clone());
    }
    let mut dst = Pixmap::new(w, h).ok_or(Error::InvalidSize {
        width: w,
        height: h,
    })?;
    let scale_x = w as f32 / src.width() as f32;
    let scale_y = h as f32 / src.height() as f32;

    dst.draw_pixmap(
        0,
        0,
        src.as_ref(),
        &PixmapPaint {
            // 默认是 Nearest，缩放后的 Logo 会明显毛刺。
            quality: FilterQuality::Bilinear,
            ..PixmapPaint::default()
        },
        Transform::from_scale(scale_x, scale_y),
        None,
    );
    Ok(dst)
}

#[cfg(feature = "svg")]
fn render_svg(svg: &str, size: SizeMode, target: (u32, u32)) -> Result<Pixmap> {
    let tree = resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default())
        .map_err(|e| Error::Svg(e.to_string()))?;
    let intrinsic = tree.size();
    let (w, h) = fit_size(
        intrinsic.width().ceil().max(1.0) as u32,
        intrinsic.height().ceil().max(1.0) as u32,
        size,
        target,
    )?;

    let mut pixmap = Pixmap::new(w, h).ok_or(Error::InvalidSize {
        width: w,
        height: h,
    })?;
    // 矢量图直接按目标尺寸光栅化，而不是渲染成固有尺寸再缩放 —— 这正是用 SVG 的意义。
    let scale = Transform::from_scale(w as f32 / intrinsic.width(), h as f32 / intrinsic.height());
    resvg::render(&tree, scale, &mut pixmap.as_mut());
    Ok(pixmap)
}

fn check_layer_size(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 || width > MAX_LAYER_SIDE || height > MAX_LAYER_SIDE {
        return Err(Error::InvalidSize { width, height });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Placement, Rgba8, TextAlign};

    fn text_spec(template: &str) -> TextSpec {
        TextSpec {
            template: template.to_owned(),
            color: Rgba8::WHITE,
            align: TextAlign::Left,
            ..TextSpec::default()
        }
    }

    fn spec_with(size: SizeMode, template: &str) -> WatermarkSpec {
        WatermarkSpec {
            content: Content::Text(text_spec(template)),
            size,
            placement: Placement::default(),
            opacity: 1.0,
            rotation_deg: 0.0,
        }
    }

    struct Env {
        lib: FontLibrary,
        fs: FontSystem,
        cache: SwashCache,
    }

    fn env() -> Env {
        let lib = FontLibrary::with_system_fonts();
        let fs = lib.font_system();
        Env {
            lib,
            fs,
            cache: SwashCache::new(),
        }
    }

    #[test]
    fn relative_width_hits_requested_width() {
        let mut e = env();
        let target = (1000, 800);
        let spec = spec_with(SizeMode::RelativeWidth(0.5), "Imprint");
        let layer = render_layer(
            &e.lib,
            &mut e.fs,
            &mut e.cache,
            &spec,
            target,
            &FieldContext::new(),
        )
        .expect("渲染图层");

        // 墨迹宽度与排版推进宽度有差异（字形两侧留白），允许 15% 偏差。
        let want = 500.0;
        let got = layer.width() as f32;
        assert!(
            (got - want).abs() / want < 0.15,
            "目标宽 {want}，实际 {got}"
        );
    }

    #[test]
    fn relative_font_size_scales_with_canvas_height() {
        let mut e = env();
        let spec = spec_with(SizeMode::RelativeFontSize(0.1), "Ag");

        let small = render_layer(
            &e.lib,
            &mut e.fs,
            &mut e.cache,
            &spec,
            (500, 500),
            &FieldContext::new(),
        )
        .unwrap();
        let large = render_layer(
            &e.lib,
            &mut e.fs,
            &mut e.cache,
            &spec,
            (1000, 1000),
            &FieldContext::new(),
        )
        .unwrap();

        // 画布高度翻倍，同一相对字号下的图层也应接近翻倍 ——
        // 这是"同一 spec 跨分辨率观感一致"的核心保证。
        let ratio = large.height() as f32 / small.height() as f32;
        assert!((ratio - 2.0).abs() < 0.2, "跨分辨率缩放比例异常: {ratio}");
    }

    #[test]
    fn dynamic_fields_are_evaluated() {
        let mut e = env();
        let spec = spec_with(SizeMode::AbsoluteFontSize { px: 32.0 }, "{filename}");
        let ctx = FieldContext::new().with_file_name("photo.jpg");

        let with_name =
            render_layer(&e.lib, &mut e.fs, &mut e.cache, &spec, (800, 600), &ctx).unwrap();
        // 模板求值为空时不应产出空白图层，而应明确报错。
        let empty = render_layer(
            &e.lib,
            &mut e.fs,
            &mut e.cache,
            &spec,
            (800, 600),
            &FieldContext::new(),
        );

        assert!(with_name.width() > 0);
        assert!(matches!(empty, Err(Error::EmptyContent)));
    }

    #[test]
    fn image_layer_preserves_aspect_ratio() {
        // 4:1 的纯色图，缩放后宽高比必须保持。
        let mut src = image::RgbaImage::new(400, 100);
        for p in src.pixels_mut() {
            *p = image::Rgba([255, 0, 0, 255]);
        }
        let mut png = std::io::Cursor::new(Vec::new());
        src.write_to(&mut png, image::ImageFormat::Png).unwrap();

        let layer = image_layer(
            &ImageSource::Raster(png.into_inner()),
            SizeMode::RelativeWidth(0.5),
            (1000, 1000),
        )
        .expect("图片图层");

        assert_eq!(layer.width(), 500);
        assert_eq!(layer.height(), 125, "宽高比未保持");
    }

    #[test]
    fn font_size_mode_drives_image_height() {
        let mut src = image::RgbaImage::new(400, 100);
        for p in src.pixels_mut() {
            *p = image::Rgba([0, 255, 0, 255]);
        }
        let mut png = std::io::Cursor::new(Vec::new());
        src.write_to(&mut png, image::ImageFormat::Png).unwrap();

        // *FontSize 对图片按高度解释。
        let layer = image_layer(
            &ImageSource::Raster(png.into_inner()),
            SizeMode::AbsoluteFontSize { px: 50.0 },
            (1000, 1000),
        )
        .unwrap();

        assert_eq!(layer.height(), 50);
        assert_eq!(layer.width(), 200);
    }

    #[test]
    fn rejects_zero_target() {
        let mut e = env();
        let spec = spec_with(SizeMode::RelativeWidth(0.5), "x");
        assert!(matches!(
            render_layer(
                &e.lib,
                &mut e.fs,
                &mut e.cache,
                &spec,
                (0, 100),
                &FieldContext::new()
            ),
            Err(Error::InvalidSize { .. })
        ));
    }

    #[test]
    fn rejects_absurd_absolute_size() {
        // 不加拦截的话 zeno 会在 u32 乘法上溢出：debug panic，release 静默回绕。
        let mut e = env();
        for px in [500_000.0, 100_000.0, MAX_FONT_SIZE + 1.0] {
            let spec = spec_with(SizeMode::AbsoluteFontSize { px }, "x");
            assert!(
                matches!(
                    render_layer(
                        &e.lib,
                        &mut e.fs,
                        &mut e.cache,
                        &spec,
                        (800, 600),
                        &FieldContext::new()
                    ),
                    Err(Error::InvalidSize { .. })
                ),
                "字号 {px} 未被拦截"
            );
        }
    }

    #[test]
    fn rejects_oversized_relative_width() {
        // 相对宽度模式同样会反解出巨大字号，必须走同一道拦截。
        let mut e = env();
        let spec = spec_with(SizeMode::AbsoluteWidth { px: 4_000_000 }, "x");
        assert!(
            render_layer(
                &e.lib,
                &mut e.fs,
                &mut e.cache,
                &spec,
                (800, 600),
                &FieldContext::new()
            )
            .is_err()
        );
    }
}
