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
            let outcomes = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                batch::run_image_batch(fonts, &items, &spec, &options, &token, |p| {
                    // 锁内只做一次赋值，临界区短到不会和 UI 线程抢。
                    if let Ok(mut guard) = shared.lock()
                        && p.completed >= guard.completed
                    {
                        *guard = p;
                    }
                    // 没有这一下，界面会停在最后一次交互的那帧上不动。
                    egui_ctx.request_repaint();
                })
            }));
            if let Ok(outcomes) = outcomes {
                let _ = tx.send(outcomes);
            }
            drop(tx);
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
    pub fn poll(&mut self) -> Option<Result<Vec<ItemOutcome>, String>> {
        if self.finished {
            return None;
        }
        match self.results.try_recv() {
            Ok(outcomes) => {
                self.finished = true;
                Some(Ok(outcomes))
            }
            // 发送端被丢弃说明工作线程 panic 了，不能让 UI 永远停在"进行中"。
            Err(mpsc::TryRecvError::Disconnected) => {
                self.finished = true;
                Some(Err("后台处理意外中断，请重试并查看日志".to_owned()))
            }
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }
}

/// 单张素材的加载与导出同样离开 UI 线程，完成或异常退出都会唤醒界面。
pub struct Background<T> {
    receiver: mpsc::Receiver<anyhow::Result<T>>,
}

impl<T: Send + 'static> Background<T> {
    pub fn spawn(
        ctx: egui::Context,
        work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
    ) -> Self {
        let (tx, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("后台处理意外中断")));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        Self { receiver }
    }

    pub fn poll(&self) -> Option<anyhow::Result<T>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!("后台处理意外中断"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_panic_is_reported_as_failure() {
        let task = Background::<()>::spawn(egui::Context::default(), || {
            panic!("simulated worker failure")
        });
        let result = task
            .receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(result.is_err());
    }
}
