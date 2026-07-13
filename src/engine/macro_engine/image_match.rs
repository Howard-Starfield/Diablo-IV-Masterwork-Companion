use crate::engine::types::{Rect, ScreenImage};
use anyhow::{Result, bail};
use image::{GrayImage, imageops};
use imageproc::template_matching::{MatchTemplateMethod, match_template};

#[derive(Debug, Clone, PartialEq)]
pub struct ImageMatchConfig {
    pub threshold: f32,
    pub scales_percent: Vec<u16>,
}

impl ImageMatchConfig {
    pub fn exact_scale(threshold: f32) -> Self {
        Self {
            threshold,
            scales_percent: vec![100],
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageMatchCandidate {
    pub rect: Rect,
    pub score: f32,
    pub scale_percent: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RawImageMatch {
    pub best: ImageMatchCandidate,
    pub candidates: Vec<ImageMatchCandidate>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ImageMatcher;

fn scaled_dimension(dimension: u32, scale_percent: u16) -> Result<u32> {
    let rounded = u64::from(dimension)
        .checked_mul(u64::from(scale_percent))
        .and_then(|value| value.checked_add(50))
        .map(|value| value / 100)
        .ok_or_else(|| anyhow::anyhow!("scaled template dimension calculation overflowed"))?;
    u32::try_from(rounded).map_err(|_| anyhow::anyhow!("scaled template dimension exceeds u32"))
}

impl ImageMatcher {
    pub fn match_screen_image(
        &self,
        search: &ScreenImage,
        capture_bounds: Rect,
        template: &GrayImage,
        config: &ImageMatchConfig,
    ) -> Result<RawImageMatch> {
        if search.rgba.dimensions() != (capture_bounds.width, capture_bounds.height) {
            bail!("capture bounds dimensions do not match screen image");
        }
        let gray = image::DynamicImage::ImageRgba8(search.rgba.clone()).into_luma8();
        let mut result = self.match_template(&gray, template, config)?;
        for candidate in &mut result.candidates {
            candidate.rect.x += capture_bounds.x;
            candidate.rect.y += capture_bounds.y;
        }
        result.best.rect.x += capture_bounds.x;
        result.best.rect.y += capture_bounds.y;
        Ok(result)
    }

    pub fn match_template(
        &self,
        search: &GrayImage,
        template: &GrayImage,
        config: &ImageMatchConfig,
    ) -> Result<RawImageMatch> {
        if !(0.0..=1.0).contains(&config.threshold) {
            bail!("image match threshold must be between 0 and 1");
        }
        if config.scales_percent.is_empty() {
            bail!("image match requires at least one scale");
        }

        let mut candidates = Vec::new();
        let mut best: Option<ImageMatchCandidate> = None;
        for &scale_percent in &config.scales_percent {
            if scale_percent == 0 {
                bail!("image match scale must be greater than zero");
            }
            let width = scaled_dimension(template.width(), scale_percent)?;
            let height = scaled_dimension(template.height(), scale_percent)?;
            if width == 0 || height == 0 {
                continue;
            }
            if width > search.width() || height > search.height() {
                continue;
            }
            let scaled = if scale_percent == 100 {
                template.clone()
            } else {
                imageops::resize(template, width, height, imageops::FilterType::Triangle)
            };
            let scores = match_template(
                search,
                &scaled,
                MatchTemplateMethod::CrossCorrelationNormalized,
            );
            for (x, y, pixel) in scores.enumerate_pixels() {
                let candidate = ImageMatchCandidate {
                    rect: Rect::new(x as i32, y as i32, width, height),
                    score: pixel[0],
                    scale_percent,
                };
                if best
                    .as_ref()
                    .is_none_or(|current| candidate.score > current.score)
                {
                    best = Some(candidate.clone());
                }
                if candidate.score >= config.threshold {
                    candidates.push(candidate);
                }
            }
        }

        let Some(best) = best else {
            bail!("template does not fit search image at any configured scale");
        };
        Ok(RawImageMatch { best, candidates })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::types::ScreenImage;
    use image::{GrayImage, Luma};

    fn fixture_icon() -> GrayImage {
        GrayImage::from_fn(7, 5, |x, y| {
            let value = match (x, y) {
                (0, 0) | (6, 0) | (0, 4) | (6, 4) => 20,
                (3, 1..=3) | (1..=5, 2) => 240,
                _ => 80 + ((x * 17 + y * 29) % 90) as u8,
            };
            Luma([value])
        })
    }

    fn fixture_search_with_icon_at(x: u32, y: u32) -> GrayImage {
        let mut search = GrayImage::from_fn(64, 48, |px, py| {
            Luma([40 + ((px * 11 + py * 7) % 31) as u8])
        });
        let icon = fixture_icon();
        for (icon_x, icon_y, pixel) in icon.enumerate_pixels() {
            search.put_pixel(x + icon_x, y + icon_y, *pixel);
        }
        search
    }

    #[test]
    fn normalized_correlation_reports_expected_best_location() {
        let search = fixture_search_with_icon_at(23, 17);
        let template = fixture_icon();
        let result = ImageMatcher::default()
            .match_template(&search, &template, &ImageMatchConfig::exact_scale(0.95))
            .unwrap();

        assert_eq!((result.best.rect.x, result.best.rect.y), (23, 17));
        assert!(result.best.score >= 0.95);
    }

    #[test]
    fn equal_size_template_produces_single_origin_match() {
        let search = fixture_icon();
        let result = ImageMatcher::default()
            .match_template(&search, &search, &ImageMatchConfig::exact_scale(0.95))
            .unwrap();

        assert_eq!((result.best.rect.x, result.best.rect.y), (0, 0));
        assert_eq!(result.candidates, vec![result.best]);
    }

    #[test]
    fn scaled_dimension_rejects_result_larger_than_u32() {
        let error = scaled_dimension(u32::MAX, u16::MAX).unwrap_err();

        assert!(error.to_string().contains("exceeds u32"));
    }

    #[test]
    fn screen_match_offsets_candidates_to_capture_coordinates() {
        let search = fixture_search_with_icon_at(23, 17);
        let rgba = image::DynamicImage::ImageLuma8(search).into_rgba8();
        let capture_bounds = Rect::new(-120, 75, 64, 48);
        let result = ImageMatcher::default()
            .match_screen_image(
                &ScreenImage::new(rgba),
                capture_bounds,
                &fixture_icon(),
                &ImageMatchConfig::exact_scale(0.95),
            )
            .unwrap();

        assert_eq!((result.best.rect.x, result.best.rect.y), (-97, 92));
    }
}
