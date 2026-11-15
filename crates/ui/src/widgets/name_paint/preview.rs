use super::*;
use crust_core::model::{SenderNamePaintShadow, SenderNamePaintStop};
use std::time::Instant;

fn stop(at: f32, color: &str) -> SenderNamePaintStop {
    SenderNamePaintStop {
        at,
        color: color.to_owned(),
    }
}

fn gradient(function: &str, angle: f32, repeat: bool) -> SenderNamePaint {
    SenderNamePaint {
        function: function.to_owned(),
        angle: Some(angle),
        repeat,
        stops: vec![
            stop(0.0, "#ff8565"),
            stop(0.45, "#ffde79"),
            stop(1.0, "#8d9eff"),
        ],
        ..Default::default()
    }
}

fn shadow(x: f32, y: f32, radius: f32, color: &str) -> SenderNamePaintShadow {
    SenderNamePaintShadow {
        x_offset: x,
        y_offset: y,
        radius,
        color: color.to_owned(),
    }
}

fn image_pattern(transparency: bool) -> ColorImage {
    let mut pixels = Vec::new();
    for y in 0..16 {
        for x in 0..32 {
            let color = match (x / 3 + y / 3) % 4 {
                0 => [255, 100, 150],
                1 => [75, 220, 235],
                2 => [255, 215, 80],
                _ => [135, 115, 255],
            };
            let alpha = if transparency {
                [0, 85, 170, 255][(x / 4) % 4]
            } else {
                255
            };
            pixels.push(Color32::from_rgba_unmultiplied(
                color[0], color[1], color[2], alpha,
            ));
        }
    }
    ColorImage {
        size: [32, 16],
        pixels,
    }
}

fn raster(
    ctx: &Context,
    text: &str,
    size: f32,
    paint: &SenderNamePaint,
    settings: PaintOptions,
    base: Color32,
    colon: Color32,
    source: Option<&ColorImage>,
) -> ColorImage {
    let galley = ctx.fonts(|fonts| {
        fonts.layout_no_wrap(
            text.to_owned(),
            egui::FontId::proportional(size),
            Color32::WHITE,
        )
    });
    let mask =
        name_mask(ctx, &galley, paint, settings).expect("preview text must fit the paint budget");
    let shadow = shadow_layer(&mask, paint, settings);
    render_name(&mask, &shadow, paint, base, colon, source)
}

fn stamp(canvas: &mut image::RgbaImage, image: &ColorImage, x: i64, y: i64) {
    let rgba = image::RgbaImage::from_fn(image.size[0] as u32, image.size[1] as u32, |x, y| {
        image::Rgba(image.pixels[y as usize * image.size[0] + x as usize].to_srgba_unmultiplied())
    });
    image::imageops::overlay(canvas, &rgba, x, y);
}

fn label(
    ctx: &Context,
    canvas: &mut image::RgbaImage,
    text: &str,
    x: i64,
    y: i64,
    size: f32,
    color: Color32,
) {
    let plain = SenderNamePaint::default();
    let image = raster(
        ctx,
        text,
        size,
        &plain,
        PaintOptions::default(),
        color,
        color,
        None,
    );
    stamp(canvas, &image, x, y);
}

#[test]
#[ignore = "writes a synthetic name-paint contact sheet to an explicit output path"]
fn write_name_paint_preview() {
    let path = std::path::PathBuf::from(
        std::env::var("CRUST_NAME_PAINT_PREVIEW_PATH")
            .expect("set CRUST_NAME_PAINT_PREVIEW_PATH to a PNG output path"),
    );
    let ctx = Context::default();
    let normal = PaintOptions {
        large_shadows: false,
        ..Default::default()
    };
    let mut transparent = gradient("linear-gradient", 90.0, false);
    transparent.stops = vec![
        stop(0.0, "#ff606000"),
        stop(0.5, "#ff606080"),
        stop(1.0, "#ff6060ff"),
    ];
    let mut repeating = gradient("linear-gradient", 90.0, true);
    repeating.stops = vec![
        stop(0.2, "#7ae4dc"),
        stop(0.35, "#e6c875"),
        stop(0.5, "#b29aff"),
    ];
    let mut radial_repeat = repeating.clone();
    radial_repeat.function = "radial-gradient".to_owned();
    let mut hard = gradient("linear-gradient", 90.0, false);
    hard.stops = vec![
        stop(0.0, "#ff9e89"),
        stop(0.5, "#ff9e89"),
        stop(0.5, "#7bd9f4"),
        stop(1.0, "#7bd9f4"),
    ];
    let image_paint = SenderNamePaint {
        function: "url".to_owned(),
        ..Default::default()
    };
    let mut glow = gradient("linear-gradient", 90.0, false);
    glow.shadows = vec![shadow(0.0, 1.0, 4.0, "#a78bffb0")];
    let mut sequential = gradient("linear-gradient", 90.0, false);
    sequential.shadows = vec![
        shadow(-3.0, 0.0, 2.0, "#ff609cb0"),
        shadow(3.0, 2.0, 3.0, "#6ce6ffb0"),
    ];
    let mut shifted = gradient("linear-gradient", 90.0, false);
    shifted.shadows = vec![shadow(4.0, 4.0, 2.0, "#090b14d0")];

    let cases = vec![
        (
            "Linear, left to right",
            "Full name coordinates",
            gradient("linear-gradient", 90.0, false),
            normal,
            None,
            false,
        ),
        (
            "Linear, bottom to top",
            "Vertical color progression",
            gradient("linear-gradient", 0.0, false),
            normal,
            None,
            false,
        ),
        (
            "Linear, diagonal",
            "45 degree direction",
            gradient("linear-gradient", 45.0, false),
            normal,
            None,
            false,
        ),
        (
            "Transparent gradient",
            "Blue username below the paint",
            transparent,
            normal,
            None,
            false,
        ),
        (
            "Radial gradient",
            "Centered circular fill",
            gradient("radial-gradient", 0.0, false),
            normal,
            None,
            false,
        ),
        (
            "Repeating linear",
            "Stops from 20 to 50 percent",
            repeating,
            normal,
            None,
            false,
        ),
        (
            "Repeating radial",
            "Stops from 20 to 50 percent",
            radial_repeat,
            normal,
            None,
            false,
        ),
        (
            "Hard color stop",
            "Two colors meet at 50 percent",
            hard,
            normal,
            None,
            false,
        ),
        (
            "Image detail",
            "32 by 16 pixel checker source",
            image_paint.clone(),
            normal,
            Some(image_pattern(false)),
            false,
        ),
        (
            "Transparent image",
            "Four alpha levels over blue",
            image_paint.clone(),
            normal,
            Some(image_pattern(true)),
            false,
        ),
        (
            "Image unavailable",
            "Black username stays readable while loading",
            image_paint,
            normal,
            None,
            false,
        ),
        (
            "Regular shadow",
            "Radius 4, normal size",
            glow.clone(),
            normal,
            None,
            false,
        ),
        (
            "Large shadow",
            "Radius 4, enlarged size",
            glow.clone(),
            PaintOptions::default(),
            None,
            false,
        ),
        (
            "Sequential shadows",
            "Pink and blue offset layers",
            sequential,
            normal,
            None,
            false,
        ),
        (
            "Shadows disabled",
            "Same paint with shadows off",
            glow,
            PaintOptions {
                shadows: false,
                ..normal
            },
            None,
            false,
        ),
        (
            "Light background",
            "Offset shadow and neutral colon",
            shifted,
            normal,
            None,
            true,
        ),
    ];
    let columns = 3u32;
    let cell_width = 360u32;
    let cell_height = 174u32;
    let header = 76u32;
    let rows = (cases.len() as u32).div_ceil(columns);
    let mut canvas = image::RgbaImage::from_pixel(
        columns * cell_width,
        header + rows * cell_height + 12,
        image::Rgba([23, 25, 32, 255]),
    );
    let _ = ctx.run(Default::default(), |ctx| {
        label(
            ctx,
            &mut canvas,
            "7TV name paint rendering",
            22,
            14,
            24.0,
            Color32::from_rgb(235, 238, 247),
        );
        label(
            ctx,
            &mut canvas,
            "Synthetic examples. Each colon keeps the surrounding text color.",
            22,
            46,
            13.0,
            Color32::from_rgb(164, 174, 196),
        );
        for (index, (title, caption, paint, settings, source, light)) in cases.iter().enumerate() {
            let x = (index as u32 % columns) * cell_width;
            let y = header + (index as u32 / columns) * cell_height;
            let background = if *light {
                [236, 240, 248, 255]
            } else {
                [32, 36, 47, 255]
            };
            for row in y + 5..y + cell_height - 5 {
                for column in x + 6..x + cell_width - 6 {
                    canvas.put_pixel(column, row, image::Rgba(background));
                }
            }
            let title_color = if *light {
                Color32::from_rgb(38, 47, 67)
            } else {
                Color32::from_rgb(226, 231, 243)
            };
            let detail_color = if *light {
                Color32::from_rgb(79, 91, 113)
            } else {
                Color32::from_rgb(154, 168, 193)
            };
            label(
                ctx,
                &mut canvas,
                title,
                i64::from(x + 18),
                i64::from(y + 16),
                15.0,
                title_color,
            );
            label(
                ctx,
                &mut canvas,
                caption,
                i64::from(x + 18),
                i64::from(y + cell_height - 28),
                12.0,
                detail_color,
            );
            let name = raster(
                ctx,
                "RainbowName:",
                28.0,
                paint,
                *settings,
                if is_image_paint(paint) && source.is_none() {
                    Color32::BLACK
                } else {
                    Color32::from_rgb(94, 152, 255)
                },
                title_color,
                source.as_ref(),
            );
            let px = i64::from(x) + (i64::from(cell_width) - name.size[0] as i64) / 2;
            let py = i64::from(y + 86) - name.size[1] as i64 / 2;
            stamp(&mut canvas, &name, px, py);
        }
    });
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).unwrap();
    }
    canvas.save(&path).unwrap();
    eprintln!("Wrote synthetic paint contact sheet: {}", path.display());
}

fn draw_benchmark_names(ctx: &Context, images: &UiImageCache, paint: Option<&SenderNamePaint>) {
    let _ = ctx.run(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(640.0, 1200.0))),
            ..Default::default()
        },
        |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                for index in 0..40 {
                    show_sender_name(
                        ui,
                        &format!("Painter{}:", index % 8),
                        Color32::from_rgb(94, 152, 255),
                        paint,
                        images,
                        true,
                    );
                }
            });
        },
    );
}

#[test]
#[ignore = "isolated cold and cached name-paint rendering timing"]
fn perf_name_paint_cold_and_cached() {
    let mut paint = gradient("linear-gradient", 90.0, false);
    paint.shadows.push(shadow(1.0, 1.0, 2.0, "#a78bff80"));
    let mut cold = Vec::new();
    let mut cached = Vec::new();
    let mut retained = 0;
    for _ in 0..11 {
        let ctx = Context::default();
        let images = UiImageCache::default();
        configure(
            &ctx,
            PaintOptions {
                large_shadows: false,
                ..Default::default()
            },
        );
        // Initialize font data outside the timed passes.
        draw_benchmark_names(&ctx, &images, None);
        let started = Instant::now();
        draw_benchmark_names(&ctx, &images, Some(&paint));
        cold.push(started.elapsed().as_secs_f64() * 1000.0);
        for _ in 0..20 {
            let started = Instant::now();
            draw_benchmark_names(&ctx, &images, Some(&paint));
            cached.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        let cache = ctx
            .data(|data| data.get_temp::<Arc<Mutex<NameCache>>>(Id::new("name_paint_cache")))
            .unwrap();
        let cache = cache.lock().unwrap();
        assert_eq!(cache.names.len(), 8);
        assert_eq!(
            cache.builds, 0,
            "cached passes must reuse all painted names"
        );
        assert!(cache.bytes <= CACHE_BYTES);
        retained = retained.max(cache.bytes);
    }
    cold.sort_by(f64::total_cmp);
    cached.sort_by(f64::total_cmp);
    let percentile =
        |samples: &[f64], percentage: usize| samples[(samples.len() - 1) * percentage / 100];
    eprintln!("painted_names=40, distinct_names=8, cold_samples={}, cached_samples={}, cold_median_ms={:.4}, cold_p95_ms={:.4}, cached_median_ms={:.4}, cached_p95_ms={:.4}, retained_bytes={retained}",
        cold.len(), cached.len(), percentile(&cold, 50), percentile(&cold, 95),
        percentile(&cached, 50), percentile(&cached, 95));
}
