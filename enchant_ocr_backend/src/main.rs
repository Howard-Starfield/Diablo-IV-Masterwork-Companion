use std::env;

use anyhow::{Context, Result, bail};
use enchant_ocr_backend::{
    config::EnchantConfig,
    enchant_loop::{EnchantEvent, EnchantRunner},
    match_affix,
    platform::{
        EscStopSignal, SendInputController, WindowsOcrReader, XcapRegionCapture,
        enable_per_monitor_dpi_awareness,
    },
    types::Rect,
};

fn main() -> Result<()> {
    enable_per_monitor_dpi_awareness();

    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        print_usage();
        return Ok(());
    };

    match command.as_str() {
        "sample-config" => {
            let path = args
                .next()
                .unwrap_or_else(|| "enchant_config.sample.json".to_string());
            EnchantConfig::sample().save(&path)?;
            println!("wrote {path}");
        }
        "match" => {
            let raw = args.next().context("missing raw OCR text")?;
            let target = args.next().context("missing target text")?;
            let result = match_affix(&raw, &[target], 0.78);
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        "ocr-region" => {
            let rect = parse_rect(args.collect::<Vec<_>>())?;
            let capture = XcapRegionCapture;
            let ocr = WindowsOcrReader::default();
            let image = enchant_ocr_backend::RegionCapture::capture_region(&capture, rect)?;
            let raw = enchant_ocr_backend::OcrReader::read_text(&ocr, &image)?;
            let result = match_affix(&raw, &[], 0.78);
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        "ocr-file" => {
            let path = args.next().context("missing image path")?;
            let rgba = image::open(&path)
                .with_context(|| format!("failed to open image {path}"))?
                .to_rgba8();
            let rect = Rect::new(0, 0, rgba.width(), rgba.height());
            let image = enchant_ocr_backend::ScreenImage::new(rect, rgba);
            let ocr = WindowsOcrReader::default();
            let raw = enchant_ocr_backend::OcrReader::read_text(&ocr, &image)?;
            let result = match_affix(&raw, &[], 0.78);
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        "run" => {
            let path = args.next().context("missing config path")?;
            let config = EnchantConfig::load(&path)?;
            let runner = EnchantRunner::new(
                config,
                XcapRegionCapture,
                WindowsOcrReader::default(),
                SendInputController,
                EscStopSignal::new(),
            );
            let outcome = runner.run(|event| print_event(&event))?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
        }
        _ => {
            print_usage();
        }
    }

    Ok(())
}

fn parse_rect(args: Vec<String>) -> Result<Rect> {
    if args.len() != 4 {
        bail!("ocr-region requires: x y width height");
    }
    Ok(Rect {
        x: args[0].parse()?,
        y: args[1].parse()?,
        width: args[2].parse()?,
        height: args[3].parse()?,
    })
}

fn print_event(event: &EnchantEvent) {
    match event {
        EnchantEvent::AttemptStarted { attempt } => println!("attempt {attempt}: start"),
        EnchantEvent::ClickEnchant { point } => {
            println!("click enchant at {}, {}", point.x, point.y)
        }
        EnchantEvent::OcrReadStarted { rect } => {
            println!(
                "ocr region {}, {} {}x{}",
                rect.x, rect.y, rect.width, rect.height
            )
        }
        EnchantEvent::OcrReadFinished {
            result,
            ocr_time_ms,
        } => {
            println!(
                "ocr raw={:?} normalized={:?} matched={} target={:?} score={:.3} time={}ms",
                result.raw_text,
                result.normalized_text,
                result.matched,
                result.target,
                result.score,
                ocr_time_ms
            )
        }
        EnchantEvent::TargetFound { attempt, result } => {
            println!("attempt {attempt}: found target {:?}", result.target)
        }
        EnchantEvent::ClickReplace { point } => {
            println!("click replace at {}, {}", point.x, point.y)
        }
        EnchantEvent::ClickClose { point } => println!("click close at {}, {}", point.x, point.y),
        EnchantEvent::AttemptFinished { attempt } => println!("attempt {attempt}: finished"),
        EnchantEvent::MaxAttemptsReached { attempts } => {
            println!("max attempts reached: {attempts}")
        }
        EnchantEvent::Stopped => println!("stopped"),
    }
}

fn print_usage() {
    println!(
        "Enchant OCR Backend\n\n\
         Commands:\n\
         sample-config [path]\n\
         match <raw-ocr-text> <target-text>\n\
         ocr-region <x> <y> <width> <height>\n\
         ocr-file <image-path>\n\
         run <config-path>\n"
    );
}
