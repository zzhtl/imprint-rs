//! 水印文本的模板字段求值。
//!
//! 模板形如 `"{filename} · {exif.model}"`。两类"取不到值"被刻意区别对待：
//! - **未知字段**（拼错了）原样保留 `{xxx}`，让用户一眼看出问题；
//! - **已知但缺失**（这张图没有 EXIF）替换为空串。
//!
//! 字面大括号用 `{{` / `}}` 转义。

use chrono::{DateTime, Local};

/// 求值一次模板所需的全部上下文。
///
/// 时间在构造时一次性取定而非每次求值现取：批量导出上百个文件时，
/// 同一批产物应当带同一个时间戳。
#[derive(Debug, Clone)]
pub struct FieldContext {
    /// 不含扩展名的文件名。
    pub filename: Option<String>,
    /// 含扩展名的文件名。
    pub filename_with_ext: Option<String>,
    /// 素材像素尺寸。
    pub dimensions: Option<(u32, u32)>,
    pub exif: ExifFields,
    now: DateTime<Local>,
}

impl Default for FieldContext {
    fn default() -> Self {
        Self {
            filename: None,
            filename_with_ext: None,
            dimensions: None,
            exif: ExifFields::default(),
            now: Local::now(),
        }
    }
}

impl FieldContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// 固定"当前时间"，供测试与可复现导出使用。
    pub fn with_now(mut self, now: DateTime<Local>) -> Self {
        self.now = now;
        self
    }

    /// 从文件名填充 `{filename}` / `{filename_ext}`。
    ///
    /// 取 `&str` 而非 `&Path`：移动端拿到的是 `content://` URI，没有文件系统路径。
    pub fn with_file_name(mut self, name: &str) -> Self {
        self.filename_with_ext = Some(name.to_owned());
        let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
        self.filename = Some(stem.to_owned());
        self
    }

    pub fn with_dimensions(mut self, width: u32, height: u32) -> Self {
        self.dimensions = Some((width, height));
        self
    }

    pub fn with_exif(mut self, exif: ExifFields) -> Self {
        self.exif = exif;
        self
    }

    /// 查一个字段。`None` = 字段名未知；`Some("")` = 已知但本次无值。
    fn lookup(&self, key: &str) -> Option<String> {
        let v = match key {
            "filename" => self.filename.clone().unwrap_or_default(),
            "filename_ext" => self.filename_with_ext.clone().unwrap_or_default(),
            "width" => self
                .dimensions
                .map(|(w, _)| w.to_string())
                .unwrap_or_default(),
            "height" => self
                .dimensions
                .map(|(_, h)| h.to_string())
                .unwrap_or_default(),
            "date" => self.now.format("%Y-%m-%d").to_string(),
            "time" => self.now.format("%H:%M:%S").to_string(),
            "datetime" => self.now.format("%Y-%m-%d %H:%M:%S").to_string(),
            "exif.datetime" => self.exif.datetime.clone().unwrap_or_default(),
            "exif.make" => self.exif.make.clone().unwrap_or_default(),
            "exif.model" => self.exif.model.clone().unwrap_or_default(),
            "exif.lens" => self.exif.lens.clone().unwrap_or_default(),
            "exif.iso" => self.exif.iso.clone().unwrap_or_default(),
            "exif.fnumber" => self.exif.f_number.clone().unwrap_or_default(),
            "exif.exposure" => self.exif.exposure.clone().unwrap_or_default(),
            "exif.focal" => self.exif.focal_length.clone().unwrap_or_default(),
            _ => return None,
        };
        Some(v)
    }
}

/// 从 EXIF 中提取的、可用于水印的字段。
///
/// 全部保存为已格式化的字符串：水印只需要展示，不需要数值语义，
/// 而 EXIF 的有理数（光圈、曝光）转字符串的规则是展示层决定的。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExifFields {
    pub datetime: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<String>,
    pub f_number: Option<String>,
    pub exposure: Option<String>,
    pub focal_length: Option<String>,
}

impl ExifFields {
    /// 解析 `image` 的 `ImageDecoder::exif_metadata()` 给出的原始 EXIF blob。
    ///
    /// `image` 只负责搬运这段不透明字节，解析必须由第三方库完成 —— 这正是
    /// 本 crate 依赖 `kamadak-exif` 的唯一理由。
    ///
    /// 解析失败返回全空而非报错：EXIF 缺失或损坏不应让整个水印任务失败。
    pub fn parse(raw: &[u8]) -> Self {
        match Self::try_parse(raw) {
            Ok(f) => f,
            Err(e) => {
                log::debug!("EXIF 解析失败，按无 EXIF 处理: {e}");
                Self::default()
            }
        }
    }

    fn try_parse(raw: &[u8]) -> Result<Self, exif::Error> {
        use exif::{In, Tag};

        // image 交出的 blob 从 TIFF header 起算，正是 read_raw 期待的形状。
        let exif = exif::Reader::new().read_raw(raw.to_vec())?;

        let text = |tag: Tag| -> Option<String> {
            exif.get_field(tag, In::PRIMARY).map(|f| {
                // display_value() 已按标签语义做好格式化（光圈 f/2.8、曝光 1/250 s 等）。
                f.display_value().with_unit(&exif).to_string()
            })
        };

        Ok(Self {
            datetime: text(Tag::DateTimeOriginal).or_else(|| text(Tag::DateTime)),
            make: text(Tag::Make),
            model: text(Tag::Model),
            lens: text(Tag::LensModel),
            iso: text(Tag::PhotographicSensitivity),
            f_number: text(Tag::FNumber),
            exposure: text(Tag::ExposureTime),
            focal_length: text(Tag::FocalLength),
        })
    }
}

/// 模板里是否含占位符。
///
/// 图层缓存据此决定 key 是否必须带上求值后的文本 —— 否则批量处理时
/// 第一个文件的 `{filename}` 会被后续文件复用，导致串味。
pub fn has_placeholder(template: &str) -> bool {
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' if bytes.get(i + 1) == Some(&b'{') => i += 2,
            b'{' => return bytes[i + 1..].contains(&b'}'),
            _ => i += 1,
        }
    }
    false
}

/// 按上下文求值模板。
pub fn render(template: &str, ctx: &FieldContext) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(pos) = rest.find(['{', '}']) {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];

        // `{{` / `}}` 转义为字面量。
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push(tail.as_bytes()[0] as char);
            rest = &tail[2..];
            continue;
        }

        // 落单的 `}` 没有配对意义，原样输出。
        if let Some(after) = tail.strip_prefix('}') {
            out.push('}');
            rest = after;
            continue;
        }

        match tail.find('}') {
            Some(end) => {
                let key = &tail[1..end];
                match ctx.lookup(key) {
                    Some(v) => out.push_str(&v),
                    // 未知字段原样保留，让拼写错误可见。
                    None => out.push_str(&tail[..=end]),
                }
                rest = &tail[end + 1..];
            }
            // 没有闭合大括号，剩下的全是字面量。
            None => {
                out.push_str(tail);
                return out;
            }
        }
    }

    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ctx() -> FieldContext {
        FieldContext::new()
            .with_now(Local.with_ymd_and_hms(2026, 9, 15, 14, 30, 5).unwrap())
            .with_file_name("IMG_0042.jpg")
            .with_dimensions(4032, 3024)
    }

    #[test]
    fn renders_known_fields() {
        assert_eq!(render("{filename}", &ctx()), "IMG_0042");
        assert_eq!(render("{filename_ext}", &ctx()), "IMG_0042.jpg");
        assert_eq!(render("{width}x{height}", &ctx()), "4032x3024");
        assert_eq!(render("{date}", &ctx()), "2026-09-15");
        assert_eq!(render("{datetime}", &ctx()), "2026-09-15 14:30:05");
    }

    #[test]
    fn keeps_unknown_fields_verbatim() {
        assert_eq!(render("{nope} ok", &ctx()), "{nope} ok");
    }

    #[test]
    fn known_but_missing_renders_empty() {
        let c = FieldContext::new();
        assert_eq!(render("[{filename}]", &c), "[]");
        assert_eq!(render("[{exif.model}]", &c), "[]");
    }

    #[test]
    fn handles_escapes_and_strays() {
        assert_eq!(render("{{literal}}", &ctx()), "{literal}");
        assert_eq!(render("100% {{", &ctx()), "100% {");
        assert_eq!(render("unclosed {filename", &ctx()), "unclosed {filename");
        assert_eq!(render("stray } here", &ctx()), "stray } here");
    }

    #[test]
    fn multibyte_text_is_preserved() {
        // 中文水印是主要场景，切片边界不能按字节乱切。
        assert_eq!(
            render("拍摄于 {date} 中文", &ctx()),
            "拍摄于 2026-09-15 中文"
        );
    }

    #[test]
    fn placeholder_detection() {
        assert!(has_placeholder("{filename}"));
        assert!(has_placeholder("a {x} b"));
        assert!(!has_placeholder("no fields"));
        assert!(!has_placeholder("{{escaped}}"));
        assert!(!has_placeholder("unclosed {"));
    }
}
