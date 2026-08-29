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

use crate::engine::types::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrPixelFormat {
    Gray8,
    Bgra8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionedOcrWord {
    pub text: String,
    pub rect: Rect,
    pub line_index: u32,
    pub word_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrFrame {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pixel_format: OcrPixelFormat,
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
            pixel_format: OcrPixelFormat::Gray8,
        }
    }

    pub fn from_bgra_screen_image(image: &ScreenImage) -> Self {
        let mut pixels = Vec::with_capacity(image.rgba.as_raw().len());
        for pixel in image.rgba.pixels() {
            pixels.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
        Self {
            pixels,
            width: image.rgba.width(),
            height: image.rgba.height(),
            pixel_format: OcrPixelFormat::Bgra8,
        }
    }
}

#[derive(Debug, Default)]
pub struct WindowsTextRecognizer {
    engines: Mutex<HashMap<String, OcrEngine>>,
}

impl WindowsTextRecognizer {
    pub fn recognize(&self, frame: &OcrFrame, language_tag: &str) -> Result<String> {
        let bitmap = software_bitmap_from_frame(frame)?;
        let engine = self.engine_for_language(language_tag)?;
        let result = future::block_on(engine.RecognizeAsync(&bitmap)?.into_future())?;
        Ok(result.Text()?.to_string_lossy())
    }

    pub fn recognize_words(
        &self,
        frame: &OcrFrame,
        language_tag: &str,
    ) -> Result<Vec<PositionedOcrWord>> {
        let bitmap = software_bitmap_from_frame(frame)?;
        let engine = self.engine_for_language(language_tag)?;
        let result = future::block_on(engine.RecognizeAsync(&bitmap)?.into_future())?;
        let lines = result.Lines()?;
        let mut positioned = Vec::new();
        for line_index in 0..lines.Size()? {
            let line = lines.GetAt(line_index)?;
            let words = line.Words()?;
            for word_index in 0..words.Size()? {
                let word = words.GetAt(word_index)?;
                let bounds = word.BoundingRect()?;
                positioned.push(PositionedOcrWord {
                    text: word.Text()?.to_string_lossy(),
                    rect: enclosing_integer_rect(
                        bounds.X,
                        bounds.Y,
                        bounds.Width,
                        bounds.Height,
                        frame.width,
                        frame.height,
                    )?,
                    line_index,
                    word_index,
                });
            }
        }
        Ok(positioned)
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
    software_bitmap_from_frame(&OcrFrame {
        pixels: pixels.to_vec(),
        width,
        height,
        pixel_format: OcrPixelFormat::Gray8,
    })
}

fn software_bitmap_from_frame(frame: &OcrFrame) -> Result<SoftwareBitmap> {
    let bytes_per_pixel = match frame.pixel_format {
        OcrPixelFormat::Gray8 => 1,
        OcrPixelFormat::Bgra8 => 4,
    };
    let (width, height) = validated_dimensions(
        frame.pixels.len(),
        frame.width,
        frame.height,
        bytes_per_pixel,
    )?;
    let buffer = CryptographicBuffer::CreateFromByteArray(&frame.pixels)?;
    Ok(SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        match frame.pixel_format {
            OcrPixelFormat::Gray8 => BitmapPixelFormat::Gray8,
            OcrPixelFormat::Bgra8 => BitmapPixelFormat::Bgra8,
        },
        width,
        height,
    )?)
}

fn validated_gray8_dimensions(pixels_len: usize, width: u32, height: u32) -> Result<(i32, i32)> {
    validated_dimensions(pixels_len, width, height, 1)
}

fn validated_dimensions(
    pixels_len: usize,
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
) -> Result<(i32, i32)> {
    if width == 0 || height == 0 {
        bail!("OCR frame dimensions must be non-zero");
    }
    let winrt_width =
        i32::try_from(width).context("OCR frame width exceeds Windows bitmap limits")?;
    let winrt_height =
        i32::try_from(height).context("OCR frame height exceeds Windows bitmap limits")?;
    let expected_len = checked_pixel_len(width as usize, height as usize)?
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| anyhow::anyhow!("OCR byte count overflows usize"))?;
    if pixels_len != expected_len {
        bail!(
            "OCR buffer length {} does not match {width}x{height} at {bytes_per_pixel} bytes per pixel",
            pixels_len,
        );
    }
    Ok((winrt_width, winrt_height))
}

fn enclosing_integer_rect(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    frame_width: u32,
    frame_height: u32,
) -> Result<Rect> {
    if ![x, y, width, height].iter().all(|value| value.is_finite()) {
        bail!("OCR word bounds must be finite");
    }
    if width <= 0.0 || height <= 0.0 {
        bail!("OCR word bounds must have positive size");
    }
    if frame_width == 0
        || frame_height == 0
        || frame_width > i32::MAX as u32
        || frame_height > i32::MAX as u32
    {
        bail!("OCR frame bounds exceed capture-relative integer limits");
    }
    let x = f64::from(x);
    let y = f64::from(y);
    let width = f64::from(width);
    let height = f64::from(height);
    let raw_left = x.floor();
    let raw_top = y.floor();
    let raw_right = (x + width).ceil();
    let raw_bottom = (y + height).ceil();
    if [raw_left, raw_top, raw_right, raw_bottom]
        .iter()
        .any(|edge| *edge < i64::MIN as f64 || *edge > i64::MAX as f64)
    {
        bail!("OCR word bounds exceed capture-relative integer limits");
    }
    let left = raw_left.clamp(0.0, f64::from(frame_width));
    let top = raw_top.clamp(0.0, f64::from(frame_height));
    let right = raw_right.clamp(0.0, f64::from(frame_width));
    let bottom = raw_bottom.clamp(0.0, f64::from(frame_height));
    if right <= left || bottom <= top {
        bail!("OCR word bounds do not intersect the processed frame");
    }
    Ok(Rect::new(
        left as i32,
        top as i32,
        (right - left) as u32,
        (bottom - top) as u32,
    ))
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
    fn original_bgra_frame_creates_software_bitmap_without_a_file() {
        let image = ScreenImage::new(image::RgbaImage::from_pixel(
            2,
            1,
            image::Rgba([10, 20, 30, 255]),
        ));
        let frame = OcrFrame::from_bgra_screen_image(&image);

        let bitmap = software_bitmap_from_frame(&frame).unwrap();

        assert_eq!(
            bitmap.BitmapPixelFormat().unwrap(),
            BitmapPixelFormat::Bgra8
        );
        assert_eq!(
            (bitmap.PixelWidth().unwrap(), bitmap.PixelHeight().unwrap()),
            (2, 1)
        );
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

    #[test]
    fn fractional_winrt_bounds_become_enclosing_integer_rectangle() {
        let rect = enclosing_integer_rect(10.75, 5.25, 30.5, 12.5, 100, 100).unwrap();

        assert_eq!(rect, crate::engine::types::Rect::new(10, 5, 32, 13));
    }

    #[test]
    fn negative_and_beyond_frame_word_bounds_are_clamped() {
        assert_eq!(
            enclosing_integer_rect(-2.25, -1.5, 5.0, 4.0, 10, 10).unwrap(),
            Rect::new(0, 0, 3, 3)
        );
        assert_eq!(
            enclosing_integer_rect(8.25, 8.5, 5.0, 5.0, 10, 10).unwrap(),
            Rect::new(8, 8, 2, 2)
        );
    }

    #[test]
    fn wholly_outside_word_bounds_are_rejected() {
        assert!(enclosing_integer_rect(-5.0, 1.0, 2.0, 2.0, 10, 10).is_err());
        assert!(enclosing_integer_rect(11.0, 1.0, 2.0, 2.0, 10, 10).is_err());
        assert!(enclosing_integer_rect(1.0, 11.0, 2.0, 2.0, 10, 10).is_err());
    }

    #[test]
    fn zero_sized_fractional_word_bounds_are_rejected() {
        assert!(enclosing_integer_rect(0.5, 1.0, 0.0, 2.0, 10, 10).is_err());
        assert!(enclosing_integer_rect(1.0, 0.5, 2.0, 0.0, 10, 10).is_err());
    }

    #[test]
    fn represented_f32_edges_are_added_in_f64_before_ceiling() {
        let x = f32::from_bits(0x4281_000d); // 64.5000991821289f32
        let width = f32::from_bits(0x4280_fff4); // 64.49990844726562f32

        let rect = enclosing_integer_rect(x, 5.0, width, 10.0, 200, 100).unwrap();

        assert_eq!(rect, Rect::new(64, 5, 66, 10));
    }

    #[test]
    fn nonfinite_and_unrepresentable_word_bounds_are_rejected() {
        assert!(enclosing_integer_rect(f32::NAN, 0.0, 1.0, 1.0, 10, 10).is_err());
        assert!(enclosing_integer_rect(0.0, f32::INFINITY, 1.0, 1.0, 10, 10).is_err());
        assert!(enclosing_integer_rect(f32::MAX, 0.0, f32::MAX, 1.0, 10, 10).is_err());
    }
}
