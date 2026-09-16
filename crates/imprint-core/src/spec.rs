//! 水印参数模型。
//!
//! 位置与尺寸一律以归一化 / 相对量表达，绝对像素只作可选模式。这是
//! "预览（缩略图）→ 导出（原图）→ 批量（混合分辨率）" 三者结果一致的前提，
//! 也是鼠标拖拽定位能跨分辨率成立的基础。

use serde::{Deserialize, Serialize};

/// 一条完整的水印规格。
///
/// 同一份 spec 会被三处消费：预览、图片合成、视频 overlay 的 PNG 输入。
/// 因此这里不含任何与具体素材尺寸绑定的绝对量（除非调用方显式选了 `Absolute*`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatermarkSpec {
    pub content: Content,
    pub size: SizeMode,
    pub placement: Placement,
    /// 整体不透明度，0.0 全透明，1.0 不透明。
    pub opacity: f32,
    /// 顺时针旋转角度（度）。tiny-skia 的 `Transform::from_rotate_at` 也收度，不需换算。
    pub rotation_deg: f32,
}

impl Default for WatermarkSpec {
    fn default() -> Self {
        Self {
            content: Content::Text(TextSpec::default()),
            size: SizeMode::default(),
            placement: Placement::default(),
            opacity: 0.6,
            rotation_deg: 0.0,
        }
    }
}

impl WatermarkSpec {
    /// 把越界参数收敛到合法区间。
    ///
    /// 参数来自 UI 滑块与反序列化的预设，越界是可修复的输入而非错误，
    /// 所以这里就地钳制，不返回 `Result`。
    pub fn sanitize(&mut self) {
        self.opacity = self.opacity.clamp(0.0, 1.0);
        if !self.rotation_deg.is_finite() {
            self.rotation_deg = 0.0;
        }
        self.size.sanitize();
        self.placement.sanitize();
        self.content.sanitize();
    }
}

/// 水印画什么。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Content {
    Text(TextSpec),
    Image(ImageSpec),
}

impl Content {
    fn sanitize(&mut self) {
        match self {
            Self::Text(t) => t.sanitize(),
            Self::Image(_) => {}
        }
    }

    /// 内容是否含运行期才能求值的模板字段。
    ///
    /// 图层缓存要据此决定 key 是否必须带上求值后的文本，否则批量处理会跨文件串味。
    pub fn has_dynamic_fields(&self) -> bool {
        match self {
            Self::Text(t) => crate::fields::has_placeholder(&t.template),
            Self::Image(_) => false,
        }
    }
}

/// 文字水印。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextSpec {
    /// 可含模板字段，如 `"{filename} · {datetime}"`，导出时按每个文件求值。
    pub template: String,
    pub family: FontFamily,
    /// CSS 数值字重：400 常规，700 粗体。
    pub weight: u16,
    pub italic: bool,
    pub color: Rgba8,
    pub stroke: Option<StrokeSpec>,
    pub shadow: Option<ShadowSpec>,
    /// 行高倍数，相对字号。
    pub line_height: f32,
    pub align: TextAlign,
}

impl Default for TextSpec {
    fn default() -> Self {
        Self {
            template: String::new(),
            family: FontFamily::SansSerif,
            weight: 400,
            italic: false,
            color: Rgba8::WHITE,
            stroke: None,
            shadow: None,
            line_height: 1.2,
            align: TextAlign::Left,
        }
    }
}

impl TextSpec {
    fn sanitize(&mut self) {
        self.weight = self.weight.clamp(100, 900);
        if !self.line_height.is_finite() || self.line_height <= 0.0 {
            self.line_height = 1.2;
        }
        if let Some(s) = &mut self.stroke {
            s.sanitize();
        }
        if let Some(s) = &mut self.shadow {
            s.sanitize();
        }
    }
}

/// 字体族选择。
///
/// `Name` 走 cosmic-text 的字族查询；`Embedded` 指向内置的兜底字体 ——
/// 移动端必须用后者：cosmic-text 在 Android 的 fallback 表是空的，
/// 在 iOS 则错误地套用了 Linux 字族名，两端都无法靠系统回退渲染中文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FontFamily {
    SansSerif,
    Serif,
    Monospace,
    Name(String),
    Embedded,
}

/// 文字描边。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrokeSpec {
    /// 描边宽度，相对字号的倍数 —— 跟随字号缩放，避免大图上描边显得过细。
    pub width_ratio: f32,
    pub color: Rgba8,
}

impl StrokeSpec {
    fn sanitize(&mut self) {
        if !self.width_ratio.is_finite() || self.width_ratio < 0.0 {
            self.width_ratio = 0.0;
        }
        self.width_ratio = self.width_ratio.min(0.5);
    }
}

/// 文字投影。
///
/// tiny-skia 0.12 没有模糊，这里是硬阴影（按偏移量重绘一次）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShadowSpec {
    /// 偏移量，相对字号的倍数。
    pub offset_ratio: (f32, f32),
    pub color: Rgba8,
}

impl ShadowSpec {
    fn sanitize(&mut self) {
        let (x, y) = self.offset_ratio;
        self.offset_ratio = (
            if x.is_finite() {
                x.clamp(-1.0, 1.0)
            } else {
                0.0
            },
            if y.is_finite() {
                y.clamp(-1.0, 1.0)
            } else {
                0.0
            },
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// 图片 / Logo 水印。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageSpec {
    pub source: ImageSource,
}

/// 水印图源。
///
/// 存字节而非路径：`imprint-core` 要能在 Android 上工作，那里文件选择返回的是
/// `content://` URI 而非文件系统路径。预设序列化时路径也会失效。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageSource {
    /// 位图字节（PNG / JPEG / WebP ...），由 `image` 解码。
    Raster(Vec<u8>),
    /// SVG 源码，按目标尺寸光栅化，不失真。
    #[cfg(feature = "svg")]
    Svg(String),
}

/// 水印尺寸的表达方式。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SizeMode {
    /// 水印宽 = 素材宽 × ratio。批量跨分辨率的默认选择。
    RelativeWidth(f32),
    /// 字号 = 素材高 × ratio。文字水印比按宽度更符合直觉。
    RelativeFontSize(f32),
    AbsoluteWidth {
        px: u32,
    },
    AbsoluteFontSize {
        px: f32,
    },
}

impl Default for SizeMode {
    fn default() -> Self {
        Self::RelativeFontSize(0.04)
    }
}

impl SizeMode {
    fn sanitize(&mut self) {
        match self {
            Self::RelativeWidth(r) | Self::RelativeFontSize(r) => {
                if !r.is_finite() || *r <= 0.0 {
                    *r = 0.04;
                }
                *r = r.min(1.0);
            }
            Self::AbsoluteWidth { px } => *px = (*px).max(1),
            Self::AbsoluteFontSize { px } => {
                if !px.is_finite() || *px <= 0.0 {
                    *px = 32.0;
                }
            }
        }
    }
}

/// 水印摆在哪。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Placement {
    /// 九宫格锚点 + 相对边距。
    Anchor { anchor: Anchor9, margin: Margin },
    /// 归一化坐标（0..1），鼠标拖拽产生。`anchor` 指明该坐标对应水印的哪个点。
    Normalized { x: f32, y: f32, anchor: Anchor9 },
    /// 平铺满屏，用于防截图 / 防泄密。
    Tile {
        /// 相邻水印的间距，相对水印自身尺寸的倍数。
        spacing_ratio: (f32, f32),
        /// 奇数行错半格，避免形成明显的竖直通道。
        stagger: bool,
    },
}

impl Default for Placement {
    fn default() -> Self {
        Self::Anchor {
            anchor: Anchor9::BottomRight,
            margin: Margin::default(),
        }
    }
}

impl Placement {
    fn sanitize(&mut self) {
        match self {
            Self::Anchor { margin, .. } => margin.sanitize(),
            Self::Normalized { x, y, .. } => {
                *x = if x.is_finite() {
                    x.clamp(0.0, 1.0)
                } else {
                    0.5
                };
                *y = if y.is_finite() {
                    y.clamp(0.0, 1.0)
                } else {
                    0.5
                };
            }
            Self::Tile { spacing_ratio, .. } => {
                let (sx, sy) = *spacing_ratio;
                // 间距为 0 会让平铺退化成实心覆盖，下限留一点空隙。
                *spacing_ratio = (
                    if sx.is_finite() {
                        sx.clamp(0.05, 10.0)
                    } else {
                        0.5
                    },
                    if sy.is_finite() {
                        sy.clamp(0.05, 10.0)
                    } else {
                        0.5
                    },
                );
            }
        }
    }
}

/// 九宫格锚点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Anchor9 {
    TopLeft,
    TopCenter,
    TopRight,
    CenterLeft,
    Center,
    CenterRight,
    BottomLeft,
    BottomCenter,
    #[default]
    BottomRight,
}

impl Anchor9 {
    /// 锚点在水印自身包围盒内的归一化位置。
    pub fn unit_offset(self) -> (f32, f32) {
        let x = match self {
            Self::TopLeft | Self::CenterLeft | Self::BottomLeft => 0.0,
            Self::TopCenter | Self::Center | Self::BottomCenter => 0.5,
            Self::TopRight | Self::CenterRight | Self::BottomRight => 1.0,
        };
        let y = match self {
            Self::TopLeft | Self::TopCenter | Self::TopRight => 0.0,
            Self::CenterLeft | Self::Center | Self::CenterRight => 0.5,
            Self::BottomLeft | Self::BottomCenter | Self::BottomRight => 1.0,
        };
        (x, y)
    }
}

/// 相对边距，按素材尺寸的比例计算 —— 绝对像素在 4K 与 720p 上观感差异过大。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Margin {
    pub x_ratio: f32,
    pub y_ratio: f32,
}

impl Default for Margin {
    fn default() -> Self {
        Self {
            x_ratio: 0.02,
            y_ratio: 0.02,
        }
    }
}

impl Margin {
    fn sanitize(&mut self) {
        self.x_ratio = if self.x_ratio.is_finite() {
            self.x_ratio.clamp(0.0, 0.5)
        } else {
            0.02
        };
        self.y_ratio = if self.y_ratio.is_finite() {
            self.y_ratio.clamp(0.0, 0.5)
        } else {
            0.02
        };
    }
}

/// 非预乘的 8 位 RGBA。
///
/// 刻意不复用 `tiny_skia::Color`：那是预乘语义且不实现 `Serialize`，
/// 而这里要能原样存进预设文件。转换在 `layer` 边界上一次性做。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    pub const WHITE: Self = Self::new(255, 255, 255, 255);
    pub const BLACK: Self = Self::new(0, 0, 0, 255);

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}
