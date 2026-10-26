use std::{
    io::{self, BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use image::{Rgba, RgbaImage};
use imprint_core::{
    FieldContext, FontLibrary, ImageOptions, Renderer, batch, blind, image_job, spec::*,
};

fn directory() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "imprint-regression-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn image() -> RgbaImage {
    RgbaImage::from_fn(320, 240, |x, y| {
        let v = (110.0 + 60.0 * (x as f32 / 37.0).sin() + 40.0 * (y as f32 / 23.0).cos())
            .clamp(0.0, 255.0) as u8;
        Rgba([v, v.saturating_add(18), v.saturating_sub(12), 255])
    })
}

fn renderer() -> Renderer {
    Renderer::new(Arc::new(FontLibrary::with_system_fonts()))
}

fn spec() -> WatermarkSpec {
    WatermarkSpec {
        content: Content::Text(TextSpec {
            template: "Review".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn in_place_write_succeeds_and_failed_write_keeps_original() {
    let dir = directory();
    let path = dir.join("source.png");
    image().save(&path).unwrap();
    let mut renderer = renderer();
    let options = ImageOptions {
        format: image_job::OutputFormat::Png,
        ..Default::default()
    };
    renderer
        .watermark_image_file(&path, &path, &spec(), &options)
        .unwrap();
    assert_eq!(image::open(&path).unwrap().width(), 320);
    let before = std::fs::read(&path).unwrap();
    assert!(
        renderer
            .watermark_image_file(&path, &path, &WatermarkSpec::default(), &options)
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let failed: imprint_core::Result<()> = image_job::write_atomic(&path, |writer| {
        writer.write_all(b"partial output")?;
        Err(io::Error::other("simulated encoder failure").into())
    });
    assert!(failed.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn duplicate_outputs_are_rejected_before_any_write() {
    let dir = directory();
    let sources = [dir.join("a.png"), dir.join("b.png")];
    for src in &sources {
        image().save(src).unwrap();
    }
    let dst = dir.join("out.png");
    let items: Vec<_> = sources
        .iter()
        .map(|src| batch::BatchItem {
            src: src.clone(),
            dst: dst.clone(),
        })
        .collect();
    let outcomes = batch::run_image_batch(
        Arc::new(FontLibrary::with_system_fonts()),
        &items,
        &spec(),
        &Default::default(),
        &Default::default(),
        |_| {},
    );
    assert!(outcomes.iter().all(|o| o.result.is_err()));
    assert!(!dst.exists());
    let overwrite_other = [
        batch::BatchItem {
            src: sources[0].clone(),
            dst: sources[1].clone(),
        },
        batch::BatchItem {
            src: sources[1].clone(),
            dst,
        },
    ];
    assert!(batch::validate_output_paths(&overwrite_other).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn buffered_write_errors_are_returned() {
    struct Full;
    impl Write for Full {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("full"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let small = RgbaImage::from_pixel(8, 8, Rgba([0, 0, 0, 255]));
    assert!(
        image_job::encode(
            &small,
            BufWriter::new(Full),
            image_job::OutputFormat::Png,
            &Default::default()
        )
        .is_err()
    );
}

#[test]
fn every_blind_strength_round_trips_without_compression() {
    for image in [
        image(),
        RgbaImage::from_pixel(320, 240, Rgba([127, 127, 127, 255])),
    ] {
        for strength in [
            blind::Strength::SUBTLE,
            blind::Strength::DEFAULT,
            blind::Strength::ROBUST,
        ] {
            let marked = blind::embed(&image, b"user-42@example.com", strength).unwrap();
            assert_eq!(
                blind::extract(&marked).as_deref(),
                Some(b"user-42@example.com".as_slice()),
                "{strength:?}"
            );
        }
        let strength = blind::Strength::new(23.0).unwrap();
        let marked = blind::embed(&image, b"custom", strength).unwrap();
        assert_eq!(
            blind::extract_with_strength(&marked, strength).as_deref(),
            Some(b"custom".as_slice())
        );
    }
}

#[test]
fn stroke_and_shadow_change_pixels_and_include_their_bounds() {
    let mut renderer = renderer();
    let plain = spec();
    let base = renderer
        .render_layer(&plain, (800, 600), &FieldContext::new())
        .unwrap();
    for stroke in [false, true] {
        let mut styled = plain.clone();
        if let Content::Text(t) = &mut styled.content {
            if stroke {
                t.stroke = Some(StrokeSpec {
                    width_ratio: 0.2,
                    color: Rgba8::new(255, 0, 0, 255),
                });
            } else {
                t.shadow = Some(ShadowSpec {
                    offset_ratio: (-0.4, 0.4),
                    color: Rgba8::new(0, 0, 255, 255),
                });
            }
        }
        let result = renderer
            .render_layer(&styled, (800, 600), &FieldContext::new())
            .unwrap();
        assert!(result.width() > base.width() && result.height() > base.height());
        assert!(result.pixels().iter().any(|p| p.red() != p.blue()));
    }
}

#[test]
fn cached_exports_follow_dynamic_text_size_and_placement() {
    let decoded = image_job::DecodedImage {
        image: image(),
        metadata: Default::default(),
    };
    let mut reused = renderer();
    let options = ImageOptions {
        format: image_job::OutputFormat::Png,
        ..Default::default()
    };
    let mut spec = spec();
    if let Content::Text(text) = &mut spec.content {
        text.template = "{filename}".into();
    }
    for (i, name) in ["first.png", "second.png", "second.png"].iter().enumerate() {
        if i == 2 {
            spec.size = SizeMode::RelativeFontSize(0.1);
            spec.placement = Placement::Normalized {
                x: 0.5,
                y: 0.5,
                anchor: Anchor9::Center,
            };
        }
        let fields = FieldContext::new().with_file_name(name);
        let mut actual = Vec::new();
        let mut expected = Vec::new();
        reused
            .watermark_decoded(&decoded, &mut actual, &spec, &options, &fields)
            .unwrap();
        renderer()
            .watermark_decoded(&decoded, &mut expected, &spec, &options, &fields)
            .unwrap();
        assert_eq!(actual, expected, "cached export differs for {name}");
    }
}
