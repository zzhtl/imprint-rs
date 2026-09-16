//! 后台批量任务。
//!
//! 批处理必须离开 UI 线程：一百张 4K 图要跑几十秒，同步做会把界面冻住。
//! 进度用 `Arc<Mutex<_>>` 共享而不是 channel —— egui 是每帧轮询的即时模式，
//! 它要的是"当前值"，而不是一条需要排空的事件流。

use std::sync::{Arc, Mutex, mpsc};

use imprint_core::{
    BatchItem, BatchProgress, CancelToken, FontLibrary, ImageOptions, ItemOutcome, batch,
    spec::WatermarkSpec,
};

/// 一个正在运行（或刚结束）的批量任务。
pub struct BatchTask {
    progress: Arc<Mutex<BatchProgress>>,
    cancel: CancelToken,
    results: mpsc::Receiver<Vec<ItemOutcome>>,
    finished: bool,
}

impl BatchTask {
    /// 起一个后台线程处理整批文件。
    pub fn spawn(
        fonts: Arc<FontLibrary>,
        items: Vec<BatchItem>,
        spec: WatermarkSpec,
        options: ImageOptions,
        egui_ctx: egui::Context,
    ) -> Self {
        let total = items.len();
        let progress = Arc::new(Mutex::new(BatchProgress {
            completed: 0,
            failed: 0,
            total,
        }));
        let cancel = CancelToken::new();
        let (tx, results) = mpsc::channel();

        let shared = Arc::clone(&progress);
        let token = cancel.clone();
        std::thread::spawn(move || {
            let outcomes = batch::run_image_batch(fonts, &items, &spec, &options, &token, |p| {
                // 锁内只做一次赋值，临界区短到不会和 UI 线程抢。
                if let Ok(mut guard) = shared.lock() {
                    *guard = p;
                }
                // 没有这一下，界面会停在最后一次交互的那帧上不动。
                egui_ctx.request_repaint();
            });
            let _ = tx.send(outcomes);
            egui_ctx.request_repaint();
        });

        Self {
            progress,
            cancel,
            results,
            finished: false,
        }
    }

    pub fn progress(&self) -> BatchProgress {
        self.progress
            .lock()
            .map(|g| *g)
            // 锁中毒说明工作线程 panic 了，这里退化成"全部失败"而不是跟着 panic。
            .unwrap_or(BatchProgress {
                completed: 0,
                failed: 0,
                total: 0,
            })
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// 取回结果；任务还没跑完时返回 `None`。
    pub fn poll(&mut self) -> Option<Vec<ItemOutcome>> {
        if self.finished {
            return None;
        }
        match self.results.try_recv() {
            Ok(outcomes) => {
                self.finished = true;
                Some(outcomes)
            }
            // 发送端被丢弃说明工作线程 panic 了，不能让 UI 永远停在"进行中"。
            Err(mpsc::TryRecvError::Disconnected) => {
                self.finished = true;
                Some(Vec::new())
            }
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }
}
