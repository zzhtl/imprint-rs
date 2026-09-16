//! 视频水印管线。
//!
//! 核心取舍：**水印图层被渲染成一张与视频等尺寸的透明 PNG，整幅 overlay 上去**，
//! 而不是把定位交给 ffmpeg 的 overlay 表达式。这样平铺、旋转、锚点、透明度全部
//! 走 [`crate::compose`] 那一份代码，视频输出和图片输出、以及 UI 预览天然一致；
//! 代价只是一张一次性生成的全画幅 PNG。
//!
//! 帧数据全程不进 Rust —— 合成由 ffmpeg 的 filter graph 完成，音频 `-c:a copy`
//! 直通。这也是不走 libav FFI 的原因：那条路要自己写
//! demux→decode→filter→encode→mux 加 timebase/pts 换算，而性能并无差异。

use std::path::{Path, PathBuf};

use crate::{batch::CancelToken, error::Result};

#[cfg(feature = "sidecar")]
pub mod sidecar;

/// 视频基本信息。
#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    /// 时长（秒）。某些容器探测不到时为 `None`。
    pub duration_secs: Option<f64>,
    pub fps: f32,
    pub has_audio: bool,
}

/// 编码设置。
#[derive(Debug, Clone, PartialEq)]
pub struct EncodeSettings {
    /// 视频编码器名，如 `libx264`、`h264_nvenc`、`h264_videotoolbox`。
    pub video_codec: String,
    /// 质量参数。硬件编码器多数不认 CRF，此时应置 `None` 改用码率。
    pub crf: Option<u32>,
    /// x264/x265 的 preset，硬件编码器一般忽略。
    pub preset: Option<String>,
    /// 音频直通而非重编码。
    pub copy_audio: bool,
    /// 把 moov atom 挪到文件头，便于边下边播。
    pub faststart: bool,
}

impl Default for EncodeSettings {
    fn default() -> Self {
        Self {
            // libx264 永远可用，硬件编码器必须探测后才能选。
            video_codec: "libx264".to_owned(),
            crf: Some(20),
            preset: Some("medium".to_owned()),
            copy_audio: true,
            faststart: true,
        }
    }
}

/// 一次视频水印任务。
#[derive(Debug, Clone)]
pub struct VideoJob {
    pub src: PathBuf,
    pub dst: PathBuf,
    /// 与视频等尺寸的透明水印图层 PNG。
    pub overlay_png: PathBuf,
    pub encode: EncodeSettings,
}

/// 编码进度。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoProgress {
    /// 已处理到的时间点（秒）。
    pub time_secs: f64,
    /// 视频总时长（秒），探测不到时为 `None`。
    pub duration_secs: Option<f64>,
    pub fps: f32,
    /// 相对实时的倍速。
    pub speed: f32,
}

impl VideoProgress {
    /// 完成比例。总时长未知时返回 `None` —— 不要拿它硬凑一个假百分比。
    pub fn ratio(&self) -> Option<f32> {
        let total = self.duration_secs?;
        if total <= 0.0 {
            return None;
        }
        Some((self.time_secs / total).clamp(0.0, 1.0) as f32)
    }
}

/// 视频管线后端。
///
/// 抽成 trait 是为移动端留口：iOS 沙箱禁止 fork/exec，sidecar 在那里不可行，
/// 届时需要换 libav FFI 或平台原生（MediaCodec / VideoToolbox）实现。
pub trait VideoPipeline: Send + Sync {
    fn probe(&self, src: &Path) -> Result<VideoInfo>;

    fn render(
        &self,
        job: &VideoJob,
        progress: &mut dyn FnMut(VideoProgress),
        cancel: &CancelToken,
    ) -> Result<()>;
}

/// 构造 ffmpeg 的参数列表。
///
/// 独立成纯函数是为了能在**没有安装 ffmpeg 的机器上**单测命令拼装 ——
/// 参数顺序错、滤镜标签写反这类问题不该等到跑真视频才发现。
pub fn build_args(job: &VideoJob) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        // 覆盖已存在的输出文件。缺了它 ffmpeg 会卡在交互式询问上 ——
        // ffmpeg-sidecar 自己的注释也说这会让进程"看似无限挂起且毫无提示"。
        "-y".into(),
        "-i".into(),
        job.src.to_string_lossy().into_owned(),
        "-i".into(),
        job.overlay_png.to_string_lossy().into_owned(),
        // 图层已按视频分辨率渲染好，overlay 贴在原点即可，不需要 scale。
        "-filter_complex".into(),
        "[0:v][1:v]overlay=0:0:format=auto[v]".into(),
        "-map".into(),
        "[v]".into(),
        // 结尾的 `?` 让无音轨的素材也能正常处理，而不是直接报错。
        "-map".into(),
        "0:a?".into(),
        "-c:v".into(),
        job.encode.video_codec.clone(),
    ];

    if let Some(crf) = job.encode.crf {
        args.push("-crf".into());
        args.push(crf.to_string());
    }
    if let Some(preset) = &job.encode.preset {
        args.push("-preset".into());
        args.push(preset.clone());
    }

    args.push("-c:a".into());
    args.push(if job.encode.copy_audio {
        "copy".into()
    } else {
        "aac".to_owned()
    });

    if job.encode.faststart {
        args.push("-movflags".into());
        args.push("+faststart".into());
    }

    args.push(job.dst.to_string_lossy().into_owned());
    args
}

/// 探测帧的边长。
///
/// 取 16 的倍数：部分 H.264 编码器（openh264 尤其）对非宏块对齐的尺寸挑剔。
pub const PROBE_FRAME_SIDE: usize = 64;

/// 一帧 yuv420p 探测帧的字节数（Y + U/V 各四分之一）。
pub const PROBE_FRAME_BYTES: usize = PROBE_FRAME_SIDE * PROBE_FRAME_SIDE * 3 / 2;

/// 试编码探测用的参数：从 stdin 喂一帧原始数据，看指定编码器能否真正工作。
///
/// `ffmpeg -encoders` 列的是**编译进去的**编码器，不代表当前机器能用 ——
/// 没有 N 卡的机器照样会列出 `h264_nvenc`。只有试编一帧看退出码才作数。
///
/// 输入刻意走 `rawvideo` + `pipe:0` 而不是 `lavfi`：`lavfi` 属于 avdevice，
/// 裁剪版 ffmpeg 为了体积不会带它，用它探测会把所有编码器都误判成不可用。
pub fn probe_encoder_args(codec: &str) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-s".into(),
        format!("{PROBE_FRAME_SIDE}x{PROBE_FRAME_SIDE}"),
        "-i".into(),
        "pipe:0".into(),
        "-frames:v".into(),
        "1".into(),
        "-c:v".into(),
        codec.to_owned(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]
}

/// 当前平台值得尝试的编码器，按优先级排列：硬件 → 软件 H.264 → 内建保底。
///
/// **`libopenh264` 不能少**：LGPL 版 ffmpeg（唯一能安心打包进闭源软件的版本）
/// 配置里明确写着 `--disable-libx264 --enable-libopenh264`，只认 libx264 的候选表
/// 会在那种构建上一个都试不通。末位的 `mpeg4` 是 ffmpeg 内建编码器，任何构建都有，
/// 用来保证"至少能出片"。
pub fn encoder_candidates() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &["h264_videotoolbox", "libx264", "libopenh264", "mpeg4"]
    }
    #[cfg(target_os = "windows")]
    {
        &[
            "h264_nvenc",
            "h264_qsv",
            "h264_amf",
            "libx264",
            "libopenh264",
            "mpeg4",
        ]
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // h264_vaapi 不在候选里：它需要 -vaapi_device 和 hwupload 滤镜链，
        // 与 CPU 侧 overlay 的滤镜图形状冲突，得单独排期支持。
        &["h264_nvenc", "libx264", "libopenh264", "mpeg4"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> VideoJob {
        VideoJob {
            src: PathBuf::from("/in/clip.mp4"),
            dst: PathBuf::from("/out/clip_wm.mp4"),
            overlay_png: PathBuf::from("/tmp/overlay.png"),
            encode: EncodeSettings::default(),
        }
    }

    fn joined(job: &VideoJob) -> String {
        build_args(job).join(" ")
    }

    #[test]
    fn overlays_at_origin_without_scaling() {
        let args = joined(&job());
        assert!(args.contains("[0:v][1:v]overlay=0:0:format=auto[v]"));
        // 图层已按视频分辨率渲染，出现 scale 就说明多做了一次重采样。
        assert!(!args.contains("scale="), "不该有多余的 scale 滤镜");
    }

    #[test]
    fn maps_optional_audio_and_copies_it() {
        let args = build_args(&job());
        let map_positions: Vec<&String> = args
            .iter()
            .skip_while(|a| *a != "-map")
            .filter(|a| a.starts_with("0:a") || a.starts_with("[v"))
            .collect();
        assert!(map_positions.iter().any(|a| *a == "[v]"));
        // 结尾的 `?` 是无音轨素材不报错的关键。
        assert!(
            map_positions.iter().any(|a| *a == "0:a?"),
            "音频映射必须带 `?`"
        );
        assert!(joined(&job()).contains("-c:a copy"));
    }

    #[test]
    fn always_overwrites_output() {
        // 少了 -y，ffmpeg 会卡在交互式确认上，在无人值守场景里等同于挂死。
        assert!(build_args(&job()).contains(&"-y".to_owned()));
    }

    #[test]
    fn hardware_encoder_omits_crf_and_preset() {
        let mut j = job();
        j.encode = EncodeSettings {
            video_codec: "h264_videotoolbox".into(),
            crf: None,
            preset: None,
            ..EncodeSettings::default()
        };
        let args = joined(&j);
        assert!(args.contains("-c:v h264_videotoolbox"));
        assert!(!args.contains("-crf"), "硬件编码器多数不认 CRF");
        assert!(!args.contains("-preset"));
    }

    #[test]
    fn audio_transcode_when_copy_disabled() {
        let mut j = job();
        j.encode.copy_audio = false;
        assert!(joined(&j).contains("-c:a aac"));
    }

    #[test]
    fn faststart_can_be_disabled() {
        let mut j = job();
        assert!(joined(&j).contains("-movflags +faststart"));
        j.encode.faststart = false;
        assert!(!joined(&j).contains("faststart"));
    }

    #[test]
    fn progress_ratio_is_none_without_duration() {
        let p = VideoProgress {
            time_secs: 5.0,
            duration_secs: None,
            fps: 30.0,
            speed: 1.0,
        };
        // 总时长未知时不应编造百分比。
        assert!(p.ratio().is_none());

        let p2 = VideoProgress {
            duration_secs: Some(10.0),
            ..p
        };
        assert_eq!(p2.ratio(), Some(0.5));
    }

    #[test]
    fn encoder_candidates_end_with_builtin_fallback() {
        let c = encoder_candidates();
        assert_eq!(
            *c.last().unwrap(),
            "mpeg4",
            "末位必须是 ffmpeg 内建编码器，任何构建都有"
        );
        // LGPL 版 ffmpeg 没有 x264，只认 libx264 会在那种构建上全军覆没。
        assert!(
            c.contains(&"libopenh264"),
            "候选表必须包含 libopenh264，否则 LGPL 构建上无软件 H.264 可用"
        );
        // 硬件编码器应当排在软件编码器前面。
        let soft = c.iter().position(|e| *e == "libx264").unwrap();
        assert!(
            c[..soft].iter().all(|e| e.contains('_')),
            "软件编码器之前应当只有硬件编码器"
        );
    }

    #[test]
    fn probe_args_encode_a_single_frame() {
        let args = probe_encoder_args("h264_nvenc").join(" ");
        assert!(args.contains("-frames:v 1"));
        assert!(args.contains("-c:v h264_nvenc"));
        assert!(args.contains("-f null"));
        assert!(args.contains("-i pipe:0"));
        // lavfi 属于 avdevice，裁剪版 ffmpeg 不带它；用它探测会全军覆没。
        assert!(!args.contains("lavfi"), "探测不能依赖 lavfi");
    }

    #[test]
    fn probe_frame_is_macroblock_aligned() {
        // openh264 对非 16 对齐的尺寸挑剔。
        assert_eq!(PROBE_FRAME_SIDE % 16, 0);
        assert_eq!(PROBE_FRAME_BYTES, 64 * 64 * 3 / 2);
    }
}
