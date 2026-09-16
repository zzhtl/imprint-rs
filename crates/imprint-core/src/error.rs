//! `imprint-core` 的公开错误类型。
//!
//! 库层错误用 `thiserror` 枚举而非 `anyhow`，调用方（UI、将来的移动端 FFI 层）
//! 才能按变体分流 —— 比如"ffmpeg 没装"要引导安装，"素材过大"要提示降分辨率，
//! 二者的处理完全不同。

use thiserror::Error;

/// `imprint-core` 的统一结果类型。
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// 水印处理过程中的失败。
///
/// 标 `#[non_exhaustive]`：后续加变体（例如新的视频后端）不应成为下游的破坏性变更。
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// 图像解码或编码失败。
    ///
    /// 这里刻意用 `#[from]` 把 `image::ImageError` 纳入公开契约：`image` 是本 crate
    /// 的核心依赖，它的大版本升级本来就构成 breaking change，包装一层并不能减轻
    /// 兼容负担，反而丢掉调用方区分"格式不支持"与"文件损坏"的能力。
    #[error("图像编解码失败")]
    Image(#[from] image::ImageError),

    #[error("IO 失败")]
    Io(#[from] std::io::Error),

    /// 画布或水印尺寸为 0，或超出 `tiny_skia::Pixmap` 能分配的上限。
    #[error("无效的画布尺寸 {width}x{height}")]
    InvalidSize { width: u32, height: u32 },

    /// 素材尺寸超出配置的解码上限。
    ///
    /// 独立于 [`Error::Image`]：这是可以让用户调高上限重试的情况，
    /// 而不是素材本身有问题。
    #[error("素材过大：{width}x{height} 需要约 {needed_mib} MiB，超过上限 {limit_mib} MiB")]
    SourceTooLarge {
        width: u32,
        height: u32,
        needed_mib: u64,
        limit_mib: u64,
    },

    /// 找不到可用字体，或选定字族不含所需字形。
    ///
    /// 移动端尤其容易触发：cosmic-text 在 Android 的字体回退表是空的，
    /// 在 iOS 则套用了 Linux 字族名。此时应改用 [`crate::spec::FontFamily::Embedded`]。
    #[error("字体不可用：{0}")]
    Font(String),

    /// 水印内容求值后为空（例如模板字段全部缺失），无可渲染之物。
    #[error("水印内容为空")]
    EmptyContent,

    #[cfg(feature = "svg")]
    #[error("SVG 解析失败：{0}")]
    Svg(String),

    /// 找不到 ffmpeg 可执行文件。
    ///
    /// 本 crate 刻意关掉了 `ffmpeg-sidecar` 的 `download_ffmpeg` feature
    /// （它会引入 `cc` + `pkg-config` 构建依赖），因此定位与安装引导由调用方负责。
    #[cfg(feature = "sidecar")]
    #[error("未找到 ffmpeg 可执行文件")]
    FfmpegNotFound,

    /// ffmpeg 进程以非零状态退出。
    #[cfg(feature = "sidecar")]
    #[error("ffmpeg 执行失败（退出码 {code:?}）：{stderr_tail}")]
    Ffmpeg {
        code: Option<i32>,
        /// stderr 尾部若干行 —— 完整日志可能上万行，对诊断无益。
        stderr_tail: String,
    },

    /// 任务被调用方取消。
    ///
    /// 用 `Err` 而非 `Ok`：取消意味着输出文件不完整，强制调用方走失败分支，
    /// 避免把半成品当成功结果使用。
    #[error("任务已取消")]
    Cancelled,
}
