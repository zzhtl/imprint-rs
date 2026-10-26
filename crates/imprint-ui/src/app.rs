//! 应用状态与界面布局。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use egui::{Color32, TextureHandle};
use image::RgbaImage;
use imprint_core::{
    BatchItem, FieldContext, FontLibrary, ImageOptions, ItemOutcome, OutputFormat, Renderer, batch,
    image_job,
    spec::{
        Anchor9, Content, FontFamily, Margin, Placement, Rgba8, ShadowSpec, SizeMode, StrokeSpec,
        TextAlign, TextSpec, WatermarkSpec,
    },
};

use crate::{
    fonts,
    job::{Background, BatchTask},
    preview::PreviewState,
    theme,
};

/// 预览底图的长边上限。
///
/// wgpu 后端下 `TextureOptions` 的 mipmap 不生效（egui 文档写明只有 egui_glow 支持），
/// 所以必须在 CPU 侧预降采样，否则缩小显示的 4K 图会明显走样。
const PREVIEW_MAX_SIDE: u32 = 2048;
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "tif", "tiff", "bmp", "gif"];

/// 当前打开的素材。
pub struct Document {
    pub path: PathBuf,
    pub file_name: String,
    /// 全尺寸原图，导出时用。
    pub decoded: Arc<image_job::DecodedImage>,
    pub fields: FieldContext,
    /// 预览底图纹理，打开文件时上传一次，之后不动。
    pub base_texture: TextureHandle,
}

impl Document {
    pub fn size(&self) -> (u32, u32) {
        self.decoded.image.dimensions()
    }
}

pub struct ImprintApp {
    fonts: Arc<FontLibrary>,
    renderer: Renderer,
    /// 批量队列的输入文件。
    queue: Vec<PathBuf>,
    task: Option<BatchTask>,
    last_results: Vec<ItemOutcome>,
    spec: WatermarkSpec,
    doc: Option<Document>,
    preview: PreviewState,
    output: ImageOptions,
    status: String,
    loading: Option<Background<LoadedDocument>>,
    exporting: Option<Background<PathBuf>>,
    smoke: Option<Smoke>,
}

struct LoadedDocument {
    path: PathBuf,
    decoded: image_job::DecodedImage,
    preview: egui::ColorImage,
}

/// 冒烟模式：启动后自动载入素材、等界面稳定、截图、退出。
///
/// 由 `IMPRINT_SMOKE_SHOT`（截图输出路径）触发，可选 `IMPRINT_SMOKE_OPEN`
/// 指定要载入的素材。无头环境（CI、Xvfb）下验证 UI 真能起来靠它。
struct Smoke {
    shot_path: PathBuf,
    warmup_frames: u32,
    requested: bool,
}

impl Smoke {
    fn from_env() -> Option<Self> {
        let shot_path = std::env::var_os("IMPRINT_SMOKE_SHOT")?;
        Some(Self {
            shot_path: PathBuf::from(shot_path),
            // 纹理上传和字体光栅化都要几帧才稳定，太早截会拍到空窗口。
            warmup_frames: 12,
            requested: false,
        })
    }
}

impl ImprintApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let fonts = Arc::new(FontLibrary::with_system_fonts());
        fonts::install_cjk(&cc.egui_ctx, &fonts);
        theme::install(&cc.egui_ctx);

        let spec = WatermarkSpec {
            content: Content::Text(TextSpec {
                template: "imprint 水印 · {date}".to_owned(),
                family: FontFamily::SansSerif,
                color: Rgba8::WHITE,
                align: TextAlign::Left,
                ..TextSpec::default()
            }),
            size: SizeMode::RelativeFontSize(0.05),
            opacity: 0.85,
            ..WatermarkSpec::default()
        };

        let mut app = Self {
            renderer: Renderer::new(Arc::clone(&fonts)),
            fonts,
            queue: Vec::new(),
            task: None,
            last_results: Vec::new(),
            spec,
            doc: None,
            preview: PreviewState::default(),
            output: ImageOptions {
                format: OutputFormat::Jpeg { quality: 92 },
                ..ImageOptions::default()
            },
            status: "打开一张图片开始".to_owned(),
            loading: None,
            exporting: None,
            smoke: Smoke::from_env(),
        };

        if let Some(path) = std::env::var_os("IMPRINT_SMOKE_SPEC") {
            app.load_preset(PathBuf::from(path));
        }
        if let Some(path) = std::env::var_os("IMPRINT_SMOKE_OPEN") {
            app.open_path(&cc.egui_ctx, PathBuf::from(path));
        }
        app
    }

    fn open_path(&mut self, egui_ctx: &egui::Context, path: PathBuf) {
        if self.loading.is_some() {
            return;
        }
        self.status = format!(
            "正在打开 {}…",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        let max_alloc = self.output.max_alloc_bytes;
        self.loading = Some(Background::spawn(egui_ctx.clone(), move || {
            let decoded = image_job::decode(std::fs::File::open(&path)?, max_alloc)?;
            let preview = downscale(&decoded.image, PREVIEW_MAX_SIDE);
            let color = egui::ColorImage::from_rgba_unmultiplied(
                [preview.width() as usize, preview.height() as usize],
                preview.as_raw(),
            );
            Ok(LoadedDocument {
                path,
                decoded,
                preview: color,
            })
        }));
    }

    fn poll_background(&mut self, ctx: &egui::Context) {
        if let Some(result) = self.loading.as_ref().and_then(Background::poll) {
            self.loading = None;
            match result {
                Ok(loaded) => {
                    let file_name = loaded
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    let (w, h) = loaded.decoded.image.dimensions();
                    let mut fields = FieldContext::new()
                        .with_file_name(&file_name)
                        .with_dimensions(w, h);
                    if let Some(exif) = &loaded.decoded.metadata.exif {
                        fields = fields.with_exif(imprint_core::ExifFields::parse(exif));
                    }
                    self.status = format!("已打开 {file_name} · {w} × {h}");
                    self.doc = Some(Document {
                        path: loaded.path,
                        file_name,
                        fields,
                        decoded: Arc::new(loaded.decoded),
                        base_texture: ctx.load_texture(
                            "imprint_base",
                            loaded.preview,
                            egui::TextureOptions::LINEAR,
                        ),
                    });
                    self.preview.invalidate();
                    self.preview.show_original = false;
                }
                Err(e) => self.status = format!("打开失败：{e:#}"),
            }
        }
        if let Some(result) = self.exporting.as_ref().and_then(Background::poll) {
            self.exporting = None;
            self.status = match result {
                Ok(path) => format!("已导出 {}", path.display()),
                Err(e) => format!("导出失败：{e:#}"),
            };
        }
    }

    fn save_preset(&mut self, dst: PathBuf) {
        match serde_json::to_vec_pretty(&self.spec)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(std::fs::write(&dst, bytes)?))
        {
            Ok(()) => self.status = format!("预设已保存 {}", dst.display()),
            Err(e) => self.status = format!("预设保存失败：{e}"),
        }
    }

    fn load_preset(&mut self, src: PathBuf) {
        match load_spec(&src) {
            Ok(spec) => {
                self.spec = spec;
                self.preview.invalidate();
                self.status = format!("已加载预设 {}", src.display());
            }
            Err(e) => self.status = format!("预设加载失败：{e}"),
        }
    }

    /// 输出格式对应的扩展名。
    fn output_extension(&self) -> &'static str {
        match self.output.format {
            OutputFormat::Jpeg { .. } => "jpg",
            OutputFormat::Png => "png",
            OutputFormat::WebP => "webp",
        }
    }

    fn start_batch(&mut self, egui_ctx: &egui::Context, out_dir: PathBuf) {
        if self.queue.is_empty() {
            self.status = "队列为空".to_owned();
            return;
        }
        let ext = self.output_extension();
        let items: Vec<BatchItem> = self
            .queue
            .iter()
            .map(|src| BatchItem {
                dst: batch::output_path(src, &out_dir, ext, "_wm"),
                src: src.clone(),
            })
            .collect();

        if let Err(e) = batch::validate_output_paths(&items) {
            self.status = e.to_string();
            return;
        }
        self.last_results.clear();
        self.status = format!("批量处理 {} 个文件…", items.len());
        self.task = Some(BatchTask::spawn(
            Arc::clone(&self.fonts),
            items,
            self.spec.clone(),
            self.output.clone(),
            egui_ctx.clone(),
        ));
    }

    fn choose_image(&mut self, ctx: &egui::Context) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("图片", IMAGE_EXTENSIONS)
            .pick_file()
        {
            self.open_path(ctx, path);
        }
    }

    fn choose_export(&mut self, ctx: &egui::Context) {
        let Some(doc) = &self.doc else {
            return;
        };
        let stem = doc.path.file_stem().unwrap_or_default().to_string_lossy();
        let name = format!("{stem}_watermarked.{}", self.output_extension());
        if let Some(path) = rfd::FileDialog::new().set_file_name(name).save_file() {
            self.export(ctx, path);
        }
    }

    fn import_paths(&mut self, ctx: &egui::Context, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        if self.loading.is_some() || self.task.is_some() {
            self.status = "请等待当前加载或批量任务结束后再拖入文件".to_owned();
            return;
        }
        if paths.len() == 1 && is_image_path(&paths[0]) {
            self.open_path(ctx, paths[0].clone());
            return;
        }
        let before = self.queue.len();
        for path in paths {
            if path.is_dir() {
                self.add_dir(&path);
            } else if is_image_path(&path) {
                self.queue.push(path);
            }
        }
        self.queue.sort();
        self.queue.dedup();
        self.status = format!(
            "已添加 {} 张图片到队列；支持 JPEG / PNG / WebP / TIFF / BMP / GIF",
            self.queue.len() - before
        );
        if self.doc.is_none()
            && let Some(path) = self.queue.first().cloned()
        {
            self.open_path(ctx, path);
        }
    }

    fn export(&mut self, ctx: &egui::Context, dst: PathBuf) {
        let Some(doc) = &self.doc else {
            return;
        };
        let decoded = Arc::clone(&doc.decoded);
        let fonts = Arc::clone(&self.fonts);
        let spec = self.spec.clone();
        let options = self.output.clone();
        let fields = doc.fields.clone();
        self.status = "正在导出图片…".to_owned();
        self.exporting = Some(Background::spawn(ctx.clone(), move || {
            let mut renderer = Renderer::new(fonts);
            image_job::write_atomic(&dst, |writer| {
                renderer.watermark_decoded(&decoded, writer, &spec, &options, &fields)
            })?;
            Ok(dst)
        }));
    }
}

impl eframe::App for ImprintApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let egui_ctx = ui.ctx().clone();
        self.poll_background(&egui_ctx);
        let paths = egui_ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        self.import_paths(&egui_ctx, paths);
        if egui_ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::O))
            && self.loading.is_none()
        {
            self.choose_image(&egui_ctx);
        }
        if egui_ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S))
            && self.doc.is_some()
            && self.loading.is_none()
            && self.exporting.is_none()
        {
            self.choose_export(&egui_ctx);
        }

        egui::Panel::top("toolbar")
            .frame(theme::panel())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (logo, _) =
                        ui.allocate_exact_size(egui::vec2(36.0, 36.0), egui::Sense::hover());
                    ui.painter().rect_filled(logo, 10.0, theme::ACCENT);
                    ui.painter().text(
                        logo.center(),
                        egui::Align2::CENTER_CENTER,
                        "i",
                        egui::FontId::proportional(28.0),
                        theme::BACKGROUND,
                    );
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new("imprint").size(22.0).strong());
                        ui.label(
                            egui::RichText::new("水印工作室")
                                .size(11.0)
                                .color(theme::MUTED),
                        );
                    });
                    ui.add_space(28.0);
                    if ui
                        .add_enabled(self.loading.is_none(), egui::Button::new("打开图片"))
                        .on_hover_text("打开图片（Ctrl / ⌘ + O）")
                        .clicked()
                    {
                        self.choose_image(&egui_ctx);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let ready = self.doc.is_some()
                            && self.exporting.is_none()
                            && self.loading.is_none();
                        if ui
                            .add_enabled(ready, theme::primary("导出图片"))
                            .on_hover_text("导出图片（Ctrl / ⌘ + S）")
                            .clicked()
                        {
                            self.choose_export(&egui_ctx);
                        }
                        ui.menu_button("预设", |ui| {
                            if ui.button("加载预设…").clicked() {
                                ui.close();
                                if let Some(path) = rfd::FileDialog::new()
                                    .add_filter("预设", &["json"])
                                    .pick_file()
                                {
                                    self.load_preset(path);
                                }
                            }
                            if ui.button("保存当前预设…").clicked() {
                                ui.close();
                                if let Some(path) = rfd::FileDialog::new()
                                    .add_filter("预设", &["json"])
                                    .set_file_name("imprint_preset.json")
                                    .save_file()
                                {
                                    self.save_preset(path);
                                }
                            }
                        });
                    });
                });
            });

        egui::Panel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.loading.is_some() || self.exporting.is_some() {
                        ui.spinner();
                    } else {
                        ui.colored_label(theme::ACCENT, "●");
                    }
                    ui.add_sized(
                        [ui.available_width() - 210.0, 18.0],
                        egui::Label::new(
                            egui::RichText::new(&self.status)
                                .size(12.0)
                                .color(theme::MUTED),
                        )
                        .truncate(),
                    )
                    .on_hover_text(&self.status);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new("本地处理 · 原始分辨率导出")
                                .size(11.0)
                                .color(theme::MUTED),
                        );
                    });
                });
            });
        if self.task.is_some() {
            egui::Panel::bottom("progress")
                .frame(theme::panel())
                .show(ui, |ui| self.progress_panel(ui));
        }
        egui::Panel::left("queue")
            .default_size(236.0)
            .resizable(false)
            .frame(theme::panel())
            .show(ui, |ui| {
                if let Some(doc) = &self.doc {
                    theme::section(ui, "", "当前素材");
                    egui::Frame::new()
                        .fill(theme::CARD)
                        .corner_radius(8)
                        .inner_margin(10)
                        .show(ui, |ui| {
                            let scale =
                                (188.0 / doc.size().0 as f32).min(112.0 / doc.size().1 as f32);
                            ui.image((
                                doc.base_texture.id(),
                                egui::vec2(
                                    doc.size().0 as f32 * scale,
                                    doc.size().1 as f32 * scale,
                                ),
                            ));
                            ui.add(
                                egui::Label::new(egui::RichText::new(&doc.file_name).strong())
                                    .truncate(),
                            )
                            .on_hover_text(doc.path.display().to_string());
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} × {} px",
                                    doc.size().0,
                                    doc.size().1
                                ))
                                .size(12.0)
                                .color(theme::MUTED),
                            );
                        });
                    ui.add_space(18.0);
                }
                self.queue_panel(ui, &egui_ctx);
            });
        egui::Panel::right("params")
            .default_size(300.0)
            .resizable(false)
            .frame(theme::panel())
            .show(ui, |ui| {
                ui.label(egui::RichText::new("水印设置").size(18.0).strong());
                ui.label(
                    egui::RichText::new("调整参数，实时查看效果")
                        .size(12.0)
                        .color(theme::MUTED),
                );
                ui.add_space(8.0);
                egui::ScrollArea::vertical().show(ui, |ui| self.spec_panel(ui));
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BACKGROUND).inner_margin(20))
            .show(ui, |ui| {
                let Some(doc) = &self.doc else {
                    ui.vertical_centered(|ui| {
                        ui.add_space((ui.available_height() * 0.28).max(20.0));
                        let (icon, _) =
                            ui.allocate_exact_size(egui::vec2(84.0, 84.0), egui::Sense::hover());
                        ui.painter().rect_filled(icon, 22.0, theme::CARD);
                        ui.painter().rect_stroke(
                            icon.shrink(23.0),
                            4.0,
                            egui::Stroke::new(2.0, theme::ACCENT),
                            egui::StrokeKind::Inside,
                        );
                        ui.painter().line_segment(
                            [
                                icon.center() + egui::vec2(-12.0, 9.0),
                                icon.center() + egui::vec2(0.0, -5.0),
                            ],
                            egui::Stroke::new(2.0, theme::ACCENT),
                        );
                        ui.painter().line_segment(
                            [
                                icon.center() + egui::vec2(0.0, -5.0),
                                icon.center() + egui::vec2(13.0, 9.0),
                            ],
                            egui::Stroke::new(2.0, theme::ACCENT),
                        );
                        ui.add_space(18.0);
                        ui.label(egui::RichText::new("从一张图片开始").size(24.0).strong());
                        ui.label(
                            egui::RichText::new("拖入图片或目录，添加水印后导出作品")
                                .color(theme::MUTED),
                        );
                        ui.add_space(14.0);
                        if ui
                            .add_enabled(self.loading.is_none(), theme::primary("选择图片"))
                            .clicked()
                        {
                            self.choose_image(&egui_ctx);
                        }
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("JPEG / PNG / WebP / TIFF / BMP / GIF")
                                .size(11.0)
                                .color(theme::MUTED),
                        );
                    });
                    return;
                };

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("效果预览").size(16.0).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.selectable_value(&mut self.preview.show_original, false, "水印效果");
                        ui.selectable_value(&mut self.preview.show_original, true, "原图");
                    });
                });
                ui.add_space(8.0);
                let ctx = field_context(doc);

                let out = crate::preview::show(
                    ui,
                    &mut self.preview,
                    &mut self.renderer,
                    &self.spec,
                    &doc.base_texture,
                    doc.size(),
                    &ctx,
                );

                if let Some((x, y)) = out.dragged_to {
                    let anchor = match self.spec.placement {
                        Placement::Normalized { anchor, .. } => anchor,
                        _ => Anchor9::Center,
                    };
                    // 一旦拖动就切到归一化定位，这样位置在任何分辨率下都成立。
                    self.spec.placement = Placement::Normalized { x, y, anchor };
                }
            });

        self.drive_smoke(&egui_ctx);
    }
}

impl ImprintApp {
    fn drive_smoke(&mut self, ctx: &egui::Context) {
        let Some(smoke) = &mut self.smoke else {
            return;
        };
        if self.loading.is_some() {
            ctx.request_repaint();
            return;
        }
        if smoke.warmup_frames > 0 {
            smoke.warmup_frames -= 1;
            ctx.request_repaint();
            return;
        }
        if !smoke.requested {
            smoke.requested = true;
            // 0.36 的截图是"发命令 → 下一帧在 Event::Screenshot 里收结果"，不是回调。
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            ctx.request_repaint();
            return;
        }

        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });

        let Some(image) = shot else {
            // 截图还没回来，继续等。
            ctx.request_repaint();
            return;
        };

        let size = image.size;
        match image::save_buffer(
            &smoke.shot_path,
            image.as_raw(),
            size[0] as u32,
            size[1] as u32,
            image::ColorType::Rgba8,
        ) {
            Ok(()) => log::info!("冒烟截图已写入 {}", smoke.shot_path.display()),
            Err(e) => log::error!("冒烟截图写入失败: {e}"),
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl ImprintApp {
    fn queue_panel(&mut self, ui: &mut egui::Ui, egui_ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("批量处理").size(16.0).strong());
            ui.label(
                egui::RichText::new(format!("{}", self.queue.len()))
                    .size(12.0)
                    .color(theme::ACCENT),
            );
        });
        ui.label(
            egui::RichText::new("同一套水印，应用到多张图片")
                .size(11.0)
                .color(theme::MUTED),
        );
        ui.add_space(6.0);

        let busy = self.task.as_ref().is_some_and(|t| !t.is_finished());
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                if ui.button("添加文件").clicked()
                    && let Some(paths) = rfd::FileDialog::new()
                        .add_filter("图片", IMAGE_EXTENSIONS)
                        .pick_files()
                {
                    self.queue.extend(paths);
                    self.queue.sort();
                    self.queue.dedup();
                }
                if ui.button("添加目录").clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    self.add_dir(&dir);
                }
            });
            ui.horizontal(|ui| {
                if ui.button("清空").clicked() {
                    self.queue.clear();
                    self.last_results.clear();
                }
                if ui
                    .add_enabled(!self.queue.is_empty(), egui::Button::new("批量导出"))
                    .clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    self.start_batch(egui_ctx, dir);
                }
            });
        });

        ui.separator();
        if self.queue.is_empty() {
            egui::Frame::new()
                .fill(theme::CARD)
                .corner_radius(8)
                .inner_margin(16)
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("队列还是空的").color(theme::MUTED));
                    ui.label(
                        egui::RichText::new(
                            "添加文件或整个目录，
一次完成批量导出。",
                        )
                        .size(12.0)
                        .color(theme::MUTED),
                    );
                });
        }

        egui::ScrollArea::vertical()
            .max_height(ui.available_height() * 0.6)
            .show(ui, |ui| {
                let mut remove = None;
                let mut open = None;
                for (i, path) in self.queue.iter().enumerate() {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("?")
                        .to_owned();
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!busy, egui::Button::new("×").small())
                            .clicked()
                        {
                            remove = Some(i);
                        }
                        // 长文件名会把面板撑开，截断显示、完整内容放 tooltip。
                        let selected = self.doc.as_ref().is_some_and(|doc| &doc.path == path);
                        if ui
                            .selectable_label(selected, elide(&name, 18))
                            .on_hover_text(path.display().to_string())
                            .clicked()
                        {
                            open = Some(path.clone());
                        }
                    });
                }
                if let Some(i) = remove {
                    self.queue.remove(i);
                }
                if let Some(path) = open {
                    self.open_path(egui_ctx, path);
                }
            });

        if !self.last_results.is_empty() {
            ui.separator();
            let failed: Vec<&ItemOutcome> = self
                .last_results
                .iter()
                .filter(|o| {
                    o.result
                        .as_ref()
                        .is_err_and(|e| !matches!(e, imprint_core::Error::Cancelled))
                })
                .collect();
            if self.last_results.iter().all(ItemOutcome::is_ok) {
                ui.colored_label(Color32::from_rgb(120, 200, 120), "全部成功");
            } else if failed.is_empty() {
                ui.colored_label(theme::MUTED, "已取消，已导出的文件保留");
            } else {
                ui.colored_label(
                    Color32::from_rgb(230, 150, 120),
                    format!("{} 个失败", failed.len()),
                );
                egui::ScrollArea::vertical()
                    .id_salt("failed")
                    .show(ui, |ui| {
                        for item in failed {
                            if let Err(e) = &item.result {
                                let name =
                                    item.src.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                                ui.small(format!("{name}: {e}"))
                                    .on_hover_text(item.src.display().to_string());
                            }
                        }
                    });
            }
        }
    }

    fn add_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            self.status = format!("无法读取目录 {}", dir.display());
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if is_image_path(&path) {
                self.queue.push(path);
            }
        }
        self.queue.sort();
        self.queue.dedup();
    }

    fn progress_panel(&mut self, ui: &mut egui::Ui) {
        let Some(task) = &mut self.task else {
            return;
        };
        let p = task.progress();
        let cancelled = task.is_cancelled();

        ui.horizontal(|ui| {
            ui.add(
                egui::ProgressBar::new(p.ratio())
                    .desired_width(240.0)
                    .show_percentage(),
            );
            ui.label(format!("{}/{}", p.completed, p.total));
            if p.failed > 0 {
                ui.colored_label(
                    Color32::from_rgb(230, 150, 120),
                    format!("失败 {}", p.failed),
                );
            }
            if cancelled {
                ui.label("正在取消…");
            } else if ui.button("取消").clicked() {
                task.cancel();
            }
        });

        if let Some(result) = task.poll() {
            let results = match result {
                Ok(results) => results,
                Err(e) => {
                    self.status = e;
                    self.task = None;
                    return;
                }
            };
            self.status = batch_summary(&results);
            self.last_results = results;
            self.task = None;
        }
    }

    fn spec_panel(&mut self, ui: &mut egui::Ui) {
        theme::section(ui, "01", "内容");

        if let Content::Text(text) = &mut self.spec.content {
            ui.add(
                egui::TextEdit::multiline(&mut text.template)
                    .desired_width(f32::INFINITY)
                    .desired_rows(3)
                    .hint_text("支持 {filename} {date} {width}x{height} {exif.model}"),
            );

            ui.horizontal(|ui| {
                ui.label("颜色");
                let mut c = Color32::from_rgba_unmultiplied(
                    text.color.r,
                    text.color.g,
                    text.color.b,
                    text.color.a,
                );
                if ui.color_edit_button_srgba(&mut c).changed() {
                    let [r, g, b, a] = c.to_srgba_unmultiplied();
                    text.color = Rgba8::new(r, g, b, a);
                }

                ui.label("字重");
                ui.add(
                    egui::DragValue::new(&mut text.weight)
                        .range(100..=900)
                        .speed(50),
                );
            });

            ui.horizontal(|ui| {
                ui.checkbox(&mut text.italic, "斜体");
                ui.label("行高");
                ui.add(
                    egui::DragValue::new(&mut text.line_height)
                        .range(0.6..=3.0)
                        .speed(0.05),
                );
            });
        }

        ui.separator();
        theme::section(ui, "02", "尺寸");
        size_controls(ui, &mut self.spec.size);

        ui.separator();
        theme::section(ui, "03", "位置");
        placement_controls(ui, &mut self.spec.placement);

        ui.separator();
        theme::section(ui, "04", "样式");
        ui.horizontal(|ui| {
            ui.label("不透明度");
            ui.add(egui::Slider::new(&mut self.spec.opacity, 0.0..=1.0).fixed_decimals(2));
        });
        ui.horizontal(|ui| {
            ui.label("旋转");
            ui.add(egui::Slider::new(&mut self.spec.rotation_deg, -180.0..=180.0).suffix("°"));
        });
        if let Content::Text(text) = &mut self.spec.content {
            ui.horizontal(|ui| {
                let mut stroke = text.stroke.is_some();
                if ui.checkbox(&mut stroke, "描边").changed() {
                    text.stroke = stroke.then_some(StrokeSpec {
                        width_ratio: 0.06,
                        color: Rgba8::new(12, 16, 24, 220),
                    });
                }
                let mut shadow = text.shadow.is_some();
                if ui.checkbox(&mut shadow, "阴影").changed() {
                    text.shadow = shadow.then_some(ShadowSpec {
                        offset_ratio: (0.08, 0.08),
                        color: Rgba8::new(0, 0, 0, 160),
                    });
                }
            });
            if let Some(stroke) = &mut text.stroke {
                ui.add(egui::Slider::new(&mut stroke.width_ratio, 0.01..=0.25).text("描边粗细"));
            }
        }

        ui.separator();
        theme::section(ui, "05", "输出");
        output_controls(ui, &mut self.output);
    }
}

fn is_image_path(path: &Path) -> bool {
    path.is_file()
        && path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|ext| e.eq_ignore_ascii_case(ext))
        })
}

fn batch_summary(results: &[ItemOutcome]) -> String {
    let success = results.iter().filter(|o| o.is_ok()).count();
    let cancelled = results
        .iter()
        .filter(|o| matches!(o.result, Err(imprint_core::Error::Cancelled)))
        .count();
    let failed = results.len() - success - cancelled;
    if cancelled > 0 {
        format!("已取消：{success} 成功 / {failed} 失败 / {cancelled} 未处理")
    } else if failed == 0 {
        format!("批量完成：{success} 个全部成功")
    } else {
        format!("批量完成：{success} 成功 / {failed} 失败")
    }
}

fn size_controls(ui: &mut egui::Ui, size: &mut SizeMode) {
    let label = match size {
        SizeMode::RelativeFontSize(_) => "按画布高度比例（字号）",
        SizeMode::RelativeWidth(_) => "按画布宽度比例（整体宽）",
        SizeMode::AbsoluteFontSize { .. } => "绝对字号（px）",
        SizeMode::AbsoluteWidth { .. } => "绝对宽度（px）",
    };
    egui::ComboBox::from_id_salt("size_mode")
        .selected_text(label)
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(
                    matches!(size, SizeMode::RelativeFontSize(_)),
                    "按画布高度比例（字号）",
                )
                .clicked()
            {
                *size = SizeMode::RelativeFontSize(0.05);
            }
            if ui
                .selectable_label(
                    matches!(size, SizeMode::RelativeWidth(_)),
                    "按画布宽度比例（整体宽）",
                )
                .clicked()
            {
                *size = SizeMode::RelativeWidth(0.3);
            }
            if ui
                .selectable_label(
                    matches!(size, SizeMode::AbsoluteFontSize { .. }),
                    "绝对字号（px）",
                )
                .clicked()
            {
                *size = SizeMode::AbsoluteFontSize { px: 48.0 };
            }
            if ui
                .selectable_label(
                    matches!(size, SizeMode::AbsoluteWidth { .. }),
                    "绝对宽度（px）",
                )
                .clicked()
            {
                *size = SizeMode::AbsoluteWidth { px: 400 };
            }
        });

    match size {
        SizeMode::RelativeFontSize(r) | SizeMode::RelativeWidth(r) => {
            ui.add(
                egui::Slider::new(r, 0.005..=1.0)
                    .fixed_decimals(3)
                    .text("比例"),
            );
        }
        SizeMode::AbsoluteFontSize { px } => {
            ui.add(egui::Slider::new(px, 8.0..=512.0).text("字号"));
        }
        SizeMode::AbsoluteWidth { px } => {
            ui.add(egui::Slider::new(px, 16..=4096).text("宽度"));
        }
    }
}

fn placement_controls(ui: &mut egui::Ui, placement: &mut Placement) {
    ui.horizontal(|ui| {
        if ui
            .selectable_label(matches!(placement, Placement::Anchor { .. }), "九宫格")
            .clicked()
        {
            *placement = Placement::Anchor {
                anchor: Anchor9::BottomRight,
                margin: Margin::default(),
            };
        }
        if ui
            .selectable_label(
                matches!(placement, Placement::Normalized { .. }),
                "自由拖拽",
            )
            .clicked()
        {
            *placement = Placement::Normalized {
                x: 0.5,
                y: 0.5,
                anchor: Anchor9::Center,
            };
        }
        if ui
            .selectable_label(matches!(placement, Placement::Tile { .. }), "平铺")
            .clicked()
        {
            *placement = Placement::Tile {
                spacing_ratio: (0.6, 1.2),
                stagger: true,
            };
        }
    });

    match placement {
        Placement::Anchor { anchor, margin } => {
            anchor_grid(ui, anchor);
            ui.add(
                egui::Slider::new(&mut margin.x_ratio, 0.0..=0.4)
                    .fixed_decimals(3)
                    .text("水平边距"),
            );
            ui.add(
                egui::Slider::new(&mut margin.y_ratio, 0.0..=0.4)
                    .fixed_decimals(3)
                    .text("垂直边距"),
            );
        }
        Placement::Normalized { x, y, .. } => {
            ui.label("直接在预览图上拖动水印");
            ui.add(egui::Slider::new(x, 0.0..=1.0).fixed_decimals(3).text("X"));
            ui.add(egui::Slider::new(y, 0.0..=1.0).fixed_decimals(3).text("Y"));
        }
        Placement::Tile {
            spacing_ratio,
            stagger,
        } => {
            ui.add(
                egui::Slider::new(&mut spacing_ratio.0, 0.05..=4.0)
                    .fixed_decimals(2)
                    .text("水平间距"),
            );
            ui.add(
                egui::Slider::new(&mut spacing_ratio.1, 0.05..=4.0)
                    .fixed_decimals(2)
                    .text("垂直间距"),
            );
            ui.checkbox(stagger, "奇数行错开");
        }
    }
}

fn anchor_grid(ui: &mut egui::Ui, anchor: &mut Anchor9) {
    const GRID: [[(Anchor9, &str); 3]; 3] = [
        [
            (Anchor9::TopLeft, "↖"),
            (Anchor9::TopCenter, "↑"),
            (Anchor9::TopRight, "↗"),
        ],
        [
            (Anchor9::CenterLeft, "←"),
            (Anchor9::Center, "•"),
            (Anchor9::CenterRight, "→"),
        ],
        [
            (Anchor9::BottomLeft, "↙"),
            (Anchor9::BottomCenter, "↓"),
            (Anchor9::BottomRight, "↘"),
        ],
    ];
    for row in GRID {
        ui.horizontal(|ui| {
            for (value, glyph) in row {
                if ui
                    .selectable_label(*anchor == value, egui::RichText::new(glyph).size(16.0))
                    .clicked()
                {
                    *anchor = value;
                }
            }
        });
    }
}

fn output_controls(ui: &mut egui::Ui, options: &mut ImageOptions) {
    ui.horizontal(|ui| {
        if ui
            .selectable_label(matches!(options.format, OutputFormat::Jpeg { .. }), "JPEG")
            .clicked()
        {
            options.format = OutputFormat::Jpeg { quality: 92 };
        }
        if ui
            .selectable_label(matches!(options.format, OutputFormat::Png), "PNG")
            .clicked()
        {
            options.format = OutputFormat::Png;
        }
        if ui
            .selectable_label(matches!(options.format, OutputFormat::WebP), "WebP")
            .clicked()
        {
            options.format = OutputFormat::WebP;
        }
    });
    if let OutputFormat::Jpeg { quality } = &mut options.format {
        ui.add(egui::Slider::new(quality, 1..=100).text("质量"));
    }
    ui.checkbox(&mut options.keep_metadata, "保留 EXIF / ICC");
}

/// 从文件读取一份水印预设。
fn load_spec(path: &PathBuf) -> anyhow::Result<WatermarkSpec> {
    let bytes = std::fs::read(path)?;
    let mut spec: WatermarkSpec = serde_json::from_slice(&bytes)?;
    // 外部文件的内容不可信，先收敛到合法区间再用。
    spec.sanitize();
    Ok(spec)
}

/// 过长的文件名截断中间，保留首尾。
fn elide(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_owned();
    }
    let head: String = s.chars().take(max_chars / 2).collect();
    let tail: String = s.chars().skip(count - max_chars / 2 + 1).collect();
    format!("{head}…{tail}")
}

/// 从当前素材构造模板字段上下文。
///
/// 写成自由函数而非方法：借 `&self` 的方法会和 `&mut self.preview`
/// 的字段借用冲突，而字段级的分割借用是允许的。
fn field_context(doc: &Document) -> FieldContext {
    doc.fields.clone()
}

/// 把图片降采样到长边不超过 `max_side`。
fn downscale(src: &RgbaImage, max_side: u32) -> RgbaImage {
    let (w, h) = src.dimensions();
    let longest = w.max(h);
    if longest <= max_side || longest == 0 {
        return src.clone();
    }
    let scale = max_side as f32 / longest as f32;
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    image::imageops::resize(src, nw, nh, image::imageops::FilterType::Triangle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_cancellation_is_not_reported_as_failure() {
        let results = [
            Ok(()),
            Err(imprint_core::Error::Cancelled),
            Err(imprint_core::Error::EmptyContent),
        ]
        .into_iter()
        .map(|result| ItemOutcome {
            src: PathBuf::from("input.png"),
            dst: PathBuf::from("output.png"),
            result,
        })
        .collect::<Vec<_>>();
        assert_eq!(
            batch_summary(&results),
            "已取消：1 成功 / 1 失败 / 1 未处理"
        );
    }

    #[test]
    fn background_export_uses_loaded_pixels_even_after_source_is_removed() {
        let ctx = egui::Context::default();
        let fonts = Arc::new(FontLibrary::with_system_fonts());
        let mut app = ImprintApp {
            renderer: Renderer::new(Arc::clone(&fonts)),
            fonts,
            queue: Vec::new(),
            task: None,
            last_results: Vec::new(),
            spec: WatermarkSpec {
                content: Content::Text(TextSpec {
                    template: "Saved".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            doc: None,
            preview: PreviewState::default(),
            output: ImageOptions {
                format: OutputFormat::Png,
                ..Default::default()
            },
            status: String::new(),
            loading: None,
            exporting: None,
            smoke: None,
        };
        let dir =
            std::env::temp_dir().join(format!("imprint-ui-background-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("source.png");
        let dst = dir.join("output.png");
        RgbaImage::from_pixel(240, 160, image::Rgba([80, 100, 120, 255]))
            .save(&src)
            .unwrap();
        app.open_path(&ctx, src.clone());
        let wait = |app: &mut ImprintApp| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while app.loading.is_some() || app.exporting.is_some() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "background operation timed out"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
                app.poll_background(&ctx);
            }
        };
        wait(&mut app);
        assert_eq!(app.doc.as_ref().unwrap().size(), (240, 160));
        std::fs::remove_file(&src).unwrap();
        app.export(&ctx, dst.clone());
        wait(&mut app);
        let output = image::open(&dst).unwrap().to_rgba8();
        assert_eq!(output.dimensions(), (240, 160));
        assert!(output.pixels().any(|p| p.0 != [80, 100, 120, 255]));
        app.open_path(&ctx, src);
        wait(&mut app);
        assert!(app.status.starts_with("打开失败"));
        assert!(
            app.doc.is_some(),
            "load failure must preserve the current document"
        );
        let second = dir.join("second.TIF");
        let ignored = dir.join("notes.txt");
        std::fs::write(&second, b"queued by extension").unwrap();
        std::fs::write(&ignored, b"not an image").unwrap();
        app.import_paths(&ctx, vec![dir.clone(), dst.clone(), ignored]);
        assert_eq!(app.queue, vec![dst.clone(), second]);
        app.import_paths(&ctx, vec![dir.clone(), dst.clone()]);
        assert_eq!(
            app.queue.len(),
            2,
            "repeated drops must not duplicate queued files"
        );
        app.import_paths(&ctx, vec![dst]);
        wait(&mut app);
        assert_eq!(app.doc.as_ref().unwrap().file_name, "output.png");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn elide_keeps_short_names_intact() {
        assert_eq!(elide("short.png", 22), "short.png");
        assert_eq!(elide("", 10), "");
    }

    #[test]
    fn elide_shortens_long_names() {
        let long = "a_very_long_file_name_that_will_not_fit.png";
        let out = elide(long, 20);
        assert!(out.chars().count() <= 21, "截断后仍过长: {out}");
        assert!(out.contains('…'));
        assert!(out.starts_with("a_very_lon"));
    }

    #[test]
    fn elide_handles_multibyte_and_tiny_limits() {
        // 中文文件名按字符切，不能按字节 —— 否则会切出非法 UTF-8 而 panic。
        let cn = "这是一个很长的中文文件名称示例.png";
        let out = elide(cn, 8);
        assert!(out.chars().count() <= 9, "{out}");
        // 极小上限不能触发下溢 panic。
        for n in 0..4 {
            let _ = elide(cn, n);
            let _ = elide("abc", n);
        }
    }

    #[test]
    fn downscale_preserves_aspect_ratio() {
        let src = RgbaImage::new(4000, 2000);
        let out = downscale(&src, 1000);
        assert_eq!(out.dimensions(), (1000, 500));
    }

    #[test]
    fn downscale_leaves_small_images_alone() {
        let src = RgbaImage::new(100, 80);
        let out = downscale(&src, 2048);
        assert_eq!(out.dimensions(), (100, 80));
    }

    #[test]
    fn downscale_never_produces_zero_dimension() {
        // 极端长条图降采样后短边会趋近于 0，必须兜住，否则后面建纹理会失败。
        let src = RgbaImage::new(10000, 3);
        let out = downscale(&src, 100);
        assert!(
            out.width() >= 1 && out.height() >= 1,
            "{:?}",
            out.dimensions()
        );
    }
}
