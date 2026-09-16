//! 文字光栅化：把一段文本渲染成刚好包住墨迹的 premultiplied RGBA 位图。
//!
//! 刻意不走 `Buffer::draw`：它的回调是**每像素**触发一次（`cosmic-text` 内部的
//! `LegacyRenderer::glyph` 把宽高硬编码成 `1, 1`），对水印这种成百上千字形的场景
//! 会慢上一两个数量级。这里改为直接遍历 `layout_runs()`，用 `SwashCache::get_image`
//! 取出整块字形位图再一次性 blit。
//!
//! 同样不实现 `cosmic_text::Renderer` trait：`Buffer::render` 会独占
//! `&mut FontSystem`，而 trait 的 `glyph()` 又必须拿它去调 `get_image`，
//! 二者借用冲突。owned `Buffer` 的 `layout_runs()` 只借 `&self`，正好绕开。

use cosmic_text::{
    Align, Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, SwashCache, SwashContent,
    Weight, Wrap,
};
use tiny_skia::{Pixmap, PremultipliedColorU8};

use crate::{
    error::{Error, Result},
    spec::{Rgba8, TextAlign},
};

/// 一次光栅化的已解析样式。
///
/// 这里全是绝对像素 —— 相对量（`SizeMode`）与模板字段必须在调用之前求值完毕，
/// 这样本模块不必知道素材尺寸，也就能被预览和导出两条路径共用。
#[derive(Debug, Clone)]
pub struct TextStyle<'a> {
    pub family: Family<'a>,
    /// CSS 数值字重，400 常规 / 700 粗体。
    pub weight: u16,
    pub italic: bool,
    pub font_size: f32,
    /// 行高，绝对像素。
    pub line_height: f32,
    pub color: Rgba8,
    pub align: TextAlign,
}

/// 文本的排版尺寸测量结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextMetrics {
    /// 最宽一行的推进宽度。
    ///
    /// 这是排版推进量，不是墨迹包围盒：斜体、装饰性字形的墨迹可能超出它。
    /// 按宽度百分比换算缩放比例用它足够，需要精确边界时看 [`rasterize`] 的产物。
    pub width: f32,
    /// 全部行的总高度。
    pub height: f32,
}

/// 测量文本在给定样式下的排版尺寸。
///
/// `SizeMode::RelativeWidth` 靠它换算：先在参考字号下测一次，
/// 再按 `目标宽 / 实测宽` 求出缩放比例。
pub fn measure(
    font_system: &mut FontSystem,
    text: &str,
    style: &TextStyle<'_>,
) -> Result<TextMetrics> {
    ensure_usable_fonts(font_system)?;
    let buffer = build_buffer(font_system, text, style);
    Ok(measure_buffer(&buffer))
}

/// 在进入排版前确认字体库可用。
///
/// cosmic-text 找不到任何可用字体时会**直接 panic**
/// （`shape.rs` 里的 `font_iter.next().expect("no default font found")`），
/// 而不是降级到 `.notdef`。空字体库正是移动端最常见的处境 —— 那里既没有系统字体
/// 扫描，平台回退表又是空的 —— 所以必须在这里拦住，否则整个 App 会崩。
///
/// 注意这只是必要条件：字体库非空、但请求的字族无法匹配且平台回退表为空时，
/// 仍有残余的 panic 风险。桌面端回退表非空，实际触发不到。
fn ensure_usable_fonts(font_system: &mut FontSystem) -> Result<()> {
    if font_system.db_mut().is_empty() {
        return Err(Error::Font(
            "字体库为空，无法排版：请先用 FontLibrary::add_font 注入字体".to_owned(),
        ));
    }
    Ok(())
}

fn measure_buffer(buffer: &Buffer) -> TextMetrics {
    let mut width: f32 = 0.0;
    let mut height: f32 = 0.0;
    for run in buffer.layout_runs() {
        width = width.max(run.line_w);
        height = height.max(run.line_top + run.line_height);
    }
    TextMetrics { width, height }
}

/// 把文本光栅化成一块刚好包住墨迹的 premultiplied RGBA 位图。
///
/// 返回的位图尺寸由实际墨迹决定，不含排版留白 —— 这样水印定位就是对可见像素定位，
/// 不会因为字体的行距设置而莫名偏移。
pub fn rasterize(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    text: &str,
    style: &TextStyle<'_>,
) -> Result<Pixmap> {
    if text.trim().is_empty() {
        return Err(Error::EmptyContent);
    }
    ensure_usable_fonts(font_system)?;

    let buffer = build_buffer(font_system, text, style);

    // 第一遍：求墨迹包围盒。走的是 SwashCache，第二遍会全部命中缓存。
    let bounds = ink_bounds(&buffer, font_system, swash_cache).ok_or(Error::EmptyContent)?;
    let (min_x, min_y, max_x, max_y) = bounds;

    let width = (max_x - min_x) as u32;
    let height = (max_y - min_y) as u32;
    let mut pixmap = Pixmap::new(width, height).ok_or(Error::InvalidSize { width, height })?;

    // 第二遍：逐字形整块 blit。
    for run in buffer.layout_runs() {
        for glyph in run.glyphs {
            let physical = glyph.physical((0.0, run.line_y), 1.0);
            let Some(image) = swash_cache.get_image(font_system, physical.cache_key) else {
                continue;
            };
            let place = image.placement;
            if place.width == 0 || place.height == 0 {
                continue;
            }

            // 偏移语义取自 cosmic-text 的 `SwashCache::with_pixels`：
            // x 加 placement.left，y **减** placement.top。
            let dst_x = physical.x + place.left - min_x;
            let dst_y = physical.y - place.top - min_y;

            match image.content {
                SwashContent::Mask => {
                    blit_mask(&mut pixmap, &image.data, place, dst_x, dst_y, style.color);
                }
                SwashContent::Color => {
                    blit_color(&mut pixmap, &image.data, place, dst_x, dst_y, style.color.a);
                }
                // cosmic-text 自身在这个分支也只打了一行 TODO，没有可参考的实现。
                SwashContent::SubpixelMask => {
                    log::warn!("字形使用 SubpixelMask，当前不支持，已跳过");
                }
            }
        }
    }

    Ok(pixmap)
}

fn build_buffer(font_system: &mut FontSystem, text: &str, style: &TextStyle<'_>) -> Buffer {
    // set_metrics 内部对 0 直接 assert，越界值必须在这里挡住而不是让它 panic。
    let font_size = if style.font_size.is_finite() && style.font_size > 0.0 {
        style.font_size
    } else {
        16.0
    };
    let line_height = if style.line_height.is_finite() && style.line_height > 0.0 {
        style.line_height
    } else {
        font_size * 1.2
    };

    let mut buffer = Buffer::new(font_system, Metrics::new(font_size, line_height));
    // 水印不做自动换行：宽度不设限，换行完全由用户在文本里打 `\n` 控制。
    // 自动换行会让水印宽度随素材尺寸跳变，破坏"同一 spec 跨分辨率一致"的前提。
    buffer.set_size(None, None);
    buffer.set_wrap(Wrap::None);

    let attrs = Attrs {
        family: style.family,
        weight: Weight(style.weight),
        style: if style.italic {
            Style::Italic
        } else {
            Style::Normal
        },
        ..Attrs::new()
    };

    let align = match style.align {
        TextAlign::Left => Align::Left,
        TextAlign::Center => Align::Center,
        TextAlign::Right => Align::Right,
    };

    // Shaping::Advanced 是中文的硬要求：Basic 明确不会去系统字体里找缺失字形。
    buffer.set_text(text, &attrs, Shaping::Advanced, Some(align));
    buffer.shape_until_scroll(font_system, false);
    buffer
}

/// 求所有字形墨迹的并集包围盒，返回 `(min_x, min_y, max_x, max_y)`。
fn ink_bounds(
    buffer: &Buffer,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
) -> Option<(i32, i32, i32, i32)> {
    let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
    let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);
    let mut any = false;

    for run in buffer.layout_runs() {
        for glyph in run.glyphs {
            let physical = glyph.physical((0.0, run.line_y), 1.0);
            let Some(image) = swash_cache.get_image(font_system, physical.cache_key) else {
                continue;
            };
            let place = image.placement;
            if place.width == 0 || place.height == 0 {
                continue;
            }

            let x0 = physical.x + place.left;
            let y0 = physical.y - place.top;
            min_x = min_x.min(x0);
            min_y = min_y.min(y0);
            max_x = max_x.max(x0 + place.width as i32);
            max_y = max_y.max(y0 + place.height as i32);
            any = true;
        }
    }

    any.then_some((min_x, min_y, max_x, max_y))
}

/// blit 单通道 alpha 字形，用样式颜色着色。
fn blit_mask(
    pixmap: &mut Pixmap,
    data: &[u8],
    place: cosmic_text::Placement,
    dst_x: i32,
    dst_y: i32,
    color: Rgba8,
) {
    let pw = pixmap.width() as i32;
    let ph = pixmap.height() as i32;
    let pixels = pixmap.pixels_mut();

    for row in 0..place.height as i32 {
        let y = dst_y + row;
        if y < 0 || y >= ph {
            continue;
        }
        for col in 0..place.width as i32 {
            let x = dst_x + col;
            if x < 0 || x >= pw {
                continue;
            }
            let Some(&mask) = data.get((row * place.width as i32 + col) as usize) else {
                continue;
            };
            if mask == 0 {
                continue;
            }
            // 字形覆盖率 × 样式自身的 alpha
            let a = mul255(mask, color.a);
            if a == 0 {
                continue;
            }
            let src = premultiply(color.r, color.g, color.b, a);
            let idx = (y * pw + x) as usize;
            pixels[idx] = source_over(src, pixels[idx]);
        }
    }
}

/// blit 彩色字形（emoji）。
///
/// swash 给的是 straight RGBA（对照 cosmic-text 的 `with_pixels`，它直接喂给了
/// 非预乘的 `Color::rgba`），写进 tiny-skia 之前必须自己预乘。
fn blit_color(
    pixmap: &mut Pixmap,
    data: &[u8],
    place: cosmic_text::Placement,
    dst_x: i32,
    dst_y: i32,
    style_alpha: u8,
) {
    let pw = pixmap.width() as i32;
    let ph = pixmap.height() as i32;
    let pixels = pixmap.pixels_mut();

    for row in 0..place.height as i32 {
        let y = dst_y + row;
        if y < 0 || y >= ph {
            continue;
        }
        for col in 0..place.width as i32 {
            let x = dst_x + col;
            if x < 0 || x >= pw {
                continue;
            }
            let base = ((row * place.width as i32 + col) * 4) as usize;
            let Some(chunk) = data.get(base..base + 4) else {
                continue;
            };
            let a = mul255(chunk[3], style_alpha);
            if a == 0 {
                continue;
            }
            let src = premultiply(chunk[0], chunk[1], chunk[2], a);
            let idx = (y * pw + x) as usize;
            pixels[idx] = source_over(src, pixels[idx]);
        }
    }
}

/// 定点数的 `a * b / 255`，带四舍五入。
#[inline]
fn mul255(a: u8, b: u8) -> u8 {
    let t = a as u32 * b as u32 + 128;
    ((t + (t >> 8)) >> 8) as u8
}

#[inline]
fn premultiply(r: u8, g: u8, b: u8, a: u8) -> PremultipliedColorU8 {
    // 预乘后各通道必然 <= a，构造不会失败；真失败也只应丢掉这个像素而非 panic。
    PremultipliedColorU8::from_rgba(mul255(r, a), mul255(g, a), mul255(b, a), a)
        .unwrap_or(PremultipliedColorU8::TRANSPARENT)
}

/// 预乘空间下的 source-over：`dst = src + dst * (1 - src_a)`。
///
/// 字形之间可能重叠（连笔、组合记号），直接覆盖会啃掉下层像素。
#[inline]
fn source_over(src: PremultipliedColorU8, dst: PremultipliedColorU8) -> PremultipliedColorU8 {
    if src.alpha() == 255 || dst.alpha() == 0 {
        return src;
    }
    let inv = 255 - src.alpha();
    PremultipliedColorU8::from_rgba(
        src.red() + mul255(dst.red(), inv),
        src.green() + mul255(dst.green(), inv),
        src.blue() + mul255(dst.blue(), inv),
        src.alpha() + mul255(dst.alpha(), inv),
    )
    .unwrap_or(src)
}

/// 文本在给定样式下是否存在无法渲染的字形。
///
/// 字体回退全部失败时字形会落到 `.notdef`（glyph id 0），显示成俗称的"豆腐块"。
/// 移动端尤其需要这个检查：cosmic-text 在 Android 的回退表是空的，
/// 在 iOS 指向的是一串 Linux 字族名，中文极易整段落到 `.notdef`。
pub fn has_missing_glyphs(font_system: &mut FontSystem, text: &str, style: &TextStyle<'_>) -> bool {
    // 空库时 shape 会 panic，而"全部字形都缺失"本就是这种情况的正确答案。
    if ensure_usable_fonts(font_system).is_err() {
        return true;
    }
    let buffer = build_buffer(font_system, text, style);
    buffer
        .layout_runs()
        .any(|run| run.glyphs.iter().any(|g| g.glyph_id == 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fonts::FontLibrary;
    use crate::spec::FontFamily;

    fn style_with(family: Family<'_>, size: f32) -> TextStyle<'_> {
        TextStyle {
            family,
            weight: 400,
            italic: false,
            font_size: size,
            line_height: size * 1.2,
            color: Rgba8::WHITE,
            align: TextAlign::Left,
        }
    }

    fn ink_ratio(pm: &Pixmap) -> f32 {
        let total = pm.pixels().len();
        let ink = pm.pixels().iter().filter(|p| p.alpha() > 0).count();
        ink as f32 / total as f32
    }

    #[test]
    fn rasterizes_ascii() {
        let lib = FontLibrary::with_system_fonts();
        let mut fs = lib.font_system();
        let mut cache = SwashCache::new();

        let style = style_with(Family::SansSerif, 48.0);
        let pm = rasterize(&mut fs, &mut cache, "Imprint", &style).expect("光栅化");

        assert!(pm.width() > 0 && pm.height() > 0);
        // 墨迹应占据合理比例：太低说明基本没画上，太高说明整块糊了。
        let ratio = ink_ratio(&pm);
        assert!(ratio > 0.05, "墨迹覆盖率过低: {ratio}");
        assert!(ratio < 0.95, "墨迹覆盖率过高: {ratio}");
    }

    #[test]
    fn rasterizes_chinese_without_tofu() {
        let lib = FontLibrary::with_system_fonts();
        let mut fs = lib.font_system();
        let mut cache = SwashCache::new();
        let style = style_with(Family::SansSerif, 48.0);

        if has_missing_glyphs(&mut fs, "水印", &style) {
            eprintln!("跳过：本机无可渲染中文的字体");
            return;
        }

        let pm = rasterize(&mut fs, &mut cache, "水印测试", &style).expect("光栅化");
        assert!(pm.width() > 0 && pm.height() > 0);
        assert!(ink_ratio(&pm) > 0.05);

        // 四个方块汉字应当明显宽于高（单行），且宽度约等于 4 个字宽。
        assert!(pm.width() > pm.height(), "四字单行应当更宽");
    }

    #[test]
    fn rasterizes_color_emoji() {
        let lib = FontLibrary::with_system_fonts();
        let mut fs = lib.font_system();
        let mut cache = SwashCache::new();
        let style = style_with(Family::SansSerif, 48.0);

        if has_missing_glyphs(&mut fs, "🎨", &style) {
            eprintln!("跳过：本机无 emoji 字体");
            return;
        }

        let pm = rasterize(&mut fs, &mut cache, "🎨", &style).expect("光栅化");
        assert!(pm.width() > 0 && pm.height() > 0);

        // 彩色 emoji 走 SwashContent::Color 分支，颜色来自字体而非样式。
        // 样式给的是纯白，所以只要出现非灰度像素，就证明确实走了彩色路径。
        let has_chroma = pm
            .pixels()
            .iter()
            .any(|p| p.alpha() > 0 && (p.red() != p.green() || p.green() != p.blue()));
        assert!(has_chroma, "彩色 emoji 应当产生非灰度像素");
    }

    #[test]
    fn measure_scales_linearly_with_font_size() {
        let lib = FontLibrary::with_system_fonts();
        let mut fs = lib.font_system();

        let small = measure(&mut fs, "Imprint", &style_with(Family::SansSerif, 20.0)).unwrap();
        let large = measure(&mut fs, "Imprint", &style_with(Family::SansSerif, 40.0)).unwrap();

        assert!(small.width > 0.0 && large.width > 0.0);
        // 字号翻倍，推进宽度应当接近翻倍 —— 这是 RelativeWidth 换算成立的前提。
        let ratio = large.width / small.width;
        assert!((ratio - 2.0).abs() < 0.15, "宽度缩放比例异常: {ratio}");
    }

    #[test]
    fn multiline_is_taller_than_single_line() {
        let lib = FontLibrary::with_system_fonts();
        let mut fs = lib.font_system();
        let style = style_with(Family::SansSerif, 24.0);

        let one = measure(&mut fs, "line", &style).unwrap();
        let two = measure(&mut fs, "line\nline", &style).unwrap();
        assert!(two.height > one.height * 1.5, "多行高度未累加");
    }

    #[test]
    fn empty_text_is_rejected() {
        let lib = FontLibrary::with_system_fonts();
        let mut fs = lib.font_system();
        let mut cache = SwashCache::new();
        let style = style_with(Family::SansSerif, 24.0);

        assert!(matches!(
            rasterize(&mut fs, &mut cache, "   ", &style),
            Err(Error::EmptyContent)
        ));
    }

    #[test]
    fn empty_font_library_errors_instead_of_panicking() {
        // cosmic-text 在空库上 shape 会 panic，core 必须把它转成可处理的错误。
        let lib = FontLibrary::without_system_fonts("zh-CN");
        let mut fs = lib.font_system();
        let mut cache = SwashCache::new();
        let style = style_with(Family::SansSerif, 32.0);

        assert!(matches!(
            rasterize(&mut fs, &mut cache, "水印", &style),
            Err(Error::Font(_))
        ));
        assert!(matches!(
            measure(&mut fs, "水印", &style),
            Err(Error::Font(_))
        ));
    }

    #[test]
    fn missing_glyphs_detected_with_empty_font_library() {
        // 空字体库 = 移动端未注入字体的处境，必须能被检出而不是静默出豆腐块。
        let lib = FontLibrary::without_system_fonts("zh-CN");
        let mut fs = lib.font_system();
        let family = lib.resolve(&FontFamily::SansSerif);
        assert!(has_missing_glyphs(
            &mut fs,
            "水印",
            &style_with(family, 32.0)
        ));
    }
}
