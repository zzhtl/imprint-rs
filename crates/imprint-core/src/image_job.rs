//! 图片管线：解码 → 元数据提取 → 合成 → 编码。
//!
//! 接口收 `Read + Seek` / `Write` 而非 `&Path`：Android 的文件选择返回
//! `content://` URI，根本没有文件系统路径可用。便利的路径版本在 [`crate::Renderer`]。

use std::io::{BufReader, Read, Seek, Write};

use image::{
    DynamicImage, ImageDecoder, ImageReader, RgbaImage,
    codecs::{jpeg::JpegEncoder, png::PngEncoder, webp::WebPEncoder},
    metadata::Orientation,
};

use crate::error::{Error, Result};

/// 输出格式与编码参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// `quality` 取 1..=100。
    ///
    /// 必须显式给值：`DynamicImage::save()` 那条便利路径会**静默**用 75，
    /// 对水印工具来说是肉眼可见的质量损失。
    Jpeg {
        quality: u8,
    },
    Png,
    /// 无损 WebP。
    WebP,
}

impl Default for OutputFormat {
    fn default() -> Self {
        Self::Jpeg { quality: 90 }
    }
}

/// 解码时随像素一起取出的元数据。
///
/// `exif` / `icc` 是不透明字节，原样搬运到输出即可；要读出其中的字段
/// （动态水印的 `{exif.*}`）得靠 `kamadak-exif`，见 [`crate::ExifFields`]。
#[derive(Clone, Default)]
pub struct Metadata {
    pub exif: Option<Vec<u8>>,
    pub icc: Option<Vec<u8>>,
}

// 手写而非 derive：EXIF / ICC 动辄几 KB，derive 出来的 Debug 会把整串字节喷进日志。
impl std::fmt::Debug for Metadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metadata")
            .field("exif_bytes", &self.exif.as_ref().map(Vec::len))
            .field("icc_bytes", &self.icc.as_ref().map(Vec::len))
            .finish()
    }
}

/// 解码结果。
pub struct DecodedImage {
    pub image: RgbaImage,
    pub metadata: Metadata,
}

// 手写而非 derive：`ImageBuffer` 的 Debug 会逐字节打印整张图，
// 一张 512×512 就能刷出几 MB 日志。
impl std::fmt::Debug for DecodedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedImage")
            .field("dimensions", &self.image.dimensions())
            .field("metadata", &self.metadata)
            .finish()
    }
}

/// 解码一张图片，并把 EXIF 方向应用到像素上。
///
/// `max_alloc` 是解码期的内存上限（字节）。`image` 默认 512 MiB，
/// 一张 12000×9000 的 RGBA 解码后是 412 MiB，再加上合成所需的副本就会超。
/// 传 `None` 表示不设限，风险自负。
pub fn decode<R: Read + Seek>(mut src: R, max_alloc: Option<u64>) -> Result<DecodedImage> {
    // 先只读文件头拿尺寸：一旦后面撞上内存上限，才能报出"多大的图、需要多少内存"
    // 这种可操作的信息，而不是一句干巴巴的 limits error。代价只是一次头部读取。
    let (width, height) = probe_dimensions(&mut src)?;
    src.rewind()?;

    // 必须自己按尺寸把关，不能指望 `image` 的 `max_alloc`：
    // `PngDecoder::set_limits` 里挂着一条明确的 TODO —— 它只校验尺寸上限，
    // **不约束 PNG 内部的解码缓冲**。依赖它防 OOM 会落空。
    if let Some(limit) = max_alloc {
        const MIB: u64 = 1024 * 1024;
        let needed = u64::from(width) * u64::from(height) * 4;
        if needed > limit {
            return Err(Error::SourceTooLarge {
                width,
                height,
                needed_mib: needed / MIB,
                limit_mib: limit / MIB,
            });
        }
    }

    let mut reader = ImageReader::new(BufReader::new(src)).with_guessed_format()?;

    // 仍然设上，作为纵深防御 —— 对确实实现了该检查的格式有效。
    // `Limits` 是 non_exhaustive，只能先取默认值再改字段。
    let mut limits = image::Limits::default();
    limits.max_alloc = max_alloc;
    reader.limits(limits);

    let mut decoder = reader
        .into_decoder()
        .map_err(|e| map_limit_error(e, width, height, max_alloc))?;

    // 元数据必须在 from_decoder 消费掉 decoder 之前取走。
    let exif = decoder.exif_metadata()?;
    let icc = decoder.icc_profile()?;
    let orientation = decoder.orientation()?;

    let mut image = DynamicImage::from_decoder(decoder)
        .map_err(|e| map_limit_error(e, width, height, max_alloc))?;

    // 手机照片普遍带方向 tag。必须在合成前把旋转落到像素上，否则查看器二次旋转后
    // 水印会跑到错误的角落。落定之后原 tag 也要清掉，不然会被再转一次。
    image.apply_orientation(orientation);
    let exif = exif.map(|mut blob| {
        // 返回的是被移除的方向值，这里用不上：它已经由 decoder.orientation() 取过了。
        let _ = Orientation::remove_from_exif_chunk(&mut blob);
        blob
    });

    Ok(DecodedImage {
        image: image.into_rgba8(),
        metadata: Metadata { exif, icc },
    })
}

/// 不解码像素，只读出图片尺寸。
///
/// 批量处理时先用它判断素材是否过大，避免为了知道尺寸而付出整张解码的代价。
pub fn probe_dimensions<R: Read + Seek>(reader: R) -> Result<(u32, u32)> {
    let reader = ImageReader::new(BufReader::new(reader)).with_guessed_format()?;
    Ok(reader.into_dimensions()?)
}

/// 编码并写出。
pub fn encode<W: Write>(
    image: &RgbaImage,
    writer: W,
    format: OutputFormat,
    metadata: &Metadata,
) -> Result<()> {
    match format {
        OutputFormat::Jpeg { quality } => {
            let mut encoder = JpegEncoder::new_with_quality(writer, quality.clamp(1, 100));
            attach_metadata(&mut encoder, metadata);
            // JPEG 没有 alpha 通道，必须先降到 RGB；源图若带透明，透明处会塌成黑色，
            // 这是格式限制而非缺陷 —— 需要保留透明就该选 PNG / WebP。
            DynamicImage::ImageRgba8(image.clone())
                .to_rgb8()
                .write_with_encoder(encoder)?;
        }
        OutputFormat::Png => {
            let mut encoder = PngEncoder::new(writer);
            attach_metadata(&mut encoder, metadata);
            image.write_with_encoder(encoder)?;
        }
        OutputFormat::WebP => {
            let mut encoder = WebPEncoder::new_lossless(writer);
            attach_metadata(&mut encoder, metadata);
            image.write_with_encoder(encoder)?;
        }
    }
    Ok(())
}

/// 尽力把 EXIF / ICC 挂到编码器上。
///
/// 各编码器对元数据的支持程度不同（JPEG 两者都写，PNG/WebP 未必），
/// 失败只降级为"这张图没带上元数据"，不该让整个水印任务失败。
fn attach_metadata(encoder: &mut impl image::ImageEncoder, metadata: &Metadata) {
    if let Some(exif) = &metadata.exif
        && let Err(e) = encoder.set_exif_metadata(exif.clone())
    {
        log::debug!("该格式不支持写入 EXIF，已跳过: {e}");
    }
    if let Some(icc) = &metadata.icc
        && let Err(e) = encoder.set_icc_profile(icc.clone())
    {
        log::debug!("该格式不支持写入 ICC，已跳过: {e}");
    }
}

/// 把 `image` 的内存上限错误翻译成更可操作的 [`Error::SourceTooLarge`]。
///
/// 两者的区别对调用方是有意义的：撞上限是"调高上限就能重试"，
/// 而 [`Error::Image`] 多半是素材本身有问题。
fn map_limit_error(err: image::ImageError, width: u32, height: u32, limit: Option<u64>) -> Error {
    let hit_limit = matches!(
        &err,
        image::ImageError::Limits(e)
            if matches!(e.kind(), image::error::LimitErrorKind::InsufficientMemory)
    );
    if !hit_limit {
        return Error::Image(err);
    }
    const MIB: u64 = 1024 * 1024;
    Error::SourceTooLarge {
        width,
        height,
        needed_mib: u64::from(width) * u64::from(height) * 4 / MIB,
        limit_mib: limit.unwrap_or(0) / MIB,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample_png(w: u32, h: u32) -> Vec<u8> {
        let mut img = RgbaImage::new(w, h);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]);
        }
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[test]
    fn decodes_png_round_trip() {
        let png = sample_png(16, 9);
        let decoded = decode(Cursor::new(&png), None).expect("解码");
        assert_eq!(decoded.image.dimensions(), (16, 9));
    }

    #[test]
    fn probe_dimensions_does_not_decode() {
        let png = sample_png(640, 480);
        assert_eq!(probe_dimensions(Cursor::new(&png)).unwrap(), (640, 480));
    }

    #[test]
    fn jpeg_quality_affects_output_size() {
        let img = RgbaImage::from_fn(128, 128, |x, y| {
            image::Rgba([(x * 2) as u8, (y * 2) as u8, ((x + y) % 256) as u8, 255])
        });
        let meta = Metadata::default();

        let mut low = Vec::new();
        encode(&img, &mut low, OutputFormat::Jpeg { quality: 20 }, &meta).unwrap();
        let mut high = Vec::new();
        encode(&img, &mut high, OutputFormat::Jpeg { quality: 95 }, &meta).unwrap();

        // 质量参数必须真的起作用 —— 走 save() 那条路它会被静默钉死在 75。
        assert!(
            high.len() > low.len() * 2,
            "质量参数未生效: q20={} q95={}",
            low.len(),
            high.len()
        );
    }

    #[test]
    fn png_preserves_alpha() {
        let mut img = RgbaImage::new(4, 4);
        img.put_pixel(0, 0, image::Rgba([255, 0, 0, 0]));
        img.put_pixel(1, 1, image::Rgba([0, 255, 0, 128]));

        let mut out = Vec::new();
        encode(&img, &mut out, OutputFormat::Png, &Metadata::default()).unwrap();
        let back = decode(Cursor::new(&out), None).unwrap().image;

        assert_eq!(back.get_pixel(0, 0).0[3], 0);
        assert_eq!(back.get_pixel(1, 1).0[3], 128);
    }

    #[test]
    fn webp_round_trips_losslessly() {
        let img = RgbaImage::from_fn(32, 32, |x, y| {
            image::Rgba([(x * 8) as u8, (y * 8) as u8, 64, 255])
        });
        let mut out = Vec::new();
        encode(&img, &mut out, OutputFormat::WebP, &Metadata::default()).unwrap();

        let back = decode(Cursor::new(&out), None).unwrap().image;
        assert_eq!(back.dimensions(), (32, 32));
        assert_eq!(back.as_raw(), img.as_raw(), "无损 WebP 应当逐像素一致");
    }

    #[test]
    fn memory_limit_reports_actionable_error() {
        let png = sample_png(512, 512);
        // 512×512×4 = 1 MiB，给 64 KiB 必然不够。
        let err = decode(Cursor::new(&png), Some(64 * 1024)).unwrap_err();
        // 必须是"素材过大"而不是笼统的解码失败 —— 调用方据此提示用户调高上限。
        assert!(
            matches!(
                err,
                Error::SourceTooLarge {
                    width: 512,
                    height: 512,
                    ..
                }
            ),
            "错误类型不可操作: {err:?}"
        );
    }

    #[test]
    fn rejects_garbage_input() {
        let junk = vec![0u8; 128];
        assert!(decode(Cursor::new(&junk), None).is_err());
    }
}
