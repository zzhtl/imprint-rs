//! 生成几种水印效果的样例图，用于肉眼验证渲染结果。
//!
//! ```sh
//! cargo run -p imprint-core --example demo -- /tmp/out
//! ```

use std::{path::PathBuf, sync::Arc};

use imprint_core::{
    FieldContext, FontLibrary, ImageOptions, OutputFormat, Renderer,
    spec::{
        Anchor9, Content, Margin, Placement, Rgba8, SizeMode, TextAlign, TextSpec, WatermarkSpec,
    },
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/demo".into())
        .into();
    std::fs::create_dir_all(&out_dir)?;

    // 造一张渐变底图，方便看清半透明水印的混色效果。
    let base = image::RgbaImage::from_fn(1280, 720, |x, y| {
        let t = x as f32 / 1280.0;
        let v = y as f32 / 720.0;
        image::Rgba([
            (40.0 + 180.0 * t) as u8,
            (60.0 + 120.0 * v) as u8,
            (140.0 - 100.0 * t * v) as u8,
            255,
        ])
    });
    let base_path = out_dir.join("_base.png");
    base.save(&base_path)?;

    let mut renderer = Renderer::new(Arc::new(FontLibrary::with_system_fonts()));
    let opts = ImageOptions {
        format: OutputFormat::Png,
        ..ImageOptions::default()
    };

    let text = |template: &str, color: Rgba8| {
        Content::Text(TextSpec {
            template: template.to_owned(),
            color,
            align: TextAlign::Left,
            ..TextSpec::default()
        })
    };

    let cases: Vec<(&str, WatermarkSpec)> = vec![
        (
            "01_corner_cn",
            WatermarkSpec {
                content: text("青藤云安全 · 内部资料", Rgba8::WHITE),
                size: SizeMode::RelativeFontSize(0.05),
                placement: Placement::Anchor {
                    anchor: Anchor9::BottomRight,
                    margin: Margin::default(),
                },
                opacity: 0.85,
                rotation_deg: 0.0,
            },
        ),
        (
            "02_dynamic_fields",
            WatermarkSpec {
                content: text("{filename} · {date} · {width}x{height}", Rgba8::WHITE),
                size: SizeMode::RelativeWidth(0.6),
                placement: Placement::Anchor {
                    anchor: Anchor9::BottomCenter,
                    margin: Margin::default(),
                },
                opacity: 0.9,
                rotation_deg: 0.0,
            },
        ),
        (
            "03_tiled_rotated",
            WatermarkSpec {
                content: text("机密 CONFIDENTIAL", Rgba8::WHITE),
                size: SizeMode::RelativeFontSize(0.035),
                placement: Placement::Tile {
                    spacing_ratio: (0.6, 1.6),
                    stagger: true,
                },
                opacity: 0.22,
                rotation_deg: -30.0,
            },
        ),
        (
            "04_center_emoji",
            WatermarkSpec {
                content: text("样例 Sample 🎨", Rgba8::new(255, 230, 80, 255)),
                size: SizeMode::RelativeWidth(0.5),
                placement: Placement::Normalized {
                    x: 0.5,
                    y: 0.5,
                    anchor: Anchor9::Center,
                },
                opacity: 0.95,
                rotation_deg: -8.0,
            },
        ),
    ];

    for (name, spec) in cases {
        let dst = out_dir.join(format!("{name}.png"));
        let ctx = FieldContext::new().with_file_name("holiday_photo.jpg");
        let input = std::fs::File::open(&base_path)?;
        let output = std::io::BufWriter::new(std::fs::File::create(&dst)?);
        renderer.watermark_image(input, output, &spec, &opts, &ctx)?;
        println!("{}", dst.display());
    }

    Ok(())
}
