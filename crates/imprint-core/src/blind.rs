//! 盲水印：把标识信息嵌进图像的 DCT 频域，肉眼不可见。
//!
//! 它解决的不是"阻止别人去掉水印"——可见水印终究能被裁掉或用 AI 修复抹平。
//! 它解决的是**抹掉之后仍能溯源**：即使可见水印没了、图片被重新压缩，
//! 仍能提取出当初嵌入的标识，据此追责。
//!
//! # 为什么选 DCT 中频
//!
//! JPEG 压缩本身就是 8×8 分块 DCT + 量化。在同一个域里嵌入，才能和压缩共存：
//!
//! - **低频**系数承载画面主体能量，改动会肉眼可见；
//! - **高频**系数会被 JPEG 的量化表直接抹成 0，嵌进去等于没嵌；
//! - **中频**是两者的平衡点，这也是本模块采用的位置。
//!
//! 嵌入方式是 QIM（量化索引调制）：把选定系数量化到奇/偶格子来表示 1/0。
//! 步长 [`Strength`] 越大越抗压缩，但越可能露出块状痕迹——这个权衡点由实测确定。
//!
//! # 已知限制
//!
//! - **不抗几何攻击**。裁剪、旋转、缩放会破坏 8×8 块的对齐，本实现没有做同步恢复。
//! - **不抗 AI 重绘**。那等于重新生成了一张图，任何频域水印都不复存在。
//! - 载荷容量受图像尺寸限制，越小的图能嵌的信息越少。

use image::RgbaImage;

use crate::error::{Error, Result};

/// 分块边长，与 JPEG 对齐。
const BLOCK: usize = 8;

/// 用于承载数据的中频系数位置（行, 列）。
///
/// `(2, 3)` 在 zigzag 序里处于中段：避开了会影响观感的低频，
/// 也避开了会被量化表抹平的高频。
const COEF: (usize, usize) = (2, 3);

/// 载荷前的魔数，用来在提取时快速判断"这张图到底有没有水印"。
const MAGIC: u16 = 0xA5C3;

/// 计算 bit 槽位时使用的虚拟行宽。
///
/// 槽位取自块的**坐标**而非线性序号：线性序号依赖每行的块数，一旦从右侧裁掉
/// 几列，之后所有块的序号都会平移，槽位随之全错。改用坐标后，只要图像的左上角
/// 还在原处（即从右侧或底部裁剪），槽位就保持不变，水印依然可提取。
/// 取**质数**：若用 2 的幂（如 8192），当帧长也含相同因子时
/// `by * STRIDE % frame_bits` 会恒为 0，槽位退化成只由 bx 决定，
/// 大量槽位一票都收不到。质数能保证与任何帧长互质，槽位分布均匀。
const SLOT_STRIDE: u64 = 8191;

/// 载荷最大字节数。
///
/// 盲水印是用来放标识的（UUID、邮箱、工号），不是用来传文件的；
/// 限制长度也能给重复嵌入留出足够冗余。
pub const MAX_PAYLOAD: usize = 64;

/// QIM 量化步长，决定鲁棒性与不可见性的平衡。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Strength(f32);

impl Strength {
    /// 默认强度。
    ///
    /// 实测在 JPEG 质量 75 以上可靠提取，且肉眼看不出差别。具体数据见模块测试。
    pub const DEFAULT: Self = Self(18.0);

    /// 更强：能扛更低质量的压缩，代价是平坦区域可能出现轻微块状痕迹。
    pub const ROBUST: Self = Self(28.0);

    /// 更弱：几乎不可能被看出来，但只能扛住轻度压缩。
    pub const SUBTLE: Self = Self(10.0);

    pub fn new(step: f32) -> Result<Self> {
        if !step.is_finite() || step <= 0.0 {
            return Err(Error::InvalidSize {
                width: 0,
                height: 0,
            });
        }
        Ok(Self(step.clamp(2.0, 80.0)))
    }

    pub fn get(self) -> f32 {
        self.0
    }
}

impl Default for Strength {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// 把载荷嵌入图像，返回带盲水印的新图。
///
/// 载荷会被反复写满整张图：提取时对每个 bit 的多份副本做多数表决，
/// 因而能容忍局部损坏。
pub fn embed(image: &RgbaImage, payload: &[u8], strength: Strength) -> Result<RgbaImage> {
    if payload.is_empty() || payload.len() > MAX_PAYLOAD {
        return Err(Error::Font(format!(
            "盲水印载荷长度须在 1~{MAX_PAYLOAD} 字节之间，实际 {}",
            payload.len()
        )));
    }

    let bits = encode_frame(payload);
    let (width, height) = image.dimensions();
    let blocks_x = width as usize / BLOCK;
    let blocks_y = height as usize / BLOCK;
    let capacity = blocks_x * blocks_y;

    if capacity < bits.len() {
        return Err(Error::Font(format!(
            "图像太小：需要至少 {} 个 8×8 块才能放下这条载荷，当前只有 {capacity} 个",
            bits.len()
        )));
    }

    let mut out = image.clone();
    let mut luma = extract_luma(image);

    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let bit = bits[slot_of(bx, by, bits.len())];

            let mut block = read_block(&luma, width as usize, bx, by);
            dct8x8(&mut block);
            block[COEF.0 * BLOCK + COEF.1] =
                quantize_to_parity(block[COEF.0 * BLOCK + COEF.1], bit, strength.get());
            idct8x8(&mut block);
            write_block(&mut luma, width as usize, bx, by, &block);
        }
    }

    apply_luma(&mut out, image, &luma);
    Ok(out)
}

/// 从图像中提取盲水印载荷。
///
/// 返回 `None` 表示没找到有效水印（魔数或校验不匹配）。
pub fn extract(image: &RgbaImage) -> Option<Vec<u8>> {
    let (width, height) = image.dimensions();
    let blocks_x = width as usize / BLOCK;
    let blocks_y = height as usize / BLOCK;
    if blocks_x == 0 || blocks_y == 0 {
        return None;
    }

    let luma = extract_luma(image);
    // 先把每个块的 bit 连同坐标读出来，再按候选帧长归入槽位统计。
    let mut raw = Vec::with_capacity(blocks_x * blocks_y);
    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let mut block = read_block(&luma, width as usize, bx, by);
            dct8x8(&mut block);
            raw.push((bx, by, parity_of(block[COEF.0 * BLOCK + COEF.1])));
        }
    }

    // 帧长取决于载荷长度，事先并不知道，因此逐个长度试解。
    // 帧头自带魔数与 CRC，猜错长度几乎不可能通过校验。
    for len in 1..=MAX_PAYLOAD {
        let frame_bits = frame_bit_len(len);
        if frame_bits > raw.len() {
            break;
        }
        if let Some(payload) = decode_with_voting(&raw, frame_bits, len) {
            return Some(payload);
        }
    }
    None
}

/// 块坐标到 bit 槽位的映射。
fn slot_of(bx: usize, by: usize, frame_bits: usize) -> usize {
    ((by as u64 * SLOT_STRIDE + bx as u64) % frame_bits as u64) as usize
}

/// 对同一 bit 的多份副本做多数表决，再校验帧。
fn decode_with_voting(
    raw: &[(usize, usize, u8)],
    frame_bits: usize,
    payload_len: usize,
) -> Option<Vec<u8>> {
    let mut ones = vec![0u32; frame_bits];
    let mut total = vec![0u32; frame_bits];
    for &(bx, by, b) in raw {
        let slot = slot_of(bx, by, frame_bits);
        total[slot] += 1;
        ones[slot] += u32::from(b);
    }

    // 某个槽位一票都没收到（图像太小或裁得太狠）时无法判定，直接放弃这个候选。
    if total.contains(&0) {
        return None;
    }
    let bits: Vec<u8> = (0..frame_bits)
        .map(|i| u8::from(ones[i] * 2 > total[i]))
        .collect();
    decode_frame(&bits, payload_len)
}

/// 一帧的 bit 数：魔数(16) + 长度(8) + 载荷 + CRC(16)。
fn frame_bit_len(payload_len: usize) -> usize {
    16 + 8 + payload_len * 8 + 16
}

fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(3 + payload.len() + 2);
    bytes.extend_from_slice(&MAGIC.to_be_bytes());
    bytes.push(payload.len() as u8);
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(&crc16(payload).to_be_bytes());

    let mut bits = Vec::with_capacity(bytes.len() * 8);
    for byte in bytes {
        for shift in (0..8).rev() {
            bits.push((byte >> shift) & 1);
        }
    }
    bits
}

fn decode_frame(bits: &[u8], expected_len: usize) -> Option<Vec<u8>> {
    let (byte_chunks, _) = bits.as_chunks::<8>();
    let bytes: Vec<u8> = byte_chunks
        .iter()
        .map(|c| c.iter().fold(0u8, |acc, &b| (acc << 1) | b))
        .collect();
    if bytes.len() < 5 {
        return None;
    }

    if u16::from_be_bytes([bytes[0], bytes[1]]) != MAGIC {
        return None;
    }
    let len = bytes[2] as usize;
    if len != expected_len || len == 0 || bytes.len() < 3 + len + 2 {
        return None;
    }

    let payload = &bytes[3..3 + len];
    let crc = u16::from_be_bytes([bytes[3 + len], bytes[4 + len]]);
    if crc != crc16(payload) {
        return None;
    }
    Some(payload.to_vec())
}

/// CRC-16/CCITT-FALSE。
///
/// 它的作用不是纠错，而是判定"这次提取到底成没成"——没有它，
/// 从一张没嵌过水印的图里也能读出一串看似合理的乱码。
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// 把系数量化到奇/偶格子以承载 1 bit。
fn quantize_to_parity(value: f32, bit: u8, step: f32) -> f32 {
    let q = (value / step).round();
    // 把 q 调整到与目标 bit 同奇偶，并选择离原值更近的那一侧，减少画面改动。
    let target = if (q as i64).rem_euclid(2) as u8 == bit {
        q
    } else if value >= q * step {
        q + 1.0
    } else {
        q - 1.0
    };
    target * step
}

fn parity_of(value: f32) -> u8 {
    ((value / Strength::DEFAULT.get()).round() as i64).rem_euclid(2) as u8
}

/// 取出亮度通道（BT.601）。
///
/// 只在亮度上做改动、保持色度不变，可避免出现彩色噪点。
fn extract_luma(image: &RgbaImage) -> Vec<f32> {
    image
        .pixels()
        .map(|p| 0.299 * f32::from(p.0[0]) + 0.587 * f32::from(p.0[1]) + 0.114 * f32::from(p.0[2]))
        .collect()
}

/// 把修改后的亮度写回，保持原有色度。
fn apply_luma(out: &mut RgbaImage, original: &RgbaImage, luma: &[f32]) {
    for ((dst, src), &new_y) in out.pixels_mut().zip(original.pixels()).zip(luma.iter()) {
        let old_y =
            0.299 * f32::from(src.0[0]) + 0.587 * f32::from(src.0[1]) + 0.114 * f32::from(src.0[2]);
        let delta = new_y - old_y;
        for c in 0..3 {
            dst.0[c] = (f32::from(src.0[c]) + delta).round().clamp(0.0, 255.0) as u8;
        }
    }
}

fn read_block(luma: &[f32], width: usize, bx: usize, by: usize) -> [f32; BLOCK * BLOCK] {
    let mut block = [0f32; BLOCK * BLOCK];
    for y in 0..BLOCK {
        let row = (by * BLOCK + y) * width + bx * BLOCK;
        block[y * BLOCK..(y + 1) * BLOCK].copy_from_slice(&luma[row..row + BLOCK]);
    }
    block
}

fn write_block(luma: &mut [f32], width: usize, bx: usize, by: usize, block: &[f32; BLOCK * BLOCK]) {
    for y in 0..BLOCK {
        let row = (by * BLOCK + y) * width + bx * BLOCK;
        luma[row..row + BLOCK].copy_from_slice(&block[y * BLOCK..(y + 1) * BLOCK]);
    }
}

/// 8×8 二维 DCT-II，行列分离实现。
fn dct8x8(block: &mut [f32; BLOCK * BLOCK]) {
    let mut tmp = [0f32; BLOCK * BLOCK];
    for y in 0..BLOCK {
        let row: [f32; BLOCK] = block[y * BLOCK..(y + 1) * BLOCK].try_into().unwrap();
        let out = dct1d(&row);
        tmp[y * BLOCK..(y + 1) * BLOCK].copy_from_slice(&out);
    }
    for x in 0..BLOCK {
        let col: [f32; BLOCK] = std::array::from_fn(|y| tmp[y * BLOCK + x]);
        let out = dct1d(&col);
        for y in 0..BLOCK {
            block[y * BLOCK + x] = out[y];
        }
    }
}

fn idct8x8(block: &mut [f32; BLOCK * BLOCK]) {
    let mut tmp = [0f32; BLOCK * BLOCK];
    for x in 0..BLOCK {
        let col: [f32; BLOCK] = std::array::from_fn(|y| block[y * BLOCK + x]);
        let out = idct1d(&col);
        for y in 0..BLOCK {
            tmp[y * BLOCK + x] = out[y];
        }
    }
    for y in 0..BLOCK {
        let row: [f32; BLOCK] = tmp[y * BLOCK..(y + 1) * BLOCK].try_into().unwrap();
        let out = idct1d(&row);
        block[y * BLOCK..(y + 1) * BLOCK].copy_from_slice(&out);
    }
}

/// 8 点一维 DCT-II（正交归一）。
fn dct1d(input: &[f32; BLOCK]) -> [f32; BLOCK] {
    std::array::from_fn(|k| {
        let scale = if k == 0 {
            (1.0f32 / BLOCK as f32).sqrt()
        } else {
            (2.0f32 / BLOCK as f32).sqrt()
        };
        let sum: f32 = (0..BLOCK)
            .map(|n| {
                input[n]
                    * ((std::f32::consts::PI / BLOCK as f32) * (n as f32 + 0.5) * k as f32).cos()
            })
            .sum();
        scale * sum
    })
}

fn idct1d(input: &[f32; BLOCK]) -> [f32; BLOCK] {
    std::array::from_fn(|n| {
        (0..BLOCK)
            .map(|k| {
                let scale = if k == 0 {
                    (1.0f32 / BLOCK as f32).sqrt()
                } else {
                    (2.0f32 / BLOCK as f32).sqrt()
                };
                scale
                    * input[k]
                    * ((std::f32::consts::PI / BLOCK as f32) * (n as f32 + 0.5) * k as f32).cos()
            })
            .sum()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一张有真实纹理的测试图。
    ///
    /// 纯色图对盲水印是过于理想的场景（DCT 高频几乎为零），必须用带纹理的图，
    /// 否则测出来的鲁棒性是虚的。
    fn sample(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, y| {
            let fx = x as f32;
            let fy = y as f32;
            let v = 110.0
                + 60.0 * (fx / 37.0).sin()
                + 40.0 * (fy / 23.0).cos()
                + 25.0 * ((fx + fy) / 11.0).sin();
            let c = v.clamp(0.0, 255.0) as u8;
            image::Rgba([c, c.saturating_add(18), c.saturating_sub(12), 255])
        })
    }

    fn jpeg_roundtrip(image: &RgbaImage, quality: u8) -> RgbaImage {
        let mut buf = Vec::new();
        let rgb = image::DynamicImage::ImageRgba8(image.clone()).to_rgb8();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
        image::DynamicImage::ImageRgb8(rgb)
            .write_with_encoder(encoder)
            .unwrap();
        image::load_from_memory(&buf).unwrap().to_rgba8()
    }

    fn psnr(a: &RgbaImage, b: &RgbaImage) -> f64 {
        let mut sum = 0f64;
        let mut n = 0u64;
        for (pa, pb) in a.pixels().zip(b.pixels()) {
            for c in 0..3 {
                let d = f64::from(pa.0[c]) - f64::from(pb.0[c]);
                sum += d * d;
                n += 1;
            }
        }
        let mse = sum / n as f64;
        if mse <= f64::EPSILON {
            return 99.0;
        }
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }

    #[test]
    fn dct_roundtrip_is_lossless() {
        let mut block: [f32; 64] = std::array::from_fn(|i| ((i * 7) % 251) as f32 - 128.0);
        let original = block;
        dct8x8(&mut block);
        idct8x8(&mut block);
        for (a, b) in original.iter().zip(block.iter()) {
            assert!((a - b).abs() < 1e-3, "DCT 往返误差过大: {a} vs {b}");
        }
    }

    #[test]
    fn dct_concentrates_energy_in_low_frequency() {
        // 平坦块的能量应当几乎全在 DC 分量上——这是 DCT 实现正确的基本特征。
        let mut block = [100f32; 64];
        dct8x8(&mut block);
        assert!(block[0].abs() > 700.0, "DC 分量异常: {}", block[0]);
        for (i, v) in block.iter().enumerate().skip(1) {
            assert!(v.abs() < 1.0, "系数 {i} 本应接近 0，实际 {v}");
        }
    }

    #[test]
    fn embeds_and_extracts_without_attack() {
        let img = sample(320, 240);
        let payload = b"user-42@example.com";
        let marked = embed(&img, payload, Strength::DEFAULT).unwrap();
        assert_eq!(extract(&marked).as_deref(), Some(payload.as_slice()));
    }

    #[test]
    fn watermark_is_visually_negligible() {
        let img = sample(320, 240);
        let marked = embed(&img, b"invisible", Strength::DEFAULT).unwrap();
        let quality = psnr(&img, &marked);
        // 40 dB 以上属于肉眼难以分辨的范畴。
        assert!(quality > 38.0, "盲水印改动过大，PSNR 仅 {quality:.1} dB");
    }

    #[test]
    fn clean_image_yields_no_false_positive() {
        // 没嵌过水印的图必须提取不到东西。若无魔数与 CRC 把关，
        // 这里会读出一串看似合理的乱码。
        let img = sample(256, 256);
        assert_eq!(extract(&img), None);
    }

    #[test]
    fn survives_jpeg_compression() {
        let img = sample(480, 360);
        let payload = b"trace-id-8891";
        let marked = embed(&img, payload, Strength::DEFAULT).unwrap();

        // 声明能力边界的依据：质量 75 以上必须可靠提取。
        for quality in [95u8, 90, 85, 80, 75] {
            let attacked = jpeg_roundtrip(&marked, quality);
            assert_eq!(
                extract(&attacked).as_deref(),
                Some(payload.as_slice()),
                "JPEG 质量 {quality} 下提取失败"
            );
        }
    }

    #[test]
    fn stronger_setting_survives_harsher_compression() {
        let img = sample(480, 360);
        let payload = b"robust";
        let marked = embed(&img, payload, Strength::ROBUST).unwrap();
        let attacked = jpeg_roundtrip(&marked, 60);
        assert_eq!(
            extract(&attacked).as_deref(),
            Some(payload.as_slice()),
            "ROBUST 强度未能扛住质量 60 的压缩"
        );
    }

    #[test]
    fn payload_bounds_are_enforced() {
        let img = sample(128, 128);
        assert!(
            embed(&img, b"", Strength::DEFAULT).is_err(),
            "空载荷应被拒绝"
        );
        let too_long = vec![b'x'; MAX_PAYLOAD + 1];
        assert!(embed(&img, &too_long, Strength::DEFAULT).is_err());
    }

    #[test]
    fn tiny_image_is_rejected_with_clear_reason() {
        // 16×16 只有 4 个块，放不下一帧。
        let img = sample(16, 16);
        let err = embed(&img, b"some-identifier", Strength::DEFAULT).unwrap_err();
        assert!(
            format!("{err}").contains("块"),
            "错误信息应当说明容量不足: {err}"
        );
    }

    #[test]
    fn survives_partial_damage() {
        // 涂掉图像的一角，模拟局部编辑。载荷被反复写满整图，
        // 多数表决应当仍能恢复。
        let img = sample(480, 360);
        let payload = b"survive";
        let mut marked = embed(&img, payload, Strength::DEFAULT).unwrap();
        for y in 0..120 {
            for x in 0..160 {
                marked.put_pixel(x, y, image::Rgba([0, 0, 0, 255]));
            }
        }
        assert_eq!(extract(&marked).as_deref(), Some(payload.as_slice()));
    }

    /// 裁掉图像右下角，左上角原点保持不动。
    fn crop_br(img: &RgbaImage, cut_w: u32, cut_h: u32) -> RgbaImage {
        let (w, h) = img.dimensions();
        image::imageops::crop_imm(img, 0, 0, w - cut_w, h - cut_h).to_image()
    }

    #[test]
    fn survives_cropping_right_and_bottom() {
        // 裁掉角标水印是最常见的裁剪方式，必须扛住。
        // 槽位按块坐标计算，只要左上角原点没动，槽位就不会错位。
        let img = sample(640, 480);
        let payload = b"crop-test";
        let marked = embed(&img, payload, Strength::DEFAULT).unwrap();

        for (cw, ch) in [(160u32, 0u32), (0, 104), (160, 104), (37, 29)] {
            let cropped = crop_br(&marked, cw, ch);
            assert_eq!(
                extract(&cropped).as_deref(),
                Some(payload.as_slice()),
                "裁掉 {cw}x{ch} 后无法提取"
            );
        }
    }

    #[test]
    fn cropping_the_origin_away_breaks_extraction() {
        // 如实锁定另一半边界：从左侧/顶部裁会平移块坐标，槽位随之错位。
        // 恢复它需要搜索同步偏移，本实现没有做——这条测试防止文档与实现脱节。
        let img = sample(640, 480);
        let marked = embed(&img, b"origin", Strength::DEFAULT).unwrap();
        let shifted = image::imageops::crop_imm(&marked, 80, 48, 480, 360).to_image();
        assert_eq!(
            extract(&shifted),
            None,
            "若这条通过了，说明实现已能抗原点平移，文档需要同步更新"
        );
    }

    #[test]
    fn different_payloads_are_distinguishable() {
        let img = sample(320, 240);
        let a = embed(&img, b"alice", Strength::DEFAULT).unwrap();
        let b = embed(&img, b"bob", Strength::DEFAULT).unwrap();
        assert_eq!(extract(&a).as_deref(), Some(b"alice".as_slice()));
        assert_eq!(extract(&b).as_deref(), Some(b"bob".as_slice()));
    }
}
