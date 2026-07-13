use std::{collections::HashMap, io::Cursor};

use anyhow::{Context, Result, bail};
use image::{DynamicImage, GrayImage, ImageDecoder, Limits, Luma, codecs::png::PngDecoder};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::engine::types::Rect;

use super::{
    AssetRef, IMAGE_RULE_VERIFICATION_VERSION, ImageRule, ImageRuleVerificationArtifact,
    ImageVerificationPreprocess, MacroDefinition,
    image_match::{
        ImageResourceKind, ImageWorkPolicy, MIN_TEMPLATE_VARIANCE, template_variance,
        validate_mask_reference, validated_scale_plan,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindingProblem {
    Missing,
    InvalidMargin,
    InvalidScore,
    InvalidProvenance,
    Stale,
}

#[derive(Serialize)]
struct FingerprintBinding<'a> {
    version: u32,
    preprocess: ImageVerificationPreprocess,
    rule_id: &'a str,
    rule_revision: u64,
    template: &'a AssetRef,
    transparent_mask: &'a Option<AssetRef>,
    captured_dpi: u32,
    region_id: &'a str,
    region_revision: u64,
    search_width: u32,
    search_height: u32,
    scales_percent: &'a [u16],
    threshold: f32,
    minimum_runner_up_margin: f32,
    negative_corpus_sha256: &'a str,
    negative_sample_count: u64,
    best_negative_score: f32,
    active_mask_variance: f32,
}

pub(crate) fn normalized_score(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

pub(crate) fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Decodes template pixels using the single grayscale preprocessing contract shared by
/// authoring, compilation, and live detection.
pub(crate) fn decode_template_png(bytes: &[u8]) -> Result<GrayImage> {
    decode_bounded_png(bytes, ImageResourceKind::Template)
        .context("image template asset cannot be decoded")
        .map(|image| image.into_luma8())
}

/// Decodes a portable mask. PNG alpha is authoritative when present; formats without alpha
/// retain their grayscale luminance for backwards-compatible explicit masks.
pub(crate) fn decode_mask_png(bytes: &[u8]) -> Result<GrayImage> {
    let image = decode_bounded_png(bytes, ImageResourceKind::Mask)
        .context("image mask asset cannot be decoded")?;
    if image.color().has_alpha() {
        let rgba = image.into_rgba8();
        Ok(GrayImage::from_fn(rgba.width(), rgba.height(), |x, y| {
            Luma([rgba.get_pixel(x, y)[3]])
        }))
    } else {
        Ok(image.into_luma8())
    }
}

fn decode_bounded_png(bytes: &[u8], resource: ImageResourceKind) -> Result<DynamicImage> {
    let (dimensions, decoded_bytes_per_pixel) = png_header_work(bytes, resource)?;
    let policy = ImageWorkPolicy::production();
    policy.validate_asset_dimensions(dimensions, decoded_bytes_per_pixel, resource)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(dimensions.0);
    limits.max_image_height = Some(dimensions.1);
    limits.max_alloc = policy.maximum_asset_bytes(resource);
    let decoder = PngDecoder::with_limits(Cursor::new(bytes), limits)
        .map_err(|error| anyhow::anyhow!("image {resource:?} header cannot be decoded: {error}"))?;
    if decoder.dimensions() != dimensions {
        bail!("image {resource:?} dimensions changed after header validation");
    }
    DynamicImage::from_decoder(decoder)
        .map_err(|error| anyhow::anyhow!("image {resource:?} pixels cannot be decoded: {error}"))
}

fn png_header_work(bytes: &[u8], resource: ImageResourceKind) -> Result<((u32, u32), u64)> {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 33
        || &bytes[..8] != SIGNATURE
        || &bytes[8..12] != 13_u32.to_be_bytes().as_slice()
        || &bytes[12..16] != b"IHDR"
    {
        bail!("image {resource:?} does not contain a complete PNG IHDR");
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("fixed IHDR width slice"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("fixed IHDR height slice"));
    let bit_depth = bytes[24];
    let color_type = bytes[25];
    let decoded_bytes_per_pixel = match (color_type, bit_depth) {
        (0, 1 | 2 | 4 | 8) => 2,
        (0, 16) => 4,
        (2, 8) | (3, 1 | 2 | 4 | 8) => 4,
        (2, 16) | (6, 16) => 8,
        (4, 8) => 2,
        (4, 16) => 4,
        (6, 8) => 4,
        _ => bail!("image {resource:?} has unsupported PNG color type or bit depth"),
    };
    Ok(((width, height), decoded_bytes_per_pixel))
}

pub(crate) fn fingerprint(artifact: &ImageRuleVerificationArtifact) -> String {
    let binding = FingerprintBinding {
        version: artifact.version,
        preprocess: artifact.preprocess,
        rule_id: &artifact.rule_id,
        rule_revision: artifact.rule_revision,
        template: &artifact.template,
        transparent_mask: &artifact.transparent_mask,
        captured_dpi: artifact.captured_dpi,
        region_id: &artifact.region_id,
        region_revision: artifact.region_revision,
        search_width: artifact.search_width,
        search_height: artifact.search_height,
        scales_percent: &artifact.scales_percent,
        threshold: artifact.threshold,
        minimum_runner_up_margin: artifact.minimum_runner_up_margin,
        negative_corpus_sha256: &artifact.negative_corpus_sha256,
        negative_sample_count: artifact.negative_sample_count,
        best_negative_score: artifact.best_negative_score,
        active_mask_variance: artifact.active_mask_variance,
    };
    let bytes = serde_json::to_vec(&binding)
        .expect("verification fingerprint binding contains only serializable values");
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn build_artifact(
    rule: &ImageRule,
    captured_dpi: u32,
    region_revision: u64,
    search_dimensions: (u32, u32),
    negative_corpus_sha256: String,
    negative_sample_count: u64,
    best_negative_score: f32,
    active_mask_variance: f32,
) -> ImageRuleVerificationArtifact {
    let mut artifact = ImageRuleVerificationArtifact {
        version: IMAGE_RULE_VERIFICATION_VERSION,
        preprocess: ImageVerificationPreprocess::GrayscaleNormalizedCrossCorrelation,
        rule_id: rule.id.clone(),
        rule_revision: rule.revision,
        template: rule.template.clone(),
        transparent_mask: rule.transparent_mask.clone(),
        captured_dpi,
        region_id: rule.region_id.clone(),
        region_revision,
        search_width: search_dimensions.0,
        search_height: search_dimensions.1,
        scales_percent: rule.scales_percent.clone(),
        threshold: rule.threshold,
        minimum_runner_up_margin: rule.minimum_runner_up_margin,
        negative_corpus_sha256,
        negative_sample_count,
        best_negative_score,
        active_mask_variance,
        verification_fingerprint_sha256: String::new(),
    };
    artifact.verification_fingerprint_sha256 = fingerprint(&artifact);
    artifact
}

pub(crate) fn validate_binding<'a>(
    definition: &MacroDefinition,
    rule: &'a ImageRule,
) -> std::result::Result<&'a ImageRuleVerificationArtifact, BindingProblem> {
    if !normalized_score(rule.minimum_runner_up_margin) {
        return Err(BindingProblem::InvalidMargin);
    }
    if !normalized_score(rule.threshold) {
        return Err(BindingProblem::InvalidScore);
    }
    let artifact = rule.verification.as_ref().ok_or(BindingProblem::Missing)?;
    if !normalized_score(artifact.threshold)
        || !normalized_score(artifact.minimum_runner_up_margin)
        || !normalized_score(artifact.best_negative_score)
        || !artifact.active_mask_variance.is_finite()
        || artifact.active_mask_variance < MIN_TEMPLATE_VARIANCE
    {
        return Err(BindingProblem::InvalidScore);
    }
    if !valid_sha256(&artifact.negative_corpus_sha256)
        || artifact.negative_sample_count == 0
        || !valid_sha256(&artifact.verification_fingerprint_sha256)
    {
        return Err(BindingProblem::InvalidProvenance);
    }
    if artifact.threshold - artifact.best_negative_score < artifact.minimum_runner_up_margin {
        return Err(BindingProblem::InvalidScore);
    }
    if fingerprint(artifact) != artifact.verification_fingerprint_sha256 {
        return Err(BindingProblem::Stale);
    }
    let Some(region) = definition
        .regions
        .iter()
        .find(|region| region.id == rule.region_id)
    else {
        return Err(BindingProblem::Stale);
    };
    let client = Rect::new(
        0,
        0,
        definition.target.captured_client_width,
        definition.target.captured_client_height,
    );
    let search = client.rect_from_ratio(region.rect);
    let current = artifact.version == IMAGE_RULE_VERIFICATION_VERSION
        && artifact.preprocess == ImageVerificationPreprocess::GrayscaleNormalizedCrossCorrelation
        && artifact.rule_id == rule.id
        && artifact.rule_revision == rule.revision
        && artifact.template == rule.template
        && artifact.transparent_mask == rule.transparent_mask
        && artifact.captured_dpi == definition.target.captured_dpi
        && artifact.region_id == region.id
        && artifact.region_revision == region.revision
        && artifact.search_width == search.width
        && artifact.search_height == search.height
        && artifact.scales_percent == rule.scales_percent
        && artifact.threshold == rule.threshold
        && artifact.minimum_runner_up_margin == rule.minimum_runner_up_margin;
    current.then_some(artifact).ok_or(BindingProblem::Stale)
}

pub(crate) fn validate_decoded_rule(
    definition: &MacroDefinition,
    rule: &ImageRule,
    template: &GrayImage,
    mask: Option<&GrayImage>,
) -> Result<()> {
    let artifact = validate_binding(definition, rule)
        .map_err(|problem| anyhow::anyhow!("image verification binding is invalid: {problem:?}"))?;
    validated_scale_plan(
        (artifact.search_width, artifact.search_height),
        template,
        mask,
        &rule.scales_percent,
        ImageWorkPolicy::production(),
    )?;
    let mask = validate_mask_reference(rule, template, mask).map_err(anyhow::Error::new)?;
    let variance = template_variance(template, mask);
    if !variance.is_finite() || variance < MIN_TEMPLATE_VARIANCE {
        bail!(
            "template variance {variance} is below {}",
            MIN_TEMPLATE_VARIANCE
        );
    }
    if variance != artifact.active_mask_variance {
        bail!(
            "template variance {variance} does not match verified variance {}",
            artifact.active_mask_variance
        );
    }
    Ok(())
}

pub(crate) fn trusted_remap_definition_assets(
    definition: &mut MacroDefinition,
    remaps: &HashMap<(String, u64), String>,
) -> Result<()> {
    for rule in &definition.image_rules {
        if rule.verification.is_some() {
            validate_binding(definition, rule).map_err(|problem| {
                anyhow::anyhow!(
                    "cannot remap invalid image verification for rule '{}': {problem:?}",
                    rule.id
                )
            })?;
        }
    }
    let remap = |asset: &mut AssetRef| {
        if let Some(id) = remaps.get(&(asset.id.clone(), asset.revision)) {
            asset.id = id.clone();
        }
    };
    for rule in &mut definition.image_rules {
        remap(&mut rule.template);
        if let Some(mask) = &mut rule.transparent_mask {
            remap(mask);
        }
        if let Some(artifact) = &mut rule.verification {
            artifact.template = rule.template.clone();
            artifact.transparent_mask = rule.transparent_mask.clone();
            artifact.verification_fingerprint_sha256 = fingerprint(artifact);
        }
    }
    for rule in &definition.image_rules {
        if rule.verification.is_some() {
            validate_binding(definition, rule).map_err(|problem| {
                anyhow::anyhow!(
                    "remapped image verification for rule '{}' is invalid: {problem:?}",
                    rule.id
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{
        DynamicImage, GrayAlphaImage, GrayImage, ImageFormat, Luma, LumaA, Rgba, RgbaImage,
    };

    use super::*;

    fn png(image: DynamicImage) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & (0_u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }

    fn huge_rgba_png_header(width: u32, height: u32) -> Vec<u8> {
        let source = RgbaImage::from_pixel(1, 1, Rgba([255, 255, 255, 0]));
        let mut bytes = png(DynamicImage::ImageRgba8(source));
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = crc32(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&crc.to_be_bytes());
        bytes
    }

    #[test]
    fn rgba_mask_uses_alpha_instead_of_white_luminance() {
        let image = RgbaImage::from_fn(2, 1, |x, _| {
            if x == 0 {
                Rgba([255, 255, 255, 0])
            } else {
                Rgba([255, 255, 255, 128])
            }
        });

        let decoded = decode_mask_png(&png(DynamicImage::ImageRgba8(image))).unwrap();

        assert_eq!(decoded.as_raw(), &[0, 128]);
    }

    #[test]
    fn grayscale_alpha_mask_uses_alpha_and_grayscale_mask_uses_luminance() {
        let la = GrayAlphaImage::from_pixel(1, 1, LumaA([255, 17]));
        let gray = GrayImage::from_pixel(1, 1, Luma([91]));

        assert_eq!(
            decode_mask_png(&png(DynamicImage::ImageLumaA8(la)))
                .unwrap()
                .as_raw(),
            &[17]
        );
        assert_eq!(
            decode_mask_png(&png(DynamicImage::ImageLuma8(gray)))
                .unwrap()
                .as_raw(),
            &[91]
        );
    }

    #[test]
    fn huge_sparse_capable_png_is_rejected_from_header_before_pixel_decode() {
        let error = decode_mask_png(&huge_rgba_png_header(65_535, 65_535)).unwrap_err();

        assert_eq!(
            error.downcast_ref::<super::super::image_match::ImageMatchError>(),
            Some(
                &super::super::image_match::ImageMatchError::ResourcePixelLimit {
                    resource: super::super::image_match::ImageResourceKind::Mask,
                    actual: 4_294_836_225,
                    maximum: super::super::image_match::DEFAULT_MAX_MASK_PIXELS,
                }
            )
        );
    }
}
