use std::{collections::HashMap, sync::Mutex};

use crate::engine::types::{Rect, ScreenImage};
use anyhow::{Context, Result, bail};
use image::{GrayImage, ImageBuffer, Luma, imageops};
use imageproc::template_matching::{MatchTemplateMethod, match_template};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::engine::automation::CaptureSource;

use super::{
    Condition, ConditionDetector, DetectorEvidence, IMAGE_RULE_VERIFICATION_VERSION, ImageRule,
    ImageRuleVerificationArtifact, ImageVerificationPreprocess, MacroDefinition,
    MatchSelectionPolicy, ObservationRequest,
};

/// Authoring starts here, but every rule must retain its own verified threshold.
pub const INITIAL_SIMILARITY_THRESHOLD: f32 = 0.95;
/// Initial bounded work policy for the v1 640x360 three-scale envelope.
/// Task 14 may lower this score-cell budget after named-hardware release benchmarks.
pub const DEFAULT_MAX_SCORE_CELLS: u64 = 750_000;
/// Grayscale intensity variance below this value is too flat for safe correlation.
const MIN_TEMPLATE_VARIANCE: f32 = 16.0;
const DEFAULT_MAX_STABILITY_STATES: usize = 256;

pub(crate) mod verification {
    use super::*;

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
        template: &'a super::super::AssetRef,
        transparent_mask: &'a Option<super::super::AssetRef>,
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
            && artifact.preprocess
                == ImageVerificationPreprocess::GrayscaleNormalizedCrossCorrelation
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
        let artifact = validate_binding(definition, rule).map_err(|problem| {
            anyhow::anyhow!("image verification binding is invalid: {problem:?}")
        })?;
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
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ImageMatchError {
    #[error("image match coordinate exceeds the supported signed screen range")]
    CoordinateOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClusterPolicy {
    pub minimum_overlap_ratio: f32,
    pub maximum_center_distance_px: f32,
    pub maximum_scale_delta_percent: u16,
}

impl Default for ClusterPolicy {
    fn default() -> Self {
        // Product-owned fixed policy: merge candidates with >=30% IoU or centers within four
        // pixels, but never bridge scale searches more than ten percentage points apart.
        Self {
            minimum_overlap_ratio: 0.30,
            maximum_center_distance_px: 4.0,
            maximum_scale_delta_percent: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CandidateCluster {
    pub best: ImageMatchCandidate,
    pub members: Vec<ImageMatchCandidate>,
}

impl CandidateCluster {
    pub fn from_peak(peak: ImageMatchCandidate) -> Self {
        Self {
            best: peak.clone(),
            members: vec![peak],
        }
    }

    fn add(&mut self, peak: ImageMatchCandidate) {
        if candidate_order(&peak, &self.best).is_lt() {
            self.best = peak.clone();
        }
        self.members.push(peak);
        self.members.sort_by(candidate_order);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageMatchResult {
    pub matched: bool,
    pub selected: Option<CandidateCluster>,
    pub best: Option<CandidateCluster>,
    pub runner_up: Option<CandidateCluster>,
    pub ambiguity_margin: Option<f32>,
    pub clusters: Vec<CandidateCluster>,
}

impl ImageMatchResult {
    pub fn select(mut clusters: Vec<CandidateCluster>, rule: &ImageRule) -> Self {
        clusters.sort_by(cluster_order);
        let best = clusters.first().cloned();
        let runner_up = clusters.get(1).cloned();
        let ambiguity_margin = best
            .as_ref()
            .zip(runner_up.as_ref())
            .map(|(first, second)| first.best.score - second.best.score);
        let ambiguous =
            ambiguity_margin.is_some_and(|margin| margin < rule.minimum_runner_up_margin);
        let selected = match rule.match_policy {
            MatchSelectionPolicy::ExactlyOne => (clusters.len() == 1).then(|| clusters[0].clone()),
            MatchSelectionPolicy::HighestScore => best.clone(),
            MatchSelectionPolicy::FirstReadingOrder => clusters
                .iter()
                .min_by_key(|cluster| (cluster.best.rect.y, cluster.best.rect.x))
                .cloned(),
            MatchSelectionPolicy::Topmost => clusters
                .iter()
                .min_by_key(|cluster| (cluster.best.rect.y, cluster.best.rect.x))
                .cloned(),
            MatchSelectionPolicy::Bottommost => clusters
                .iter()
                .max_by(|left, right| {
                    left.best
                        .rect
                        .y
                        .cmp(&right.best.rect.y)
                        .then_with(|| right.best.rect.x.cmp(&left.best.rect.x))
                })
                .cloned(),
        };
        let matched = selected.is_some() && !ambiguous;
        Self {
            matched,
            selected: matched.then_some(selected).flatten(),
            best,
            runner_up,
            ambiguity_margin,
            clusters,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImageFrameMetadata {
    pub frame_id: u64,
    pub captured_at_ms: u64,
    pub window_id: u64,
    pub window_revision: u64,
    pub geometry_revision: u64,
    pub display_profile_revision: u64,
    pub dpi: u32,
    pub region_revision: u64,
    pub rule_revision: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StableImageMatch {
    pub frame: ImageFrameMetadata,
    pub candidate: ImageMatchCandidate,
}

#[derive(Debug, Clone)]
pub struct StabilityTracker {
    required_frames: u8,
    minimum_elapsed_ms: u64,
    maximum_center_drift_px: u32,
    last: Option<StableImageMatch>,
    stable_frames: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StabilityOutcome {
    Accepted { stable_frames: u8, qualified: bool },
    Ignored { stable_frames: u8 },
    Reset { stable_frames: u8 },
}

impl StabilityOutcome {
    fn stable_frames(self) -> u8 {
        match self {
            Self::Accepted { stable_frames, .. }
            | Self::Ignored { stable_frames }
            | Self::Reset { stable_frames } => stable_frames,
        }
    }

    fn qualified_current_frame(self) -> bool {
        matches!(
            self,
            Self::Accepted {
                qualified: true,
                ..
            }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ImageStabilityKey {
    run_id: String,
    generation: u64,
    source_block_id: String,
    rule_id: String,
    region_id: String,
}

pub struct ImageDetector {
    matcher: ImageMatcher,
    stability: Mutex<HashMap<ImageStabilityKey, StabilityTracker>>,
    maximum_stability_states: usize,
}

impl ImageDetector {
    pub fn new() -> Self {
        Self::with_stability_capacity(DEFAULT_MAX_STABILITY_STATES)
    }

    pub fn with_stability_capacity(maximum_stability_states: usize) -> Self {
        assert!(maximum_stability_states > 0);
        Self {
            matcher: ImageMatcher,
            stability: Mutex::new(HashMap::new()),
            maximum_stability_states,
        }
    }

    pub fn clear_run(&self, run_id: &str) -> Result<usize> {
        let mut stability = self
            .stability
            .lock()
            .map_err(|_| anyhow::anyhow!("image detector stability lock is poisoned"))?;
        let before = stability.len();
        stability.retain(|key, _| key.run_id != run_id);
        Ok(before - stability.len())
    }

    fn observe_image(
        &self,
        request: &ObservationRequest<'_>,
        capture: &(dyn CaptureSource + Send + Sync),
        source_block_id: &str,
        rule_id: &str,
    ) -> Result<DetectorEvidence> {
        let definition = request.compiled.definition();
        let rule = definition
            .image_rules
            .iter()
            .find(|rule| rule.id == rule_id)
            .with_context(|| format!("compiled image rule {rule_id:?} is missing"))?;
        let region = definition
            .regions
            .iter()
            .find(|region| region.id == rule.region_id)
            .with_context(|| format!("compiled image region {:?} is missing", rule.region_id))?;
        let client = Rect::new(
            0,
            0,
            definition.target.captured_client_width,
            definition.target.captured_client_height,
        );
        let capture_rect = client.rect_from_ratio(region.rect);
        let captured = capture.capture_frame(capture_rect)?;
        let frame = ImageFrameMetadata {
            frame_id: captured.metadata.frame_id,
            captured_at_ms: captured.metadata.captured_at_ms,
            window_id: captured.metadata.window_id,
            window_revision: captured.metadata.window_revision,
            geometry_revision: captured.metadata.geometry_revision,
            display_profile_revision: captured.metadata.display_profile_revision,
            dpi: captured.metadata.dpi,
            region_revision: region.revision,
            rule_revision: rule.revision,
        };
        if frame.region_revision != region.revision || frame.rule_revision != rule.revision {
            bail!("image frame metadata does not match compiled region/rule revisions");
        }
        if frame.dpi != definition.target.captured_dpi {
            bail!(
                "image template DPI {} is stale for current DPI {}",
                definition.target.captured_dpi,
                frame.dpi
            );
        }
        let image = captured.image;
        let template = decode_gray_asset(request, &rule.template, "template")?;
        let mask = rule
            .transparent_mask
            .as_ref()
            .map(|asset| decode_gray_asset(request, asset, "mask"))
            .transpose()?;
        validate_runtime_image_rule(definition, rule, &template, mask.as_ref(), capture_rect)?;
        let raw = self.matcher.match_screen_image_masked(
            &image,
            capture_rect,
            &template,
            mask.as_ref(),
            &ImageMatchConfig {
                threshold: rule.threshold,
                scales_percent: rule.scales_percent.clone(),
            },
        )?;
        let result = ImageMatchResult::select(
            cluster_peaks(raw.candidates, ClusterPolicy::default()),
            rule,
        );
        let key = ImageStabilityKey {
            run_id: request.run_id.to_string(),
            generation: request.generation,
            source_block_id: source_block_id.to_string(),
            rule_id: rule.id.clone(),
            region_id: region.id.clone(),
        };
        let mut stability = self
            .stability
            .lock()
            .map_err(|_| anyhow::anyhow!("image detector stability lock is poisoned"))?;
        if !stability.contains_key(&key) && stability.len() >= self.maximum_stability_states {
            bail!(
                "image stability state capacity {} is exhausted; clear completed runs",
                self.maximum_stability_states
            );
        }
        let tracker = stability.entry(key).or_insert_with(|| {
            StabilityTracker::new(
                rule.stable_frames,
                rule.poll_interval_ms,
                rule.maximum_center_drift_px,
            )
        });
        let selected = result.selected.as_ref().map(|cluster| cluster.best.clone());
        let stability_outcome = selected.as_ref().map(|candidate| {
            tracker.observe(StableImageMatch {
                frame,
                candidate: candidate.clone(),
            })
        });
        if selected.is_none() {
            tracker.reset();
        }
        let stable_frames = stability_outcome.map_or(0, StabilityOutcome::stable_frames);
        let qualified = result.matched
            && stability_outcome.is_some_and(StabilityOutcome::qualified_current_frame);
        let score = result
            .best
            .as_ref()
            .map(|cluster| f64::from(cluster.best.score));
        let match_rect = selected.as_ref().map(|candidate| candidate.rect);
        let details = serde_json::json!({
            "source_block_id": source_block_id,
            "rule_id": rule.id,
            "rule_revision": rule.revision,
            "region_id": region.id,
            "region_revision": region.revision,
            "cluster_count": result.clusters.len(),
            "runner_up_score": result.runner_up.as_ref().map(|cluster| cluster.best.score),
            "ambiguity_margin": result.ambiguity_margin,
            "selected_scale_percent": selected.as_ref().map(|candidate| candidate.scale_percent),
            "raw_best_score": raw.best.score,
        });
        Ok(DetectorEvidence::image_match(
            qualified,
            frame,
            match_rect,
            score,
            u32::try_from(result.clusters.len()).unwrap_or(u32::MAX),
            stable_frames,
            details,
        ))
    }
}

impl ConditionDetector for ImageDetector {
    fn observe(
        &self,
        request: &ObservationRequest<'_>,
        capture: &(dyn CaptureSource + Send + Sync),
    ) -> Result<DetectorEvidence> {
        match request.condition {
            Condition::Image {
                source_block_id,
                rule_id,
                ..
            } => self.observe_image(request, capture, source_block_id, rule_id),
            Condition::Text { .. } => bail!("image detector cannot observe a text condition"),
        }
    }
}

fn decode_gray_asset(
    request: &ObservationRequest<'_>,
    asset: &super::AssetRef,
    kind: &str,
) -> Result<GrayImage> {
    let pinned = request
        .compiled
        .pinned_assets
        .iter()
        .find(|pinned| pinned.asset == *asset)
        .with_context(|| format!("compiled image {kind} asset is missing"))?;
    image::load_from_memory(&pinned.bytes)
        .with_context(|| format!("compiled image {kind} asset cannot be decoded"))
        .map(|image| image.into_luma8())
}

fn validate_runtime_image_rule(
    definition: &MacroDefinition,
    rule: &ImageRule,
    template: &GrayImage,
    mask: Option<&GrayImage>,
    search_rect: Rect,
) -> Result<()> {
    verification::validate_decoded_rule(definition, rule, template, mask)?;
    let score_cells = estimate_score_cells(
        (search_rect.width, search_rect.height),
        template.dimensions(),
        &rule.scales_percent,
    );
    if score_cells > DEFAULT_MAX_SCORE_CELLS {
        bail!("image score-map work {score_cells} exceeds maximum {DEFAULT_MAX_SCORE_CELLS}");
    }
    Ok(())
}

impl StabilityTracker {
    pub fn new(required_frames: u8, minimum_elapsed_ms: u64, maximum_center_drift_px: u32) -> Self {
        Self {
            required_frames: required_frames.max(1),
            minimum_elapsed_ms,
            maximum_center_drift_px,
            last: None,
            stable_frames: 0,
        }
    }

    pub fn observe(&mut self, observed: StableImageMatch) -> StabilityOutcome {
        let Some(previous) = self.last.as_ref() else {
            self.last = Some(observed);
            self.stable_frames = 1;
            return StabilityOutcome::Accepted {
                stable_frames: self.stable_frames,
                qualified: self.stable_frames >= self.required_frames,
            };
        };
        if !same_frame_identity(previous.frame, observed.frame) {
            self.last = Some(observed);
            self.stable_frames = 1;
            return StabilityOutcome::Reset { stable_frames: 1 };
        }
        if observed.frame.frame_id == previous.frame.frame_id {
            return StabilityOutcome::Ignored {
                stable_frames: self.stable_frames,
            };
        }
        if observed
            .frame
            .captured_at_ms
            .saturating_sub(previous.frame.captured_at_ms)
            < self.minimum_elapsed_ms
        {
            return StabilityOutcome::Ignored {
                stable_frames: self.stable_frames,
            };
        }
        let compatible = previous.candidate.scale_percent == observed.candidate.scale_percent
            && centers_within(
                previous.candidate.rect,
                observed.candidate.rect,
                self.maximum_center_drift_px as f32,
            );
        self.stable_frames = if compatible {
            self.stable_frames.saturating_add(1)
        } else {
            1
        };
        self.last = Some(observed);
        if compatible {
            StabilityOutcome::Accepted {
                stable_frames: self.stable_frames,
                qualified: self.stable_frames >= self.required_frames,
            }
        } else {
            StabilityOutcome::Reset { stable_frames: 1 }
        }
    }

    pub fn reset(&mut self) {
        self.last = None;
        self.stable_frames = 0;
    }

    pub fn stable_frames(&self) -> u8 {
        self.stable_frames
    }
}

fn same_frame_identity(left: ImageFrameMetadata, right: ImageFrameMetadata) -> bool {
    left.window_id == right.window_id
        && left.window_revision == right.window_revision
        && left.geometry_revision == right.geometry_revision
        && left.display_profile_revision == right.display_profile_revision
        && left.dpi == right.dpi
        && left.region_revision == right.region_revision
        && left.rule_revision == right.rule_revision
}

#[derive(Debug, Clone)]
pub struct ImageRuleVerificationInput<'a> {
    pub rule: &'a ImageRule,
    pub template: &'a GrayImage,
    pub mask: Option<&'a GrayImage>,
    pub captured_dpi: u32,
    pub current_dpi: u32,
    pub region_revision: u64,
    pub search_dimensions: (u32, u32),
    pub negative_corpus_sha256: &'a str,
    pub negative_sample_count: u64,
    pub best_negative_score: f32,
    pub observed_clusters: &'a [CandidateCluster],
    pub maximum_score_cells: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageRuleVerification {
    pub threshold: f32,
    pub template_variance: f32,
    pub negative_margin: f32,
    pub ambiguity_margin: Option<f32>,
    pub score_cells: u64,
    pub artifact: ImageRuleVerificationArtifact,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ImageRuleVerificationError {
    #[error("image threshold must be finite and between zero and one")]
    InvalidThreshold,
    #[error("runner-up margin must be finite and between zero and one")]
    InvalidRunnerUpMargin,
    #[error("best negative score must be finite and between zero and one")]
    InvalidNegativeScore,
    #[error("negative corpus must have a canonical lowercase SHA-256 and at least one sample")]
    InvalidNegativeCorpus,
    #[error("configured transparent mask asset is missing")]
    MissingMask,
    #[error("transparent mask is invalid: {reason}")]
    InvalidMask { reason: String },
    #[error("template variance {variance} is below {minimum}")]
    LowTemplateVariance { variance: f32, minimum: f32 },
    #[error("template DPI {captured} is stale for current DPI {current}")]
    StaleDpi { captured: u32, current: u32 },
    #[error("score-map work {score_cells} exceeds configured maximum {maximum}")]
    WorkLimitExceeded { score_cells: u64, maximum: u64 },
    #[error("negative margin {margin} is below required {minimum}")]
    InsufficientNegativeMargin { margin: f32, minimum: f32 },
    #[error("candidate ambiguity margin {margin} is below required {minimum}")]
    AmbiguousCandidates { margin: f32, minimum: f32 },
}

impl ImageRuleVerification {
    pub fn verify(
        input: ImageRuleVerificationInput<'_>,
    ) -> std::result::Result<Self, ImageRuleVerificationError> {
        if !verification::normalized_score(input.rule.threshold) {
            return Err(ImageRuleVerificationError::InvalidThreshold);
        }
        if !verification::normalized_score(input.rule.minimum_runner_up_margin) {
            return Err(ImageRuleVerificationError::InvalidRunnerUpMargin);
        }
        if !verification::normalized_score(input.best_negative_score) {
            return Err(ImageRuleVerificationError::InvalidNegativeScore);
        }
        if !verification::valid_sha256(input.negative_corpus_sha256)
            || input.negative_sample_count == 0
        {
            return Err(ImageRuleVerificationError::InvalidNegativeCorpus);
        }
        let mask = validate_mask_reference(input.rule, input.template, input.mask)?;
        let variance = template_variance(input.template, mask);
        if variance < MIN_TEMPLATE_VARIANCE {
            return Err(ImageRuleVerificationError::LowTemplateVariance {
                variance,
                minimum: MIN_TEMPLATE_VARIANCE,
            });
        }
        if input.captured_dpi != input.current_dpi {
            return Err(ImageRuleVerificationError::StaleDpi {
                captured: input.captured_dpi,
                current: input.current_dpi,
            });
        }
        let score_cells = estimate_score_cells(
            input.search_dimensions,
            input.template.dimensions(),
            &input.rule.scales_percent,
        );
        if score_cells > input.maximum_score_cells {
            return Err(ImageRuleVerificationError::WorkLimitExceeded {
                score_cells,
                maximum: input.maximum_score_cells,
            });
        }
        let negative_margin = input.rule.threshold - input.best_negative_score;
        if negative_margin < input.rule.minimum_runner_up_margin {
            return Err(ImageRuleVerificationError::InsufficientNegativeMargin {
                margin: negative_margin,
                minimum: input.rule.minimum_runner_up_margin,
            });
        }
        let mut clusters = input.observed_clusters.to_vec();
        clusters.sort_by(cluster_order);
        let ambiguity_margin = clusters
            .first()
            .zip(clusters.get(1))
            .map(|(first, second)| first.best.score - second.best.score);
        if ambiguity_margin.is_some_and(|margin| margin < input.rule.minimum_runner_up_margin) {
            return Err(ImageRuleVerificationError::AmbiguousCandidates {
                margin: ambiguity_margin.unwrap(),
                minimum: input.rule.minimum_runner_up_margin,
            });
        }
        let mut artifact = ImageRuleVerificationArtifact {
            version: IMAGE_RULE_VERIFICATION_VERSION,
            preprocess: ImageVerificationPreprocess::GrayscaleNormalizedCrossCorrelation,
            rule_id: input.rule.id.clone(),
            rule_revision: input.rule.revision,
            template: input.rule.template.clone(),
            transparent_mask: input.rule.transparent_mask.clone(),
            captured_dpi: input.captured_dpi,
            region_id: input.rule.region_id.clone(),
            region_revision: input.region_revision,
            search_width: input.search_dimensions.0,
            search_height: input.search_dimensions.1,
            scales_percent: input.rule.scales_percent.clone(),
            threshold: input.rule.threshold,
            minimum_runner_up_margin: input.rule.minimum_runner_up_margin,
            negative_corpus_sha256: input.negative_corpus_sha256.to_string(),
            negative_sample_count: input.negative_sample_count,
            best_negative_score: input.best_negative_score,
            active_mask_variance: variance,
            verification_fingerprint_sha256: String::new(),
        };
        artifact.verification_fingerprint_sha256 = verification::fingerprint(&artifact);
        Ok(Self {
            threshold: input.rule.threshold,
            template_variance: variance,
            negative_margin,
            ambiguity_margin,
            score_cells,
            artifact,
        })
    }
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

fn candidate_order(left: &ImageMatchCandidate, right: &ImageMatchCandidate) -> std::cmp::Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| left.rect.y.cmp(&right.rect.y))
        .then_with(|| left.rect.x.cmp(&right.rect.x))
        .then_with(|| left.scale_percent.cmp(&right.scale_percent))
}

fn cluster_order(left: &CandidateCluster, right: &CandidateCluster) -> std::cmp::Ordering {
    candidate_order(&left.best, &right.best)
}

fn scale_delta(left: u16, right: u16) -> u16 {
    left.abs_diff(right)
}

fn centers_within(left: Rect, right: Rect, maximum_distance: f32) -> bool {
    let left_x = f64::from(left.x) + f64::from(left.width) / 2.0;
    let left_y = f64::from(left.y) + f64::from(left.height) / 2.0;
    let right_x = f64::from(right.x) + f64::from(right.width) / 2.0;
    let right_y = f64::from(right.y) + f64::from(right.height) / 2.0;
    let dx = left_x - right_x;
    let dy = left_y - right_y;
    dx * dx + dy * dy <= f64::from(maximum_distance).powi(2)
}

fn overlap_ratio(left: Rect, right: Rect) -> f32 {
    let left_right = i64::from(left.x) + i64::from(left.width);
    let left_bottom = i64::from(left.y) + i64::from(left.height);
    let right_right = i64::from(right.x) + i64::from(right.width);
    let right_bottom = i64::from(right.y) + i64::from(right.height);
    let width = (left_right.min(right_right) - i64::from(left.x.max(right.x))).max(0) as u64;
    let height = (left_bottom.min(right_bottom) - i64::from(left.y.max(right.y))).max(0) as u64;
    let intersection = width.saturating_mul(height);
    let union = u64::from(left.width)
        .saturating_mul(u64::from(left.height))
        .saturating_add(u64::from(right.width).saturating_mul(u64::from(right.height)))
        .saturating_sub(intersection);
    if union == 0 {
        0.0
    } else {
        intersection as f32 / union as f32
    }
}

fn candidates_represent_same_object(
    left: &ImageMatchCandidate,
    right: &ImageMatchCandidate,
    policy: ClusterPolicy,
) -> bool {
    scale_delta(left.scale_percent, right.scale_percent) <= policy.maximum_scale_delta_percent
        && (overlap_ratio(left.rect, right.rect) >= policy.minimum_overlap_ratio
            || centers_within(left.rect, right.rect, policy.maximum_center_distance_px))
}

pub fn cluster_peaks(
    mut peaks: Vec<ImageMatchCandidate>,
    policy: ClusterPolicy,
) -> Vec<CandidateCluster> {
    peaks.sort_by(candidate_order);
    let mut clusters: Vec<CandidateCluster> = Vec::new();
    for peak in peaks {
        if let Some(cluster) = clusters.iter_mut().find(|cluster| {
            cluster
                .members
                .iter()
                .any(|member| candidates_represent_same_object(member, &peak, policy))
        }) {
            cluster.add(peak);
        } else {
            clusters.push(CandidateCluster::from_peak(peak));
        }
    }
    clusters.sort_by(cluster_order);
    clusters
}

fn extract_local_maxima(
    scores: &ImageBuffer<Luma<f32>, Vec<f32>>,
    template_dimensions: (u32, u32),
    scale_percent: u16,
    threshold: f32,
) -> Result<Vec<ImageMatchCandidate>> {
    let mut maxima = Vec::new();
    for y in 0..scores.height() {
        for x in 0..scores.width() {
            let score = scores.get_pixel(x, y)[0];
            if score < threshold {
                continue;
            }
            let mut is_maximum = true;
            'neighbors: for neighbor_y in y.saturating_sub(1)..=(y + 1).min(scores.height() - 1) {
                for neighbor_x in x.saturating_sub(1)..=(x + 1).min(scores.width() - 1) {
                    if neighbor_x == x && neighbor_y == y {
                        continue;
                    }
                    let neighbor = scores.get_pixel(neighbor_x, neighbor_y)[0];
                    if neighbor > score || (neighbor == score && (neighbor_y, neighbor_x) < (y, x))
                    {
                        is_maximum = false;
                        break 'neighbors;
                    }
                }
            }
            if is_maximum {
                maxima.push(ImageMatchCandidate {
                    rect: Rect::new(
                        i32::try_from(x).map_err(|_| ImageMatchError::CoordinateOverflow)?,
                        i32::try_from(y).map_err(|_| ImageMatchError::CoordinateOverflow)?,
                        template_dimensions.0,
                        template_dimensions.1,
                    ),
                    score,
                    scale_percent,
                });
            }
        }
    }
    Ok(maxima)
}

fn offset_rect(rect: Rect, origin: Rect) -> Result<Rect> {
    Ok(Rect::new(
        rect.x
            .checked_add(origin.x)
            .ok_or(ImageMatchError::CoordinateOverflow)?,
        rect.y
            .checked_add(origin.y)
            .ok_or(ImageMatchError::CoordinateOverflow)?,
        rect.width,
        rect.height,
    ))
}

fn masked_match_template(
    search: &GrayImage,
    template: &GrayImage,
    mask: &GrayImage,
) -> ImageBuffer<Luma<f32>, Vec<f32>> {
    let output_width = search.width() - template.width() + 1;
    let output_height = search.height() - template.height() + 1;
    let active = mask
        .enumerate_pixels()
        .filter_map(|(x, y, pixel)| (pixel[0] != 0).then_some((x, y, f64::from(pixel[0]) / 255.0)))
        .collect::<Vec<_>>();
    let template_energy = active
        .iter()
        .map(|&(x, y, weight)| {
            let value = f64::from(template.get_pixel(x, y)[0]) * weight;
            value.powi(2)
        })
        .sum::<f64>();
    ImageBuffer::from_fn(output_width, output_height, |origin_x, origin_y| {
        let mut numerator = 0.0;
        let mut search_energy = 0.0;
        for &(x, y, weight) in &active {
            let template_value = f64::from(template.get_pixel(x, y)[0]) * weight;
            let search_value = f64::from(search.get_pixel(origin_x + x, origin_y + y)[0]) * weight;
            numerator += template_value * search_value;
            search_energy += search_value.powi(2);
        }
        let denominator = (template_energy * search_energy).sqrt();
        let score = if denominator > f64::EPSILON {
            (numerator / denominator).clamp(-1.0, 1.0) as f32
        } else {
            0.0
        };
        Luma([score])
    })
}

fn validate_mask_reference<'a>(
    rule: &ImageRule,
    template: &GrayImage,
    mask: Option<&'a GrayImage>,
) -> std::result::Result<Option<&'a GrayImage>, ImageRuleVerificationError> {
    match (&rule.transparent_mask, mask) {
        (Some(_), None) => Err(ImageRuleVerificationError::MissingMask),
        (None, Some(_)) => Err(ImageRuleVerificationError::InvalidMask {
            reason: "mask bytes were supplied for a rule without a mask asset".to_string(),
        }),
        (_, Some(mask)) if mask.dimensions() != template.dimensions() => {
            Err(ImageRuleVerificationError::InvalidMask {
                reason: "mask dimensions do not match the template".to_string(),
            })
        }
        (_, Some(mask)) if !mask.pixels().any(|pixel| pixel[0] != 0) => {
            Err(ImageRuleVerificationError::InvalidMask {
                reason: "mask excludes every template pixel".to_string(),
            })
        }
        (_, mask) => Ok(mask),
    }
}

fn template_variance(template: &GrayImage, mask: Option<&GrayImage>) -> f32 {
    let values = template
        .enumerate_pixels()
        .filter(|(x, y, _)| mask.is_none_or(|mask| mask.get_pixel(*x, *y)[0] != 0))
        .map(|(_, _, pixel)| f64::from(pixel[0]))
        .collect::<Vec<_>>();
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    (values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / values.len() as f64) as f32
}

fn estimate_score_cells(
    search_dimensions: (u32, u32),
    template_dimensions: (u32, u32),
    scales_percent: &[u16],
) -> u64 {
    scales_percent
        .iter()
        .filter_map(|&scale| {
            let width = scaled_dimension(template_dimensions.0, scale).ok()?;
            let height = scaled_dimension(template_dimensions.1, scale).ok()?;
            (width > 0
                && height > 0
                && width <= search_dimensions.0
                && height <= search_dimensions.1)
                .then(|| {
                    u64::from(search_dimensions.0 - width + 1)
                        .saturating_mul(u64::from(search_dimensions.1 - height + 1))
                })
        })
        .fold(0_u64, u64::saturating_add)
}

impl ImageMatcher {
    pub fn match_screen_image(
        &self,
        search: &ScreenImage,
        capture_bounds: Rect,
        template: &GrayImage,
        config: &ImageMatchConfig,
    ) -> Result<RawImageMatch> {
        self.match_screen_image_masked(search, capture_bounds, template, None, config)
    }

    pub fn match_screen_image_masked(
        &self,
        search: &ScreenImage,
        capture_bounds: Rect,
        template: &GrayImage,
        mask: Option<&GrayImage>,
        config: &ImageMatchConfig,
    ) -> Result<RawImageMatch> {
        if search.rgba.dimensions() != (capture_bounds.width, capture_bounds.height) {
            bail!("capture bounds dimensions do not match screen image");
        }
        let gray = image::DynamicImage::ImageRgba8(search.rgba.clone()).into_luma8();
        let mut result = self.match_template_masked(&gray, template, mask, config)?;
        for candidate in &mut result.candidates {
            candidate.rect = offset_rect(candidate.rect, capture_bounds)?;
        }
        result.best.rect = offset_rect(result.best.rect, capture_bounds)?;
        Ok(result)
    }

    pub fn match_template(
        &self,
        search: &GrayImage,
        template: &GrayImage,
        config: &ImageMatchConfig,
    ) -> Result<RawImageMatch> {
        self.match_template_masked(search, template, None, config)
    }

    pub fn match_template_masked(
        &self,
        search: &GrayImage,
        template: &GrayImage,
        mask: Option<&GrayImage>,
        config: &ImageMatchConfig,
    ) -> Result<RawImageMatch> {
        if !(0.0..=1.0).contains(&config.threshold) {
            bail!("image match threshold must be between 0 and 1");
        }
        if config.scales_percent.is_empty() {
            bail!("image match requires at least one scale");
        }
        if let Some(mask) = mask {
            if mask.dimensions() != template.dimensions() {
                bail!("image match mask dimensions must match the template");
            }
            if !mask.pixels().any(|pixel| pixel[0] != 0) {
                bail!("image match mask must retain at least one template pixel");
            }
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
            let scaled_mask = mask.map(|mask| {
                if scale_percent == 100 {
                    mask.clone()
                } else {
                    imageops::resize(mask, width, height, imageops::FilterType::Nearest)
                }
            });
            let scores = scaled_mask.as_ref().map_or_else(
                || {
                    match_template(
                        search,
                        &scaled,
                        MatchTemplateMethod::CrossCorrelationNormalized,
                    )
                },
                |mask| masked_match_template(search, &scaled, mask),
            );
            for (x, y, pixel) in scores.enumerate_pixels() {
                let candidate = ImageMatchCandidate {
                    rect: Rect::new(
                        i32::try_from(x).map_err(|_| ImageMatchError::CoordinateOverflow)?,
                        i32::try_from(y).map_err(|_| ImageMatchError::CoordinateOverflow)?,
                        width,
                        height,
                    ),
                    score: pixel[0],
                    scale_percent,
                };
                if best
                    .as_ref()
                    .is_none_or(|current| candidate.score > current.score)
                {
                    best = Some(candidate.clone());
                }
            }
            candidates.extend(extract_local_maxima(
                &scores,
                (width, height),
                scale_percent,
                config.threshold,
            )?);
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
    use crate::engine::macro_engine::{
        AssetRef, Block, BlockKind, CompiledMacro, Condition, ConditionDetector, FocusLossPolicy,
        ImageRule, Limit, MACRO_SCHEMA_VERSION, MacroDefinition, MatchSelectionPolicy,
        ObservationRequest, ObserveMode, PinnedAsset, RegionDefinition, SafetyPolicy,
        SavedRevision, TargetProfile,
    };
    use crate::engine::{
        automation::{CaptureFrameMetadata, CaptureSource, CapturedScreenFrame},
        types::{RectRatio, ScreenImage},
    };
    use image::{DynamicImage, GrayImage, ImageFormat, Luma};
    use sha2::{Digest, Sha256};
    use std::{io::Cursor, sync::Mutex};

    type StabilityMutation = Box<dyn Fn(&mut StableImageMatch)>;
    const NEGATIVE_CORPUS_SHA256: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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

    fn peak(x: i32, y: i32, score: f32, scale_percent: u16) -> ImageMatchCandidate {
        let side = u32::from(scale_percent / 10);
        ImageMatchCandidate {
            rect: Rect::new(x, y, side, side),
            score,
            scale_percent,
        }
    }

    fn fixture_rule(threshold: f32) -> ImageRule {
        ImageRule {
            id: "image-rule".to_string(),
            revision: 7,
            region_id: "region".to_string(),
            template: AssetRef {
                id: "template".to_string(),
                revision: 2,
                content_hash: "hash".to_string(),
            },
            transparent_mask: None,
            threshold,
            scales_percent: vec![95, 100, 105],
            stable_frames: 2,
            maximum_center_drift_px: 3,
            minimum_runner_up_margin: 0.03,
            verification: None,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 100,
            timeout_ms: Limit::Finite(1_000),
        }
    }

    fn frame(frame_id: u64, captured_at_ms: u64, x: i32, scale_percent: u16) -> StableImageMatch {
        StableImageMatch {
            frame: ImageFrameMetadata {
                frame_id,
                captured_at_ms,
                window_id: 11,
                window_revision: 3,
                geometry_revision: 5,
                display_profile_revision: 7,
                dpi: 96,
                region_revision: 13,
                rule_revision: 17,
            },
            candidate: peak(x, 20, 0.98, scale_percent),
        }
    }

    fn capture_metadata(frame: ImageFrameMetadata) -> CaptureFrameMetadata {
        CaptureFrameMetadata {
            frame_id: frame.frame_id,
            captured_at_ms: frame.captured_at_ms,
            window_id: frame.window_id,
            window_revision: frame.window_revision,
            geometry_revision: frame.geometry_revision,
            display_profile_revision: frame.display_profile_revision,
            dpi: frame.dpi,
        }
    }

    fn captured_frame(image: GrayImage, frame: ImageFrameMetadata) -> CapturedScreenFrame {
        CapturedScreenFrame {
            image: ScreenImage::new(DynamicImage::ImageLuma8(image).into_rgba8()),
            metadata: capture_metadata(frame),
        }
    }

    fn png_bytes(image: GrayImage) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageLuma8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn hash(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn saved_image_macro(template: GrayImage, forge_verification: bool) -> SavedRevision {
        let bytes = png_bytes(template.clone());
        let asset = AssetRef {
            id: "template".to_string(),
            revision: 1,
            content_hash: hash(&bytes),
        };
        let mut rule = ImageRule {
            template: asset.clone(),
            scales_percent: vec![100],
            ..fixture_rule(0.95)
        };
        rule.verification = Some(if forge_verification {
            let mut artifact = ImageRuleVerificationArtifact {
                version: IMAGE_RULE_VERIFICATION_VERSION,
                preprocess: ImageVerificationPreprocess::GrayscaleNormalizedCrossCorrelation,
                rule_id: rule.id.clone(),
                rule_revision: rule.revision,
                template: rule.template.clone(),
                transparent_mask: None,
                captured_dpi: 96,
                region_id: rule.region_id.clone(),
                region_revision: 13,
                search_width: 64,
                search_height: 48,
                scales_percent: rule.scales_percent.clone(),
                threshold: rule.threshold,
                minimum_runner_up_margin: rule.minimum_runner_up_margin,
                negative_corpus_sha256: NEGATIVE_CORPUS_SHA256.to_string(),
                negative_sample_count: 100_000,
                best_negative_score: 0.80,
                active_mask_variance: 42.0,
                verification_fingerprint_sha256: String::new(),
            };
            artifact.verification_fingerprint_sha256 = verification::fingerprint(&artifact);
            artifact
        } else {
            ImageRuleVerification::verify(ImageRuleVerificationInput {
                rule: &rule,
                template: &template,
                mask: None,
                captured_dpi: 96,
                current_dpi: 96,
                region_revision: 13,
                search_dimensions: (64, 48),
                negative_corpus_sha256: NEGATIVE_CORPUS_SHA256,
                negative_sample_count: 100_000,
                best_negative_score: 0.80,
                observed_clusters: &[],
                maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
            })
            .unwrap()
            .artifact
        });
        let definition = MacroDefinition {
            schema_version: MACRO_SCHEMA_VERSION,
            id: "macro".to_string(),
            name: "Macro".to_string(),
            revision: 1,
            target: TargetProfile {
                process_path: "diablo.exe".to_string(),
                window_class: "Diablo".to_string(),
                title_contains: "Diablo".to_string(),
                captured_client_width: 64,
                captured_client_height: 48,
                captured_dpi: 96,
            },
            regions: vec![RegionDefinition {
                id: "region".to_string(),
                revision: 13,
                rect: RectRatio {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                },
            }],
            points: vec![],
            text_rules: vec![],
            image_rules: vec![rule],
            blocks: vec![Block {
                id: "observe".to_string(),
                enabled: true,
                kind: BlockKind::Observe {
                    condition: Condition::Image {
                        source_block_id: "observe".to_string(),
                        rule_id: "image-rule".to_string(),
                        mode: ObserveMode::CheckNow,
                    },
                },
            }],
            safety: SafetyPolicy {
                max_runtime_ms: Limit::Finite(10_000),
                max_clicks: Limit::Finite(10),
                max_observation_retries: Limit::Finite(10),
                max_observations_per_second: 20,
                minimum_click_interval_ms: 50,
                focus_loss: FocusLossPolicy::Stop,
            },
        };
        let definition_hash = hash(&serde_json::to_vec_pretty(&definition).unwrap());
        SavedRevision {
            definition,
            definition_hash,
            pinned_assets: vec![PinnedAsset { asset, bytes }],
        }
    }

    fn compiled_image_macro(template: GrayImage) -> CompiledMacro {
        CompiledMacro::compile(saved_image_macro(template, false)).unwrap()
    }

    struct FixtureCapture {
        frames: Mutex<Vec<CapturedScreenFrame>>,
    }

    impl CaptureSource for FixtureCapture {
        fn capture(&self, _rect: Rect) -> Result<ScreenImage> {
            bail!("image detector must not request pixels separately from metadata")
        }

        fn capture_frame(&self, _rect: Rect) -> Result<CapturedScreenFrame> {
            Ok(self.frames.lock().unwrap().remove(0))
        }
    }

    #[test]
    fn compile_rejects_forged_verification_for_flat_template_pixels() {
        let saved = saved_image_macro(GrayImage::from_pixel(7, 5, Luma([80])), true);

        let error = CompiledMacro::compile(saved).unwrap_err();

        assert!(error.to_string().contains("template variance"));
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

    #[test]
    fn screen_match_rejects_capture_coordinate_overflow() {
        let search = fixture_search_with_icon_at(1, 1);
        let rgba = DynamicImage::ImageLuma8(search).into_rgba8();
        let error = ImageMatcher
            .match_screen_image(
                &ScreenImage::new(rgba),
                Rect::new(i32::MAX, i32::MAX, 64, 48),
                &fixture_icon(),
                &ImageMatchConfig::exact_scale(0.95),
            )
            .unwrap_err();

        assert!(matches!(
            error.downcast_ref::<ImageMatchError>(),
            Some(ImageMatchError::CoordinateOverflow)
        ));
    }

    #[test]
    fn adjacent_score_peaks_form_one_visual_candidate() {
        let peaks = vec![peak(20, 20, 0.97, 100), peak(21, 20, 0.96, 100)];

        let clusters = cluster_peaks(peaks, ClusterPolicy::default());

        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].best.score, 0.97);
        assert_eq!(clusters[0].members.len(), 2);
    }

    #[test]
    fn same_object_across_scales_merges_before_exactly_one() {
        let peaks = vec![peak(20, 20, 0.97, 95), peak(20, 20, 0.98, 100)];
        let rule = fixture_rule(0.95);

        let result =
            ImageMatchResult::select(cluster_peaks(peaks, ClusterPolicy::default()), &rule);

        assert!(result.matched);
        assert_eq!(result.clusters.len(), 1);
        assert_eq!(result.selected.as_ref().unwrap().best.scale_percent, 100);
    }

    #[test]
    fn ambiguity_uses_distinct_runner_up_cluster() {
        let peaks = vec![
            peak(20, 20, 0.98, 100),
            peak(21, 20, 0.975, 100),
            peak(80, 20, 0.96, 100),
        ];
        let mut rule = fixture_rule(0.95);
        rule.match_policy = MatchSelectionPolicy::HighestScore;
        rule.minimum_runner_up_margin = 0.03;

        let result =
            ImageMatchResult::select(cluster_peaks(peaks, ClusterPolicy::default()), &rule);

        assert!(!result.matched);
        assert_eq!(result.runner_up.as_ref().unwrap().best.score, 0.96);
        assert!((result.ambiguity_margin.unwrap() - 0.02).abs() < 0.0001);
    }

    #[test]
    fn bottommost_policy_is_deterministic_at_minimum_screen_coordinate() {
        let mut rule = fixture_rule(0.95);
        rule.match_policy = MatchSelectionPolicy::Bottommost;
        rule.minimum_runner_up_margin = 0.0;
        let clusters = vec![
            CandidateCluster::from_peak(peak(i32::MIN, 20, 0.98, 100)),
            CandidateCluster::from_peak(peak(0, 20, 0.97, 100)),
        ];

        let result = ImageMatchResult::select(clusters, &rule);

        assert_eq!(result.selected.unwrap().best.rect.x, i32::MIN);
    }

    #[test]
    fn local_maxima_remove_adjacent_score_map_pixels_before_clustering() {
        let mut scores = ImageBuffer::from_pixel(8, 4, Luma([0.10_f32]));
        scores.put_pixel(2, 2, Luma([0.97]));
        scores.put_pixel(3, 2, Luma([0.96]));
        scores.put_pixel(7, 1, Luma([0.95]));

        let maxima = extract_local_maxima(&scores, (7, 5), 100, 0.90).unwrap();

        assert_eq!(maxima.len(), 2);
        assert!(
            maxima
                .iter()
                .any(|peak| (peak.rect.x, peak.rect.y) == (2, 2))
        );
        assert!(
            maxima
                .iter()
                .any(|peak| (peak.rect.x, peak.rect.y) == (7, 1))
        );
    }

    #[test]
    fn same_frame_cannot_satisfy_two_frame_stability() {
        let mut tracker = StabilityTracker::new(2, 40, 3);

        assert_eq!(
            tracker.observe(frame(7, 100, 20, 100)),
            StabilityOutcome::Accepted {
                stable_frames: 1,
                qualified: false,
            }
        );
        assert_eq!(
            tracker.observe(frame(7, 150, 20, 100)),
            StabilityOutcome::Ignored { stable_frames: 1 }
        );
        assert_eq!(
            tracker.observe(frame(8, 160, 21, 100)),
            StabilityOutcome::Accepted {
                stable_frames: 2,
                qualified: true,
            }
        );
    }

    #[test]
    fn stability_requires_elapsed_separation_and_exact_scale() {
        let mut tracker = StabilityTracker::new(2, 40, 3);

        assert!(matches!(
            tracker.observe(frame(1, 100, 20, 100)),
            StabilityOutcome::Accepted {
                qualified: false,
                ..
            }
        ));
        assert!(matches!(
            tracker.observe(frame(2, 120, 20, 100)),
            StabilityOutcome::Ignored { .. }
        ));
        assert!(matches!(
            tracker.observe(frame(3, 160, 20, 105)),
            StabilityOutcome::Reset { stable_frames: 1 }
        ));
        assert!(matches!(
            tracker.observe(frame(4, 205, 21, 105)),
            StabilityOutcome::Accepted {
                qualified: true,
                ..
            }
        ));
    }

    #[test]
    fn identity_reset_precedes_minimum_elapsed_and_ignored_frame_cannot_qualify() {
        let mut tracker = StabilityTracker::new(2, 40, 3);
        let _ = tracker.observe(frame(1, 100, 20, 100));
        assert!(matches!(
            tracker.observe(frame(2, 160, 20, 100)),
            StabilityOutcome::Accepted {
                qualified: true,
                ..
            }
        ));
        let mut identity_b = frame(3, 170, 80, 100);
        identity_b.frame.window_revision += 1;

        assert_eq!(
            tracker.observe(identity_b.clone()),
            StabilityOutcome::Reset { stable_frames: 1 }
        );
        let mut too_close_b = identity_b.clone();
        too_close_b.frame.frame_id = 4;
        too_close_b.frame.captured_at_ms = 180;
        assert_eq!(
            tracker.observe(too_close_b),
            StabilityOutcome::Ignored { stable_frames: 1 }
        );
        let mut eligible_b = identity_b;
        eligible_b.frame.frame_id = 5;
        eligible_b.frame.captured_at_ms = 220;
        assert!(matches!(
            tracker.observe(eligible_b),
            StabilityOutcome::Accepted {
                stable_frames: 2,
                qualified: true,
            }
        ));
    }

    #[test]
    fn any_frame_identity_or_revision_change_resets_stability() {
        let mut mutations: Vec<StabilityMutation> = vec![
            Box::new(|value| value.frame.window_id += 1),
            Box::new(|value| value.frame.window_revision += 1),
            Box::new(|value| value.frame.geometry_revision += 1),
            Box::new(|value| value.frame.display_profile_revision += 1),
            Box::new(|value| value.frame.dpi += 24),
            Box::new(|value| value.frame.region_revision += 1),
            Box::new(|value| value.frame.rule_revision += 1),
        ];
        for mutate in mutations.drain(..) {
            let mut tracker = StabilityTracker::new(2, 40, 3);
            let _ = tracker.observe(frame(1, 100, 20, 100));
            let mut changed = frame(2, 160, 20, 100);
            mutate(&mut changed);
            assert_eq!(
                tracker.observe(changed),
                StabilityOutcome::Reset { stable_frames: 1 }
            );
            assert_eq!(tracker.stable_frames(), 1);
        }
    }

    #[test]
    fn masked_matching_ignores_transparent_template_pixels() {
        let mut template = fixture_icon();
        let mut search = fixture_search_with_icon_at(23, 17);
        template.put_pixel(0, 0, Luma([255]));
        search.put_pixel(23, 17, Luma([0]));
        let mask = GrayImage::from_fn(template.width(), template.height(), |x, y| {
            Luma([u8::from((x, y) != (0, 0)) * 255])
        });

        let result = ImageMatcher
            .match_template_masked(
                &search,
                &template,
                Some(&mask),
                &ImageMatchConfig::exact_scale(0.95),
            )
            .unwrap();

        assert_eq!((result.best.rect.x, result.best.rect.y), (23, 17));
        assert!(result.best.score >= 0.95);
    }

    #[test]
    fn fully_opaque_mask_preserves_unmasked_normalized_correlation() {
        let template = fixture_icon();
        let search = GrayImage::from_fn(template.width(), template.height(), |x, y| {
            Luma([template.get_pixel(x, y)[0].saturating_add(10)])
        });
        let mask = GrayImage::from_pixel(template.width(), template.height(), Luma([255]));
        let config = ImageMatchConfig::exact_scale(0.0);

        let unmasked = ImageMatcher
            .match_template(&search, &template, &config)
            .unwrap();
        let masked = ImageMatcher
            .match_template_masked(&search, &template, Some(&mask), &config)
            .unwrap();

        assert!((unmasked.best.score - masked.best.score).abs() < 0.000_001);
    }

    #[test]
    fn verification_rejects_low_variance_stale_dpi_and_oversized_work() {
        let rule = fixture_rule(0.91);
        let flat = GrayImage::from_pixel(8, 8, Luma([128]));
        let clusters = vec![CandidateCluster::from_peak(peak(20, 20, 0.96, 100))];
        let base = ImageRuleVerificationInput {
            rule: &rule,
            template: &flat,
            mask: None,
            captured_dpi: 96,
            current_dpi: 96,
            region_revision: 13,
            search_dimensions: (640, 360),
            negative_corpus_sha256: NEGATIVE_CORPUS_SHA256,
            negative_sample_count: 100_000,
            best_negative_score: 0.80,
            observed_clusters: &clusters,
            maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
        };

        assert!(matches!(
            ImageRuleVerification::verify(base.clone()).unwrap_err(),
            ImageRuleVerificationError::LowTemplateVariance { .. }
        ));

        let varied = fixture_icon();
        let mut stale = base.clone();
        stale.template = &varied;
        stale.current_dpi = 120;
        assert_eq!(
            ImageRuleVerification::verify(stale).unwrap_err(),
            ImageRuleVerificationError::StaleDpi {
                captured: 96,
                current: 120
            }
        );

        let mut oversized = base;
        oversized.template = &varied;
        oversized.search_dimensions = (4_000, 4_000);
        assert!(matches!(
            ImageRuleVerification::verify(oversized).unwrap_err(),
            ImageRuleVerificationError::WorkLimitExceeded { .. }
        ));
    }

    #[test]
    fn verification_rejects_non_finite_or_out_of_range_margin_and_negative_scores() {
        let template = fixture_icon();
        for value in [f32::NAN, f32::INFINITY, -0.01, 1.01] {
            let mut rule = fixture_rule(0.91);
            rule.minimum_runner_up_margin = value;
            let input = ImageRuleVerificationInput {
                rule: &rule,
                template: &template,
                mask: None,
                captured_dpi: 96,
                current_dpi: 96,
                region_revision: 13,
                search_dimensions: (640, 360),
                negative_corpus_sha256: NEGATIVE_CORPUS_SHA256,
                negative_sample_count: 100_000,
                best_negative_score: 0.80,
                observed_clusters: &[],
                maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
            };
            assert_eq!(
                ImageRuleVerification::verify(input).unwrap_err(),
                ImageRuleVerificationError::InvalidRunnerUpMargin
            );

            let rule = fixture_rule(0.91);
            let input = ImageRuleVerificationInput {
                rule: &rule,
                template: &template,
                mask: None,
                captured_dpi: 96,
                current_dpi: 96,
                region_revision: 13,
                search_dimensions: (640, 360),
                negative_corpus_sha256: NEGATIVE_CORPUS_SHA256,
                negative_sample_count: 100_000,
                best_negative_score: value,
                observed_clusters: &[],
                maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
            };
            assert_eq!(
                ImageRuleVerification::verify(input).unwrap_err(),
                ImageRuleVerificationError::InvalidNegativeScore
            );
        }
    }

    #[test]
    fn verification_rejects_invalid_negative_corpus_provenance() {
        let rule = fixture_rule(0.91);
        let template = fixture_icon();
        for (digest, sample_count) in [
            ("", 100_000),
            (
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                100_000,
            ),
            (NEGATIVE_CORPUS_SHA256, 0),
        ] {
            let input = ImageRuleVerificationInput {
                rule: &rule,
                template: &template,
                mask: None,
                captured_dpi: 96,
                current_dpi: 96,
                region_revision: 13,
                search_dimensions: (640, 360),
                negative_corpus_sha256: digest,
                negative_sample_count: sample_count,
                best_negative_score: 0.80,
                observed_clusters: &[],
                maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
            };
            assert_eq!(
                ImageRuleVerification::verify(input).unwrap_err(),
                ImageRuleVerificationError::InvalidNegativeCorpus
            );
        }
    }

    #[test]
    fn verification_rejects_negative_margin_ambiguity_and_invalid_masks() {
        let mut rule = fixture_rule(0.91);
        rule.transparent_mask = Some(AssetRef {
            id: "mask".to_string(),
            revision: 1,
            content_hash: "mask-hash".to_string(),
        });
        let template = fixture_icon();
        let clusters = vec![
            CandidateCluster::from_peak(peak(20, 20, 0.96, 100)),
            CandidateCluster::from_peak(peak(80, 20, 0.945, 100)),
        ];
        let input = ImageRuleVerificationInput {
            rule: &rule,
            template: &template,
            mask: None,
            captured_dpi: 96,
            current_dpi: 96,
            region_revision: 13,
            search_dimensions: (640, 360),
            negative_corpus_sha256: NEGATIVE_CORPUS_SHA256,
            negative_sample_count: 100_000,
            best_negative_score: 0.90,
            observed_clusters: &clusters,
            maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
        };

        assert_eq!(
            ImageRuleVerification::verify(input.clone()).unwrap_err(),
            ImageRuleVerificationError::MissingMask
        );

        let invalid_mask = GrayImage::from_pixel(2, 2, Luma([0]));
        let mut invalid = input.clone();
        invalid.mask = Some(&invalid_mask);
        assert!(matches!(
            ImageRuleVerification::verify(invalid).unwrap_err(),
            ImageRuleVerificationError::InvalidMask { .. }
        ));

        let valid_mask = GrayImage::from_pixel(template.width(), template.height(), Luma([255]));
        let mut negative = input.clone();
        negative.mask = Some(&valid_mask);
        assert!(matches!(
            ImageRuleVerification::verify(negative).unwrap_err(),
            ImageRuleVerificationError::InsufficientNegativeMargin { .. }
        ));

        let mut ambiguous = input;
        ambiguous.mask = Some(&valid_mask);
        ambiguous.best_negative_score = 0.80;
        assert!(matches!(
            ImageRuleVerification::verify(ambiguous).unwrap_err(),
            ImageRuleVerificationError::AmbiguousCandidates { .. }
        ));
    }

    #[test]
    fn verification_accepts_rule_specific_threshold_below_initial_default() {
        let mut rule = fixture_rule(0.91);
        rule.scales_percent = vec![100];
        let template = fixture_icon();
        let clusters = vec![CandidateCluster::from_peak(peak(20, 20, 0.96, 100))];

        let verified = ImageRuleVerification::verify(ImageRuleVerificationInput {
            rule: &rule,
            template: &template,
            mask: None,
            captured_dpi: 96,
            current_dpi: 96,
            region_revision: 13,
            search_dimensions: (640, 360),
            negative_corpus_sha256: NEGATIVE_CORPUS_SHA256,
            negative_sample_count: 100_000,
            best_negative_score: 0.80,
            observed_clusters: &clusters,
            maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
        })
        .unwrap();

        assert_eq!(INITIAL_SIMILARITY_THRESHOLD, 0.95);
        assert_eq!(verified.threshold, 0.91);
        assert_eq!(verified.artifact.rule_id, rule.id);
        assert_eq!(verified.artifact.rule_revision, rule.revision);
        assert_eq!(verified.artifact.template, rule.template);
        assert_eq!(verified.artifact.region_revision, 13);
        assert_eq!(verified.artifact.scales_percent, vec![100]);
        assert_eq!(verified.artifact.best_negative_score, 0.80);
    }

    #[test]
    fn image_detector_emits_click_geometry_only_after_stable_immutable_frames() {
        let compiled = compiled_image_macro(fixture_icon());
        let condition = &compiled.definition().blocks[0];
        let BlockKind::Observe { condition } = &condition.kind else {
            unreachable!()
        };
        let first_frame = frame(1, 100, 23, 100).frame;
        let second_frame = frame(2, 200, 23, 100).frame;
        let detector = ImageDetector::new();
        let capture = FixtureCapture {
            frames: Mutex::new(vec![
                captured_frame(fixture_search_with_icon_at(23, 17), first_frame),
                captured_frame(fixture_search_with_icon_at(23, 17), second_frame),
            ]),
        };
        let first = detector
            .observe(
                &ObservationRequest {
                    run_id: "run",
                    generation: 1,
                    condition,
                    compiled: &compiled,
                    observed_at_ms: 100,
                },
                &capture,
            )
            .unwrap();
        let second = detector
            .observe(
                &ObservationRequest {
                    run_id: "run",
                    generation: 1,
                    condition,
                    compiled: &compiled,
                    observed_at_ms: 200,
                },
                &capture,
            )
            .unwrap();

        assert!(!first.matched);
        assert!(first.match_rect.is_none());
        assert!(second.matched);
        assert_eq!(second.match_rect, Some(Rect::new(23, 17, 7, 5)));
        assert_eq!(second.frame_metadata.unwrap().window_id, 11);
        assert_eq!(second.details["selected_scale_percent"], 100);
    }

    #[test]
    fn interleaved_runs_reach_image_stability_independently() {
        let compiled = compiled_image_macro(fixture_icon());
        let BlockKind::Observe { condition } = &compiled.definition().blocks[0].kind else {
            unreachable!()
        };
        let capture = FixtureCapture {
            frames: Mutex::new(vec![
                captured_frame(
                    fixture_search_with_icon_at(23, 17),
                    frame(1, 100, 23, 100).frame,
                ),
                captured_frame(
                    fixture_search_with_icon_at(23, 17),
                    frame(2, 110, 23, 100).frame,
                ),
                captured_frame(
                    fixture_search_with_icon_at(23, 17),
                    frame(3, 200, 23, 100).frame,
                ),
                captured_frame(
                    fixture_search_with_icon_at(23, 17),
                    frame(4, 210, 23, 100).frame,
                ),
            ]),
        };
        let detector = ImageDetector::new();
        let observe = |run_id: &str, observed_at_ms: u64| {
            detector
                .observe(
                    &ObservationRequest {
                        run_id,
                        generation: 1,
                        condition,
                        compiled: &compiled,
                        observed_at_ms,
                    },
                    &capture,
                )
                .unwrap()
        };

        assert!(!observe("run-a", 100).matched);
        assert!(!observe("run-b", 110).matched);
        assert!(observe("run-a", 200).matched);
        assert!(observe("run-b", 210).matched);
    }

    #[test]
    fn identity_transition_resets_before_elapsed_filter_and_withholds_new_geometry() {
        let compiled = compiled_image_macro(fixture_icon());
        let BlockKind::Observe { condition } = &compiled.definition().blocks[0].kind else {
            unreachable!()
        };
        let mut b1 = frame(3, 210, 31, 100).frame;
        b1.window_revision += 1;
        let mut b2 = frame(4, 220, 31, 100).frame;
        b2.window_revision += 1;
        let mut b3 = frame(5, 310, 31, 100).frame;
        b3.window_revision += 1;
        let capture = FixtureCapture {
            frames: Mutex::new(vec![
                captured_frame(
                    fixture_search_with_icon_at(23, 17),
                    frame(1, 100, 23, 100).frame,
                ),
                captured_frame(
                    fixture_search_with_icon_at(23, 17),
                    frame(2, 200, 23, 100).frame,
                ),
                captured_frame(fixture_search_with_icon_at(31, 22), b1),
                captured_frame(fixture_search_with_icon_at(31, 22), b2),
                captured_frame(fixture_search_with_icon_at(31, 22), b3),
            ]),
        };
        let detector = ImageDetector::new();
        let observe = |observed_at_ms: u64| {
            detector
                .observe(
                    &ObservationRequest {
                        run_id: "run",
                        generation: 1,
                        condition,
                        compiled: &compiled,
                        observed_at_ms,
                    },
                    &capture,
                )
                .unwrap()
        };

        assert!(!observe(100).matched);
        assert_eq!(observe(200).match_rect, Some(Rect::new(23, 17, 7, 5)));
        let first_b = observe(210);
        let ignored_b = observe(220);
        let stable_b = observe(310);

        assert!(!first_b.matched);
        assert!(first_b.match_rect.is_none());
        assert!(!ignored_b.matched);
        assert!(ignored_b.match_rect.is_none());
        assert_eq!(stable_b.match_rect, Some(Rect::new(31, 22, 7, 5)));
    }

    #[test]
    fn stability_capacity_requires_explicit_run_cleanup() {
        let compiled = compiled_image_macro(fixture_icon());
        let BlockKind::Observe { condition } = &compiled.definition().blocks[0].kind else {
            unreachable!()
        };
        let capture = FixtureCapture {
            frames: Mutex::new(vec![
                captured_frame(
                    fixture_search_with_icon_at(23, 17),
                    frame(1, 100, 23, 100).frame,
                ),
                captured_frame(
                    fixture_search_with_icon_at(23, 17),
                    frame(2, 110, 23, 100).frame,
                ),
                captured_frame(
                    fixture_search_with_icon_at(23, 17),
                    frame(3, 120, 23, 100).frame,
                ),
            ]),
        };
        let detector = ImageDetector::with_stability_capacity(1);
        let observe = |run_id: &str, observed_at_ms: u64| {
            detector.observe(
                &ObservationRequest {
                    run_id,
                    generation: 1,
                    condition,
                    compiled: &compiled,
                    observed_at_ms,
                },
                &capture,
            )
        };

        assert!(observe("run-a", 100).is_ok());
        let error = observe("run-b", 110).unwrap_err();
        assert!(error.to_string().contains("capacity 1 is exhausted"));
        assert_eq!(detector.clear_run("run-a").unwrap(), 1);
        assert!(observe("run-b", 120).is_ok());
    }
}
