//! 从带水印的图片中还原原图。
//!
//! 合成用的是 source-over：`result = 水印 × α + 原图 × (1 − α)`。
//! 只要能重建出当初那张水印图层（即知道原始 [`WatermarkSpec`]），这就是一个
//! 可解的线性方程：
//!
//! ```text
//! 原图 = (result − 水印 × α) / (1 − α)
//! ```
//!
//! 这是**精确逆运算**，不是修补猜测 —— 与 inpainting 有本质区别。
//!
//! 两个固有限制，无法靠实现绕开：
//!
//! - **α = 1 的像素信息已彻底丢失**。完全不透明的水印把原像素覆盖掉了，
//!   方程分母为零，没有任何算法能把它找回来（只能另行修补）。
//! - **α 越接近 1，误差被放大得越厉害**。带水印的图若又经过 JPEG 压缩，
//!   压缩噪声会被 `1/(1−α)` 放大；α=0.9 时放大十倍。
//!   对半透明水印（α≤0.6）效果很好，对接近不透明的水印则只能算改善。

use image::RgbaImage;
use tiny_skia::Pixmap;

use crate::error::{Error, Result};

/// 还原结果。
pub struct Unblended {
    pub image: RgbaImage,
    /// 被完全不透明的水印覆盖、无法还原的像素数。
    pub opaque_pixels: u64,
    /// 参与还原的像素数（水印覆盖到、且可解的部分）。
    pub recovered_pixels: u64,
    /// 还原结果越出 0~255 而被钳制的通道数。
    ///
    /// **这是"参数对不对"最有力的信号**：spec 与当初一致时，减掉的正是当初加上的
    /// 那一份，结果天然落在值域内，钳制极少；一旦文字、位置或透明度对不上，
    /// 就会大量算出负值或超白，钳制数随之飙升。
    pub clamped_channels: u64,
}

impl Unblended {
    /// 无法还原的像素占水印覆盖区域的比例。
    pub fn unrecoverable_ratio(&self) -> f32 {
        let covered = self.opaque_pixels + self.recovered_pixels;
        if covered == 0 {
            return 0.0;
        }
        self.opaque_pixels as f32 / covered as f32
    }

    /// 被钳制的通道占参与还原通道的比例。
    pub fn clamped_ratio(&self) -> f32 {
        if self.recovered_pixels == 0 {
            return 0.0;
        }
        self.clamped_channels as f32 / (self.recovered_pixels * 3) as f32
    }

    /// 看起来是不是用错了 spec。
    ///
    /// 阈值取 2%：实测参数正确时钳制率在千分之几以内（只有 JPEG 噪声会越界），
    /// 而文字对不上时会跳到百分之几十。
    pub fn looks_mismatched(&self) -> bool {
        self.clamped_ratio() > 0.02
    }
}

/// α 低于此值视为"没有水印"，直接照搬原像素。
///
/// 抗锯齿边缘会产生大量极低 α 的像素，对它们做除法只会放大噪声。
const ALPHA_EPSILON: f32 = 1.0 / 255.0;

/// 从带水印的图中减去指定的水印图层。
///
/// `overlay` 必须是与图片等尺寸、**premultiplied** 的水印图层 —— 正是
/// [`crate::Renderer::render_video_overlay`] 产出的那种；用同一份 spec 重建即可。
pub fn unblend(watermarked: &RgbaImage, overlay: &Pixmap) -> Result<Unblended> {
    let (width, height) = watermarked.dimensions();
    if overlay.width() != width || overlay.height() != height {
        return Err(Error::InvalidSize {
            width: overlay.width(),
            height: overlay.height(),
        });
    }

    let mut out = watermarked.clone();
    let mut opaque_pixels = 0u64;
    let mut recovered_pixels = 0u64;
    let mut clamped_channels = 0u64;

    for (dst, src) in out.pixels_mut().zip(overlay.pixels()) {
        let src_a = f32::from(src.alpha()) / 255.0;
        if src_a <= ALPHA_EPSILON {
            continue; // 没被水印盖到
        }

        let inv = 1.0 - src_a;
        if inv <= ALPHA_EPSILON {
            // α=1：原像素已被彻底覆盖，方程无解。保持现状并计数，
            // 由调用方决定是否再做修补。
            opaque_pixels += 1;
            continue;
        }

        // overlay 是预乘的，其通道值本身就等于「水印色 × α」这一项。
        let subtract = [
            f32::from(src.red()),
            f32::from(src.green()),
            f32::from(src.blue()),
        ];
        // 只动 RGB，alpha 保持不变。
        for (channel, sub) in dst.0.iter_mut().take(3).zip(subtract.iter()) {
            let original = (f32::from(*channel) - sub) / inv;
            // JPEG 噪声经 1/(1−α) 放大后可能冲出值域，必须钳回来。
            // 同时计数：大量越界意味着减掉的根本不是当初那张水印。
            if !(-0.5..=255.5).contains(&original) {
                clamped_channels += 1;
            }
            *channel = original.round().clamp(0.0, 255.0) as u8;
        }
        recovered_pixels += 1;
    }

    Ok(Unblended {
        image: out,
        opaque_pixels,
        recovered_pixels,
        clamped_channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::PremultipliedColorU8;

    /// 造一张纯色的 premultiplied 水印图层。
    fn overlay(w: u32, h: u32, rgb: [u8; 3], alpha: u8) -> Pixmap {
        let mut pm = Pixmap::new(w, h).unwrap();
        let a = f32::from(alpha) / 255.0;
        let px = PremultipliedColorU8::from_rgba(
            (f32::from(rgb[0]) * a).round() as u8,
            (f32::from(rgb[1]) * a).round() as u8,
            (f32::from(rgb[2]) * a).round() as u8,
            alpha,
        )
        .unwrap();
        for p in pm.pixels_mut() {
            *p = px;
        }
        pm
    }

    /// 按 source-over 把水印合成上去，模拟加水印的过程。
    fn composite(base: &RgbaImage, ov: &Pixmap) -> RgbaImage {
        let mut out = base.clone();
        for (dst, src) in out.pixels_mut().zip(ov.pixels()) {
            let inv = 1.0 - f32::from(src.alpha()) / 255.0;
            for c in 0..3 {
                let s = f32::from([src.red(), src.green(), src.blue()][c]);
                dst.0[c] = (s + f32::from(dst.0[c]) * inv).round().clamp(0.0, 255.0) as u8;
            }
        }
        out
    }

    #[test]
    fn recovers_original_exactly_for_semi_transparent() {
        let base = RgbaImage::from_fn(32, 32, |x, y| {
            image::Rgba([(x * 8) as u8, (y * 8) as u8, 90, 255])
        });
        let ov = overlay(32, 32, [255, 255, 255], 128);
        let marked = composite(&base, &ov);

        let result = unblend(&marked, &ov).unwrap();
        assert_eq!(result.opaque_pixels, 0);
        assert_eq!(result.recovered_pixels, 32 * 32);

        // 只有量化舍入带来的误差。
        for (a, b) in base.pixels().zip(result.image.pixels()) {
            for c in 0..3 {
                assert!(
                    a.0[c].abs_diff(b.0[c]) <= 2,
                    "还原偏差过大: {} vs {}",
                    a.0[c],
                    b.0[c]
                );
            }
        }
    }

    #[test]
    fn fully_opaque_watermark_is_reported_as_unrecoverable() {
        let base = RgbaImage::from_pixel(16, 16, image::Rgba([10, 20, 30, 255]));
        let ov = overlay(16, 16, [255, 0, 0], 255);
        let marked = composite(&base, &ov);

        let result = unblend(&marked, &ov).unwrap();
        // α=1 时方程无解，必须如实上报而不是给出一个看似合理的猜测。
        assert_eq!(result.opaque_pixels, 16 * 16);
        assert_eq!(result.recovered_pixels, 0);
        assert_eq!(result.unrecoverable_ratio(), 1.0);
    }

    #[test]
    fn untouched_area_is_left_alone() {
        let base = RgbaImage::from_fn(8, 8, |x, _| image::Rgba([(x * 30) as u8, 7, 200, 255]));
        // 全透明图层 = 没加过水印。
        let ov = Pixmap::new(8, 8).unwrap();

        let result = unblend(&base, &ov).unwrap();
        assert_eq!(result.recovered_pixels, 0);
        assert_eq!(result.image.as_raw(), base.as_raw(), "无水印区域不该被改动");
    }

    #[test]
    fn low_alpha_recovers_better_than_high_alpha() {
        // 量化验证文档里的那句话：α 越大，误差放大得越厉害。
        let base = RgbaImage::from_fn(64, 64, |x, y| {
            image::Rgba([(x * 4) as u8, (y * 4) as u8, 128, 255])
        });

        let mean_err = |alpha: u8| -> f32 {
            let ov = overlay(64, 64, [255, 255, 255], alpha);
            let marked = composite(&base, &ov);
            let out = unblend(&marked, &ov).unwrap().image;
            let mut sum = 0f32;
            for (a, b) in base.pixels().zip(out.pixels()) {
                for c in 0..3 {
                    sum += f32::from(a.0[c].abs_diff(b.0[c]));
                }
            }
            sum / (64.0 * 64.0 * 3.0)
        };

        let light = mean_err(64); // α≈0.25
        let heavy = mean_err(230); // α≈0.90
        assert!(
            heavy > light,
            "高 α 的还原误差应当更大: light={light} heavy={heavy}"
        );
    }

    #[test]
    fn mismatched_overlay_shows_up_as_clamping() {
        let base = RgbaImage::from_pixel(64, 64, image::Rgba([40, 45, 50, 255]));
        // 用亮色水印合成，再用暗色水印去减 —— 模拟"参数对不上"。
        let applied = overlay(64, 64, [250, 250, 250], 120);
        let marked = composite(&base, &applied);

        let correct = unblend(&marked, &applied).unwrap();
        assert!(!correct.looks_mismatched(), "正确的 spec 被误判为不匹配");

        let wrong = overlay(64, 64, [5, 5, 5], 120);
        let bad = unblend(&marked, &wrong).unwrap();
        assert!(
            bad.looks_mismatched(),
            "错误的 spec 没被识别出来，钳制率 {:.3}",
            bad.clamped_ratio()
        );
    }

    #[test]
    fn size_mismatch_is_rejected() {
        let base = RgbaImage::new(10, 10);
        let ov = Pixmap::new(8, 8).unwrap();
        assert!(matches!(
            unblend(&base, &ov),
            Err(Error::InvalidSize { .. })
        ));
    }
}
