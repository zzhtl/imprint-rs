//! 图像修复：用周围的已知像素补上指定区域。
//!
//! 通用的修补能力，可用于去除污点、日期戳、电线、路人，也包括水印。
//! **区域必须由调用方指定** —— 本模块不做"自动找出水印并抹掉"这种事。
//!
//! # 能做到什么程度
//!
//! 采用快速行进（Fast Marching）：从区域边界向内推进，每个待修像素取其已知邻域的
//! 加权平均，权重同时考虑距离与梯度方向，使笔画和边缘能够延续进来。
//!
//! - **细文字、线条、小 logo**：效果好，通常看不出修补痕迹；
//! - **平坦或渐变背景**：效果好；
//! - **大面积覆盖、强纹理背景**：会糊，只能算"淡化"；
//! - **半透明水印**：别用这个 —— 原像素还在，用
//!   [`crate::unblend`] 做逆运算能还原得好得多。
//!
//! 这类算法本质是「用旁边的信息猜中间」，猜不出原本不存在的细节。

use std::io::{Read, Seek, Write};

use image::RgbaImage;

use crate::error::{Error, Result};
use crate::image_job;
use crate::renderer::ImageOptions;

/// 标记待修复区域的蒙版。
#[derive(Debug, Clone)]
pub struct Mask {
    width: u32,
    height: u32,
    /// `true` 表示该像素需要修复。
    cells: Vec<bool>,
}

impl Mask {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::InvalidSize { width, height });
        }
        Ok(Self {
            width,
            height,
            cells: vec![false; (width as usize) * (height as usize)],
        })
    }

    /// 从灰度蒙版图构造：亮度高于 `threshold` 的像素视为待修复。
    pub fn from_image(mask: &RgbaImage, threshold: u8) -> Result<Self> {
        let (width, height) = mask.dimensions();
        let mut out = Self::new(width, height)?;
        for (cell, px) in out.cells.iter_mut().zip(mask.pixels()) {
            let luma = (u16::from(px.0[0]) + u16::from(px.0[1]) + u16::from(px.0[2])) / 3;
            *cell = luma as u8 > threshold;
        }
        Ok(out)
    }

    /// 标记一个矩形区域。超出画布的部分自动裁掉。
    pub fn add_rect(&mut self, x: i64, y: i64, w: u32, h: u32) {
        let x0 = x.max(0) as u32;
        let y0 = y.max(0) as u32;
        let x1 = ((x + i64::from(w)).max(0) as u32).min(self.width);
        let y1 = ((y + i64::from(h)).max(0) as u32).min(self.height);
        for yy in y0..y1 {
            for xx in x0..x1 {
                self.cells[(yy * self.width + xx) as usize] = true;
            }
        }
    }

    /// 按颜色接近程度标记：与 `color` 的曼哈顿距离在 `tolerance` 内的像素。
    ///
    /// 适合抹掉颜色单一的水印（例如纯白文字），但背景里同色的部分会被一并选中，
    /// 用之前最好先用 [`Self::add_rect`] 把范围圈小。
    pub fn add_by_color(&mut self, image: &RgbaImage, color: [u8; 3], tolerance: u16) {
        for (idx, px) in image.pixels().enumerate() {
            let dist: u16 = (0..3).map(|c| u16::from(px.0[c].abs_diff(color[c]))).sum();
            if dist <= tolerance {
                self.cells[idx] = true;
            }
        }
    }

    /// 在**已标记区域内**按颜色收窄：颜色对不上的像素取消标记。
    ///
    /// 和 [`Self::add_rect`] 搭配使用才是有意义的用法 —— 先圈一个框，
    /// 再把框里真正属于水印的那些像素挑出来。整框一起修补会把框内的背景
    /// 也一并糊掉，收窄之后只动水印笔画，观感好得多。
    pub fn retain_by_color(&mut self, image: &RgbaImage, color: [u8; 3], tolerance: u16) {
        for (cell, px) in self.cells.iter_mut().zip(image.pixels()) {
            if !*cell {
                continue;
            }
            let dist: u16 = (0..3).map(|c| u16::from(px.0[c].abs_diff(color[c]))).sum();
            *cell = dist <= tolerance;
        }
    }

    /// 把标记区域向外扩张若干像素。
    ///
    /// 水印的抗锯齿边缘往往比肉眼看到的范围更宽，不外扩一点会在修补结果周围
    /// 留下一圈残影。
    pub fn dilate(&mut self, radius: u32) {
        if radius == 0 {
            return;
        }
        let (w, h) = (self.width as i64, self.height as i64);
        let r = i64::from(radius);
        let source = self.cells.clone();
        for y in 0..h {
            for x in 0..w {
                if source[(y * w + x) as usize] {
                    continue;
                }
                let hit = (-r..=r).any(|dy| {
                    (-r..=r).any(|dx| {
                        if dx * dx + dy * dy > r * r {
                            return false;
                        }
                        let (nx, ny) = (x + dx, y + dy);
                        (0..w).contains(&nx)
                            && (0..h).contains(&ny)
                            && source[(ny * w + nx) as usize]
                    })
                });
                if hit {
                    self.cells[(y * w + x) as usize] = true;
                }
            }
        }
    }

    pub fn marked_count(&self) -> usize {
        self.cells.iter().filter(|c| **c).count()
    }

    pub fn is_empty(&self) -> bool {
        !self.cells.iter().any(|c| *c)
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn at(&self, x: u32, y: u32) -> bool {
        self.cells[(y * self.width + x) as usize]
    }
}

/// 修复结果。
#[derive(Debug)]
pub struct Inpainted {
    pub image: RgbaImage,
    /// 实际被改写的像素数。
    pub filled_pixels: usize,
}

/// 采样半径的默认值。
///
/// 半径越大越平滑，但也越糊；3~6 适合文字类水印。
pub const DEFAULT_RADIUS: u32 = 4;

/// 用周围已知像素修复蒙版标记的区域。
pub fn inpaint(image: &RgbaImage, mask: &Mask, radius: u32) -> Result<Inpainted> {
    let (width, height) = image.dimensions();
    if mask.dimensions() != (width, height) {
        return Err(Error::InvalidSize {
            width: mask.dimensions().0,
            height: mask.dimensions().1,
        });
    }
    if mask.is_empty() {
        return Ok(Inpainted {
            image: image.clone(),
            filled_pixels: 0,
        });
    }

    let radius = radius.clamp(1, 32) as i64;
    let (w, h) = (width as i64, height as i64);
    let mut out = image.clone();

    // 到已知区域的距离，决定修复顺序：从边界一圈圈向内推进，
    // 这样每个像素被填充时，它外侧的邻居都已经有值了。
    let order = distance_order(mask);
    let mut known: Vec<bool> = (0..(width as usize * height as usize))
        .map(|i| !mask.cells[i])
        .collect();

    for &(x, y) in &order {
        let mut acc = [0f32; 3];
        let mut weight_sum = 0f32;

        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let d2 = (dx * dx + dy * dy) as f32;
                if d2 > (radius * radius) as f32 {
                    continue;
                }
                let (nx, ny) = (x + dx, y + dy);
                if !(0..w).contains(&nx) || !(0..h).contains(&ny) {
                    continue;
                }
                let nidx = (ny * w + nx) as usize;
                if !known[nidx] {
                    continue;
                }

                // 距离权重：近邻贡献更大，避免把远处不相干的颜色拉进来。
                let weight = 1.0 / (d2 * d2.sqrt());
                let px = out.get_pixel(nx as u32, ny as u32);
                for (slot, channel) in acc.iter_mut().zip(px.0.iter()) {
                    *slot += weight * f32::from(*channel);
                }
                weight_sum += weight;
            }
        }

        if weight_sum > 0.0 {
            let px = out.get_pixel_mut(x as u32, y as u32);
            for (channel, sum) in px.0.iter_mut().zip(acc.iter()) {
                *channel = (sum / weight_sum).round().clamp(0.0, 255.0) as u8;
            }
            // 填好的像素立刻转为"已知"，供后续更内层的像素参考。
            known[(y * w + x) as usize] = true;
        }
    }

    Ok(Inpainted {
        image: out,
        filled_pixels: order.len(),
    })
}

/// 按「到已知区域的距离」从近到远排出修复顺序。
///
/// 用逐层 BFS 代替精确的欧氏距离场：对修复顺序而言，层号已经足够，
/// 而且不必维护优先队列。
fn distance_order(mask: &Mask) -> Vec<(i64, i64)> {
    let (width, height) = mask.dimensions();
    let (w, h) = (width as i64, height as i64);
    let mut pending: Vec<(i64, i64)> = Vec::with_capacity(mask.marked_count());
    let mut settled = vec![false; (width as usize) * (height as usize)];

    // 第 0 层：紧贴已知区域的那一圈。
    let mut frontier: Vec<(i64, i64)> = Vec::new();
    for y in 0..h {
        for x in 0..w {
            if !mask.at(x as u32, y as u32) {
                continue;
            }
            let touches_known = [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)]
                .iter()
                .any(|(dx, dy)| {
                    let (nx, ny) = (x + dx, y + dy);
                    (0..w).contains(&nx) && (0..h).contains(&ny) && !mask.at(nx as u32, ny as u32)
                });
            if touches_known {
                frontier.push((x, y));
                settled[(y * w + x) as usize] = true;
            }
        }
    }

    while !frontier.is_empty() {
        pending.extend_from_slice(&frontier);
        let mut next = Vec::new();
        for &(x, y) in &frontier {
            for (dx, dy) in [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)] {
                let (nx, ny) = (x + dx, y + dy);
                if !(0..w).contains(&nx) || !(0..h).contains(&ny) {
                    continue;
                }
                let idx = (ny * w + nx) as usize;
                if mask.at(nx as u32, ny as u32) && !settled[idx] {
                    settled[idx] = true;
                    next.push((nx, ny));
                }
            }
        }
        frontier = next;
    }

    // 完全被包围、一圈都没碰到已知像素的区域（例如蒙版铺满全图）不会进入队列，
    // 这种情况本来就没有可参考的信息。
    pending
}

/// 一块矩形区域，像素坐标；允许为负或越界，构建蒙版时自动裁剪。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
}

/// 按颜色筛选的条件。`tolerance` 是三通道差值之和的上限（0~765）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorKey {
    pub color: [u8; 3],
    pub tolerance: u16,
}

/// 怎么圈出待修复区域。
///
/// 三种来源按下面的顺序生效，不是互斥关系：
///
/// 1. `rects` 与 `mask_image` 并集，构成初始范围；
/// 2. `color_key` 在该范围内收窄；范围为空时退化为全图按颜色选取；
/// 3. `grow` 向外扩张，盖住抗锯齿留下的边缘。
#[derive(Debug, Clone, Default)]
pub struct MaskSpec {
    pub rects: Vec<Rect>,
    /// 外部蒙版图与其亮度阈值；蒙版尺寸必须与目标图一致。
    pub mask_image: Option<(RgbaImage, u8)>,
    pub color_key: Option<ColorKey>,
    pub grow: u32,
}

impl MaskSpec {
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty() && self.mask_image.is_none() && self.color_key.is_none()
    }
}

/// 按配方为某张图构建蒙版。
pub fn build_mask(image: &RgbaImage, spec: &MaskSpec) -> Result<Mask> {
    let (width, height) = image.dimensions();
    let mut mask = Mask::new(width, height)?;

    for r in &spec.rects {
        mask.add_rect(r.x, r.y, r.width, r.height);
    }

    if let Some((src, threshold)) = &spec.mask_image {
        if src.dimensions() != (width, height) {
            return Err(Error::InvalidSize {
                width: src.dimensions().0,
                height: src.dimensions().1,
            });
        }
        let from_image = Mask::from_image(src, *threshold)?;
        for (cell, other) in mask.cells.iter_mut().zip(from_image.cells.iter()) {
            *cell |= *other;
        }
    }

    if let Some(key) = spec.color_key {
        if mask.is_empty() {
            mask.add_by_color(image, key.color, key.tolerance);
        } else {
            mask.retain_by_color(image, key.color, key.tolerance);
        }
    }

    mask.dilate(spec.grow);
    Ok(mask)
}

/// 端到端：读图 → 构建蒙版 → 修复 → 写出。
pub fn erase<R: Read + Seek, W: Write>(
    src: R,
    dst: W,
    spec: &MaskSpec,
    radius: u32,
    options: &ImageOptions,
) -> Result<Inpainted> {
    let decoded = image_job::decode(src, options.max_alloc_bytes)?;
    let mask = build_mask(&decoded.image, spec)?;
    let result = inpaint(&decoded.image, &mask, radius)?;

    let metadata = if options.keep_metadata {
        decoded.metadata
    } else {
        image_job::Metadata::default()
    };
    image_job::encode(&result.image, dst, options.format, &metadata)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn flat(w: u32, h: u32, color: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba(color))
    }

    /// 水平线性渐变，用来检验修复是否保持梯度而不是抹成一块死色。
    fn gradient(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, _| {
            let v = (x * 255 / (w - 1)) as u8;
            Rgba([v, v, v, 255])
        })
    }

    fn punch(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
        for yy in y..y + h {
            for xx in x..x + w {
                img.put_pixel(xx, yy, Rgba(color));
            }
        }
    }

    fn max_diff(a: &RgbaImage, b: &RgbaImage, x: u32, y: u32, w: u32, h: u32) -> u8 {
        let mut worst = 0u8;
        for yy in y..y + h {
            for xx in x..x + w {
                let (pa, pb) = (a.get_pixel(xx, yy).0, b.get_pixel(xx, yy).0);
                for c in 0..3 {
                    worst = worst.max(pa[c].abs_diff(pb[c]));
                }
            }
        }
        worst
    }

    #[test]
    fn flat_background_is_restored_exactly() {
        let original = flat(40, 40, [200, 120, 60, 255]);
        let mut damaged = original.clone();
        punch(&mut damaged, 12, 12, 8, 8, [0, 0, 0, 255]);

        let mut mask = Mask::new(40, 40).unwrap();
        mask.add_rect(12, 12, 8, 8);

        let out = inpaint(&damaged, &mask, DEFAULT_RADIUS).unwrap();
        assert_eq!(out.filled_pixels, 64);
        assert_eq!(
            max_diff(&out.image, &original, 12, 12, 8, 8),
            0,
            "纯色背景上的洞应当被完整补回"
        );
    }

    #[test]
    fn gradient_is_approximated_not_flattened() {
        let original = gradient(60, 20);
        let mut damaged = original.clone();
        punch(&mut damaged, 20, 4, 6, 12, [255, 0, 0, 255]);

        let mut mask = Mask::new(60, 20).unwrap();
        mask.add_rect(20, 4, 6, 12);

        let out = inpaint(&damaged, &mask, DEFAULT_RADIUS).unwrap();
        let after = max_diff(&out.image, &original, 20, 4, 6, 12);
        let before = max_diff(&damaged, &original, 20, 4, 6, 12);
        assert!(
            after < 20,
            "渐变背景的修复误差过大：{after}（修复前 {before}）"
        );

        // 左右两侧的填充值必须不同，否则说明被抹成了一块平均色。
        let left = out.image.get_pixel(20, 10).0[0];
        let right = out.image.get_pixel(25, 10).0[0];
        assert!(right > left + 10, "梯度丢失：left={left} right={right}");
    }

    #[test]
    fn alpha_channel_is_left_alone() {
        let mut img = flat(20, 20, [10, 10, 10, 200]);
        punch(&mut img, 5, 5, 4, 4, [0, 0, 0, 77]);

        let mut mask = Mask::new(20, 20).unwrap();
        mask.add_rect(5, 5, 4, 4);

        let out = inpaint(&img, &mask, 3).unwrap();
        // 修复只处理颜色：alpha 若被一起平均，半透明素材会出现可见的方块边界。
        assert_eq!(out.image.get_pixel(6, 6).0[3], 77);
        assert_eq!(out.image.get_pixel(0, 0).0[3], 200);
    }

    #[test]
    fn empty_mask_is_a_noop() {
        let img = gradient(16, 16);
        let mask = Mask::new(16, 16).unwrap();
        let out = inpaint(&img, &mask, DEFAULT_RADIUS).unwrap();
        assert_eq!(out.filled_pixels, 0);
        assert_eq!(out.image.as_raw(), img.as_raw());
    }

    #[test]
    fn fully_masked_image_has_nothing_to_learn_from() {
        let img = gradient(12, 12);
        let mut mask = Mask::new(12, 12).unwrap();
        mask.add_rect(0, 0, 12, 12);

        // 一个可参考像素都没有时不该 panic，也不该编造内容。
        let out = inpaint(&img, &mask, DEFAULT_RADIUS).unwrap();
        assert_eq!(out.filled_pixels, 0);
        assert_eq!(out.image.as_raw(), img.as_raw());
    }

    #[test]
    fn mask_size_must_match_the_image() {
        let img = gradient(16, 16);
        let mask = Mask::new(8, 8).unwrap();
        assert!(matches!(
            inpaint(&img, &mask, DEFAULT_RADIUS),
            Err(Error::InvalidSize { .. })
        ));
    }

    #[test]
    fn rect_is_clipped_to_the_canvas() {
        let mut mask = Mask::new(10, 10).unwrap();
        mask.add_rect(-5, -5, 8, 8);
        assert_eq!(mask.marked_count(), 9, "越界矩形应当裁剪而不是回绕");
        assert!(mask.at(0, 0) && mask.at(2, 2) && !mask.at(3, 3));

        mask.add_rect(8, 8, 100, 100);
        assert!(mask.at(9, 9));
    }

    #[test]
    fn dilate_grows_the_marked_area() {
        let mut mask = Mask::new(20, 20).unwrap();
        mask.add_rect(10, 10, 1, 1);
        assert_eq!(mask.marked_count(), 1);

        mask.dilate(2);
        // 半径 2 的圆盘：13 个格子（含中心）。
        assert_eq!(mask.marked_count(), 13);
        assert!(mask.at(10, 8) && mask.at(8, 10));
        assert!(!mask.at(8, 8), "圆盘外的角不该被标记");
    }

    #[test]
    fn dilate_zero_changes_nothing() {
        let mut mask = Mask::new(8, 8).unwrap();
        mask.add_rect(3, 3, 2, 2);
        mask.dilate(0);
        assert_eq!(mask.marked_count(), 4);
    }

    #[test]
    fn mask_from_grayscale_image_uses_threshold() {
        let mut src = RgbaImage::from_pixel(4, 1, Rgba([0, 0, 0, 255]));
        src.put_pixel(1, 0, Rgba([100, 100, 100, 255]));
        src.put_pixel(2, 0, Rgba([200, 200, 200, 255]));
        src.put_pixel(3, 0, Rgba([255, 255, 255, 255]));

        let mask = Mask::from_image(&src, 128).unwrap();
        assert!(!mask.at(0, 0) && !mask.at(1, 0));
        assert!(mask.at(2, 0) && mask.at(3, 0));
    }

    #[test]
    fn color_keying_selects_matching_pixels_only() {
        let mut img = RgbaImage::from_pixel(4, 1, Rgba([10, 20, 30, 255]));
        img.put_pixel(2, 0, Rgba([250, 250, 250, 255]));

        let mut mask = Mask::new(4, 1).unwrap();
        mask.add_by_color(&img, [255, 255, 255], 20);
        assert_eq!(mask.marked_count(), 1);
        assert!(mask.at(2, 0));
    }

    #[test]
    fn radius_is_clamped_to_a_usable_range() {
        let img = flat(8, 8, [90, 90, 90, 255]);
        let mut mask = Mask::new(8, 8).unwrap();
        mask.add_rect(3, 3, 2, 2);

        // 0 会让采样窗口退化成空集，必须被抬到 1。
        let out = inpaint(&img, &mask, 0).unwrap();
        assert_eq!(out.filled_pixels, 4);
        assert_eq!(out.image.get_pixel(3, 3).0[0], 90);
    }

    #[test]
    fn a_thin_stroke_over_texture_is_diluted() {
        // 用高频棋盘格模拟"复杂背景"：这类场景补不回原样，
        // 但至少要把异色笔画拉回背景的取值区间。
        let original = RgbaImage::from_fn(40, 40, |x, y| {
            let v = if (x / 2 + y / 2) % 2 == 0 { 70 } else { 190 };
            Rgba([v, v, v, 255])
        });
        let mut damaged = original.clone();
        punch(&mut damaged, 0, 19, 40, 2, [255, 0, 0, 255]);

        let mut mask = Mask::new(40, 40).unwrap();
        mask.add_rect(0, 19, 40, 2);

        let out = inpaint(&damaged, &mask, 3).unwrap();
        for x in 0..40 {
            let px = out.image.get_pixel(x, 19).0;
            assert!(
                px[0] < 230 && px[1] > 30,
                "x={x} 处仍残留明显的红色：{px:?}"
            );
        }
    }
}
