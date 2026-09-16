# imprint

给图片和视频批量添加水印。开箱即用，**无需另外安装 ffmpeg**。

- **imprint** —— 命令行工具，适合服务器、CI 和批处理脚本
- **imprint-ui** —— 桌面图形界面，实时预览、拖拽定位

## 下载

前往 [Releases](https://github.com/zzhtl/imprint-rs/releases) 下载对应平台的压缩包，解压即可运行。

| 平台 | 包 |
| --- | --- |
| Linux x86_64 | `imprint-vX.Y.Z-linux-x86_64.tar.gz` |
| Windows x86_64 | `imprint-vX.Y.Z-windows-x86_64.zip` |
| macOS Apple Silicon | `imprint-vX.Y.Z-macos-arm64.tar.gz` |

macOS 仅提供 Apple Silicon（M 系列）版本，不支持 Intel 机型。

包内含 `imprint`、`imprint-ui` 和一个裁剪版 `ffmpeg`（约 7.5 MB）。**三者必须放在同一目录**——程序按「可执行文件同目录 → PATH」的顺序查找 ffmpeg。

## 特性

- **图片与视频统一管线**：同一套水印参数，图片和视频输出完全一致
- **中文与 emoji**：自带字体回退，彩色 emoji 正常渲染
- **文字或图片水印**：文字支持描边、阴影；图片支持 PNG/JPEG/WebP/SVG
- **三种定位**：九宫格锚点、归一化坐标（可在 GUI 里拖拽）、平铺满屏（防泄密）
- **动态字段**：`{filename}`、`{date}`、`{exif.model}` 等按文件逐一求值
- **跨分辨率一致**：尺寸与位置默认用相对量表达，同一套参数在 4K 和 720p 上观感一致
- **批量并行**：多核并行处理，失败项不中断整批
- **元数据保留**：EXIF / ICC 原样搬运，自动处理手机照片的方向标记

## 快速开始

给一张图片加水印：

```bash
imprint image -i photo.jpg -o photo_wm.jpg --text "© 2026 我的工作室"
```

整个目录批量加平铺水印（防泄密场景）：

```bash
imprint image -i ./photos -o ./out --recursive \
  --text "机密 {filename} · {date}" \
  --tile --stagger --opacity 0.25 --rotate -30 --size 0.035
```

给视频加水印：

```bash
imprint video -i clip.mp4 -o clip_wm.mp4 --text "水印 {date}" --position br
```

检查环境（ffmpeg 位置、可用编码器、中文字体）：

```bash
imprint probe -i clip.mp4
```

## 命令

### `imprint image`

给图片加水印，支持目录批量。

| 参数 | 说明 |
| --- | --- |
| `-i, --input <PATH>...` | 输入文件或目录，可给多个 |
| `-o, --output <PATH>` | 输出文件（单个输入时）或输出目录 |
| `-r, --recursive` | 递归遍历子目录 |
| `--format <FORMAT>` | `jpeg` / `png` / `webp`，默认 `jpeg` |
| `--quality <N>` | JPEG 质量 1~100，默认 92 |
| `--suffix <STR>` | 输出文件名后缀，如 `_wm` |
| `-j, --jobs <N>` | 并行任务数，默认取 CPU 核心数 |
| `--strip-metadata` | 不保留源图的 EXIF / ICC |
| `--dry-run` | 只打印将要处理的文件，不实际写出 |

支持的输入格式：JPEG、PNG、WebP、TIFF、BMP、GIF。**不支持 HEIC**（iPhone 原生格式），需先转换。

### `imprint video`

给视频加水印。

| 参数 | 说明 |
| --- | --- |
| `-i, --input <PATH>...` | 输入视频文件，可给多个 |
| `-o, --output <PATH>` | 输出文件（单个输入时）或输出目录 |
| `--ffmpeg <PATH>` | 指定 ffmpeg 路径，默认按「程序同目录 → PATH」查找 |
| `--encoder <CODEC>` | 视频编码器；留空则试编一帧自动挑选可用的硬件编码器 |
| `--crf <N>` | 质量参数（软件编码器用），数值越小质量越高 |
| `--encoder-preset <NAME>` | x264/x265 的 preset，如 `fast` / `medium` / `slow` |
| `--reencode-audio` | 重新编码音频，默认直通不重编码 |
| `--suffix <STR>` | 输出文件名后缀，默认 `_wm` |

编码器按「硬件 → 软件」的顺序自动探测：macOS 优先 `h264_videotoolbox`，Windows 依次尝试 NVENC / QSV / AMF，最后回落到软件编码器。探测方式是实际试编一帧——`ffmpeg -encoders` 列出的只是编译进去的编码器，不代表当前机器真能用。

### `imprint probe`

查看素材信息与运行环境。省略 `-i` 时只检查 ffmpeg 和字体。

## 水印参数

以下参数 `image` 和 `video` 通用。

### 内容

| 参数 | 说明 |
| --- | --- |
| `--text <TEXT>` | 文字水印，支持模板字段 |
| `--logo <FILE>` | 图片水印（PNG / JPEG / WebP / SVG） |
| `--color <HEX>` | 文字颜色，`#RRGGBB` 或 `#RRGGBBAA`，默认 `#FFFFFF` |
| `--font <NAME>` | 字体族名，留空用系统默认无衬线字体 |
| `--weight <N>` | 字重 100~900，默认 400 |

### 尺寸

`--size` 支持四种写法：

| 写法 | 含义 |
| --- | --- |
| `0.05` | 字号 = 画布**高度**的 5%（默认方式） |
| `48px` | 绝对字号 48 像素 |
| `w:0.3` | 水印宽度 = 画布**宽度**的 30% |
| `w:400px` | 绝对宽度 400 像素 |

推荐用相对写法：同一套参数在不同分辨率的素材上观感一致，批量处理混合分辨率时尤其重要。

### 位置

`--position` 接受九宫格缩写或归一化坐标：

```
tl  tc  tr      左上  中上  右上
cl  c   cr      左中  居中  右中
bl  bc  br      左下  中下  右下
```

也可以写成 `0.5,0.7` 这样的归一化坐标（指水印**中心**的位置，与 GUI 拖拽的语义一致）。

`--margin <X,Y>` 设置边距占画布的比例，默认 `0.02,0.02`。

平铺：

| 参数 | 说明 |
| --- | --- |
| `--tile` | 平铺满屏 |
| `--tile-spacing <X,Y>` | 间距，相对水印自身尺寸的倍数，默认 `0.6,1.2` |
| `--stagger` | 奇数行错开半格，避免形成明显的竖直通道 |

### 样式

| 参数 | 说明 |
| --- | --- |
| `--opacity <N>` | 不透明度 0.0~1.0，默认 0.6 |
| `--rotate <DEG>` | 旋转角度，可为负 |

## 模板字段

`--text` 里可以嵌入以下字段，导出时按每个文件的实际值替换：

| 字段 | 含义 |
| --- | --- |
| `{filename}` | 文件名（不含扩展名） |
| `{filename_ext}` | 文件名（含扩展名） |
| `{width}` `{height}` | 素材像素尺寸 |
| `{date}` `{time}` `{datetime}` | 导出时的本地日期 / 时间 |
| `{exif.datetime}` | 拍摄时间 |
| `{exif.make}` `{exif.model}` | 相机厂商 / 型号 |
| `{exif.lens}` | 镜头 |
| `{exif.iso}` `{exif.fnumber}` | ISO / 光圈 |
| `{exif.exposure}` `{exif.focal}` | 快门 / 焦距 |

字面大括号用 `{{` 和 `}}` 转义。写错的字段名会原样保留（如 `{unknow}`），便于发现拼写错误；字段存在但该文件没有对应数据时替换为空。

示例：

```bash
imprint image -i ./raw -o ./out \
  --text "{exif.model} · {exif.focal} {exif.fnumber} · {exif.datetime}"
```

## 预设

GUI 里调好的参数可以存成 JSON 预设，命令行直接复用：

```bash
imprint image -i ./photos -o ./out --preset my-style.json
```

命令行显式给出的参数会覆盖预设中的对应项，其余沿用预设。

## 退出码

| 码 | 含义 |
| --- | --- |
| 0 | 全部成功 |
| 1 | 部分失败（失败项会打印在 stderr） |
| 2 | 全部失败，或参数错误 |

## 从源码构建

需要 Rust stable 工具链。

```bash
cargo build --release --workspace
```

如果要用视频功能，还需要构建内置的 ffmpeg。这一步需要 C 工具链和汇编器：

| 平台 | 依赖 |
| --- | --- |
| Debian / Ubuntu | `apt install nasm build-essential` |
| macOS | `brew install nasm`（Xcode Command Line Tools） |
| Windows | MSYS2（MINGW64），`pacman -S base-devel mingw-w64-x86_64-gcc nasm make` |

```bash
cargo xtask build-ffmpeg   # 下载源码 → 校验哈希 → 编译 → 自检，产物在 dist/
cargo xtask bundle         # 放到 target/ 下，供开发构建直接使用
```

**不要用 `--no-asm` 绕过缺少 nasm 的问题**：关掉汇编后 swscale 的色彩转换结果是错的，产出的二进制能跑、编码不报错，但画面颜色错乱。构建后的自检会拦住这种二进制。

Linux 上构建 `imprint-ui` 还需要图形相关的开发库：

```bash
sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libxrandr-dev libxi-dev libxcursor-dev libxcb-render0-dev \
  libxcb-shape0-dev libxcb-xfixes0-dev libxcb1-dev libgl1-mesa-dev libegl1-mesa-dev
```

## 项目结构

```
crates/imprint-core   水印渲染核心。纯 Rust，不依赖任何 UI，桌面与移动端共用
crates/imprint-cli    命令行工具
crates/imprint-ui     桌面图形界面（egui / eframe）
xtask                 构建辅助工具：编译裁剪版 ffmpeg、打包
```

`imprint-core` 的接口收 `impl Read + Seek` 而非文件路径，因为移动端的文件选择返回的是 `content://` URI 而不是文件系统路径。它在全部 feature 下**零 C 构建依赖**，CI 中有强制校验。

视频后端抽象为 `VideoPipeline` trait，当前实现通过子进程驱动外部 ffmpeg。水印被渲染成与视频等尺寸的透明 PNG 整幅 overlay 上去，因此平铺、旋转、锚点、透明度全部复用与图片相同的代码，两条管线的输出天然一致。

## 许可

本项目的代码以 MIT 或 Apache-2.0 双许可发布。

发布包内附带的 ffmpeg 为 **LGPL v2.1**，未启用 x264 / x265 等 GPL 组件，H.264 软件编码由 BSD 许可的 [OpenH264](https://github.com/cisco/openh264) 提供。详细的版本、源码获取方式与完整构建配置见包内的 `THIRD_PARTY_NOTICES.txt`。
