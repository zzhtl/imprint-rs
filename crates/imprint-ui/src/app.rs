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
        Anchor9, Content, FontFamily, Margin, Placement, Rgba8, SizeMode, TextAlign, TextSpec,
        WatermarkSpec,
    },
};

use crate::{fonts, job::BatchTask, preview::PreviewState};

/// 预览底图的长边上限。
///
/// wgpu 后端下 `TextureOptions` 的 mipmap 不生效（egui 文档写明只有 egui_glow 支持），
/// 所以必须在 CPU 侧预降采样，否则缩小显示的 4K 图会明显走样。
const PREVIEW_MAX_SIDE: u32 = 2048;

/// 当前打开的素材。
pub struct Document {
    pub path: Option<PathBuf>,
    pub file_name: String,
    /// 全尺寸原图，导出时用。
    pub full: RgbaImage,
    pub metadata: image_job::Metadata,
    /// 预览底图纹理，打开文件时上传一次，之后不动。
    pub base_texture: TextureHandle,
}

impl Document {
    pub fn size(&self) -> (u32, u32) {
        self.full.dimensions()
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
    /// 上一帧的 spec，用于检测内容变化并让预览纹理失效。
    last_spec: Option<WatermarkSpec>,
    smoke: Option<Smoke>,
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
            last_spec: None,
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
        match self.load(egui_ctx, &path) {
            Ok(doc) => {
                let (w, h) = doc.size();
                self.status = format!("{} · {w}×{h}", doc.file_name);
                self.doc = Some(doc);
                self.preview.invalidate();
            }
            Err(e) => self.status = format!("打开失败：{e}"),
        }
    }

    fn load(&self, egui_ctx: &egui::Context, path: &PathBuf) -> anyhow::Result<Document> {
        let file = std::fs::File::open(path)?;
        let decoded = image_job::decode(file, self.output.max_alloc_bytes)?;
        let full = decoded.image;

        let preview = downscale(&full, PREVIEW_MAX_SIDE);
        let color = egui::ColorImage::from_rgba_unmultiplied(
            [preview.width() as usize, preview.height() as usize],
            preview.as_raw(),
        );
        let base_texture =
            egui_ctx.load_texture("imprint_base", color, egui::TextureOptions::LINEAR);

        Ok(Document {
            file_name: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("untitled")
                .to_owned(),
            path: Some(path.clone()),
            full,
            metadata: decoded.metadata,
            base_texture,
        })
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

    fn export(&mut self, dst: PathBuf) {
        let Some(doc) = &self.doc else {
            return;
        };
        let src = match &doc.path {
            Some(p) => p.clone(),
            None => {
                self.status = "当前素材没有来源路径，无法导出".to_owned();
                return;
            }
        };
        match self
            .renderer
            .watermark_image_file(&src, &dst, &self.spec, &self.output)
        {
            Ok(()) => self.status = format!("已导出 {}", dst.display()),
            Err(e) => self.status = format!("导出失败：{e}"),
        }
    }
}

impl eframe::App for ImprintApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let egui_ctx = ui.ctx().clone();

        // spec 的内容部分一变就丢弃图层纹理；位置/旋转/透明度不触发重传。
        if self.last_spec.as_ref() != Some(&self.spec) {
            let content_changed = self
                .last_spec
                .as_ref()
                .is_none_or(|s| s.content != self.spec.content || s.size != self.spec.size);
            if content_changed {
                self.preview.invalidate();
            }
            self.last_spec = Some(self.spec.clone());
        }

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("打开图片…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter(
                            "图片",
                            &["jpg", "jpeg", "png", "webp", "tiff", "bmp", "gif"],
                        )
                        .pick_file()
                {
                    self.open_path(&egui_ctx, path);
                }

                ui.add_enabled_ui(self.doc.is_some(), |ui| {
                    if ui.button("导出…").clicked() {
                        let ext = match self.output.format {
                            OutputFormat::Jpeg { .. } => "jpg",
                            OutputFormat::Png => "png",
                            OutputFormat::WebP => "webp",
                        };
                        let name = self
                            .doc
                            .as_ref()
                            .map(|d| {
                                let stem = d
                                    .file_name
                                    .rsplit_once('.')
                                    .map_or(d.file_name.as_str(), |(s, _)| s);
                                format!("{stem}_watermarked.{ext}")
                            })
                            .unwrap_or_else(|| format!("output.{ext}"));
                        if let Some(path) = rfd::FileDialog::new().set_file_name(name).save_file() {
                            self.export(path);
                        }
                    }
                });

                ui.separator();

                if ui.button("保存预设…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("预设", &["json"])
                        .set_file_name("imprint_preset.json")
                        .save_file()
                {
                    self.save_preset(path);
                }
                if ui.button("加载预设…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("预设", &["json"])
                        .pick_file()
                {
                    self.load_preset(path);
                }

                ui.separator();
                ui.label(&self.status);
            });
        });

        egui::Panel::left("queue")
            .default_size(240.0)
            .show(ui, |ui| {
                self.queue_panel(ui, &egui_ctx);
            });

        if self.task.is_some() {
            egui::Panel::bottom("progress").show(ui, |ui| {
                self.progress_panel(ui);
            });
        }

        egui::Panel::right("params")
            .default_size(320.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.spec_panel(ui);
                });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            let Some(doc) = &self.doc else {
                ui.centered_and_justified(|ui| {
                    ui.label("点击「打开图片…」载入素材");
                });
                return;
            };

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

            if let Some(err) = &self.preview.last_error {
                ui.colored_label(
                    Color32::from_rgb(230, 120, 120),
                    format!("水印渲染失败：{err}"),
                );
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
        ui.add_space(6.0);
        ui.heading("批量队列");

        let busy = self.task.as_ref().is_some_and(|t| !t.is_finished());
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                if ui.button("加文件…").clicked()
                    && let Some(paths) = rfd::FileDialog::new()
                        .add_filter(
                            "图片",
                            &["jpg", "jpeg", "png", "webp", "tiff", "bmp", "gif"],
                        )
                        .pick_files()
                {
                    self.queue.extend(paths);
                    self.queue.sort();
                    self.queue.dedup();
                }
                if ui.button("加目录…").clicked()
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
                if ui.button("批量导出…").clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    self.start_batch(egui_ctx, dir);
                }
            });
        });

        ui.separator();
        ui.label(format!("{} 个文件", self.queue.len()));

        egui::ScrollArea::vertical()
            .max_height(ui.available_height() * 0.6)
            .show(ui, |ui| {
                let mut remove = None;
                for (i, path) in self.queue.iter().enumerate() {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("?")
                        .to_owned();
                    ui.horizontal(|ui| {
                        if ui.small_button("×").clicked() {
                            remove = Some(i);
                        }
                        // 长文件名会把面板撑开，截断显示、完整内容放 tooltip。
                        ui.label(elide(&name, 22))
                            .on_hover_text(path.display().to_string());
                    });
                }
                if let Some(i) = remove {
                    self.queue.remove(i);
                }
            });

        if !self.last_results.is_empty() {
            ui.separator();
            let failed: Vec<&ItemOutcome> =
                self.last_results.iter().filter(|o| !o.is_ok()).collect();
            if failed.is_empty() {
                ui.colored_label(Color32::from_rgb(120, 200, 120), "全部成功");
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
        const EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "tif", "tiff", "bmp", "gif"];
        let Ok(entries) = std::fs::read_dir(dir) else {
            self.status = format!("无法读取目录 {}", dir.display());
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ok = path.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| EXTS.iter().any(|w| e.eq_ignore_ascii_case(w)));
            if ok {
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

        if let Some(results) = task.poll() {
            let failed = results.iter().filter(|o| !o.is_ok()).count();
            self.status = if failed == 0 {
                format!("批量完成：{} 个全部成功", results.len())
            } else {
                format!("批量完成：{} 成功 / {failed} 失败", results.len() - failed)
            };
            self.last_results = results;
            self.task = None;
        }
    }

    fn spec_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("水印内容");

        if let Content::Text(text) = &mut self.spec.content {
            ui.add(
                egui::TextEdit::multiline(&mut text.template)
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
                    text.color = Rgba8::new(c.r(), c.g(), c.b(), c.a());
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
        ui.heading("尺寸");
        size_controls(ui, &mut self.spec.size);

        ui.separator();
        ui.heading("位置");
        placement_controls(ui, &mut self.spec.placement);

        ui.separator();
        ui.heading("样式");
        ui.horizontal(|ui| {
            ui.label("不透明度");
            ui.add(egui::Slider::new(&mut self.spec.opacity, 0.0..=1.0).fixed_decimals(2));
        });
        ui.horizontal(|ui| {
            ui.label("旋转");
            ui.add(egui::Slider::new(&mut self.spec.rotation_deg, -180.0..=180.0).suffix("°"));
        });

        ui.separator();
        ui.heading("输出");
        output_controls(ui, &mut self.output);
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
    let (w, h) = doc.size();
    let mut ctx = FieldContext::new()
        .with_file_name(&doc.file_name)
        .with_dimensions(w, h);
    if let Some(raw) = &doc.metadata.exif {
        ctx = ctx.with_exif(imprint_core::ExifFields::parse(raw));
    }
    ctx
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
