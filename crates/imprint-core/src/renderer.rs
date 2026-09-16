//! `Renderer`：把字体系统、图层渲染与合成串成一条可复用的管线。
//!
//! 每个线程持有一个 `Renderer`（`FontSystem` 不是 `Sync`），但它们共享同一份
//! [`FontLibrary`] —— 系统字体扫描在 release 下可达一秒，逐线程重扫会让批量场景
//! 被字体加载拖垮。

use std::{
    fs::File,
    io::{BufWriter, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use cosmic_text::{FontSystem, SwashCache};
use tiny_skia::Pixmap;

use crate::{
    batch::CancelToken,
    compose,
    convert::{pixmap_to_rgba_image, rgba_image_to_pixmap},
    error::{Error, Result},
    fields::{ExifFields, FieldContext},
    fonts::FontLibrary,
    image_job::{self, OutputFormat},
    layer,
    spec::WatermarkSpec,
    video::{EncodeSettings, VideoJob, VideoPipeline, VideoProgress},
};

/// 图片输出选项。
#[derive(Debug, Clone)]
pub struct ImageOptions {
    pub format: OutputFormat,
    /// 解码期允许的内存上限（字节）。`None` 表示不设限。
    pub max_alloc_bytes: Option<u64>,
    /// 是否把源图的 EXIF / ICC 搬运到输出。
    pub keep_metadata: bool,
}

impl Default for ImageOptions {
    fn default() -> Self {
        Self {
            format: OutputFormat::default(),
            // 512 MiB 对应约 11000×11000 的 RGBA，覆盖绝大多数相机原图，
            // 又能在遇到病态尺寸时及时报错而不是 OOM。
            max_alloc_bytes: Some(512 * 1024 * 1024),
            keep_metadata: true,
        }
    }
}

/// 水印渲染管线。
pub struct Renderer {
    fonts: Arc<FontLibrary>,
    font_system: FontSystem,
    swash_cache: SwashCache,
}

impl Renderer {
    /// 基于共享字体库创建一个渲染器。
    ///
    /// rayon 批处理时每个工作线程各建一个，`Arc<FontLibrary>` 可安全共享。
    pub fn new(fonts: Arc<FontLibrary>) -> Self {
        let font_system = fonts.font_system();
        Self {
            fonts,
            font_system,
            swash_cache: SwashCache::new(),
        }
    }

    pub fn fonts(&self) -> &FontLibrary {
        &self.fonts
    }

    /// 渲染一张裸水印图层（未定位、未旋转、未施加透明度）。
    ///
    /// 同一张图层会被预览、图片合成、视频 overlay 三处共用 ——
    /// 这正是预览与输出能做到像素级一致的原因。
    pub fn render_layer(
        &mut self,
        spec: &WatermarkSpec,
        target: (u32, u32),
        ctx: &FieldContext,
    ) -> Result<Pixmap> {
        layer::render_layer(
            &self.fonts,
            &mut self.font_system,
            &mut self.swash_cache,
            spec,
            target,
            ctx,
        )
    }

    /// 给一张图片加水印。
    ///
    /// `ctx` 由调用方提供文件名一类的外部信息；图片尺寸与 EXIF 字段在内部补全，
    /// 因为它们要解码之后才知道。
    pub fn watermark_image<R: Read + Seek, W: Write>(
        &mut self,
        src: R,
        dst: W,
        spec: &WatermarkSpec,
        options: &ImageOptions,
        ctx: &FieldContext,
    ) -> Result<()> {
        let decoded = image_job::decode(src, options.max_alloc_bytes)?;
        let (width, height) = decoded.image.dimensions();

        let ctx =
            ctx.clone()
                .with_dimensions(width, height)
                .with_exif(match &decoded.metadata.exif {
                    Some(raw) => ExifFields::parse(raw),
                    None => ExifFields::default(),
                });

        let mut spec = spec.clone();
        spec.sanitize();

        let layer = self.render_layer(&spec, (width, height), &ctx)?;

        let mut canvas = rgba_image_to_pixmap(&decoded.image)?;
        compose::compose(
            &mut canvas,
            &layer,
            &spec.placement,
            spec.opacity,
            spec.rotation_deg,
        )?;
        let out = pixmap_to_rgba_image(canvas)?;

        let metadata = if options.keep_metadata {
            decoded.metadata
        } else {
            image_job::Metadata::default()
        };
        image_job::encode(&out, dst, options.format, &metadata)
    }

    /// 渲染与视频等尺寸的透明水印图层，编码成 PNG 字节。
    ///
    /// 视频水印不靠 ffmpeg 的 overlay 表达式定位，而是把整幅图层交给它贴在原点：
    /// 这样平铺、旋转、锚点、透明度全部复用 [`crate::compose`] 那一份代码，
    /// 视频输出与图片输出、UI 预览三者天然一致。
    pub fn render_video_overlay(
        &mut self,
        spec: &WatermarkSpec,
        size: (u32, u32),
        ctx: &FieldContext,
    ) -> Result<Vec<u8>> {
        let mut spec = spec.clone();
        spec.sanitize();

        let layer = self.render_layer(&spec, size, ctx)?;
        let mut canvas = tiny_skia::Pixmap::new(size.0, size.1).ok_or(Error::InvalidSize {
            width: size.0,
            height: size.1,
        })?;
        compose::compose(
            &mut canvas,
            &layer,
            &spec.placement,
            spec.opacity,
            spec.rotation_deg,
        )?;

        let rgba = pixmap_to_rgba_image(canvas)?;
        let mut png = Vec::new();
        // 必须是 PNG：全画幅图层大部分是透明的，JPEG 没有 alpha 通道。
        image_job::encode(
            &rgba,
            &mut png,
            OutputFormat::Png,
            &image_job::Metadata::default(),
        )?;
        Ok(png)
    }

    /// 给一段视频加水印。
    ///
    /// `pipeline` 决定后端（桌面用 `SidecarPipeline`；移动端将来换 FFI 或平台原生）。
    #[allow(clippy::too_many_arguments)]
    pub fn watermark_video(
        &mut self,
        pipeline: &dyn VideoPipeline,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
        spec: &WatermarkSpec,
        encode: &EncodeSettings,
        progress: &mut dyn FnMut(VideoProgress),
        cancel: &CancelToken,
    ) -> Result<()> {
        let src = src.as_ref();
        let info = pipeline.probe(src)?;

        let mut ctx = FieldContext::new().with_dimensions(info.width, info.height);
        if let Some(name) = src.file_name().and_then(|n| n.to_str()) {
            ctx = ctx.with_file_name(name);
        }

        let png = self.render_video_overlay(spec, (info.width, info.height), &ctx)?;
        let overlay = TempPng::create(png)?;

        pipeline.render(
            &VideoJob {
                src: src.to_path_buf(),
                dst: dst.as_ref().to_path_buf(),
                overlay_png: overlay.path().to_path_buf(),
                encode: encode.clone(),
            },
            progress,
            cancel,
        )
    }

    /// [`Self::watermark_image`] 的文件路径版本。
    ///
    /// 桌面端用它即可；移动端走上面那个 I/O 无关的接口，
    /// 因为 Android 拿到的是 `content://` URI 而不是路径。
    pub fn watermark_image_file(
        &mut self,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
        spec: &WatermarkSpec,
        options: &ImageOptions,
    ) -> Result<()> {
        let src = src.as_ref();
        let ctx = match src.file_name().and_then(|n| n.to_str()) {
            Some(name) => FieldContext::new().with_file_name(name),
            None => FieldContext::new(),
        };

        let input = File::open(src)?;
        let output = BufWriter::new(File::create(dst)?);
        self.watermark_image(input, output, spec, options, &ctx)
    }
}

/// 自动清理的临时 PNG。
///
/// 只为写一个临时文件引入 `tempfile` 依赖不划算：这里的用法很窄
/// （自己写、交给子进程读、用完即删），二十行足够。
struct TempPng(PathBuf);

impl TempPng {
    fn create(bytes: Vec<u8>) -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);

        // pid + 单调序号：同进程多线程、以及多进程并发都不会撞名。
        let name = format!(
            "imprint_overlay_{}_{}.png",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, bytes)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPng {
    fn drop(&mut self) {
        // 清理失败只是留个临时文件，不值得打断调用方。
        if let Err(e) = std::fs::remove_file(&self.0) {
            log::debug!("临时 overlay 清理失败 {}: {e}", self.0.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Anchor9, Content, Margin, Placement, Rgba8, SizeMode, TextAlign, TextSpec};
    use image::RgbaImage;
    use std::io::Cursor;

    fn renderer() -> Renderer {
        Renderer::new(Arc::new(FontLibrary::with_system_fonts()))
    }

    fn text_spec(template: &str) -> WatermarkSpec {
        WatermarkSpec {
            content: Content::Text(TextSpec {
                template: template.to_owned(),
                color: Rgba8::new(255, 0, 0, 255),
                align: TextAlign::Left,
                ..TextSpec::default()
            }),
            size: SizeMode::RelativeFontSize(0.1),
            placement: Placement::Anchor {
                anchor: Anchor9::BottomRight,
                margin: Margin::default(),
            },
            opacity: 1.0,
            rotation_deg: 0.0,
        }
    }

    fn black_png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 255]));
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[test]
    fn layer_rendering_is_deterministic() {
        // 相同输入必须产出相同尺寸的图层 —— 预览与导出走的是同一条渲染路径，
        // 这里不稳定的话，两边就会对不上。
        let mut r = renderer();
        let spec = text_spec("STABLE");
        let ctx = FieldContext::new();

        let a = r.render_layer(&spec, (800, 600), &ctx).unwrap();
        let b = r.render_layer(&spec, (800, 600), &ctx).unwrap();
        assert_eq!(a.width(), b.width());
        assert_eq!(a.height(), b.height());
        assert_eq!(a.data(), b.data(), "相同输入产出了不同像素");
    }

    #[test]
    fn dynamic_fields_produce_distinct_layers() {
        // 批量处理里最容易出的错：`{filename}` 逐文件不同，
        // 任何一层缓存若漏掉求值后的文本，整批产物都会带上第一个文件的名字。
        let mut r = renderer();
        let spec = text_spec("{filename}");

        let a = r
            .render_layer(
                &spec,
                (800, 600),
                &FieldContext::new().with_file_name("first.jpg"),
            )
            .unwrap();
        let b = r
            .render_layer(
                &spec,
                (800, 600),
                &FieldContext::new().with_file_name("second_much_longer.jpg"),
            )
            .unwrap();
        assert_ne!(a.width(), b.width(), "不同文件名应当产出不同宽度的图层");
    }

    #[test]
    fn video_overlay_is_full_frame_png_with_transparency() {
        let mut r = renderer();
        let size = (640, 360);
        let png = r
            .render_video_overlay(&text_spec("VIDEO"), size, &FieldContext::new())
            .expect("渲染 overlay");

        let decoded = image_job::decode(Cursor::new(&png), None).unwrap().image;
        // 必须是全画幅：ffmpeg 会把它贴在原点，尺寸不符就会错位。
        assert_eq!(decoded.dimensions(), size);

        // 左上角应当完全透明（水印在右下角），否则会糊住整个画面。
        assert_eq!(decoded.get_pixel(5, 5).0[3], 0, "overlay 背景必须透明");
        // 右下角应当有不透明的水印像素。
        let has_ink = (size.1 / 2..size.1)
            .flat_map(|y| (size.0 / 2..size.0).map(move |x| (x, y)))
            .any(|(x, y)| decoded.get_pixel(x, y).0[3] > 0);
        assert!(has_ink, "overlay 上没有水印");
    }

    #[test]
    fn temp_overlay_is_removed_on_drop() {
        let path = {
            let tmp = TempPng::create(vec![1, 2, 3]).unwrap();
            let p = tmp.path().to_path_buf();
            assert!(p.exists());
            p
        };
        assert!(!path.exists(), "临时 overlay 未被清理");
    }

    #[test]
    fn watermarks_an_image_end_to_end() {
        let mut r = renderer();
        let src = black_png(400, 300);
        let mut out = Vec::new();

        r.watermark_image(
            Cursor::new(&src),
            &mut out,
            &text_spec("TEST"),
            &ImageOptions {
                format: OutputFormat::Png,
                ..ImageOptions::default()
            },
            &FieldContext::new(),
        )
        .expect("加水印");

        let result = image_job::decode(Cursor::new(&out), None).unwrap().image;
        assert_eq!(result.dimensions(), (400, 300), "输出尺寸必须与源图一致");

        // 源图纯黑，水印是红色，右下角必须出现红色像素。
        let red_in_bottom_right = (150..300)
            .flat_map(|y| (200..400).map(move |x| (x, y)))
            .any(|(x, y)| {
                let p = result.get_pixel(x, y).0;
                p[0] > 100 && p[1] < 60
            });
        assert!(red_in_bottom_right, "右下角未出现水印");

        // 左上角应当保持原样。
        assert_eq!(result.get_pixel(5, 5).0, [0, 0, 0, 255]);
    }

    #[test]
    fn same_spec_lands_consistently_across_resolutions() {
        // 同一 spec 在不同分辨率上，水印重心的归一化位置必须一致 ——
        // 这是"预览调好 → 全尺寸导出"不跑偏的保证。
        let mut r = renderer();
        let spec = WatermarkSpec {
            placement: Placement::Normalized {
                x: 0.3,
                y: 0.7,
                anchor: Anchor9::Center,
            },
            ..text_spec("XY")
        };

        let centroid_at = |r: &mut Renderer, w: u32, h: u32| -> (f32, f32) {
            let src = black_png(w, h);
            let mut out = Vec::new();
            r.watermark_image(
                Cursor::new(&src),
                &mut out,
                &spec,
                &ImageOptions {
                    format: OutputFormat::Png,
                    ..ImageOptions::default()
                },
                &FieldContext::new(),
            )
            .unwrap();
            let img = image_job::decode(Cursor::new(&out), None).unwrap().image;

            let (mut sx, mut sy, mut n) = (0.0f64, 0.0f64, 0u64);
            for (x, y, p) in img.enumerate_pixels() {
                if p.0[0] > 100 {
                    sx += x as f64;
                    sy += y as f64;
                    n += 1;
                }
            }
            assert!(n > 0);
            (
                (sx / n as f64) as f32 / w as f32,
                (sy / n as f64) as f32 / h as f32,
            )
        };

        let small = centroid_at(&mut r, 400, 400);
        let large = centroid_at(&mut r, 1600, 1600);

        assert!(
            (small.0 - large.0).abs() < 0.01 && (small.1 - large.1).abs() < 0.01,
            "跨分辨率水印位置漂移: {small:?} vs {large:?}"
        );
        assert!((small.0 - 0.3).abs() < 0.03 && (small.1 - 0.7).abs() < 0.03);
    }

    #[test]
    fn dynamic_filename_field_is_filled_by_file_helper() {
        let dir = std::env::temp_dir().join("imprint_rs_test_dyn");
        std::fs::create_dir_all(&dir).unwrap();
        let src_path = dir.join("holiday.png");
        let dst_path = dir.join("out.png");
        std::fs::write(&src_path, black_png(600, 200)).unwrap();

        let mut r = renderer();
        r.watermark_image_file(
            &src_path,
            &dst_path,
            &text_spec("{filename}"),
            &ImageOptions {
                format: OutputFormat::Png,
                ..ImageOptions::default()
            },
        )
        .expect("按路径加水印");

        let out = image::open(&dst_path).unwrap().to_rgba8();
        assert_eq!(out.dimensions(), (600, 200));
        assert!(
            out.pixels().any(|p| p.0[0] > 100),
            "模板 {{filename}} 未渲染出可见水印"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn jpeg_output_preserves_exif() {
        // 造一张带 EXIF 的 JPEG：先写出带 EXIF 的源图，加水印后 EXIF 应当仍在。
        let img = RgbaImage::from_pixel(120, 80, image::Rgba([10, 10, 10, 255]));
        let mut src = Vec::new();
        {
            use image::ImageEncoder;
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut src, 90);
            // 最小合法 EXIF：小端 TIFF header + 0 个 IFD 条目。
            let exif: Vec<u8> = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            enc.set_exif_metadata(exif).unwrap();
            enc.write_image(
                &image::DynamicImage::ImageRgba8(img).to_rgb8(),
                120,
                80,
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        }

        let before = image_job::decode(Cursor::new(&src), None).unwrap();
        assert!(before.metadata.exif.is_some(), "测试素材本身应当带 EXIF");

        let mut r = renderer();
        let mut out = Vec::new();
        r.watermark_image(
            Cursor::new(&src),
            &mut out,
            &text_spec("C"),
            &ImageOptions::default(),
            &FieldContext::new(),
        )
        .unwrap();

        let after = image_job::decode(Cursor::new(&out), None).unwrap();
        assert!(after.metadata.exif.is_some(), "EXIF 在加水印后丢失了");
    }

    #[test]
    fn metadata_can_be_stripped_on_request() {
        let img = RgbaImage::from_pixel(60, 40, image::Rgba([10, 10, 10, 255]));
        let mut src = Vec::new();
        {
            use image::ImageEncoder;
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut src, 90);
            enc.set_exif_metadata(vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0])
                .unwrap();
            enc.write_image(
                &image::DynamicImage::ImageRgba8(img).to_rgb8(),
                60,
                40,
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        }

        let mut r = renderer();
        let mut out = Vec::new();
        r.watermark_image(
            Cursor::new(&src),
            &mut out,
            &text_spec("C"),
            &ImageOptions {
                keep_metadata: false,
                ..ImageOptions::default()
            },
            &FieldContext::new(),
        )
        .unwrap();

        let after = image_job::decode(Cursor::new(&out), None).unwrap();
        assert!(after.metadata.exif.is_none(), "要求剥离时 EXIF 不该保留");
    }
}
