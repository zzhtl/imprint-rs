//! 给 egui 装上中文字形。
//!
//! egui 内置字体不含 CJK，不装的话界面文字和中文水印预览都是豆腐块。
//! 复用 `imprint-core` 已经扫好的字体库，避免为 UI 再扫一遍系统字体。

use std::sync::Arc;

use imprint_core::FontLibrary;

/// 常见中文字族，按桌面平台的优先级排列。
const PREFERRED: &[&str] = &[
    "Noto Sans CJK SC",
    "Source Han Sans SC",
    "Microsoft YaHei",
    "PingFang SC",
    "WenQuanYi Micro Hei",
    "Noto Sans SC",
    "Heiti SC",
    "SimHei",
];

/// 把一份能渲染中文的字体注册为 egui 的首选字族。
///
/// 找不到时只记一条日志：界面会退化成豆腐块，但不该让程序起不来。
pub fn install_cjk(ctx: &egui::Context, fonts: &FontLibrary) {
    let blob = PREFERRED
        .iter()
        .find_map(|name| fonts.load_family(name))
        // 字族名在各发行版之间差异很大，兜底靠"真跑一遍排版看谁接得住"。
        .or_else(|| fonts.load_font_for_text("水印中文"));

    let Some(blob) = blob else {
        log::warn!("未找到中文字体，界面中文将显示为豆腐块");
        return;
    };
    log::info!("UI 中文字体: {} (face #{})", blob.family, blob.face_index);

    let mut font_data = egui::FontData::from_owned(blob.data);
    // 中文字体常以 .ttc 分发，一个文件里塞着 SC/TC/JP/KR；丢掉序号会取到日韩字形。
    font_data.index = blob.face_index;

    let mut defs = egui::FontDefinitions::default();
    let key = blob.family.clone();
    defs.font_data.insert(key.clone(), Arc::new(font_data));

    // 必须插到最前：egui 按顺序回退，排在内置拉丁字体之后就轮不到它渲染中文。
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        defs.families
            .entry(family)
            .or_default()
            .insert(0, key.clone());
    }
    ctx.set_fonts(defs);
}
