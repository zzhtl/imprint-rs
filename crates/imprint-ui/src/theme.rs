//! 桌面工作区的配色与常用控件。

use egui::{Color32, RichText, Stroke, TextStyle};

pub const BACKGROUND: Color32 = Color32::from_rgb(17, 21, 29);
pub const PANEL: Color32 = Color32::from_rgb(24, 29, 39);
pub const CARD: Color32 = Color32::from_rgb(31, 37, 49);
pub const LINE: Color32 = Color32::from_rgb(47, 56, 71);
pub const ACCENT: Color32 = Color32::from_rgb(116, 154, 255);
pub const TEXT: Color32 = Color32::from_rgb(227, 233, 244);
pub const MUTED: Color32 = Color32::from_rgb(145, 157, 179);

pub fn install(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = PANEL;
    style.visuals.window_fill = PANEL;
    style.visuals.faint_bg_color = CARD;
    style.visuals.extreme_bg_color = BACKGROUND;
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.selection.bg_fill = Color32::from_rgb(45, 67, 112);
    style.visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    style.visuals.widgets.inactive.bg_fill = CARD;
    style.visuals.widgets.inactive.weak_bg_fill = CARD;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, LINE);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(44, 55, 78);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(44, 55, 78);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(50, 71, 118);
    style.visuals.widgets.active.weak_bg_fill = Color32::from_rgb(50, 71, 118);
    for widget in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
    ] {
        widget.corner_radius = 6.into();
    }
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size.y = 30.0;
    style.spacing.slider_width = 110.0;
    style
        .text_styles
        .insert(TextStyle::Body, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Button, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Small, egui::FontId::proportional(12.0));
    style
        .text_styles
        .insert(TextStyle::Heading, egui::FontId::proportional(19.0));
    ctx.set_style_of(egui::Theme::Dark, style);
}

pub fn primary(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).color(BACKGROUND).strong())
        .fill(ACCENT)
        .min_size(egui::vec2(112.0, 34.0))
}

pub fn section(ui: &mut egui::Ui, number: &str, title: &str) {
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(number).size(11.0).color(ACCENT));
        ui.label(RichText::new(title).size(15.0).strong());
    });
    ui.add_space(2.0);
}

pub fn panel() -> egui::Frame {
    egui::Frame::new().fill(PANEL).inner_margin(16)
}
