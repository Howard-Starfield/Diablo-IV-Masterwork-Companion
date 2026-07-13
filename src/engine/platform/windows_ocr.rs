use std::{collections::HashMap, sync::Mutex};

use crate::engine::types::ScreenImage;
use anyhow::{Context, Result, bail};
use futures_lite::future;
use image::GrayImage;
use windows::{
    Globalization::Language,
    Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap},
    Media::Ocr::OcrEngine,
    Security::Cryptography::CryptographicBuffer,
    core::HSTRING,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrFrame {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl OcrFrame {
    pub fn from_screen_image(image: &ScreenImage) -> Self {
        let gray = image::DynamicImage::ImageRgba8(image.rgba.clone()).into_luma8();
        Self::from_gray_image(&gray)
    }

    pub fn from_gray_image(image: &GrayImage) -> Self {
        Self {
            pixels: image.as_raw().clone(),
            width: image.width(),
            height: image.height(),
        }
    }
}

#[derive(Debug, Default)]
pub struct WindowsTextRecognizer {
    engines: Mutex<HashMap<String, OcrEngine>>,
}

impl WindowsTextRecognizer {
    pub fn recognize(&self, frame: &OcrFrame, language_tag: &str) -> Result<String> {
        let bitmap = software_bitmap_from_gray8(&frame.pixels, frame.width, frame.height)?;
        let engine = self.engine_for_language(language_tag)?;
        let result = future::block_on(engine.RecognizeAsync(&bitmap)?.into_future())?;
        Ok(result.Text()?.to_string_lossy())
    }

    fn engine_for_language(&self, language_tag: &str) -> Result<OcrEngine> {
        let mut engines = self
            .engines
            .lock()
            .map_err(|_| anyhow::anyhow!("Windows OCR engine cache lock is poisoned"))?;
        if let Some(engine) = engines.get(language_tag) {
            return Ok(engine.clone());
        }

        let language = Language::CreateLanguage(&HSTRING::from(language_tag))
            .with_context(|| format!("invalid OCR language tag {language_tag:?}"))?;
        let engine = OcrEngine::TryCreateFromLanguage(&language)
            .with_context(|| format!("OCR language {language_tag:?} is not installed"))?;
        engines.insert(language_tag.to_owned(), engine.clone());
        Ok(engine)
    }
}

fn software_bitmap_from_gray8(pixels: &[u8], width: u32, height: u32) -> Result<SoftwareBitmap> {
    let (width, height) = validated_gray8_dimensions(pixels.len(), width, height)?;
    let buffer = CryptographicBuffer::CreateFromByteArray(pixels)?;
    Ok(SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Gray8,
        width,
        height,
    )?)
}

fn validated_gray8_dimensions(pixels_len: usize, width: u32, height: u32) -> Result<(i32, i32)> {
    if width == 0 || height == 0 {
        bail!("OCR frame dimensions must be non-zero");
    }
    let winrt_width =
        i32::try_from(width).context("OCR frame width exceeds Windows bitmap limits")?;
    let winrt_height =
        i32::try_from(height).context("OCR frame height exceeds Windows bitmap limits")?;
    let expected_len = checked_pixel_len(width as usize, height as usize)?;
    if pixels_len != expected_len {
        bail!(
            "OCR Gray8 buffer length {} does not match {width}x{height}",
            pixels_len
        );
    }
    Ok((winrt_width, winrt_height))
}

fn checked_pixel_len(width: usize, height: usize) -> Result<usize> {
    width
        .checked_mul(height)
        .ok_or_else(|| anyhow::anyhow!("OCR Gray8 pixel count overflows usize"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::types::ScreenImage;

    #[test]
    fn creates_gray8_software_bitmap_without_png_file() {
        let pixels = vec![255u8; 32 * 16];
        let bitmap = software_bitmap_from_gray8(&pixels, 32, 16).unwrap();

        assert_eq!(bitmap.PixelWidth().unwrap(), 32);
        assert_eq!(bitmap.PixelHeight().unwrap(), 16);
    }

    #[test]
    fn ocr_frame_converts_screen_image_to_owned_gray8_pixels() {
        let rgba = image::RgbaImage::from_pixel(3, 2, image::Rgba([255, 255, 255, 255]));
        let frame = OcrFrame::from_screen_image(&ScreenImage::new(rgba));

        assert_eq!((frame.width, frame.height), (3, 2));
        assert_eq!(frame.pixels, vec![255; 6]);
    }

    #[test]
    fn rejects_dimension_above_winrt_i32_limit_before_length_check() {
        let error = validated_gray8_dimensions(0, i32::MAX as u32 + 1, 1).unwrap_err();

        assert!(error.to_string().contains("Windows bitmap limits"));
    }

    #[test]
    fn checked_pixel_len_rejects_usize_product_overflow() {
        let error = checked_pixel_len(usize::MAX, 2).unwrap_err();

        assert!(error.to_string().contains("pixel count overflows usize"));
    }
}
