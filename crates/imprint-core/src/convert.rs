//! `image::RgbaImage` 与 `tiny_skia::Pixmap` 之间的像素格式转换。
//!
//! 两者的 alpha 语义不同，这是最容易出错的地方：
//! - `tiny_skia::Pixmap` 存 **premultiplied** RGBA（`Pixmap::from_vec` 只校验长度，
//!   **不会**替你预乘，塞进未预乘的数据会得到发亮的边缘）；
//! - `image::RgbaImage` 存 **straight**（未预乘）RGBA。

use image::RgbaImage;
use rayon::prelude::*;
use tiny_skia::{Pixmap, PremultipliedColorU8};

use crate::error::{Error, Result};

/// straight RGBA → premultiplied `Pixmap`。
pub fn rgba_image_to_pixmap(img: &RgbaImage) -> Result<Pixmap> {
    let (width, height) = img.dimensions();
    let mut pixmap = Pixmap::new(width, height).ok_or(Error::InvalidSize { width, height })?;

    // 大图（4K 起步就是 800 万像素）逐像素预乘值得并行；按行切分避免伪共享。
    let row_len = width as usize;
    pixmap
        .pixels_mut()
        .par_chunks_mut(row_len)
        .zip(img.as_raw().par_chunks(row_len * 4))
        .for_each(|(dst_row, src_row)| {
            // as_chunks 给出的是 `&[u8; 4]`，长度在编译期已知，省掉逐次索引的边界检查；
            // 余数必然为空，因为 RGBA 每像素恰好 4 字节。
            let (pixels, _) = src_row.as_chunks::<4>();
            for (dst, src) in dst_row.iter_mut().zip(pixels) {
                *dst = premultiply(src[0], src[1], src[2], src[3]);
            }
        });

    Ok(pixmap)
}

/// premultiplied `Pixmap` → straight RGBA。
///
/// 走 `take_demultiplied()`：tiny-skia 自己就做了反预乘，不必手写一遍。
pub fn pixmap_to_rgba_image(pixmap: Pixmap) -> Result<RgbaImage> {
    let (width, height) = (pixmap.width(), pixmap.height());
    RgbaImage::from_raw(width, height, pixmap.take_demultiplied())
        .ok_or(Error::InvalidSize { width, height })
}

/// 定点数的 `a * b / 255`，带四舍五入。
#[inline]
pub(crate) fn mul255(a: u8, b: u8) -> u8 {
    let t = a as u32 * b as u32 + 128;
    ((t + (t >> 8)) >> 8) as u8
}

#[inline]
fn premultiply(r: u8, g: u8, b: u8, a: u8) -> PremultipliedColorU8 {
    if a == 255 {
        // 不透明像素占绝大多数（JPEG 背景全是），走直通省掉三次乘法。
        return PremultipliedColorU8::from_rgba(r, g, b, a)
            .unwrap_or(PremultipliedColorU8::TRANSPARENT);
    }
    PremultipliedColorU8::from_rgba(mul255(r, a), mul255(g, a), mul255(b, a), a)
        .unwrap_or(PremultipliedColorU8::TRANSPARENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_pixels_round_trip_exactly() {
        let mut img = RgbaImage::new(3, 2);
        for (i, px) in img.pixels_mut().enumerate() {
            *px = image::Rgba([(i * 40) as u8, 128, 255 - (i * 30) as u8, 255]);
        }
        let back = pixmap_to_rgba_image(rgba_image_to_pixmap(&img).unwrap()).unwrap();
        assert_eq!(img.as_raw(), back.as_raw(), "不透明像素往返必须完全无损");
    }

    #[test]
    fn premultiplication_is_applied() {
        let mut img = RgbaImage::new(1, 1);
        img.put_pixel(0, 0, image::Rgba([255, 255, 255, 128]));
        let pm = rgba_image_to_pixmap(&img).unwrap();
        let px = pm.pixels()[0];

        assert_eq!(px.alpha(), 128);
        // 预乘后 RGB 必须被 alpha 压下来；若忘记预乘这里会是 255。
        assert!(px.red() < 200, "RGB 未被预乘: {}", px.red());
        assert_eq!(px.red(), mul255(255, 128));
    }

    #[test]
    fn fully_transparent_stays_transparent() {
        let mut img = RgbaImage::new(1, 1);
        img.put_pixel(0, 0, image::Rgba([200, 100, 50, 0]));
        let pm = rgba_image_to_pixmap(&img).unwrap();
        assert_eq!(pm.pixels()[0].alpha(), 0);

        let back = pixmap_to_rgba_image(pm).unwrap();
        assert_eq!(back.get_pixel(0, 0).0[3], 0);
    }

    #[test]
    fn semi_transparent_round_trip_within_rounding_error() {
        let mut img = RgbaImage::new(1, 1);
        img.put_pixel(0, 0, image::Rgba([200, 100, 50, 128]));
        let back = pixmap_to_rgba_image(rgba_image_to_pixmap(&img).unwrap()).unwrap();
        let out = back.get_pixel(0, 0).0;

        assert_eq!(out[3], 128);
        // 预乘 / 反预乘是有损的，半透明像素只能要求落在舍入误差内。
        for (i, (&got, &want)) in out.iter().zip([200u8, 100, 50, 128].iter()).enumerate() {
            assert!(
                got.abs_diff(want) <= 2,
                "通道 {i} 往返误差过大: {got} vs {want}"
            );
        }
    }

    #[test]
    fn zero_sized_image_is_rejected() {
        let img = RgbaImage::new(0, 0);
        assert!(matches!(
            rgba_image_to_pixmap(&img),
            Err(Error::InvalidSize { .. })
        ));
    }
}
