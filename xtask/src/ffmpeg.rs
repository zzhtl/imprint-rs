//! 编译裁剪版 ffmpeg。
//!
//! 官方发行的静态构建单文件 80~136 MB，因为它启用了 Vulkan、libplacebo、AOM、
//! SVT-AV1 等一大堆水印场景用不到的东西。这里从 `--disable-everything` 出发，
//! 只把真正需要的组件加回来。
//!
//! 许可上刻意**不启用** x264/x265（GPL），改用 Cisco 的 libopenh264（BSD）做
//! H.264 软件编码，这样产出的二进制是 LGPL v2.1，可以安心随闭源软件分发。

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail};
use clap::Args;
use sha2::{Digest, Sha256};

/// 默认编译的 ffmpeg 版本。
///
/// 不取最新的大版本：水印管线用不到新特性，成熟的补丁版更省事。
pub const FFMPEG_VERSION: &str = "8.1.2";

/// 源码包的 SHA-256，防止下载被篡改。
///
/// 换版本时必须同步更新，否则构建会直接失败 —— 这是有意的：
/// 静默接受一个未知哈希的源码包等于没有校验。
const FFMPEG_SHA256: &str = "464beb5e7bf0c311e68b45ae2f04e9cc2af88851abb4082231742a74d97b524c";

/// Cisco 的 H.264 编解码器版本。
///
/// 它替代 x264 提供 H.264 软件编码：x264 是 GPL，会让产出的二进制无法随
/// 闭源软件分发；openh264 是 BSD 许可，配合 LGPL 的 ffmpeg 主体正好满足需求。
const OPENH264_VERSION: &str = "2.6.0";

#[derive(Debug, Args)]
pub struct BuildArgs {
    /// ffmpeg 版本
    #[arg(long, default_value = FFMPEG_VERSION)]
    pub version: String,

    /// 产物输出目录
    #[arg(long, default_value = "dist")]
    pub out: PathBuf,

    /// 构建工作目录
    #[arg(long, default_value = "target/ffmpeg-build")]
    pub work: PathBuf,

    /// 并行编译任务数，默认取 CPU 核心数
    #[arg(short, long)]
    pub jobs: Option<usize>,

    /// 复用已有的源码与中间产物
    #[arg(long)]
    pub incremental: bool,

    /// 跳过源码包哈希校验（仅用于本地试验新版本）
    #[arg(long)]
    pub skip_checksum: bool,

    /// 关闭 x86 汇编优化。
    ///
    /// **只用于排查问题，产出的二进制不可分发**：实测在 x86-64 上关掉汇编后，
    /// swscale 的 RGB↔YUV 转换结果是错的（往返 PSNR 从 38 dB 掉到 4.9 dB），
    /// 表现为视频画面颜色错乱、横向撕裂，而编码过程不会报任何错。
    /// 构建后自检会拦住这种二进制。
    #[arg(long)]
    pub no_asm: bool,
}

pub fn run(args: &BuildArgs) -> anyhow::Result<()> {
    if args.no_asm {
        eprintln!(
            "警告：--no-asm 产出的二进制画面是坏的（swscale 色彩转换出错），\n\
             只可用于排查问题，不要分发。"
        );
    } else {
        ensure_assembler()?;
    }

    // 路径必须先转成绝对的：configure 和 make 都在源码子目录里执行，
    // 相对路径会被解析到那里去（曾经导致 openh264 被装进
    // `openh264-2.6.0/target/ffmpeg-build/...`，随后的存在性检查自然找不到）。
    std::fs::create_dir_all(&args.work)?;
    let work = std::path::absolute(&args.work)
        .with_context(|| format!("解析工作目录 {}", args.work.display()))?;

    // 先把 openh264 编出来：ffmpeg 的 configure 要靠它的 .pc 文件才能探测到。
    let openh264_pkgconfig = build_openh264(args, &work)?;

    let src = ensure_source(args, &work)?;
    configure(args, &src, &openh264_pkgconfig)?;
    make(args, &src)?;

    let binary = src.join(exe_name());
    if !binary.is_file() {
        bail!("编译结束但没找到产物 {}", binary.display());
    }

    std::fs::create_dir_all(&args.out)?;
    let out = std::path::absolute(&args.out)
        .with_context(|| format!("解析输出目录 {}", args.out.display()))?;
    let dst = out.join(exe_name());
    std::fs::copy(&binary, &dst)
        .with_context(|| format!("复制 {} -> {}", binary.display(), dst.display()))?;

    let size = std::fs::metadata(&dst)?.len();
    println!(
        "\n产物：{}  ({:.1} MB)",
        dst.display(),
        size as f64 / (1024.0 * 1024.0)
    );

    self_check(&dst, &work)?;
    println!("自检通过");

    write_notices(args, &out)?;
    Ok(())
}

/// 随产物写出第三方许可声明。
///
/// 这不是可选项：随程序分发 LGPL 的 FFmpeg 必须声明其许可、给出源码获取方式，
/// 并说明它是怎么构建的（好让使用者能自己替换成同版本的库）。
/// 由 xtask 生成而不是手写，是为了让版本号和 configure 参数永远与实际产物一致。
pub fn write_notices(args: &BuildArgs, out: &Path) -> anyhow::Result<()> {
    // 逐行拼接而不是写一大段多行字符串字面量：rustfmt 会把带 `\` 续行的字符串
    // 合并成一行，并把源码里的缩进空格留在内容里，产出的文本会一身空格。
    let configure = configure_args(args).join(" \\\n    ");
    let text = [
        "第三方组件许可声明".to_owned(),
        "==================".to_owned(),
        String::new(),
        "本程序捆绑了以下第三方组件。".to_owned(),
        String::new(),
        "FFmpeg".to_owned(),
        "------".to_owned(),
        format!("版本：     {}", args.version),
        "许可：     GNU Lesser General Public License v2.1 或更高版本".to_owned(),
        "许可全文： https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html".to_owned(),
        format!(
            "源码：     https://ffmpeg.org/releases/ffmpeg-{}.tar.xz",
            args.version
        ),
        format!("SHA-256：  {FFMPEG_SHA256}"),
        String::new(),
        "本程序捆绑的 ffmpeg 可执行文件由上述源码按如下配置编译得到，".to_owned(),
        "使用者可据此自行重建并替换它：".to_owned(),
        String::new(),
        format!("    ./configure \\\n    {configure}"),
        String::new(),
        "该构建未启用 x264 / x265 等 GPL 组件。".to_owned(),
        String::new(),
        "OpenH264".to_owned(),
        "--------".to_owned(),
        format!("版本：     {OPENH264_VERSION}"),
        "许可：     BSD 2-Clause".to_owned(),
        format!("源码：     https://github.com/cisco/openh264/tree/v{OPENH264_VERSION}"),
        String::new(),
        "它为 FFmpeg 提供 H.264 软件编码，用以替代 GPL 许可的 x264。".to_owned(),
        String::new(),
    ]
    .join("\n");

    let path = out.join("THIRD_PARTY_NOTICES.txt");
    std::fs::write(&path, text).with_context(|| format!("写入 {}", path.display()))?;
    println!("许可声明：{}", path.display());
    Ok(())
}

/// 自检最低可接受的 PSNR（dB）。
///
/// RGB→YUV420p→RGB 往返本身有损（色度抽样），完整功能的 ffmpeg 实测约 38 dB。
/// 配置出问题时会掉到个位数，30 这条线能干净地把两者分开。
const SELF_CHECK_MIN_PSNR: f64 = 30.0;

/// 构建后自检：验证色彩转换确实正确。
///
/// **体积对了不代表画面对。** 裁剪配置漏组件、或踩到 ffmpeg 某条少用的代码路径时，
/// 最典型的症状就是 swscale 的 RGB↔YUV 转换出错 —— 二进制跑得起来、编码不报错、
/// 输出文件大小也正常，只有画面是烂的。这个检查用一张已知图案跑一次往返来兜住它。
fn self_check(ffmpeg: &Path, work: &Path) -> anyhow::Result<()> {
    println!("自检：色彩转换往返…");
    let src = work.join("selfcheck_src.png");
    let dst = work.join("selfcheck_out.png");
    let reference = make_test_image();
    reference
        .save(&src)
        .with_context(|| format!("写入自检素材 {}", src.display()))?;

    let _ = std::fs::remove_file(&dst);
    run_command(
        Command::new(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&src)
            // 一来一回都经过 swscale，正是出问题的那条路径。
            .args(["-vf", "format=yuv420p,format=rgb24", "-frames:v", "1"])
            .arg(&dst),
    )
    .context("自检时 ffmpeg 执行失败")?;

    let actual = image::open(&dst)
        .with_context(|| format!("读取自检结果 {}", dst.display()))?
        .to_rgb8();
    if actual.dimensions() != reference.dimensions() {
        bail!(
            "自检输出尺寸不符：期望 {:?}，实际 {:?}",
            reference.dimensions(),
            actual.dimensions()
        );
    }

    let psnr = psnr(&reference, &actual);
    println!("  PSNR {psnr:.1} dB");
    if psnr < SELF_CHECK_MIN_PSNR {
        bail!(
            "色彩转换自检未通过（PSNR {psnr:.1} dB < {SELF_CHECK_MIN_PSNR} dB）。\n             常见原因：构建机缺少 nasm/yasm 而使用了 --no-asm。\n             这种二进制能跑、不报错，但输出画面是坏的，不能拿去分发。"
        );
    }

    check_encoder_probe(ffmpeg)?;
    Ok(())
}

/// 验证编码器探测路径可用。
///
/// 直接调用产品侧的 `probe_encoder_args`，让"自检通过"等价于"产品探测得到编码器"。
/// 裁剪配置漏组件时（例如漏掉 rawvideo **解码器**，只加了同名的解复用器），
/// 体积和色彩转换都正常，产品却会在运行期报"没有任何候选编码器可用"。
fn check_encoder_probe(ffmpeg: &Path) -> anyhow::Result<()> {
    use imprint_core::video::{PROBE_FRAME_BYTES, probe_encoder_args};

    println!("自检：编码器探测…");
    let mut child = Command::new(ffmpeg)
        .args(probe_encoder_args("libopenh264"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("启动编码器探测失败")?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write as _;
        let _ = stdin.write_all(&vec![0u8; PROBE_FRAME_BYTES]);
    }
    let output = child.wait_with_output().context("等待编码器探测失败")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(4).collect();
        bail!(
            "编码器探测自检未通过：产品会报告「没有任何候选编码器可用」。\n\
             多半是裁剪配置漏了组件。ffmpeg 输出：\n  {}",
            tail.into_iter().rev().collect::<Vec<_>>().join("\n  ")
        );
    }
    println!("  libopenh264 可用");
    Ok(())
}

/// 造一张含渐变与高频细节的测试图 —— 平滑区暴露色带，锐利边缘暴露色度错位。
fn make_test_image() -> image::RgbImage {
    image::RgbImage::from_fn(256, 256, |x, y| {
        if (x / 8 + y / 8) % 2 == 0 {
            image::Rgb([x as u8, y as u8, 255u8.saturating_sub(x as u8)])
        } else {
            image::Rgb([255u8.saturating_sub(y as u8), 64, x as u8])
        }
    })
}

fn psnr(a: &image::RgbImage, b: &image::RgbImage) -> f64 {
    let mut sum = 0f64;
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        for i in 0..3 {
            let d = f64::from(pa.0[i]) - f64::from(pb.0[i]);
            sum += d * d;
        }
    }
    let mse = sum / (a.width() as f64 * a.height() as f64 * 3.0);
    if mse <= f64::EPSILON {
        return 99.0;
    }
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

/// 确认构建机上有汇编器。
///
/// 没有它 ffmpeg 只能走 C 代码路径，而那条路径在 x86-64 上会让 swscale 的
/// 色彩转换算错 —— 二进制照样能跑、编码也不报错，只有画面是坏的。
/// 与其事后靠自检发现，不如一开始就说清楚缺什么。
fn ensure_assembler() -> anyhow::Result<()> {
    for exe in ["nasm", "yasm"] {
        if Command::new(exe)
            .arg("-v")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return Ok(());
        }
    }
    bail!(
        "找不到 nasm 或 yasm，无法启用 x86 汇编优化。\n\
         请先安装（Debian/Ubuntu: apt install nasm；macOS: brew install nasm；\n\
         Windows/MSYS2: pacman -S nasm），或把自建的 nasm 加进 PATH。\n\
         不要用 --no-asm 绕过：那样产出的二进制画面是坏的。"
    );
}

/// 下载并解压源码，返回源码目录。
fn ensure_source(args: &BuildArgs, work: &Path) -> anyhow::Result<PathBuf> {
    let dir = work.join(format!("ffmpeg-{}", args.version));
    if args.incremental && dir.is_dir() {
        println!("复用已有源码 {}", dir.display());
        return Ok(dir);
    }

    let tarball = work.join(format!("ffmpeg-{}.tar.xz", args.version));
    if !tarball.is_file() {
        let url = format!("https://ffmpeg.org/releases/ffmpeg-{}.tar.xz", args.version);
        println!("下载 {url}");
        download(&url, &tarball)?;
    }

    if args.skip_checksum {
        eprintln!("警告：已跳过源码包校验");
    } else {
        verify_checksum(&tarball, args)?;
    }

    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    println!("解压 {}", tarball.display());
    // 用系统 tar 而不是 Rust 解压库：构建 ffmpeg 本来就要求有 make/gcc 的环境，
    // tar 在 Linux/macOS/MSYS2 上都是既有工具，没必要再引一层依赖。
    run_command(
        Command::new("tar")
            .arg("-xf")
            .arg(&tarball)
            .current_dir(work),
    )?;

    if !dir.is_dir() {
        bail!("解压后没有找到 {}", dir.display());
    }
    Ok(dir)
}

fn download(url: &str, dst: &Path) -> anyhow::Result<()> {
    let mut resp = ureq::get(url).call().context("下载失败")?;
    let mut body = resp.body_mut().as_reader();
    let tmp = dst.with_extension("part");
    let mut file = std::fs::File::create(&tmp)?;
    std::io::copy(&mut body, &mut file)?;
    // 先写临时文件再改名：中途失败不会留下一个看起来完整的坏包。
    std::fs::rename(&tmp, dst)?;
    Ok(())
}

fn verify_checksum(tarball: &Path, args: &BuildArgs) -> anyhow::Result<()> {
    if args.version != FFMPEG_VERSION {
        bail!(
            "版本 {} 没有内置哈希。请先用 --skip-checksum 试验，确认无误后把哈希写进 FFMPEG_SHA256",
            args.version
        );
    }
    let bytes = std::fs::read(tarball)?;
    // sha2 0.11 的摘要类型不再实现 LowerHex，自己转十六进制。
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if digest != FFMPEG_SHA256 {
        // 留下坏包会让下次构建直接复用它，必须删掉。
        let _ = std::fs::remove_file(tarball);
        bail!("源码包哈希不匹配\n  期望 {FFMPEG_SHA256}\n  实际 {digest}");
    }
    println!("源码包校验通过");
    Ok(())
}

/// 编译 openh264 静态库，返回它的 pkgconfig 目录。
fn build_openh264(args: &BuildArgs, work: &Path) -> anyhow::Result<PathBuf> {
    let prefix = work.join("openh264-prefix");
    let pkgconfig = prefix.join("lib/pkgconfig");
    if args.incremental && pkgconfig.join("openh264.pc").is_file() {
        println!("复用已编译的 openh264");
        return Ok(pkgconfig);
    }

    let src = work.join(format!("openh264-{OPENH264_VERSION}"));
    if !src.is_dir() {
        // 用 git 按 tag 取源码，而不是下载 GitHub 自动生成的 tarball ——
        // 那种 tarball 的字节内容并不保证长期稳定，钉死哈希反而会无故失败。
        println!("拉取 openh264 v{OPENH264_VERSION}");
        run_command(
            Command::new("git")
                .args(["clone", "--depth", "1", "--branch"])
                .arg(format!("v{OPENH264_VERSION}"))
                .arg("https://github.com/cisco/openh264.git")
                .arg(&src),
        )?;
    }

    let jobs = jobs(args);
    println!("编译 openh264（-j{jobs}）…");
    let mut make_args: Vec<String> = vec![format!("-j{jobs}")];
    if args.no_asm {
        make_args.push("USE_ASM=No".into());
    }
    run_command(Command::new("make").args(&make_args).current_dir(&src))?;

    let mut install_args: Vec<String> = vec![
        format!("PREFIX={}", shell_path(&prefix)),
        "install-static".into(),
    ];
    if args.no_asm {
        install_args.push("USE_ASM=No".into());
    }
    run_command(Command::new("make").args(&install_args).current_dir(&src))?;

    if !pkgconfig.join("openh264.pc").is_file() {
        bail!("openh264 安装后没有生成 pkgconfig 文件");
    }
    Ok(pkgconfig)
}

fn configure(args: &BuildArgs, src: &Path, openh264_pkgconfig: &Path) -> anyhow::Result<()> {
    println!("配置 ffmpeg…");
    let mut cmd = shell_script("./configure");
    cmd.current_dir(src)
        // configure 靠 PKG_CONFIG_PATH 找到刚编好的 openh264。
        .env("PKG_CONFIG_PATH", openh264_pkgconfig)
        .args(configure_args(args));
    run_command(&mut cmd)
}

fn make(args: &BuildArgs, src: &Path) -> anyhow::Result<()> {
    let jobs = jobs(args);
    println!("编译 ffmpeg（-j{jobs}）…");
    run_command(
        Command::new("make")
            .arg(format!("-j{jobs}"))
            .current_dir(src),
    )
}

fn jobs(args: &BuildArgs) -> usize {
    args.jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()))
}

/// 裁剪配置。
///
/// 每一组都写明理由：没有理由的条目迟早会被人复制到别处，或者不敢删。
fn configure_args(args: &BuildArgs) -> Vec<String> {
    let mut a: Vec<String> = vec![
        // 刻意不传 --prefix：我们只从源码树里取 ffmpeg 二进制，从不 make install，
        // 传了反而要在 Windows 上处理路径格式转换。
        // 从零开始加回，而不是从默认集合里删 —— 默认会带上百个用不到的组件。
        "--disable-everything".into(),
        // 禁止 configure 自动启用它在系统上找到的库，否则构建结果会随构建机环境漂移。
        "--disable-autodetect".into(),
        "--disable-doc".into(),
        "--disable-htmlpages".into(),
        "--disable-manpages".into(),
        "--disable-podpages".into(),
        "--disable-txtpages".into(),
        // 只需要 ffmpeg 一个可执行文件：探测视频信息走 `ffmpeg -i`，不依赖 ffprobe。
        "--disable-ffplay".into(),
        "--disable-ffprobe".into(),
        // 水印只处理本地文件，网络协议是纯粹的攻击面和体积负担。
        "--disable-network".into(),
        "--disable-debug".into(),
        "--disable-shared".into(),
        "--enable-static".into(),
        // 按体积优化（-Os）而非速度。
        "--enable-small".into(),
        // PNG 解码要它：overlay 图层是 Rust 侧生成的 PNG。
        "--enable-zlib".into(),
        // H.264 软件编码。刻意不用 x264（GPL），见本模块开头的说明。
        "--enable-libopenh264".into(),
    ];

    if args.no_asm {
        a.push("--disable-x86asm".into());
    }

    // 输入容器。覆盖常见的相机与手机录制格式。
    a.push(concat_enable(
        "--enable-demuxer",
        &[
            "mov", "matroska", "avi", "flv", "mpegts", "mp3", "wav", "image2",
            // 编码器可用性探测要从 stdin 喂一帧原始数据。
            // 刻意不用 lavfi：那属于 avdevice，为探测把整个 avdevice 加回来不划算。
            "rawvideo",
        ],
    ));
    a.push(concat_enable(
        "--enable-muxer",
        &["mp4", "matroska", "mp3", "wav", "image2", "null"],
    ));

    a.push(concat_enable(
        "--enable-decoder",
        &[
            "h264",
            "hevc",
            "vp8",
            "vp9",
            "av1",
            "mpeg4",
            "mjpeg",
            "png",
            "aac",
            "mp3",
            "ac3",
            "eac3",
            "opus",
            "vorbis",
            "flac",
            "pcm_s16le",
            "pcm_s16be",
            "pcm_f32le",
            // 编码器探测要从 stdin 读一帧原始数据。只给 demuxer 不够：
            // 没有对应的 decoder，ffmpeg 会报 "no decoder found for: rawvideo"。
            "rawvideo",
        ],
    ));
    // 音频默认直通（-c:a copy），aac 编码器只在用户要求重编码时用到。
    a.push(concat_enable(
        "--enable-encoder",
        &["libopenh264", "aac", "png", "mjpeg", "mpeg4"],
    ));
    a.push(concat_enable(
        "--enable-parser",
        &[
            "h264",
            "hevc",
            "vp8",
            "vp9",
            "av1",
            "aac",
            "mpeg4video",
            "png",
            "mjpeg",
            "opus",
            "vorbis",
            "flac",
            "ac3",
        ],
    ));
    // overlay 是水印的核心；scale/format 是它做像素格式协商时要用的。
    a.push(concat_enable(
        "--enable-filter",
        &[
            "overlay",
            "scale",
            "format",
            "null",
            "copy",
            "anull",
            "aformat",
            "aresample",
            "setpts",
            "asetpts",
        ],
    ));
    a.push(concat_enable("--enable-protocol", &["file", "pipe"]));
    // 没有这些 bsf，mp4 里的 h264/aac 流无法正确重新封装。
    a.push(concat_enable(
        "--enable-bsf",
        &[
            "h264_mp4toannexb",
            "hevc_mp4toannexb",
            "aac_adtstoasc",
            "extract_extradata",
            "null",
        ],
    ));
    a.push("--enable-swscale".into());
    a.push("--enable-swresample".into());

    // macOS 的硬件编码器由系统框架提供，不增加二进制体积，白拿的性能。
    if cfg!(target_os = "macos") {
        a.push("--enable-videotoolbox".into());
        a.push("--enable-encoder=h264_videotoolbox,hevc_videotoolbox".into());
        a.push("--enable-hwaccel=h264_videotoolbox,hevc_videotoolbox".into());
    }

    a
}

fn concat_enable(flag: &str, items: &[&str]) -> String {
    format!("{flag}={}", items.join(","))
}

/// 执行源码树里的 shell 脚本（如 `./configure`）。
///
/// Windows 的 `CreateProcess` 不认 shebang，必须显式交给 bash —— CI 上由 MSYS2 提供。
fn shell_script(script: &str) -> Command {
    if cfg!(windows) {
        let mut cmd = Command::new("bash");
        cmd.arg(script);
        cmd
    } else {
        Command::new(script)
    }
}

/// 转成 MSYS2 的 bash / GNU make 都能理解的路径写法。
///
/// Windows 上光把反斜杠换成正斜杠还不够：`PREFIX=C:/foo` 交给 GNU make 时，
/// 冒号会被当成目标与依赖的分隔符，必须写成 MSYS2 的 `/c/foo` 形式。
fn shell_path(path: &Path) -> String {
    let raw = path.display().to_string();
    if !cfg!(windows) {
        return raw;
    }
    to_msys_path(&raw)
}

/// `C:\a\b` / `C:/a/b` → `/c/a/b`；其余原样返回（只换分隔符）。
fn to_msys_path(raw: &str) -> String {
    let slashed = raw.replace('\\', "/");
    let bytes = slashed.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2] == b'/'
    {
        return format!(
            "/{}/{}",
            (bytes[0] as char).to_ascii_lowercase(),
            &slashed[3..]
        );
    }
    slashed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msys_path_converts_drive_letters() {
        assert_eq!(to_msys_path(r"C:\a\b"), "/c/a/b");
        assert_eq!(to_msys_path("D:/x/y"), "/d/x/y");
        // 已经是 Unix 风格或相对路径的，只做分隔符替换。
        assert_eq!(to_msys_path("/usr/local"), "/usr/local");
        assert_eq!(to_msys_path("relative/path"), "relative/path");
        assert_eq!(to_msys_path(r"relative\path"), "relative/path");
        // 不是盘符的冒号不该被误伤。
        assert_eq!(to_msys_path("ab:/x"), "ab:/x");
    }
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

fn run_command(cmd: &mut Command) -> anyhow::Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("无法执行 {:?}", cmd.get_program()))?;
    if !status.success() {
        bail!("{:?} 失败（退出码 {:?}）", cmd.get_program(), status.code());
    }
    Ok(())
}

/// 把编译好的 ffmpeg 放到 cargo 的输出目录旁。
///
/// 产品侧的 `SidecarPipeline::discover` 会查 `可执行文件同目录 → PATH`，
/// 所以只要 ffmpeg 躺在 `target/debug/` 或 `target/release/` 里，
/// `cargo run` 出来的程序就能直接用上内置版本，不必额外配置。
#[derive(Debug, Args)]
pub struct BundleArgs {
    /// ffmpeg 二进制所在目录
    #[arg(long, default_value = "dist")]
    pub from: PathBuf,

    /// 目标目录；不指定则同时放到 target/debug 与 target/release
    #[arg(long)]
    pub to: Vec<PathBuf>,
}

/// 只生成许可声明，不编译 ffmpeg。
///
/// CI 上 ffmpeg 常从缓存恢复、跳过构建步骤，声明文件仍必须随包产出。
pub fn notices_only(args: &BuildArgs) -> anyhow::Result<()> {
    std::fs::create_dir_all(&args.out)?;
    let out = std::path::absolute(&args.out)
        .with_context(|| format!("解析输出目录 {}", args.out.display()))?;
    write_notices(args, &out)
}

pub fn bundle(args: &BundleArgs) -> anyhow::Result<()> {
    let src = args.from.join(exe_name());
    if !src.is_file() {
        bail!("没找到 {}，先跑 `cargo xtask build-ffmpeg`", src.display());
    }

    let targets: Vec<PathBuf> = if args.to.is_empty() {
        vec![
            PathBuf::from("target/debug"),
            PathBuf::from("target/release"),
        ]
    } else {
        args.to.clone()
    };

    let mut copied = 0;
    for dir in targets {
        // 只往已存在的目录放：没构建过 release 就不必凭空造一个目录。
        if !dir.is_dir() {
            continue;
        }
        let dst = dir.join(exe_name());
        std::fs::copy(&src, &dst).with_context(|| format!("复制到 {}", dst.display()))?;
        println!("已放置 {}", dst.display());
        copied += 1;
    }

    if copied == 0 {
        bail!("没有可用的目标目录，请先构建一次或用 --to 指定");
    }
    Ok(())
}
