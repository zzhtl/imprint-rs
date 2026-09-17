//! `imprint` 命令行工具。
//!
//! 面向没有桌面环境的系统（服务器、CI、容器）：图片批量水印、视频水印，
//! 参数与 GUI 共用同一份 `imprint-core`，也能直接吃 GUI 导出的 JSON 预设。

mod args;

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use imprint_core::{
    BatchItem, CancelToken, FontLibrary, ImageOptions, ItemOutcome, OutputFormat, Renderer,
    batch::{self, BatchProgress},
    video::{EncodeSettings, VideoPipeline, VideoProgress, sidecar::SidecarPipeline},
};

/// 可批量处理的图片扩展名。
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "tif", "tiff", "bmp", "gif"];

#[derive(Parser)]
#[command(
    name = "imprint",
    version,
    about = "图片 / 视频水印工具",
    long_about = "给图片和视频批量添加水印。无需图形界面，适合服务器与 CI 环境。\n\
                  水印参数与桌面端共用，可直接读取 GUI 导出的 JSON 预设。"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// 输出详细日志
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// 给图片加水印，支持目录批量
    Image(ImageCmd),
    /// 给视频加水印
    Video(VideoCmd),
    /// 从图片中移除本工具添加的水印（需提供当初的参数）
    Remove(RemoveCmd),
    /// 修补图片的指定区域，用周围像素覆盖掉污点、日期戳或水印
    Erase(EraseCmd),
    /// 嵌入肉眼不可见的盲水印，用于事后溯源
    Sign(SignCmd),
    /// 从图片中提取盲水印
    Verify(VerifyCmd),
    /// 查看素材信息与 ffmpeg 可用性
    Probe(ProbeCmd),
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum StrengthArg {
    /// 几乎不可能被看出，但只扛得住轻度压缩
    Subtle,
    /// 默认：JPEG 质量 75 以上可靠提取，且肉眼无差别
    Default,
    /// 能扛质量 60 的压缩，平坦区域可能有轻微块状痕迹
    Robust,
}

impl StrengthArg {
    fn to_strength(self) -> imprint_core::blind::Strength {
        use imprint_core::blind::Strength;
        match self {
            Self::Subtle => Strength::SUBTLE,
            Self::Default => Strength::DEFAULT,
            Self::Robust => Strength::ROBUST,
        }
    }
}

#[derive(Args)]
struct ImageCmd {
    /// 输入文件或目录，可给多个
    #[arg(short, long, required = true, num_args = 1..)]
    input: Vec<PathBuf>,

    /// 输出文件（单个输入时）或输出目录
    #[arg(short, long)]
    output: PathBuf,

    /// 递归遍历子目录
    #[arg(short, long)]
    recursive: bool,

    /// 输出格式
    #[arg(long, value_enum, default_value_t = FormatArg::Jpeg)]
    format: FormatArg,

    /// JPEG 质量 1~100
    #[arg(long, default_value_t = 92)]
    quality: u8,

    /// 输出文件名后缀，例如 `_wm`
    #[arg(long, default_value = "")]
    suffix: String,

    /// 并行任务数，默认取 CPU 核心数
    #[arg(short, long)]
    jobs: Option<usize>,

    /// 不保留源图的 EXIF / ICC
    #[arg(long)]
    strip_metadata: bool,

    /// 只打印将要处理的文件，不实际写出
    #[arg(long)]
    dry_run: bool,

    #[command(flatten)]
    watermark: args::WatermarkArgs,
}

#[derive(Args)]
struct VideoCmd {
    /// 输入视频文件，可给多个
    #[arg(short, long, required = true, num_args = 1..)]
    input: Vec<PathBuf>,

    /// 输出文件（单个输入时）或输出目录
    #[arg(short, long)]
    output: PathBuf,

    /// ffmpeg 可执行文件路径，默认按 程序同目录 → PATH 查找
    #[arg(long, value_name = "PATH")]
    ffmpeg: Option<PathBuf>,

    /// 视频编码器；留空则试编一帧自动挑选可用的硬件编码器
    #[arg(long, value_name = "CODEC")]
    encoder: Option<String>,

    /// 质量参数（软件编码器用），数值越小质量越高
    #[arg(long)]
    crf: Option<u32>,

    /// x264/x265 的 preset（如 fast / medium / slow）
    ///
    /// 不叫 `--preset`：那个名字留给水印预设，与桌面端保持一致。
    #[arg(long, value_name = "NAME")]
    encoder_preset: Option<String>,

    /// 重新编码音频而不是直通
    #[arg(long)]
    reencode_audio: bool,

    /// 输出文件名后缀
    #[arg(long, default_value = "_wm")]
    suffix: String,

    #[command(flatten)]
    watermark: args::WatermarkArgs,
}

#[derive(Args)]
struct RemoveCmd {
    /// 输入文件或目录，可给多个
    #[arg(short, long, required = true, num_args = 1..)]
    input: Vec<PathBuf>,

    /// 输出文件（单个输入时）或输出目录
    #[arg(short, long)]
    output: PathBuf,

    /// 递归遍历子目录
    #[arg(short, long)]
    recursive: bool,

    /// 输出格式。默认 PNG：逆运算的结果再经 JPEG 压缩会白白损失精度
    #[arg(long, value_enum, default_value_t = FormatArg::Png)]
    format: FormatArg,

    /// JPEG 质量 1~100（仅 --format jpeg 时有效）
    #[arg(long, default_value_t = 95)]
    quality: u8,

    /// 输出文件名后缀
    #[arg(long, default_value = "")]
    suffix: String,

    /// 加水印时那张图的原始文件名。
    ///
    /// 水印文字里若用过 {filename}，必须靠它还原出当初那段文字 ——
    /// 否则减掉的是另一段内容，结果反而更糟。
    #[arg(long, value_name = "NAME")]
    original_name: Option<String>,

    #[command(flatten)]
    watermark: args::WatermarkArgs,
}

/// 修补指定区域。
///
/// 与 `remove` 是两件事：`remove` 知道当初的水印参数，做的是精确逆运算；
/// 这里什么都不知道，只能拿周围的像素猜中间该是什么，结果是**近似**的。
#[derive(Args)]
struct EraseCmd {
    /// 输入文件或目录，可给多个
    #[arg(short, long, required = true, num_args = 1..)]
    input: Vec<PathBuf>,

    /// 输出文件（单个输入时）或输出目录
    #[arg(short, long)]
    output: PathBuf,

    /// 要修补的矩形，格式 `x,y,宽,高`（像素）。可重复给多个。
    ///
    /// 单独使用时整个框都会被周围像素糊掉，只适合小面积；框一旦大到几百像素见方，糊出来的结果会比留着水印更难看，此时务必配 --color。
    #[arg(long, value_name = "X,Y,W,H", value_parser = parse_region)]
    region: Vec<imprint_core::inpaint::Rect>,

    /// 蒙版图片：白色处需要修补。尺寸须与原图一致
    #[arg(long, value_name = "FILE")]
    mask: Option<PathBuf>,

    /// 蒙版图的亮度阈值，高于它才算需要修补
    #[arg(long, default_value_t = 128)]
    mask_threshold: u8,

    /// 只修补接近该颜色的像素，如 `#FFFFFF`。
    ///
    /// 与 --region 连用是最有效的组合：框缩到水印附近，再把框里真正属于水印的笔画挑出来，框内的背景就不会被一起糊掉。
    #[arg(long, value_name = "HEX")]
    color: Option<String>,

    /// --color 的容差：三通道差值之和的上限，取值 0~765。
    ///
    /// 半透明水印与背景混过色，低于 180 基本选不中；不透明水印几十就够。200 对两者都接近最优，再往上会开始误伤背景。
    #[arg(long, default_value_t = 200)]
    tolerance: u16,

    /// 把选区向外扩张的像素数，用来盖住水印抗锯齿的边缘
    #[arg(long, default_value_t = 2)]
    grow: u32,

    /// 采样半径。越大越平滑也越糊；文字类水印 3~6 合适
    #[arg(long, default_value_t = imprint_core::inpaint::DEFAULT_RADIUS)]
    radius: u32,

    /// 递归遍历子目录
    #[arg(short, long)]
    recursive: bool,

    /// 输出格式
    #[arg(long, value_enum, default_value_t = FormatArg::Png)]
    format: FormatArg,

    /// JPEG 质量 1~100（仅 --format jpeg 时有效）
    #[arg(long, default_value_t = 95)]
    quality: u8,

    /// 输出文件名后缀
    #[arg(long, default_value = "")]
    suffix: String,
}

/// 解析 `x,y,宽,高`。x/y 允许为负，方便圈住跨出画布边缘的水印。
fn parse_region(s: &str) -> anyhow::Result<imprint_core::inpaint::Rect> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    let [x, y, w, h] = parts.as_slice() else {
        bail!("区域应为 `x,y,宽,高` 四个数字，得到 `{s}`");
    };
    let num = |v: &str, name: &str| -> anyhow::Result<i64> {
        v.parse::<i64>()
            .with_context(|| format!("区域 `{s}` 的{name}不是整数"))
    };
    let (width, height) = (num(w, "宽")?, num(h, "高")?);
    if width <= 0 || height <= 0 {
        bail!("区域 `{s}` 的宽高必须为正");
    }
    Ok(imprint_core::inpaint::Rect {
        x: num(x, "x")?,
        y: num(y, "y")?,
        width: width as u32,
        height: height as u32,
    })
}

#[derive(Args)]
struct SignCmd {
    /// 输入文件或目录，可给多个
    #[arg(short, long, required = true, num_args = 1..)]
    input: Vec<PathBuf>,

    /// 输出文件（单个输入时）或输出目录
    #[arg(short, long)]
    output: PathBuf,

    /// 要嵌入的标识，如工号、邮箱或订单号
    #[arg(short, long, value_name = "TEXT")]
    payload: String,

    /// 递归遍历子目录
    #[arg(short, long)]
    recursive: bool,

    /// 嵌入强度
    #[arg(long, value_enum, default_value_t = StrengthArg::Default)]
    strength: StrengthArg,

    /// 输出格式
    #[arg(long, value_enum, default_value_t = FormatArg::Jpeg)]
    format: FormatArg,

    /// JPEG 质量。低于 75 会明显削弱盲水印的可提取性
    #[arg(long, default_value_t = 95)]
    quality: u8,

    /// 输出文件名后缀
    #[arg(long, default_value = "")]
    suffix: String,
}

#[derive(Args)]
struct VerifyCmd {
    /// 待检验的图片，可给多个
    #[arg(short, long, required = true, num_args = 1..)]
    input: Vec<PathBuf>,

    /// 递归遍历子目录
    #[arg(short, long)]
    recursive: bool,
}

#[derive(Args)]
struct ProbeCmd {
    /// 要查看的素材，省略则只检查 ffmpeg 环境
    #[arg(short, long, num_args = 1..)]
    input: Vec<PathBuf>,

    /// ffmpeg 可执行文件路径
    #[arg(long, value_name = "PATH")]
    ffmpeg: Option<PathBuf>,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum FormatArg {
    Jpeg,
    Png,
    Webp,
}

impl FormatArg {
    fn to_output(self, quality: u8) -> OutputFormat {
        match self {
            Self::Jpeg => OutputFormat::Jpeg { quality },
            Self::Png => OutputFormat::Png,
            Self::Webp => OutputFormat::WebP,
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Webp => "webp",
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(if cli.verbose { "info" } else { "warn" }),
    )
    .format_timestamp(None)
    .init();

    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            // 错误链一并打出来，否则只看到最外层那句会很难定位。
            eprintln!("错误：{e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Image(cmd) => run_image(cmd),
        Command::Video(cmd) => run_video(cmd),
        Command::Remove(cmd) => run_remove(cmd),
        Command::Erase(cmd) => run_erase(cmd),
        Command::Sign(cmd) => run_sign(cmd),
        Command::Verify(cmd) => run_verify(cmd),
        Command::Probe(cmd) => run_probe(cmd),
    }
}

fn run_image(cmd: ImageCmd) -> anyhow::Result<ExitCode> {
    let spec = cmd.watermark.to_spec()?;
    let sources = collect_inputs(&cmd.input, cmd.recursive, IMAGE_EXTENSIONS)?;
    if sources.is_empty() {
        bail!("没有找到可处理的图片");
    }

    let single_file_output = sources.len() == 1 && !is_dir_like(&cmd.output);
    let extension = cmd.format.extension();

    let items: Vec<BatchItem> = sources
        .iter()
        .map(|src| BatchItem {
            src: src.clone(),
            dst: if single_file_output {
                cmd.output.clone()
            } else {
                batch::output_path(src, &cmd.output, extension, &cmd.suffix)
            },
        })
        .collect();

    if cmd.dry_run {
        for item in &items {
            println!("{} -> {}", item.src.display(), item.dst.display());
        }
        eprintln!("共 {} 个文件（dry-run，未写出）", items.len());
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(jobs) = cmd.jobs {
        // 失败只说明全局线程池已被初始化过，不影响功能。
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(jobs.max(1))
            .build_global();
    }

    let options = ImageOptions {
        format: cmd.format.to_output(cmd.quality),
        keep_metadata: !cmd.strip_metadata,
        ..ImageOptions::default()
    };

    let fonts = Arc::new(FontLibrary::with_system_fonts());
    let total = items.len();
    let outcomes = batch::run_image_batch(
        fonts,
        &items,
        &spec,
        &options,
        &CancelToken::new(),
        |p: BatchProgress| report_batch_progress(&p),
    );
    eprintln!();

    summarize(&outcomes, total)
}

fn run_video(cmd: VideoCmd) -> anyhow::Result<ExitCode> {
    let spec = cmd.watermark.to_spec()?;
    let pipeline = SidecarPipeline::discover(cmd.ffmpeg.as_deref()).context(
        "未找到 ffmpeg。请安装后重试（Debian/Ubuntu: apt install ffmpeg；\
         macOS: brew install ffmpeg；Windows: 从 ffmpeg.org 下载并加入 PATH），\
         或用 --ffmpeg 指定可执行文件路径",
    )?;

    let encoder = match cmd.encoder {
        Some(codec) => codec,
        None => {
            eprintln!("正在探测可用的视频编码器…");
            pipeline.detect_encoder().context(
                "当前 ffmpeg 没有任何可用的视频编码器；可用 --encoder 显式指定，\
                 或换一个功能更完整的 ffmpeg 构建",
            )?
        }
    };
    eprintln!("ffmpeg: {}", pipeline.ffmpeg_path().display());
    eprintln!("编码器: {encoder}");

    // 硬件编码器多数不认 CRF，未显式指定时不要替用户塞一个。
    let is_hardware = encoder != "libx264" && encoder != "libx265";
    let encode = EncodeSettings {
        video_codec: encoder,
        crf: cmd.crf.or(if is_hardware { None } else { Some(20) }),
        preset: cmd.encoder_preset.or_else(|| {
            if is_hardware {
                None
            } else {
                Some("medium".to_owned())
            }
        }),
        copy_audio: !cmd.reencode_audio,
        faststart: true,
    };

    let single_file_output = cmd.input.len() == 1 && !is_dir_like(&cmd.output);
    let mut renderer = Renderer::new(Arc::new(FontLibrary::with_system_fonts()));
    let cancel = CancelToken::new();
    let mut failed = 0usize;

    for src in &cmd.input {
        if !src.is_file() {
            eprintln!("跳过（不是文件）：{}", src.display());
            failed += 1;
            continue;
        }

        let dst = if single_file_output {
            cmd.output.clone()
        } else {
            let ext = src
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("mp4")
                .to_owned();
            batch::output_path(src, &cmd.output, &ext, &cmd.suffix)
        };
        if let Some(parent) = dst.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        eprintln!("处理 {} -> {}", src.display(), dst.display());
        let mut on_progress = |p: VideoProgress| report_video_progress(&p);
        match renderer.watermark_video(
            &pipeline,
            src,
            &dst,
            &spec,
            &encode,
            &mut on_progress,
            &cancel,
        ) {
            Ok(()) => eprintln!("\r完成 {}{}", dst.display(), " ".repeat(20)),
            Err(e) => {
                eprintln!("\r失败 {}：{e}", src.display());
                failed += 1;
            }
        }
    }

    Ok(exit_code(failed, cmd.input.len()))
}

fn run_remove(cmd: RemoveCmd) -> anyhow::Result<ExitCode> {
    let spec = cmd.watermark.to_spec()?;
    let sources = collect_inputs(&cmd.input, cmd.recursive, IMAGE_EXTENSIONS)?;
    if sources.is_empty() {
        bail!("没有找到可处理的图片");
    }

    let single_file_output = sources.len() == 1 && !is_dir_like(&cmd.output);
    let extension = cmd.format.extension();
    let options = ImageOptions {
        format: cmd.format.to_output(cmd.quality),
        ..ImageOptions::default()
    };

    let mut renderer = Renderer::new(Arc::new(FontLibrary::with_system_fonts()));
    let mut failed = 0usize;
    let mut worst_unrecoverable = 0f32;
    let mut mismatched = 0usize;

    for src in &sources {
        let dst = if single_file_output {
            cmd.output.clone()
        } else {
            batch::output_path(src, &cmd.output, extension, &cmd.suffix)
        };
        if let Some(parent) = dst.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        // 模板字段必须求值成与当初完全相同的文本，因此优先用 --original-name；
        // 没给就退回当前文件名（同名时正好对得上）。
        let name = cmd
            .original_name
            .as_deref()
            .or_else(|| src.file_name().and_then(|n| n.to_str()));
        let ctx = match name {
            Some(name) => imprint_core::FieldContext::new().with_file_name(name),
            None => imprint_core::FieldContext::new(),
        };

        let outcome = std::fs::File::open(src)
            .map_err(anyhow::Error::from)
            .and_then(|input| {
                let output = std::io::BufWriter::new(std::fs::File::create(&dst)?);
                Ok(renderer.remove_watermark(input, output, &spec, &options, &ctx)?)
            });

        match outcome {
            Ok(report) => {
                let ratio = report.unrecoverable_ratio();
                worst_unrecoverable = worst_unrecoverable.max(ratio);
                if ratio > 0.0 {
                    eprintln!(
                        "{}：{:.1}% 的水印像素为完全不透明，无法还原",
                        src.display(),
                        ratio * 100.0
                    );
                }
                if report.looks_mismatched() {
                    mismatched += 1;
                    eprintln!(
                        "{}：{:.0}% 的像素越界，水印参数很可能与当初不一致",
                        src.display(),
                        report.clamped_ratio() * 100.0
                    );
                }
            }
            Err(e) => {
                eprintln!("失败 {}：{e:#}", src.display());
                failed += 1;
            }
        }
    }

    eprintln!("完成 {}/{}", sources.len() - failed, sources.len());
    if mismatched > 0 {
        eprintln!(
            "\n{mismatched} 个文件疑似参数不匹配。请核对：\n\
             - 文字、字号、位置、透明度、旋转是否与加水印时完全一致（建议直接用当初的 --preset）\n\
             - 若用过 {{filename}} 等动态字段，需用 --original-name 指定当初的文件名"
        );
    }
    if worst_unrecoverable > 0.0 {
        eprintln!("提示：完全不透明的水印会彻底覆盖原像素，这部分信息无法用任何方法还原。");
    }
    Ok(exit_code(failed, sources.len()))
}

fn run_erase(cmd: EraseCmd) -> anyhow::Result<ExitCode> {
    let mut mask_spec = imprint_core::inpaint::MaskSpec {
        rects: cmd.region.clone(),
        grow: cmd.grow,
        ..Default::default()
    };
    if let Some(path) = &cmd.mask {
        let file = std::fs::File::open(path)
            .with_context(|| format!("打开蒙版 {} 失败", path.display()))?;
        let img = imprint_core::image_job::decode(file, ImageOptions::default().max_alloc_bytes)
            .with_context(|| format!("解码蒙版 {} 失败", path.display()))?;
        mask_spec.mask_image = Some((img.image, cmd.mask_threshold));
    }
    if let Some(hex) = &cmd.color {
        let c = args::parse_color(hex)?;
        mask_spec.color_key = Some(imprint_core::inpaint::ColorKey {
            color: [c.r, c.g, c.b],
            tolerance: cmd.tolerance,
        });
    }
    if mask_spec.is_empty() {
        bail!("没有指定要修补的范围：请给 --region、--mask 或 --color 中的至少一个");
    }

    let sources = collect_inputs(&cmd.input, cmd.recursive, IMAGE_EXTENSIONS)?;
    if sources.is_empty() {
        bail!("没有找到可处理的图片");
    }

    let single_file_output = sources.len() == 1 && !is_dir_like(&cmd.output);
    let extension = cmd.format.extension();
    let options = ImageOptions {
        format: cmd.format.to_output(cmd.quality),
        ..ImageOptions::default()
    };

    let mut failed = 0usize;
    for src in &sources {
        let dst = if single_file_output {
            cmd.output.clone()
        } else {
            batch::output_path(src, &cmd.output, extension, &cmd.suffix)
        };
        if let Some(parent) = dst.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let outcome = std::fs::File::open(src)
            .map_err(anyhow::Error::from)
            .and_then(|input| {
                let output = std::io::BufWriter::new(std::fs::File::create(&dst)?);
                Ok(imprint_core::inpaint::erase(
                    input, output, &mask_spec, cmd.radius, &options,
                )?)
            });

        match outcome {
            Ok(report) if report.filled_pixels == 0 => {
                // 写出的是原样复制，用户多半会以为没生效，直说比让他自己猜好。
                eprintln!("{}：选区没有命中任何像素，输出与原图一致", src.display());
            }
            Ok(report) => {
                log::info!("{}：修补了 {} 个像素", src.display(), report.filled_pixels);
            }
            Err(e) => {
                eprintln!("失败 {}：{e:#}", src.display());
                failed += 1;
            }
        }
    }

    eprintln!("完成 {}/{}", sources.len() - failed, sources.len());
    Ok(exit_code(failed, sources.len()))
}

fn run_sign(cmd: SignCmd) -> anyhow::Result<ExitCode> {
    let sources = collect_inputs(&cmd.input, cmd.recursive, IMAGE_EXTENSIONS)?;
    if sources.is_empty() {
        bail!("没有找到可处理的图片");
    }
    if matches!(cmd.format, FormatArg::Jpeg) && cmd.quality < 75 {
        eprintln!(
            "警告：JPEG 质量 {} 低于 75，盲水印可能无法可靠提取",
            cmd.quality
        );
    }

    let single_file_output = sources.len() == 1 && !is_dir_like(&cmd.output);
    let extension = cmd.format.extension();
    let options = ImageOptions {
        format: cmd.format.to_output(cmd.quality),
        ..ImageOptions::default()
    };
    let strength = cmd.strength.to_strength();
    let mut failed = 0usize;

    for src in &sources {
        let dst = if single_file_output {
            cmd.output.clone()
        } else {
            batch::output_path(src, &cmd.output, extension, &cmd.suffix)
        };
        if let Some(parent) = dst.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let outcome = (|| -> anyhow::Result<()> {
            let decoded = imprint_core::image_job::decode(
                std::fs::File::open(src)?,
                options.max_alloc_bytes,
            )?;
            let signed =
                imprint_core::blind::embed(&decoded.image, cmd.payload.as_bytes(), strength)?;
            let out = std::io::BufWriter::new(std::fs::File::create(&dst)?);
            imprint_core::image_job::encode(&signed, out, options.format, &decoded.metadata)?;
            Ok(())
        })();

        if let Err(e) = outcome {
            eprintln!("失败 {}：{e:#}", src.display());
            failed += 1;
        }
    }

    eprintln!("完成 {}/{}", sources.len() - failed, sources.len());
    Ok(exit_code(failed, sources.len()))
}

fn run_verify(cmd: VerifyCmd) -> anyhow::Result<ExitCode> {
    let sources = collect_inputs(&cmd.input, cmd.recursive, IMAGE_EXTENSIONS)?;
    if sources.is_empty() {
        bail!("没有找到可检验的图片");
    }

    let mut found = 0usize;
    for src in &sources {
        match std::fs::File::open(src)
            .map_err(anyhow::Error::from)
            .and_then(|f| Ok(imprint_core::image_job::decode(f, None)?))
        {
            Ok(decoded) => match imprint_core::blind::extract(&decoded.image) {
                Some(payload) => {
                    found += 1;
                    let text = String::from_utf8_lossy(&payload);
                    println!("{}\t{}", src.display(), text);
                }
                None => println!("{}\t(未检出)", src.display()),
            },
            Err(e) => eprintln!("读取失败 {}：{e:#}", src.display()),
        }
    }

    eprintln!("检出 {found}/{}", sources.len());
    // 一个都没检出时给出非零退出码，便于脚本判断。
    Ok(if found == 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn run_probe(cmd: ProbeCmd) -> anyhow::Result<ExitCode> {
    match SidecarPipeline::discover(cmd.ffmpeg.as_deref()) {
        Ok(pipeline) => {
            println!("ffmpeg: {}", pipeline.ffmpeg_path().display());
            match pipeline.version() {
                Ok(v) => println!("版本:   {v}"),
                Err(e) => println!("版本:   读取失败（{e}）"),
            }
            match pipeline.detect_encoder() {
                Some(codec) => println!("编码器: {codec}"),
                None => println!("编码器: 无可用编码器（视频导出会失败）"),
            }

            for src in &cmd.input {
                println!("\n{}", src.display());
                match pipeline.probe(src) {
                    Ok(info) => {
                        println!("  分辨率: {}x{}", info.width, info.height);
                        println!("  帧率:   {:.2}", info.fps);
                        match info.duration_secs {
                            Some(d) => println!("  时长:   {d:.2}s"),
                            None => println!("  时长:   未知"),
                        }
                        println!("  音轨:   {}", if info.has_audio { "有" } else { "无" });
                    }
                    Err(e) => println!("  探测失败：{e}"),
                }
            }
        }
        Err(e) => {
            println!("ffmpeg: 未找到（{e}）");
            println!("视频功能不可用；图片水印不依赖 ffmpeg，仍可正常使用。");
        }
    }

    let fonts = FontLibrary::with_system_fonts();
    println!("\n字体库: {} 个 face", fonts.face_count());
    match fonts.load_font_for_text("水印中文") {
        Some(blob) => println!("中文字体: {} (face #{})", blob.family, blob.face_index),
        None => println!("中文字体: 未找到 —— 中文水印会渲染成豆腐块"),
    }

    Ok(ExitCode::SUCCESS)
}

/// 展开输入路径：文件直接收下，目录按扩展名筛选。
fn collect_inputs(
    inputs: &[PathBuf],
    recursive: bool,
    extensions: &[&str],
) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for input in inputs {
        if input.is_file() {
            out.push(input.clone());
        } else if input.is_dir() {
            collect_dir(input, recursive, extensions, &mut out)
                .with_context(|| format!("遍历目录 {}", input.display()))?;
        } else {
            bail!("路径不存在：{}", input.display());
        }
    }
    // 目录遍历顺序由文件系统决定，排序让输出和日志可复现。
    out.sort();
    out.dedup();
    Ok(out)
}

fn collect_dir(
    dir: &Path,
    recursive: bool,
    extensions: &[&str],
    out: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if recursive {
                collect_dir(&path, recursive, extensions, out)?;
            }
            continue;
        }
        let matches = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| extensions.iter().any(|w| e.eq_ignore_ascii_case(w)));
        if matches {
            out.push(path);
        }
    }
    Ok(())
}

/// 输出路径看起来是目录吗。
///
/// 已存在的目录，或末尾带分隔符、或没有扩展名 —— 都按目录处理。
fn is_dir_like(path: &Path) -> bool {
    path.is_dir() || path.extension().is_none()
}

fn report_batch_progress(p: &BatchProgress) {
    let mut err = std::io::stderr();
    let _ = write!(
        err,
        "\r进度 {}/{}（失败 {}）  ",
        p.completed, p.total, p.failed
    );
    let _ = err.flush();
}

fn report_video_progress(p: &VideoProgress) {
    let mut err = std::io::stderr();
    match p.ratio() {
        Some(r) => {
            let _ = write!(
                err,
                "\r  {:.1}%  {:.1}x  {:.0} fps   ",
                r * 100.0,
                p.speed,
                p.fps
            );
        }
        // 探测不到总时长就只报已处理时长，不编造百分比。
        None => {
            let _ = write!(err, "\r  {:.1}s  {:.1}x   ", p.time_secs, p.speed);
        }
    }
    let _ = err.flush();
}

fn summarize(outcomes: &[ItemOutcome], total: usize) -> anyhow::Result<ExitCode> {
    let failed: Vec<&ItemOutcome> = outcomes.iter().filter(|o| !o.is_ok()).collect();
    for item in &failed {
        if let Err(e) = &item.result {
            eprintln!("失败 {}：{e}", item.src.display());
        }
    }
    eprintln!("完成 {}/{}", total - failed.len(), total);
    Ok(exit_code(failed.len(), total))
}

/// 0 全成功；1 部分失败；2 全失败。
fn exit_code(failed: usize, total: usize) -> ExitCode {
    if failed == 0 {
        ExitCode::SUCCESS
    } else if failed < total {
        ExitCode::from(1)
    } else {
        ExitCode::from(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // clap 的自检，能抓出参数重名一类的问题 —— `--preset` 曾经同时被
        // 水印预设和 x264 preset 占用，就是被它拦下的。
        Cli::command().debug_assert();
    }

    #[test]
    fn dir_like_detection() {
        assert!(is_dir_like(Path::new("out")));
        assert!(is_dir_like(Path::new("/tmp/out/")));
        assert!(!is_dir_like(Path::new("out.jpg")));
        assert!(!is_dir_like(Path::new("/a/b/c.png")));
    }

    #[test]
    fn exit_codes_distinguish_partial_failure() {
        assert_eq!(
            format!("{:?}", exit_code(0, 5)),
            format!("{:?}", ExitCode::SUCCESS)
        );
        assert_eq!(
            format!("{:?}", exit_code(2, 5)),
            format!("{:?}", ExitCode::from(1))
        );
        assert_eq!(
            format!("{:?}", exit_code(5, 5)),
            format!("{:?}", ExitCode::from(2))
        );
    }

    #[test]
    fn collects_files_by_extension() {
        let dir = std::env::temp_dir().join("imprint_cli_collect");
        let _ = std::fs::remove_dir_all(&dir);
        let sub = dir.join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("a.png"), b"x").unwrap();
        std::fs::write(dir.join("b.JPG"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::write(sub.join("c.webp"), b"x").unwrap();

        let flat = collect_inputs(std::slice::from_ref(&dir), false, IMAGE_EXTENSIONS).unwrap();
        assert_eq!(flat.len(), 2, "非递归时不该进子目录；扩展名要大小写不敏感");

        let deep = collect_inputs(std::slice::from_ref(&dir), true, IMAGE_EXTENSIONS).unwrap();
        assert_eq!(deep.len(), 3);
        // 排序保证输出可复现。
        assert!(deep.windows(2).all(|w| w[0] <= w[1]));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_input_is_an_error() {
        let err = collect_inputs(&[PathBuf::from("/no/such/path")], false, IMAGE_EXTENSIONS);
        assert!(err.is_err());
    }
}
