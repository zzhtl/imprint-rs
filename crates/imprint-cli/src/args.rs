//! 命令行参数 → [`WatermarkSpec`] 的转换。
//!
//! 解析全部写成纯函数：参数格式（尺寸、位置、颜色）是最容易写错又最该被测到的部分，
//! 不该等到跑真素材才发现。

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Args;
use imprint_core::spec::{
    Anchor9, Content, ImageSource, ImageSpec, Margin, Placement, Rgba8, SizeMode, TextSpec,
    WatermarkSpec,
};

/// 所有水印相关的公共参数。
#[derive(Debug, Clone, Args)]
pub struct WatermarkArgs {
    /// 文字水印内容，支持模板：{filename} {date} {time} {datetime}
    /// {width} {height} {exif.model} {exif.datetime} 等
    #[arg(long, value_name = "TEXT")]
    pub text: Option<String>,

    /// 图片水印文件（PNG/JPEG/WebP/SVG）
    #[arg(long, value_name = "FILE", conflicts_with = "text")]
    pub logo: Option<PathBuf>,

    /// 尺寸。`0.05`=画布高度的 5%（字号）；`48px`=绝对字号；
    /// `w:0.3`=画布宽度的 30%；`w:400px`=绝对宽度 [默认: 0.04]
    #[arg(long, value_name = "SIZE")]
    pub size: Option<String>,

    /// 位置。九宫格 tl/tc/tr/cl/c/cr/bl/bc/br，或归一化坐标如 `0.5,0.7` [默认: br]
    #[arg(long, value_name = "POS")]
    pub position: Option<String>,

    /// 边距占画布的比例，格式 `x,y` [默认: 0.02,0.02]
    #[arg(long, value_name = "X,Y")]
    pub margin: Option<String>,

    /// 平铺满屏（防截图/防泄密）
    #[arg(long)]
    pub tile: bool,

    /// 平铺间距，相对水印自身尺寸的倍数，格式 `x,y` [默认: 0.6,1.2]
    #[arg(long, value_name = "X,Y")]
    pub tile_spacing: Option<String>,

    /// 平铺时奇数行错开半格
    #[arg(long)]
    pub stagger: bool,

    /// 不透明度 0.0~1.0 [默认: 0.6]
    #[arg(long, value_name = "N")]
    pub opacity: Option<f32>,

    /// 旋转角度（度），可为负 [默认: 0]
    #[arg(long, value_name = "DEG", allow_negative_numbers = true)]
    pub rotate: Option<f32>,

    /// 文字颜色，#RRGGBB 或 #RRGGBBAA [默认: #FFFFFF]
    #[arg(long, value_name = "HEX")]
    pub color: Option<String>,

    /// 字体族名，留空用系统默认无衬线字体
    #[arg(long, value_name = "NAME")]
    pub font: Option<String>,

    /// 字重 100~900 [默认: 400]
    #[arg(long, value_name = "N")]
    pub weight: Option<u16>,

    /// 从 JSON 预设读取水印规格；显式给出的其它参数会覆盖预设中的对应项
    #[arg(long, value_name = "FILE")]
    pub preset: Option<PathBuf>,
}

impl WatermarkArgs {
    /// 组装成 [`WatermarkSpec`]。
    ///
    /// 有预设时以预设为底、命令行显式给出的项覆盖之：既能复用 UI 存下的复杂配置，
    /// 又能在脚本里临时改一两项。所有可选项都用 `Option` 表达"未指定"，
    /// 因此不需要再去问 clap 某个值是否来自默认值。
    pub fn to_spec(&self) -> anyhow::Result<WatermarkSpec> {
        let mut spec = match &self.preset {
            Some(path) => {
                let bytes =
                    std::fs::read(path).with_context(|| format!("读取预设 {}", path.display()))?;
                serde_json::from_slice::<WatermarkSpec>(&bytes)
                    .with_context(|| format!("解析预设 {}", path.display()))?
            }
            None => WatermarkSpec::default(),
        };

        // 内容：给了 --text 或 --logo 就替换，否则沿用预设里的。
        if let Some(text) = &self.text {
            let base = match &spec.content {
                Content::Text(t) => t.clone(),
                Content::Image(_) => TextSpec::default(),
            };
            spec.content = Content::Text(TextSpec {
                template: text.clone(),
                color: match &self.color {
                    Some(c) => parse_color(c)?,
                    None => base.color,
                },
                weight: self.weight.unwrap_or(base.weight),
                family: match &self.font {
                    Some(name) => imprint_core::spec::FontFamily::Name(name.clone()),
                    None => base.family.clone(),
                },
                ..base
            });
        } else if let Some(logo) = &self.logo {
            spec.content = Content::Image(ImageSpec {
                source: load_logo(logo)?,
            });
        } else if self.preset.is_none() {
            bail!("必须指定 --text 或 --logo（或用 --preset 提供）");
        }

        if let Some(size) = &self.size {
            spec.size = parse_size(size)?;
        }

        if self.tile {
            spec.placement = Placement::Tile {
                spacing_ratio: match &self.tile_spacing {
                    Some(s) => parse_pair(s).context("解析 --tile-spacing")?,
                    None => (0.6, 1.2),
                },
                stagger: self.stagger,
            };
        } else if self.position.is_some() || self.margin.is_some() {
            spec.placement = parse_placement(
                self.position.as_deref().unwrap_or("br"),
                self.margin.as_deref().unwrap_or("0.02,0.02"),
            )?;
        }

        if let Some(opacity) = self.opacity {
            spec.opacity = opacity;
        }
        if let Some(rotate) = self.rotate {
            spec.rotation_deg = rotate;
        }

        spec.sanitize();
        Ok(spec)
    }
}

fn load_logo(path: &PathBuf) -> anyhow::Result<ImageSource> {
    let bytes = std::fs::read(path).with_context(|| format!("读取水印图片 {}", path.display()))?;
    let is_svg = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"));

    if is_svg {
        return Ok(ImageSource::Svg(
            String::from_utf8(bytes).context("SVG 不是合法 UTF-8")?,
        ));
    }
    Ok(ImageSource::Raster(bytes))
}

/// 解析尺寸表达式。
pub fn parse_size(input: &str) -> anyhow::Result<SizeMode> {
    let s = input.trim();
    // `w:` 前缀表示按宽度而非字号解释。
    let (by_width, rest) = match s.strip_prefix("w:") {
        Some(rest) => (true, rest.trim()),
        None => (false, s),
    };

    if let Some(px) = rest.strip_suffix("px") {
        let value: f32 = px
            .trim()
            .parse()
            .with_context(|| format!("无法解析绝对尺寸 `{input}`"))?;
        if !(value.is_finite() && value > 0.0) {
            bail!("尺寸必须为正数：`{input}`");
        }
        return Ok(if by_width {
            SizeMode::AbsoluteWidth {
                px: value.round() as u32,
            }
        } else {
            SizeMode::AbsoluteFontSize { px: value }
        });
    }

    let ratio: f32 = rest
        .parse()
        .with_context(|| format!("无法解析尺寸比例 `{input}`（示例：0.05、48px、w:0.3）"))?;
    if !(ratio.is_finite() && ratio > 0.0) {
        bail!("尺寸比例必须为正数：`{input}`");
    }
    Ok(if by_width {
        SizeMode::RelativeWidth(ratio)
    } else {
        SizeMode::RelativeFontSize(ratio)
    })
}

/// 解析位置表达式：九宫格缩写或归一化坐标。
pub fn parse_placement(position: &str, margin: &str) -> anyhow::Result<Placement> {
    let p = position.trim();
    if let Some(anchor) = parse_anchor(p) {
        let (x_ratio, y_ratio) = parse_pair(margin).context("解析 --margin")?;
        return Ok(Placement::Anchor {
            anchor,
            margin: Margin { x_ratio, y_ratio },
        });
    }

    let (x, y) = parse_pair(p)
        .with_context(|| format!("无法解析位置 `{position}`（九宫格如 br，或坐标如 0.5,0.7）"))?;
    Ok(Placement::Normalized {
        x,
        y,
        // 坐标指的是水印中心，这与拖拽定位的语义一致。
        anchor: Anchor9::Center,
    })
}

fn parse_anchor(s: &str) -> Option<Anchor9> {
    let key = s.to_ascii_lowercase().replace(['_', ' '], "-");
    Some(match key.as_str() {
        "tl" | "top-left" => Anchor9::TopLeft,
        "tc" | "top" | "top-center" => Anchor9::TopCenter,
        "tr" | "top-right" => Anchor9::TopRight,
        "cl" | "left" | "center-left" => Anchor9::CenterLeft,
        "c" | "center" => Anchor9::Center,
        "cr" | "right" | "center-right" => Anchor9::CenterRight,
        "bl" | "bottom-left" => Anchor9::BottomLeft,
        "bc" | "bottom" | "bottom-center" => Anchor9::BottomCenter,
        "br" | "bottom-right" => Anchor9::BottomRight,
        _ => return None,
    })
}

/// 解析 `a,b` 形式的一对浮点数。
pub fn parse_pair(s: &str) -> anyhow::Result<(f32, f32)> {
    let (a, b) = s
        .split_once(',')
        .with_context(|| format!("期望 `x,y` 格式，得到 `{s}`"))?;
    Ok((
        a.trim().parse().with_context(|| format!("解析 `{a}`"))?,
        b.trim().parse().with_context(|| format!("解析 `{b}`"))?,
    ))
}

/// 解析 `#RRGGBB` / `#RRGGBBAA`（`#` 可省）。
pub fn parse_color(s: &str) -> anyhow::Result<Rgba8> {
    let hex = s.trim().trim_start_matches('#');
    let bytes = match hex.len() {
        6 | 8 => hex,
        _ => bail!("颜色应为 #RRGGBB 或 #RRGGBBAA，得到 `{s}`"),
    };
    let parse = |i: usize| -> anyhow::Result<u8> {
        u8::from_str_radix(&bytes[i..i + 2], 16)
            .with_context(|| format!("颜色 `{s}` 含非十六进制字符"))
    };
    Ok(Rgba8::new(
        parse(0)?,
        parse(2)?,
        parse(4)?,
        if bytes.len() == 8 { parse(6)? } else { 255 },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_defaults_to_relative_font_size() {
        assert_eq!(
            parse_size("0.05").unwrap(),
            SizeMode::RelativeFontSize(0.05)
        );
        assert_eq!(
            parse_size(" 0.1 ").unwrap(),
            SizeMode::RelativeFontSize(0.1)
        );
    }

    #[test]
    fn size_px_suffix_means_absolute() {
        assert_eq!(
            parse_size("48px").unwrap(),
            SizeMode::AbsoluteFontSize { px: 48.0 }
        );
        assert_eq!(
            parse_size("w:400px").unwrap(),
            SizeMode::AbsoluteWidth { px: 400 }
        );
    }

    #[test]
    fn size_w_prefix_switches_to_width() {
        assert_eq!(parse_size("w:0.3").unwrap(), SizeMode::RelativeWidth(0.3));
    }

    #[test]
    fn size_rejects_nonsense() {
        for bad in ["", "abc", "-0.5", "0", "px", "w:"] {
            assert!(parse_size(bad).is_err(), "`{bad}` 应当被拒绝");
        }
    }

    #[test]
    fn anchors_accept_short_and_long_forms() {
        for (input, want) in [
            ("br", Anchor9::BottomRight),
            ("bottom-right", Anchor9::BottomRight),
            ("BOTTOM_RIGHT", Anchor9::BottomRight),
            ("c", Anchor9::Center),
            ("tl", Anchor9::TopLeft),
        ] {
            let Placement::Anchor { anchor, .. } = parse_placement(input, "0.02,0.02").unwrap()
            else {
                panic!("`{input}` 应解析为九宫格");
            };
            assert_eq!(anchor, want, "输入 `{input}`");
        }
    }

    #[test]
    fn coordinates_become_normalized_placement() {
        let p = parse_placement("0.25,0.75", "0.02,0.02").unwrap();
        let Placement::Normalized { x, y, anchor } = p else {
            panic!("应解析为归一化坐标");
        };
        assert_eq!((x, y), (0.25, 0.75));
        // 坐标必须指向水印中心，才能和 UI 拖拽的语义对上。
        assert_eq!(anchor, Anchor9::Center);
    }

    #[test]
    fn placement_rejects_garbage() {
        assert!(parse_placement("nowhere", "0.02,0.02").is_err());
        assert!(parse_placement("0.5", "0.02,0.02").is_err());
    }

    #[test]
    fn colors_parse_with_and_without_alpha() {
        assert_eq!(
            parse_color("#FF8000").unwrap(),
            Rgba8::new(255, 128, 0, 255)
        );
        assert_eq!(
            parse_color("ff800080").unwrap(),
            Rgba8::new(255, 128, 0, 128)
        );
        assert_eq!(parse_color("#ffffff").unwrap(), Rgba8::WHITE);
    }

    #[test]
    fn colors_reject_bad_input() {
        for bad in ["#FFF", "#GGGGGG", "", "#FFFFFFF"] {
            assert!(parse_color(bad).is_err(), "`{bad}` 应当被拒绝");
        }
    }

    #[test]
    fn pair_parsing_tolerates_spaces() {
        assert_eq!(parse_pair(" 0.6 , 1.2 ").unwrap(), (0.6, 1.2));
        assert!(parse_pair("0.6").is_err());
    }
}
