#[cfg(target_os = "windows")]
#[allow(dead_code)]
#[path = "../engine/types.rs"]
mod shared_types;

#[cfg(target_os = "windows")]
mod engine {
    pub mod types {
        pub use crate::shared_types::*;
    }
}

#[cfg(target_os = "windows")]
#[allow(dead_code)]
#[path = "../engine/macro_engine/image_match.rs"]
mod image_match;
#[cfg(target_os = "windows")]
#[path = "../engine/platform/windows_ocr.rs"]
mod windows_ocr;

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    use anyhow::{Context, bail};
    use image::imageops;
    use image_match::{ImageMatchConfig, ImageMatcher};
    use shared_types::ScreenImage;
    use windows_ocr::{OcrFrame, WindowsTextRecognizer};
    use xcap::Monitor;

    #[derive(Default)]
    struct Totals {
        capture: Duration,
        preprocess: Duration,
        ocr: Duration,
        exact_match: Duration,
        three_scale_match: Duration,
    }

    fn average(duration: Duration, iterations: u32) -> Duration {
        duration / iterations
    }

    let iterations = std::env::args()
        .nth(1)
        .map(|value| value.parse::<u32>())
        .transpose()
        .context("iteration count must be a positive integer")?
        .unwrap_or(10);
    if iterations == 0 {
        bail!("iteration count must be greater than zero");
    }

    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    let monitor = monitors
        .iter()
        .find(|monitor| monitor.is_primary().unwrap_or(false))
        .or_else(|| monitors.first())
        .context("no monitor is available for capture")?;
    let width = monitor.width()?.min(320);
    let height = monitor.height()?.min(180);
    let capture_x = (monitor.width()?.saturating_sub(width)) / 2;
    let capture_y = (monitor.height()?.saturating_sub(height)) / 2;
    let matcher = ImageMatcher;
    let recognizer = WindowsTextRecognizer::default();
    let exact_config = ImageMatchConfig::exact_scale(0.90);
    let three_scale_config = ImageMatchConfig {
        threshold: 0.90,
        scales_percent: vec![90, 100, 110],
    };
    let mut totals = Totals::default();

    // One unreported pass warms capture, WinRT OCR, and matcher code paths.
    let warm_rgba = monitor.capture_region(capture_x, capture_y, width, height)?;
    let warm_screen = ScreenImage::new(warm_rgba);
    let warm_frame = OcrFrame::from_screen_image(&warm_screen);
    let warm_gray = image::DynamicImage::ImageRgba8(warm_screen.rgba.clone()).into_luma8();
    let template_width = warm_gray.width().min(24);
    let template_height = warm_gray.height().min(16);
    let template = imageops::crop_imm(
        &warm_gray,
        (warm_gray.width() - template_width) / 2,
        (warm_gray.height() - template_height) / 2,
        template_width,
        template_height,
    )
    .to_image();
    let _ = recognizer.recognize(&warm_frame, "en-US")?;
    let _ = matcher.match_template(&warm_gray, &template, &exact_config)?;
    let _ = matcher.match_template(&warm_gray, &template, &three_scale_config)?;

    for _ in 0..iterations {
        let started = Instant::now();
        let rgba = monitor.capture_region(capture_x, capture_y, width, height)?;
        totals.capture += started.elapsed();

        let started = Instant::now();
        let screen = ScreenImage::new(rgba);
        let frame = OcrFrame::from_screen_image(&screen);
        let gray = image::DynamicImage::ImageRgba8(screen.rgba.clone()).into_luma8();
        totals.preprocess += started.elapsed();

        let started = Instant::now();
        let _ = recognizer.recognize(&frame, "en-US")?;
        totals.ocr += started.elapsed();

        let started = Instant::now();
        let _ = matcher.match_template(&gray, &template, &exact_config)?;
        totals.exact_match += started.elapsed();

        let started = Instant::now();
        let _ = matcher.match_template(&gray, &template, &three_scale_config)?;
        totals.three_scale_match += started.elapsed();
    }

    println!("macro detection benchmark: {iterations} warm iterations, {width}x{height}");
    println!(
        "capture average:            {:?}",
        average(totals.capture, iterations)
    );
    println!(
        "preprocessing average:      {:?}",
        average(totals.preprocess, iterations)
    );
    println!(
        "OCR average:                {:?}",
        average(totals.ocr, iterations)
    );
    println!(
        "serial exact-scale average: {:?}",
        average(totals.exact_match, iterations)
    );
    println!(
        "serial three-scale average: {:?}",
        average(totals.three_scale_match, iterations)
    );
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("macro_detection_bench requires Windows.Media.Ocr");
}
