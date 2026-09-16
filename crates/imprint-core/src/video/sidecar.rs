//! 用外部 ffmpeg 二进制实现的视频管线。
//!
//! 本 crate 刻意关掉了 `ffmpeg-sidecar` 的 `download_ffmpeg` feature —— 它的依赖链是
//! `xz2 → lzma-sys → build-deps: cc + pkg-config`，会破坏纯 Rust 构建链。
//! 因此 ffmpeg 的定位与安装引导由这里负责，见 [`SidecarPipeline::discover`]。

use std::{
    collections::VecDeque,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use ffmpeg_sidecar::{
    command::FfmpegCommand,
    event::{FfmpegEvent, LogLevel, StreamTypeSpecificData},
    ffmpeg_time_duration::FfmpegTimeDuration,
};

use super::{
    PROBE_FRAME_BYTES, VideoInfo, VideoJob, VideoPipeline, VideoProgress, build_args,
    probe_encoder_args,
};
use crate::{
    batch::CancelToken,
    error::{Error, Result},
};

/// 出错时回传给调用方的 stderr 行数。
///
/// ffmpeg 的日志能刷上万行，整段贴出来对诊断毫无帮助；真正有用的永远是末尾几行。
const STDERR_TAIL_LINES: usize = 12;

/// 驱动外部 ffmpeg 的视频管线。
#[derive(Debug, Clone)]
pub struct SidecarPipeline {
    ffmpeg: PathBuf,
}

impl SidecarPipeline {
    /// 按 `用户配置 → 可执行文件同目录 → PATH` 的顺序定位 ffmpeg。
    pub fn discover(configured: Option<&Path>) -> Result<Self> {
        if let Some(path) = configured
            && probe_binary(path)
        {
            return Ok(Self {
                ffmpeg: path.to_path_buf(),
            });
        }

        // 与应用同目录的 ffmpeg：便携分发时把二进制放在一起即可。
        if let Ok(sidecar) = ffmpeg_sidecar::paths::sidecar_path()
            && probe_binary(&sidecar)
        {
            return Ok(Self { ffmpeg: sidecar });
        }

        let from_path = PathBuf::from("ffmpeg");
        if probe_binary(&from_path) {
            return Ok(Self { ffmpeg: from_path });
        }

        Err(Error::FfmpegNotFound)
    }

    /// 直接指定 ffmpeg 路径。
    pub fn with_path(path: impl Into<PathBuf>) -> Result<Self> {
        let ffmpeg = path.into();
        if !probe_binary(&ffmpeg) {
            return Err(Error::FfmpegNotFound);
        }
        Ok(Self { ffmpeg })
    }

    pub fn ffmpeg_path(&self) -> &Path {
        &self.ffmpeg
    }

    pub fn version(&self) -> Result<String> {
        ffmpeg_sidecar::version::ffmpeg_version_with_path(&self.ffmpeg).map_err(|e| Error::Ffmpeg {
            code: None,
            stderr_tail: e.to_string(),
        })
    }

    /// 挑一个当前机器上**真正能用**的编码器。
    ///
    /// 不看 `ffmpeg -encoders`：那列的是编译进去的编码器，没有 N 卡的机器照样
    /// 会列出 `h264_nvenc`。只有实际编一帧、看退出码才作数。结果值得缓存，
    /// 每个候选都要起一次进程。
    ///
    /// 返回 `None` 表示一个都试不通 —— 这种情况下与其硬塞一个可能不存在的
    /// 编码器名（早先的版本在 LGPL 构建上就这么回退到了并不存在的 `libx264`），
    /// 不如让调用方明确报错。
    pub fn detect_encoder(&self) -> Option<String> {
        for codec in super::encoder_candidates() {
            if self.encoder_works(codec) {
                log::info!("选用视频编码器: {codec}");
                return Some((*codec).to_owned());
            }
            log::debug!("编码器不可用，跳过: {codec}");
        }
        log::warn!("没有任何候选编码器可用");
        None
    }

    fn encoder_works(&self, codec: &str) -> bool {
        let Ok(mut child) = Command::new(&self.ffmpeg)
            .args(probe_encoder_args(codec))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };

        if let Some(mut stdin) = child.stdin.take() {
            // 一帧全零的 yuv420p。写失败不用管：编码器不可用时 ffmpeg 会先退出，
            // 管道随即断开，这里的 EPIPE 正是"不可用"的表现之一。
            let _ = stdin.write_all(&vec![0u8; PROBE_FRAME_BYTES]);
            // 显式关闭让 ffmpeg 看到输入结束，否则它会一直等下去。
            drop(stdin);
        }

        child.wait().is_ok_and(|s| s.success())
    }
}

impl VideoPipeline for SidecarPipeline {
    fn probe(&self, src: &Path) -> Result<VideoInfo> {
        let mut command = FfmpegCommand::new_with_path(&self.ffmpeg);
        command
            .arg("-hide_banner")
            .arg("-i")
            .arg(src.as_os_str())
            // 只要头部信息，不产出任何帧。
            .args(["-t", "0", "-f", "null", "-"]);

        let mut child = command.spawn()?;
        let mut width = 0;
        let mut height = 0;
        let mut fps = 0.0;
        let mut duration_secs = None;
        let mut has_audio = false;
        let mut tail = Tail::new();

        for event in child.iter().map_err(|e| Error::Ffmpeg {
            code: None,
            stderr_tail: e.to_string(),
        })? {
            match event {
                FfmpegEvent::ParsedInputStream(stream) => match stream.type_specific_data {
                    StreamTypeSpecificData::Video(v) => {
                        // 只认第一条视频轨；有些容器会塞封面图当额外视频流。
                        if width == 0 {
                            width = v.width;
                            height = v.height;
                            fps = v.fps;
                        }
                    }
                    StreamTypeSpecificData::Audio(_) => has_audio = true,
                    _ => {}
                },
                FfmpegEvent::ParsedDuration(d) => duration_secs = Some(d.duration),
                FfmpegEvent::Log(level, msg) => tail.push(level, &msg),
                FfmpegEvent::Error(msg) => tail.push(LogLevel::Error, &msg),
                _ => {}
            }
        }
        let status = child.wait()?;

        if width == 0 || height == 0 {
            return Err(Error::Ffmpeg {
                code: status.code(),
                stderr_tail: tail.into_string(),
            });
        }

        Ok(VideoInfo {
            width,
            height,
            duration_secs,
            fps,
            has_audio,
        })
    }

    fn render(
        &self,
        job: &VideoJob,
        progress: &mut dyn FnMut(VideoProgress),
        cancel: &CancelToken,
    ) -> Result<()> {
        let mut command = FfmpegCommand::new_with_path(&self.ffmpeg);
        // 参数整体来自 build_args，与它的单元测试验证的是同一份内容。
        command.args(build_args(job));

        let mut child = command.spawn()?;
        let mut duration_secs = None;
        let mut tail = Tail::new();
        let mut quit_sent = false;

        for event in child.iter().map_err(|e| Error::Ffmpeg {
            code: None,
            stderr_tail: e.to_string(),
        })? {
            match event {
                FfmpegEvent::ParsedDuration(d) => duration_secs = Some(d.duration),
                FfmpegEvent::Progress(p) => {
                    let time_secs = FfmpegTimeDuration::from_str(&p.time)
                        .map(FfmpegTimeDuration::as_seconds)
                        .unwrap_or(0.0);
                    progress(VideoProgress {
                        time_secs,
                        duration_secs,
                        fps: p.fps,
                        speed: p.speed,
                    });

                    if cancel.is_cancelled() && !quit_sent {
                        quit_sent = true;
                        // 优先 quit（往 stdin 写 "q"）：它会刷缓冲并写出 trailer。
                        // 直接 kill 会留下没有 moov atom、根本播不了的残缺 mp4。
                        if let Err(e) = child.quit() {
                            log::warn!("发送 quit 失败，改为强杀: {e}");
                            let _ = child.kill();
                        }
                    }
                }
                FfmpegEvent::Log(level, msg) => tail.push(level, &msg),
                FfmpegEvent::Error(msg) => tail.push(LogLevel::Error, &msg),
                _ => {}
            }
        }

        let status = child.wait()?;

        // 只看 quit_sent，不看 cancel 的当前状态：ffmpeg 可能在调用方置位取消之前
        // 就已经正常收尾了，那种情况下输出文件是完整的，报成 Cancelled 会让调用方
        // 误删一个本可以用的结果。真正中断过它，quit_sent 才为真。
        if quit_sent {
            return Err(Error::Cancelled);
        }
        if !status.success() {
            return Err(Error::Ffmpeg {
                code: status.code(),
                stderr_tail: tail.into_string(),
            });
        }
        Ok(())
    }
}

/// 保留 stderr 末尾若干行。
struct Tail(VecDeque<String>);

impl Tail {
    fn new() -> Self {
        Self(VecDeque::with_capacity(STDERR_TAIL_LINES))
    }

    fn push(&mut self, level: LogLevel, msg: &str) {
        // info 级别多是逐帧刷屏，留着只会把真正的报错顶出窗口。
        if matches!(level, LogLevel::Info) {
            return;
        }
        if self.0.len() == STDERR_TAIL_LINES {
            self.0.pop_front();
        }
        self.0.push_back(msg.trim().to_owned());
    }

    fn into_string(self) -> String {
        self.0.into_iter().collect::<Vec<_>>().join("\n")
    }
}

/// ffmpeg 二进制是否存在且可执行。
fn probe_binary(path: &Path) -> bool {
    ffmpeg_sidecar::version::ffmpeg_version_with_path(path.as_os_str()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_reports_missing_binary_clearly() {
        // 指一个必然不存在的路径，必须是 FfmpegNotFound 而不是别的错误 ——
        // 调用方据此弹"去装 ffmpeg"的引导。
        let err = SidecarPipeline::with_path("/definitely/not/ffmpeg").unwrap_err();
        assert!(matches!(err, Error::FfmpegNotFound));
    }

    /// 端到端取消测试所需的环境。
    ///
    /// 依赖一个真实的 ffmpeg 和一段足够长的视频（转码需数秒才来得及中途取消），
    /// 所以用环境变量提供；缺失时跳过而不是让整个测试套件失败。
    fn cancel_test_env() -> Option<(PathBuf, PathBuf)> {
        let ffmpeg = std::env::var_os("IMPRINT_TEST_FFMPEG")?;
        let video = std::env::var_os("IMPRINT_TEST_VIDEO")?;
        Some((PathBuf::from(ffmpeg), PathBuf::from(video)))
    }

    #[test]
    fn cancelling_midway_leaves_a_readable_file() {
        let Some((ffmpeg, src)) = cancel_test_env() else {
            eprintln!("跳过：需设置 IMPRINT_TEST_FFMPEG 与 IMPRINT_TEST_VIDEO");
            return;
        };
        let pipeline = SidecarPipeline::with_path(&ffmpeg).expect("ffmpeg 不可用");
        let info = pipeline.probe(&src).expect("探测素材失败");

        // 造一张与视频等尺寸的水印图层。
        let mut renderer =
            crate::Renderer::new(std::sync::Arc::new(crate::FontLibrary::with_system_fonts()));
        let spec = crate::spec::WatermarkSpec {
            content: crate::spec::Content::Text(crate::spec::TextSpec {
                template: "CANCEL".to_owned(),
                ..crate::spec::TextSpec::default()
            }),
            ..crate::spec::WatermarkSpec::default()
        };
        let png = renderer
            .render_video_overlay(
                &spec,
                (info.width, info.height),
                &crate::FieldContext::new(),
            )
            .expect("渲染 overlay");

        let dir = std::env::temp_dir().join("imprint_cancel_test");
        let _ = std::fs::create_dir_all(&dir);
        let overlay = dir.join("overlay.png");
        std::fs::write(&overlay, png).unwrap();
        let dst = dir.join("cancelled.mp4");
        let _ = std::fs::remove_file(&dst);

        let job = VideoJob {
            src: src.clone(),
            dst: dst.clone(),
            overlay_png: overlay,
            encode: super::super::EncodeSettings {
                video_codec: "libopenh264".to_owned(),
                crf: None,
                preset: None,
                ..super::super::EncodeSettings::default()
            },
        };

        let cancel = CancelToken::new();
        let result = std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let mut on_progress = |_: VideoProgress| {};
                pipeline.render(&job, &mut on_progress, &cancel)
            });
            // 让它真正开始编码，再中断。
            std::thread::sleep(std::time::Duration::from_millis(1500));
            cancel.cancel();
            handle.join().expect("渲染线程 panic")
        });

        assert!(
            matches!(result, Err(Error::Cancelled)),
            "中途取消应当返回 Cancelled，实际 {result:?}"
        );

        // 关键断言：走的是 quit() 而不是 kill()，所以 ffmpeg 有机会写出 moov atom。
        // 直接 kill 会留下一个根本解析不了的残缺 mp4。
        let probed = pipeline.probe(&dst);
        assert!(
            probed.is_ok(),
            "取消后的输出无法解析，说明没能正常收尾: {probed:?}"
        );
        let probed = probed.unwrap();
        assert_eq!(probed.width, info.width);
        // 取消发生在中途，时长必然短于原片。
        if let (Some(got), Some(full)) = (probed.duration_secs, info.duration_secs) {
            assert!(got < full, "取消后的时长 {got} 不该等于原片 {full}");
            assert!(got > 0.0, "输出为空，等于什么都没写出来");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_keeps_last_lines_and_drops_info_noise() {
        let mut tail = Tail::new();
        for i in 0..100 {
            tail.push(LogLevel::Info, &format!("逐帧刷屏 {i}"));
        }
        tail.push(LogLevel::Error, "  真正的报错  ");
        let s = tail.into_string();
        assert!(s.contains("真正的报错"));
        assert!(!s.contains("逐帧刷屏"), "info 噪声不该占用尾部窗口");
        assert_eq!(s, "真正的报错", "应当已 trim");
    }

    #[test]
    fn tail_is_bounded() {
        let mut tail = Tail::new();
        for i in 0..1000 {
            tail.push(LogLevel::Error, &format!("e{i}"));
        }
        let s = tail.into_string();
        assert_eq!(s.lines().count(), STDERR_TAIL_LINES);
        assert!(s.contains("e999"), "必须保留最新的行");
        assert!(!s.contains("e0\n"), "最老的行应当被挤掉");
    }
}
