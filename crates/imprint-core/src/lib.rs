//! `imprint-core` —— 图片 / 视频水印渲染核心。
//!
//! 纯 Rust，无 UI 依赖，桌面与移动端共用。设计上的两个支点：
//!
//! 1. **水印图层只渲染一次**，同时服务预览、图片合成、视频 overlay 三处，
//!    因此预览与输出是像素级一致的，批量与视频也不会重复渲染。
//! 2. **API 与 I/O 解耦**：接口收 `impl Read + Seek` / `&[u8]` 而非 `&Path`。
//!    Android 的文件选择返回 `content://` URI，根本没有文件系统路径可用。

pub mod batch;
pub mod compose;
pub mod convert;
pub mod error;
pub mod fields;
pub mod fonts;
pub mod image_job;
pub mod layer;
pub mod renderer;
pub mod spec;
pub mod text;
pub mod video;

// 公开 API 里出现了 tiny_skia 的类型（如 `render_layer` 返回 `Arc<Pixmap>`），
// 必须把它重导出，否则下游根本没法命名这些类型。
pub use tiny_skia;

pub use batch::{BatchItem, BatchProgress, CancelToken, ItemOutcome};
pub use error::{Error, Result};
pub use fields::{ExifFields, FieldContext};
pub use fonts::{FontBlob, FontLibrary};
pub use image_job::{Metadata, OutputFormat};
pub use renderer::{ImageOptions, Renderer};
pub use spec::WatermarkSpec;
