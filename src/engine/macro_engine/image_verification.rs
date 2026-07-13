use std::collections::HashMap;

use anyhow::{Result, bail};
use image::GrayImage;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::engine::types::Rect;

use super::{
    AssetRef, IMAGE_RULE_VERIFICATION_VERSION, ImageRule, ImageRuleVerificationArtifact,
    ImageVerificationPreprocess, MacroDefinition,
    image_match::{
        DEFAULT_MAX_SCORE_CELLS, MIN_TEMPLATE_VARIANCE, estimate_score_cells, template_variance,
        validate_mask_reference,
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
    let score_cells = estimate_score_cells(
        (artifact.search_width, artifact.search_height),
        template.dimensions(),
        &rule.scales_percent,
    );
    if score_cells > DEFAULT_MAX_SCORE_CELLS {
        bail!("image score-map work {score_cells} exceeds maximum {DEFAULT_MAX_SCORE_CELLS}");
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
