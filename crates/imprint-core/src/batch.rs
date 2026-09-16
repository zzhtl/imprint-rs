//! 批量处理：rayon 并行 + 进度回报 + 取消。
//!
//! 每个工作线程各持一个 [`Renderer`]（`FontSystem` 不是 `Sync`），但共享同一份
//! [`FontLibrary`]，避免逐线程重扫系统字体。
//!
//! **失败项不中断整批** —— 一百张图里有一张损坏不该让另外九十九张白做，
//! 错误随结果一起返回，由调用方决定怎么呈现。

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use rayon::prelude::*;

use crate::{
    error::{Error, Result},
    fonts::FontLibrary,
    renderer::{ImageOptions, Renderer},
    spec::WatermarkSpec,
};

/// 可跨线程共享的取消令牌。
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        // Release：保证取消前写入的状态对观察到取消的线程可见。
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// 一条待处理的条目。
#[derive(Debug, Clone)]
pub struct BatchItem {
    pub src: PathBuf,
    pub dst: PathBuf,
}

/// 单条处理结果。
#[derive(Debug)]
pub struct ItemOutcome {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub result: Result<()>,
}

impl ItemOutcome {
    pub fn is_ok(&self) -> bool {
        self.result.is_ok()
    }
}

/// 进度快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchProgress {
    pub completed: usize,
    pub failed: usize,
    pub total: usize,
}

impl BatchProgress {
    pub fn ratio(&self) -> f32 {
        if self.total == 0 {
            return 1.0;
        }
        self.completed as f32 / self.total as f32
    }
}

/// 并行处理一批图片。
///
/// `on_progress` 会被多个线程并发调用，必须是 `Sync` 的；每完成一条触发一次。
pub fn run_image_batch<F>(
    fonts: Arc<FontLibrary>,
    items: &[BatchItem],
    spec: &WatermarkSpec,
    options: &ImageOptions,
    cancel: &CancelToken,
    on_progress: F,
) -> Vec<ItemOutcome>
where
    F: Fn(BatchProgress) + Sync,
{
    let total = items.len();
    let completed = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);

    let mut spec = spec.clone();
    spec.sanitize();

    items
        .par_iter()
        // 每线程建一次 Renderer：构造要克隆字体库，不能按条目建。
        .map_init(
            || Renderer::new(Arc::clone(&fonts)),
            |renderer, item| {
                if cancel.is_cancelled() {
                    return ItemOutcome {
                        src: item.src.clone(),
                        dst: item.dst.clone(),
                        result: Err(Error::Cancelled),
                    };
                }

                let result = process_one(renderer, item, &spec, options);
                if result.is_err() {
                    failed.fetch_add(1, Ordering::Relaxed);
                }
                let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                on_progress(BatchProgress {
                    completed: done,
                    failed: failed.load(Ordering::Relaxed),
                    total,
                });

                ItemOutcome {
                    src: item.src.clone(),
                    dst: item.dst.clone(),
                    result,
                }
            },
        )
        .collect()
}

fn process_one(
    renderer: &mut Renderer,
    item: &BatchItem,
    spec: &WatermarkSpec,
    options: &ImageOptions,
) -> Result<()> {
    if let Some(parent) = item.dst.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    renderer.watermark_image_file(&item.src, &item.dst, spec, options)
}

/// 目标目录下的输出路径：保留原文件名，按输出格式换扩展名。
pub fn output_path(src: &Path, out_dir: &Path, extension: &str, suffix: &str) -> PathBuf {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
    out_dir.join(format!("{stem}{suffix}.{extension}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        image_job::OutputFormat,
        spec::{Content, Rgba8, SizeMode, TextSpec},
    };
    use std::sync::atomic::AtomicU32;

    fn spec() -> WatermarkSpec {
        WatermarkSpec {
            content: Content::Text(TextSpec {
                template: "{filename}".to_owned(),
                color: Rgba8::new(255, 0, 0, 255),
                ..TextSpec::default()
            }),
            size: SizeMode::RelativeFontSize(0.1),
            ..WatermarkSpec::default()
        }
    }

    fn make_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("imprint_batch_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_png(path: &Path, w: u32, h: u32) {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 255]));
        img.save(path).unwrap();
    }

    #[test]
    fn processes_all_items_and_reports_progress() {
        let dir = make_dir("ok");
        let out = dir.join("out");
        let items: Vec<BatchItem> = (0..8)
            .map(|i| {
                let src = dir.join(format!("in_{i}.png"));
                // 混合分辨率，验证相对尺寸在批量里跨分辨率成立。
                write_png(&src, 200 + i * 40, 150 + i * 20);
                BatchItem {
                    dst: output_path(&src, &out, "png", "_wm"),
                    src,
                }
            })
            .collect();

        let ticks = AtomicU32::new(0);
        let last_total = AtomicUsize::new(0);
        let outcomes = run_image_batch(
            Arc::new(FontLibrary::with_system_fonts()),
            &items,
            &spec(),
            &ImageOptions {
                format: OutputFormat::Png,
                ..ImageOptions::default()
            },
            &CancelToken::new(),
            |p| {
                ticks.fetch_add(1, Ordering::Relaxed);
                last_total.store(p.total, Ordering::Relaxed);
            },
        );

        assert_eq!(outcomes.len(), 8);
        assert!(outcomes.iter().all(ItemOutcome::is_ok), "有条目处理失败");
        assert_eq!(ticks.load(Ordering::Relaxed), 8, "进度回调次数应等于条目数");
        assert_eq!(last_total.load(Ordering::Relaxed), 8);
        for item in &items {
            assert!(item.dst.exists(), "输出缺失: {}", item.dst.display());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_item_does_not_abort_the_batch() {
        let dir = make_dir("partial");
        let out = dir.join("out");

        let good = dir.join("good.png");
        write_png(&good, 200, 200);
        let broken = dir.join("broken.png");
        std::fs::write(&broken, b"not a png at all").unwrap();

        let items = vec![
            BatchItem {
                dst: output_path(&good, &out, "png", ""),
                src: good.clone(),
            },
            BatchItem {
                dst: output_path(&broken, &out, "png", ""),
                src: broken.clone(),
            },
        ];

        let outcomes = run_image_batch(
            Arc::new(FontLibrary::with_system_fonts()),
            &items,
            &spec(),
            &ImageOptions {
                format: OutputFormat::Png,
                ..ImageOptions::default()
            },
            &CancelToken::new(),
            |_| {},
        );

        assert_eq!(outcomes.len(), 2);
        let ok = outcomes.iter().filter(|o| o.is_ok()).count();
        assert_eq!(ok, 1, "好的那条也该产出");
        assert!(
            outcomes
                .iter()
                .any(|o| o.result.is_err() && o.src == broken),
            "坏条目应当被记为失败而不是让整批挂掉"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancellation_short_circuits_remaining_items() {
        let dir = make_dir("cancel");
        let out = dir.join("out");
        let items: Vec<BatchItem> = (0..16)
            .map(|i| {
                let src = dir.join(format!("c_{i}.png"));
                write_png(&src, 300, 300);
                BatchItem {
                    dst: output_path(&src, &out, "png", ""),
                    src,
                }
            })
            .collect();

        let cancel = CancelToken::new();
        // 第一条完成就取消，后续应当迅速以 Cancelled 收尾。
        let outcomes = run_image_batch(
            Arc::new(FontLibrary::with_system_fonts()),
            &items,
            &spec(),
            &ImageOptions {
                format: OutputFormat::Png,
                ..ImageOptions::default()
            },
            &cancel,
            |_| cancel.cancel(),
        );

        assert_eq!(outcomes.len(), 16, "每条都要有结果，取消也不例外");
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o.result, Err(Error::Cancelled))),
            "取消后应出现 Cancelled 结果"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn output_path_keeps_stem_and_applies_suffix() {
        let p = output_path(
            Path::new("/a/b/photo.jpeg"),
            Path::new("/out"),
            "png",
            "_wm",
        );
        assert_eq!(p, PathBuf::from("/out/photo_wm.png"));
    }

    #[test]
    fn cancel_token_is_observable_across_clones() {
        let a = CancelToken::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled());
    }
}
