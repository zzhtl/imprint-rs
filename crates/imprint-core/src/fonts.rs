//! 字体库：系统字体只扫一次，跨线程低成本复用。
//!
//! `FontSystem` 不是 `Sync`，rayon 并行批处理时每个线程都要独立实例；而系统字体
//! 扫描在 release 下可达一秒（debug 十倍），逐线程重扫会让批量场景被字体加载拖垮。
//! 这里把扫描结果拆成 `(locale, fontdb::Database)` 保存，按线程廉价克隆出 `FontSystem`。

use std::sync::Arc;

use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping,
    fontdb::{self, Source},
};

use crate::{error::Result, spec::FontFamily};

/// 一份可交给外部字体栈（如 egui）使用的字体数据。
#[derive(Clone)]
pub struct FontBlob {
    pub family: String,
    pub data: Vec<u8>,
    /// 集合字体（`.ttc`）中的 face 序号。
    ///
    /// 中文字体常以 `.ttc` 分发（本机就是 `NotoSansCJK-Regular.ttc`），
    /// 一个文件里塞着 SC/TC/JP/KR 多套字形。丢掉这个序号会取到日文或韩文字形。
    pub face_index: u32,
}

impl std::fmt::Debug for FontBlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 字体动辄十几 MB，Debug 里只能给元信息。
        f.debug_struct("FontBlob")
            .field("family", &self.family)
            .field("bytes", &self.data.len())
            .field("face_index", &self.face_index)
            .finish()
    }
}

/// 可跨线程复用的字体库。
pub struct FontLibrary {
    locale: String,
    db: fontdb::Database,
    embedded_family: Option<String>,
}

impl FontLibrary {
    /// 扫描系统字体。
    ///
    /// 借道 `FontSystem::new()` 再拆解，是为了复用 cosmic-text 自己的 locale 探测
    /// 和默认字族设置（它会 `set_sans_serif_family("Open Sans")` 等），
    /// 避免我们重复实现一遍、还实现得不一致。
    ///
    /// 整个进程只应调用一次。
    pub fn with_system_fonts() -> Self {
        let (locale, db) = FontSystem::new().into_locale_and_db();
        log::debug!("字体库就绪：locale={locale}, {} 个 face", db.len());
        Self {
            locale,
            db,
            embedded_family: None,
        }
    }

    /// 建一个不含系统字体的空库，字体全靠 [`Self::add_font`] 注入。
    ///
    /// 移动端必须走这条路：cosmic-text 在 Android 的平台回退表三个方法全返回空切片，
    /// 在 iOS 则错误地匹配到 unix 分支、拿到一串 Linux 字族名（`Noto Sans`、`DejaVu Sans`），
    /// 这些在 iOS 上一个都不存在。两端都等于零回退，中文必然渲染成豆腐块。
    ///
    /// `locale` 由调用方从平台 API 获取（如 `zh-CN`），它决定 CJK 统一码点取哪套字形。
    pub fn without_system_fonts(locale: impl Into<String>) -> Self {
        Self {
            locale: locale.into(),
            db: fontdb::Database::new(),
            embedded_family: None,
        }
    }

    /// 注入一份字体数据，返回它的字族名。
    ///
    /// 首次注入的字体会自动成为 [`FontFamily::Embedded`] 的目标。
    pub fn add_font(&mut self, data: Vec<u8>) -> Result<String> {
        let ids = self.db.load_font_source(Source::Binary(Arc::new(data)));
        let family = ids
            .iter()
            .find_map(|id| self.db.face(*id))
            // families 的首项按约定是 English US 名称，用它作为稳定标识。
            .and_then(|face| face.families.first())
            .map(|(name, _)| name.clone())
            .ok_or_else(|| crate::Error::Font("字体数据无法解析出任何 face".to_owned()))?;

        if self.embedded_family.is_none() {
            self.embedded_family = Some(family.clone());
        }
        Ok(family)
    }

    /// 指定 [`FontFamily::Embedded`] 解析到哪个字族。
    pub fn set_embedded_family(&mut self, family: impl Into<String>) {
        self.embedded_family = Some(family.into());
    }

    pub fn embedded_family(&self) -> Option<&str> {
        self.embedded_family.as_deref()
    }

    pub fn locale(&self) -> &str {
        &self.locale
    }

    /// 库中 face 的数量。为 0 意味着任何文字都渲染不出来。
    pub fn face_count(&self) -> usize {
        self.db.len()
    }

    /// 为调用线程造一个 `FontSystem`。
    ///
    /// 克隆的是 `Database` 的 face 元信息；字体数据本身在 `Source` 里是
    /// `PathBuf` 或 `Arc<[u8]>`，不会被复制。
    pub fn font_system(&self) -> FontSystem {
        FontSystem::new_with_locale_and_db(self.locale.clone(), self.db.clone())
    }

    /// 按字族名取出字体数据。
    pub fn load_family(&self, family: &str) -> Option<FontBlob> {
        let id = self.db.query(&fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            ..fontdb::Query::default()
        })?;
        self.blob_for(id)
    }

    /// 找一份**确实能渲染**给定文本的字体。
    ///
    /// 不靠猜字族名，而是真跑一遍排版、看哪个 face 接下了这些字形 ——
    /// 字族名在各平台各发行版之间差异很大，实际排版才是唯一可靠的判据。
    pub fn load_font_for_text(&self, text: &str) -> Option<FontBlob> {
        // 空库上 shape 会 panic（见 crate::text::ensure_usable_fonts 的说明）。
        if self.db.is_empty() {
            return None;
        }
        let mut font_system = self.font_system();
        let mut buffer = Buffer::new(&mut font_system, Metrics::new(16.0, 20.0));
        buffer.set_text(text, &Attrs::new(), Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut font_system, false);

        let font_id = buffer
            .layout_runs()
            .flat_map(|run| run.glyphs.iter())
            // glyph_id 0 是 .notdef，落到它说明这个 face 并不真的支持该字符。
            .find(|g| g.glyph_id != 0)
            .map(|g| g.font_id)?;
        self.blob_for(font_id)
    }

    fn blob_for(&self, id: fontdb::ID) -> Option<FontBlob> {
        let face = self.db.face(id)?;
        let family = face.families.first()?.0.clone();
        let face_index = face.index;
        let data = self.db.with_face_data(id, |data, _| data.to_vec())?;
        Some(FontBlob {
            family,
            data,
            face_index,
        })
    }

    /// 把 spec 的字族选择翻译成 cosmic-text 的查询。
    pub fn resolve<'a>(&'a self, family: &'a FontFamily) -> Family<'a> {
        match family {
            FontFamily::SansSerif => Family::SansSerif,
            FontFamily::Serif => Family::Serif,
            FontFamily::Monospace => Family::Monospace,
            FontFamily::Name(name) => Family::Name(name),
            FontFamily::Embedded => match &self.embedded_family {
                Some(name) => Family::Name(name),
                None => {
                    log::warn!("请求了内嵌字体但未注入任何字体，降级到 sans-serif");
                    Family::SansSerif
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_font_that_actually_renders_chinese() {
        let lib = FontLibrary::with_system_fonts();
        let Some(blob) = lib.load_font_for_text("水印中文") else {
            eprintln!("跳过：本机无中文字体");
            return;
        };
        assert!(!blob.data.is_empty());
        // 本机中文字体多为 .ttc 集合，face_index 必须一并带出，否则会取到日韩字形。
        assert!(!blob.family.is_empty(), "字族名不应为空");
    }

    #[test]
    fn load_font_for_text_on_empty_library_returns_none() {
        // 不能 panic —— 空库正是移动端未注入字体时的处境。
        let lib = FontLibrary::without_system_fonts("zh-CN");
        assert!(lib.load_font_for_text("中").is_none());
    }

    #[test]
    fn empty_library_has_no_faces() {
        let lib = FontLibrary::without_system_fonts("zh-CN");
        assert_eq!(lib.face_count(), 0);
        assert_eq!(lib.locale(), "zh-CN");
        assert!(lib.embedded_family().is_none());
    }

    #[test]
    fn embedded_without_injection_falls_back() {
        let lib = FontLibrary::without_system_fonts("zh-CN");
        // 不 panic，降级到 sans-serif —— 渲染结果会是豆腐块，但不该让整个任务崩掉。
        assert!(matches!(
            lib.resolve(&FontFamily::Embedded),
            Family::SansSerif
        ));
    }

    #[test]
    fn rejects_garbage_font_data() {
        let mut lib = FontLibrary::without_system_fonts("en-US");
        assert!(lib.add_font(vec![0u8; 64]).is_err());
        assert_eq!(lib.face_count(), 0);
    }

    #[test]
    fn injected_font_becomes_embedded_target() {
        let mut lib = FontLibrary::without_system_fonts("zh-CN");
        let Some(path) = first_system_font() else {
            eprintln!("跳过：本机没有可用于测试的 TTF");
            return;
        };
        let data = std::fs::read(path).expect("读取字体");
        let family = lib.add_font(data).expect("注入字体");

        assert_eq!(lib.embedded_family(), Some(family.as_str()));
        assert!(lib.face_count() > 0);
        assert!(matches!(lib.resolve(&FontFamily::Embedded), Family::Name(n) if n == family));
    }

    /// 找一个本机的 TTF 用于测试；找不到就跳过，不让测试依赖特定发行版。
    fn first_system_font() -> Option<std::path::PathBuf> {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db.faces().find_map(|f| match &f.source {
            Source::File(p) if p.extension().is_some_and(|e| e == "ttf") => Some(p.clone()),
            _ => None,
        })
    }
}
