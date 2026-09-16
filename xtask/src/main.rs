//! imprint 的构建辅助工具。
//!
//! ```sh
//! cargo xtask build-ffmpeg            # 编译裁剪版 ffmpeg 到 dist/
//! cargo xtask build-ffmpeg --no-asm   # 构建机没有 nasm/yasm 时
//! cargo xtask bundle                  # 放到 target/ 下供开发构建使用
//! cargo xtask notices                 # 只生成第三方许可声明
//! ```

mod ffmpeg;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "imprint 构建辅助工具")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 编译裁剪版 ffmpeg（LGPL v2.1，可随闭源软件分发）
    BuildFfmpeg(ffmpeg::BuildArgs),
    /// 把编译好的 ffmpeg 放到 target/ 下，让开发构建直接可用
    Bundle(ffmpeg::BundleArgs),
    /// 只生成第三方许可声明（ffmpeg 来自缓存、跳过构建时用）
    Notices(ffmpeg::BuildArgs),
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::BuildFfmpeg(args) => ffmpeg::run(&args),
        Command::Bundle(args) => ffmpeg::bundle(&args),
        Command::Notices(args) => ffmpeg::notices_only(&args),
    }
}
