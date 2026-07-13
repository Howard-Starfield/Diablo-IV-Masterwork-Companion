use std::{
    collections::{BTreeSet, HashMap},
    sync::Mutex,
};

use crate::engine::types::{Rect, ScreenImage};
use anyhow::{Context, Result, bail};
use image::{GrayImage, ImageBuffer, Luma, imageops};
use imageproc::template_matching::{MatchTemplateMethod, match_template};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::engine::automation::CaptureSource;

use super::{
    Condition, ConditionDetector, DetectorEvidence, ImageRule, ImageRuleVerificationArtifact,
    ImageVerificationPreprocess, MacroDefinition, MatchSelectionPolicy, ObservationRequest,
    image_verification as verification,
};

/// Authoring starts here, but every rule must retain its own verified threshold.
pub const INITIAL_SIMILARITY_THRESHOLD: f32 = 0.95;
/// Initial bounded work policy for the v1 640x360 three-scale envelope.
/// Task 14 may lower this score-cell budget after named-hardware release benchmarks.
pub const DEFAULT_MAX_SCORE_CELLS: u64 = 750_000;
/// Caps the dominant normalized-correlation work: score cells times active template pixels.
pub const DEFAULT_MAX_PIXEL_OPERATIONS: u64 = 50_000_000;
/// Caps retained local maxima before deterministic spatial clustering.
pub const DEFAULT_MAX_CANDIDATES: usize = 4_096;
pub const DEFAULT_MAX_SCALES: usize = 32;
pub const DEFAULT_MAX_NEGATIVE_SAMPLES: usize = 4_096;
/// V1 decoded/preprocess caps. A 4096x4096 BGRA capture fits exactly in the search budget.
pub const DEFAULT_MAX_SEARCH_PIXELS: u64 = 16_777_216;
pub const DEFAULT_MAX_SEARCH_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_MAX_TEMPLATE_PIXELS: u64 = 4_194_304;
pub const DEFAULT_MAX_TEMPLATE_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_MASK_PIXELS: u64 = 4_194_304;
pub const DEFAULT_MAX_MASK_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_SCALED_TEMPLATE_PIXELS: u64 = 4_194_304;
pub const DEFAULT_MAX_TOTAL_SCALED_PIXELS: u64 = 8_388_608;
pub const DEFAULT_MAX_TOTAL_SCALED_BYTES: u64 = 16 * 1024 * 1024;
const DEFAULT_MAX_CLUSTER_COMPARISONS: u64 = 100_000;
/// Grayscale intensity variance below this value is too flat for safe correlation.
pub(super) const MIN_TEMPLATE_VARIANCE: f32 = 16.0;
const DEFAULT_MAX_STABILITY_STATES: usize = 256;

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
    #[error("image match scale must be greater than zero")]
    ZeroScale,
    #[error("image match scale {scale_percent}% is duplicated")]
    DuplicateScale { scale_percent: u16 },
    #[error("image match scale count exceeds maximum {maximum}")]
    ScaleCountLimit { maximum: usize },
    #[error("image match scale {scale_percent}% overflows supported dimensions")]
    ScaleDimensionOverflow { scale_percent: u16 },
    #[error("image match scale {scale_percent}% does not fit the search region")]
    ScaleDoesNotFit { scale_percent: u16 },
    #[error("image match requires at least one fitting scale")]
    NoFittingScale,
    #[error("image score-map work {actual} exceeds maximum {maximum}")]
    ScoreCellLimit { actual: u64, maximum: u64 },
    #[error("image pixel work {actual} exceeds maximum {maximum}")]
    PixelOperationLimit { actual: u64, maximum: u64 },
    #[error("image candidate count exceeds maximum {maximum}")]
    CandidateLimit { maximum: usize },
    #[error("image spatial clustering comparisons exceed maximum {maximum}")]
    ClusterComparisonLimit { maximum: u64 },
    #[error("{resource:?} image dimensions overflow resource accounting")]
    ResourceDimensionOverflow { resource: ImageResourceKind },
    #[error("{resource:?} image pixels {actual} exceed maximum {maximum}")]
    ResourcePixelLimit {
        resource: ImageResourceKind,
        actual: u64,
        maximum: u64,
    },
    #[error("{resource:?} image bytes {actual} exceed maximum {maximum}")]
    ResourceByteLimit {
        resource: ImageResourceKind,
        actual: u64,
        maximum: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageResourceKind {
    Template,
    Mask,
    Search,
    ScaledTemplate,
    TotalScaledTemplates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ImageWorkPolicy {
    maximum_score_cells: u64,
    maximum_pixel_operations: u64,
    maximum_candidates: usize,
    maximum_search_pixels: u64,
    maximum_search_bytes: u64,
    maximum_template_pixels: u64,
    maximum_template_bytes: u64,
    maximum_mask_pixels: u64,
    maximum_mask_bytes: u64,
    maximum_scaled_template_pixels: u64,
    maximum_total_scaled_pixels: u64,
    maximum_total_scaled_bytes: u64,
}

impl ImageWorkPolicy {
    pub(super) const fn production() -> Self {
        Self {
            maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
            maximum_pixel_operations: DEFAULT_MAX_PIXEL_OPERATIONS,
            maximum_candidates: DEFAULT_MAX_CANDIDATES,
            maximum_search_pixels: DEFAULT_MAX_SEARCH_PIXELS,
            maximum_search_bytes: DEFAULT_MAX_SEARCH_BYTES,
            maximum_template_pixels: DEFAULT_MAX_TEMPLATE_PIXELS,
            maximum_template_bytes: DEFAULT_MAX_TEMPLATE_BYTES,
            maximum_mask_pixels: DEFAULT_MAX_MASK_PIXELS,
            maximum_mask_bytes: DEFAULT_MAX_MASK_BYTES,
            maximum_scaled_template_pixels: DEFAULT_MAX_SCALED_TEMPLATE_PIXELS,
            maximum_total_scaled_pixels: DEFAULT_MAX_TOTAL_SCALED_PIXELS,
            maximum_total_scaled_bytes: DEFAULT_MAX_TOTAL_SCALED_BYTES,
        }
    }

    const fn with_maximum_score_cells(maximum_score_cells: u64) -> Self {
        Self {
            maximum_score_cells,
            ..Self::production()
        }
    }

    pub(super) fn validate_asset_dimensions(
        self,
        dimensions: (u32, u32),
        decoded_bytes_per_pixel: u64,
        resource: ImageResourceKind,
    ) -> std::result::Result<(), ImageMatchError> {
        let (maximum_pixels, maximum_bytes) = match resource {
            ImageResourceKind::Template => {
                (self.maximum_template_pixels, self.maximum_template_bytes)
            }
            ImageResourceKind::Mask => (self.maximum_mask_pixels, self.maximum_mask_bytes),
            _ => {
                return Err(ImageMatchError::ResourceDimensionOverflow { resource });
            }
        };
        validate_resource_dimensions(
            dimensions,
            decoded_bytes_per_pixel,
            resource,
            maximum_pixels,
            maximum_bytes,
        )?;
        Ok(())
    }

    pub(super) const fn maximum_asset_bytes(self, resource: ImageResourceKind) -> Option<u64> {
        match resource {
            ImageResourceKind::Template => Some(self.maximum_template_bytes),
            ImageResourceKind::Mask => Some(self.maximum_mask_bytes),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ValidatedScale {
    percent: u16,
    width: u32,
    height: u32,
    score_cells: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ValidatedScalePlan {
    scales: Vec<ValidatedScale>,
    pub(super) score_cells: u64,
    pixel_operations: u64,
    scaled_template_pixels: u64,
    scaled_template_bytes: u64,
    maximum_candidates: usize,
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
    #[serde(default)]
    pub client_x: i32,
    #[serde(default)]
    pub client_y: i32,
    #[serde(default)]
    pub client_width: u32,
    #[serde(default)]
    pub client_height: u32,
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

    fn clear_run_generations(&self, run_id: &str, generations: &[u64]) -> Result<usize> {
        let mut stability = self
            .stability
            .lock()
            .map_err(|_| anyhow::anyhow!("image detector stability lock is poisoned"))?;
        let before = stability.len();
        stability.retain(|key, _| key.run_id != run_id || !generations.contains(&key.generation));
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
        validate_search_dimensions(
            (capture_rect.width, capture_rect.height),
            ImageWorkPolicy::production(),
        )?;
        let captured = capture.capture_frame(capture_rect)?;
        let frame = ImageFrameMetadata {
            frame_id: captured.metadata.frame_id,
            captured_at_ms: captured.metadata.captured_at_ms,
            window_id: captured.metadata.window_id,
            window_revision: captured.metadata.window_revision,
            client_x: captured.metadata.client_x,
            client_y: captured.metadata.client_y,
            client_width: captured.metadata.client_width,
            client_height: captured.metadata.client_height,
            geometry_revision: captured.metadata.geometry_revision,
            display_profile_revision: captured.metadata.display_profile_revision,
            dpi: captured.metadata.dpi,
            region_revision: region.revision,
            rule_revision: rule.revision,
        };
        if frame.region_revision != region.revision || frame.rule_revision != rule.revision {
            bail!("image frame metadata does not match compiled region/rule revisions");
        }
        if (frame.client_width, frame.client_height)
            != (
                definition.target.captured_client_width,
                definition.target.captured_client_height,
            )
        {
            bail!(
                "image template client geometry {}x{} is stale for current client geometry {}x{}",
                definition.target.captured_client_width,
                definition.target.captured_client_height,
                frame.client_width,
                frame.client_height
            );
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
            cluster_peaks(raw.candidates, ClusterPolicy::default())?,
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

    fn run_finished(&self, run_id: &str, generations: &[u64]) {
        let _ = self.clear_run_generations(run_id, generations);
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
    match kind {
        "mask" => ImageRuleVerification::decode_mask_png(&pinned.bytes),
        _ => ImageRuleVerification::decode_template_png(&pinned.bytes),
    }
    .with_context(|| format!("compiled image {kind} asset cannot be decoded"))
}

fn validate_runtime_image_rule(
    definition: &MacroDefinition,
    rule: &ImageRule,
    template: &GrayImage,
    mask: Option<&GrayImage>,
    search_rect: Rect,
) -> Result<()> {
    verification::validate_decoded_rule(definition, rule, template, mask)?;
    validated_scale_plan(
        (search_rect.width, search_rect.height),
        template,
        mask,
        &rule.scales_percent,
        ImageWorkPolicy::production(),
    )?;
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
        && left.client_width == right.client_width
        && left.client_height == right.client_height
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
    pub negative_samples: &'a [NegativeCorpusSample],
    pub observed_clusters: &'a [CandidateCluster],
    pub maximum_score_cells: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NegativeSampleEvaluationInputs {
    pub preprocess: ImageVerificationPreprocess,
    pub template: super::AssetRef,
    pub transparent_mask: Option<super::AssetRef>,
    pub captured_dpi: u32,
    pub region_id: String,
    pub region_revision: u64,
    pub search_width: u32,
    pub search_height: u32,
    pub scales_percent: Vec<u16>,
    pub threshold: f32,
    pub minimum_runner_up_margin: f32,
}

impl NegativeSampleEvaluationInputs {
    pub fn for_rule(
        rule: &ImageRule,
        captured_dpi: u32,
        region_revision: u64,
        search_dimensions: (u32, u32),
    ) -> Self {
        Self {
            preprocess: ImageVerificationPreprocess::GrayscaleNormalizedCrossCorrelation,
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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NegativeCorpusSample {
    pub stable_id: String,
    pub content_sha256: String,
    pub measured_score: f32,
    pub evaluation: NegativeSampleEvaluationInputs,
}

#[derive(Serialize)]
struct CanonicalNegativeCorpus<'a> {
    version: u32,
    samples: &'a [&'a NegativeCorpusSample],
}

#[derive(Debug, Clone, PartialEq)]
struct VerifiedNegativeCorpus {
    sha256: String,
    sample_count: u64,
    best_score: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageRuleVerification {
    threshold: f32,
    template_variance: f32,
    negative_margin: f32,
    ambiguity_margin: Option<f32>,
    score_cells: u64,
    artifact: ImageRuleVerificationArtifact,
}

impl ImageRuleVerification {
    pub fn decode_template_png(bytes: &[u8]) -> Result<GrayImage> {
        verification::decode_template_png(bytes)
    }

    pub fn decode_mask_png(bytes: &[u8]) -> Result<GrayImage> {
        verification::decode_mask_png(bytes)
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    pub fn template_variance(&self) -> f32 {
        self.template_variance
    }

    pub fn negative_margin(&self) -> f32 {
        self.negative_margin
    }

    pub fn ambiguity_margin(&self) -> Option<f32> {
        self.ambiguity_margin
    }

    pub fn score_cells(&self) -> u64 {
        self.score_cells
    }

    pub fn artifact(&self) -> &ImageRuleVerificationArtifact {
        &self.artifact
    }

    pub fn into_artifact(self) -> ImageRuleVerificationArtifact {
        self.artifact
    }
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
    #[error("negative corpus sample count exceeds maximum {maximum}")]
    NegativeCorpusLimit { maximum: usize },
    #[error("negative sample stable ID is invalid: {stable_id}")]
    InvalidNegativeSampleId { stable_id: String },
    #[error("negative sample {stable_id} has an invalid content SHA-256")]
    InvalidNegativeSampleHash { stable_id: String },
    #[error("negative sample stable ID is duplicated: {stable_id}")]
    DuplicateNegativeSample { stable_id: String },
    #[error("negative sample content SHA-256 is duplicated: {content_sha256}")]
    DuplicateNegativeSampleContent { content_sha256: String },
    #[error("negative sample {stable_id} was evaluated with different inputs")]
    NegativeSampleEvaluationMismatch { stable_id: String },
    #[error("configured transparent mask asset is missing")]
    MissingMask,
    #[error("transparent mask is invalid: {reason}")]
    InvalidMask { reason: String },
    #[error("template variance {variance} is below {minimum}")]
    LowTemplateVariance { variance: f32, minimum: f32 },
    #[error("template DPI {captured} is stale for current DPI {current}")]
    StaleDpi { captured: u32, current: u32 },
    #[error(transparent)]
    InvalidWorkPlan(#[from] ImageMatchError),
    #[error("observed image candidate cluster is empty")]
    EmptyObservedCluster,
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
        let plan = validated_scale_plan(
            input.search_dimensions,
            input.template,
            input.mask,
            &input.rule.scales_percent,
            ImageWorkPolicy::with_maximum_score_cells(input.maximum_score_cells),
        )?;
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
        let score_cells = plan.score_cells;
        let observed_candidates =
            input
                .observed_clusters
                .iter()
                .try_fold(0_usize, |count, cluster| {
                    if cluster.members.is_empty() {
                        return Err(ImageRuleVerificationError::EmptyObservedCluster);
                    }
                    count.checked_add(cluster.members.len()).ok_or({
                        ImageRuleVerificationError::InvalidWorkPlan(
                            ImageMatchError::CandidateLimit {
                                maximum: plan.maximum_candidates,
                            },
                        )
                    })
                })?;
        if input.observed_clusters.len() > plan.maximum_candidates
            || observed_candidates > plan.maximum_candidates
        {
            return Err(ImageMatchError::CandidateLimit {
                maximum: plan.maximum_candidates,
            }
            .into());
        }
        let negative_corpus = verify_negative_corpus(
            input.rule,
            input.captured_dpi,
            input.region_revision,
            input.search_dimensions,
            input.negative_samples,
        )?;
        let negative_margin = input.rule.threshold - negative_corpus.best_score;
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
        let artifact = verification::build_artifact(
            input.rule,
            input.captured_dpi,
            input.region_revision,
            input.search_dimensions,
            negative_corpus.sha256,
            negative_corpus.sample_count,
            negative_corpus.best_score,
            variance,
        );
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

fn verify_negative_corpus(
    rule: &ImageRule,
    captured_dpi: u32,
    region_revision: u64,
    search_dimensions: (u32, u32),
    samples: &[NegativeCorpusSample],
) -> std::result::Result<VerifiedNegativeCorpus, ImageRuleVerificationError> {
    if samples.is_empty() {
        return Err(ImageRuleVerificationError::InvalidNegativeCorpus);
    }
    if samples.len() > DEFAULT_MAX_NEGATIVE_SAMPLES {
        return Err(ImageRuleVerificationError::NegativeCorpusLimit {
            maximum: DEFAULT_MAX_NEGATIVE_SAMPLES,
        });
    }
    let expected = NegativeSampleEvaluationInputs::for_rule(
        rule,
        captured_dpi,
        region_revision,
        search_dimensions,
    );
    let mut canonical = samples.iter().collect::<Vec<_>>();
    canonical.sort_by(|left, right| {
        left.stable_id
            .cmp(&right.stable_id)
            .then_with(|| left.content_sha256.cmp(&right.content_sha256))
            .then_with(|| left.measured_score.total_cmp(&right.measured_score))
    });
    let mut stable_ids = std::collections::HashSet::with_capacity(samples.len());
    let mut content_hashes = std::collections::HashSet::with_capacity(samples.len());
    for sample in samples {
        if sample.stable_id.is_empty()
            || sample.stable_id.len() > 256
            || sample.stable_id.trim() != sample.stable_id
            || sample.stable_id.chars().any(char::is_control)
        {
            return Err(ImageRuleVerificationError::InvalidNegativeSampleId {
                stable_id: sample.stable_id.clone(),
            });
        }
        if !stable_ids.insert(sample.stable_id.as_str()) {
            return Err(ImageRuleVerificationError::DuplicateNegativeSample {
                stable_id: sample.stable_id.clone(),
            });
        }
        if !verification::valid_sha256(&sample.content_sha256) {
            return Err(ImageRuleVerificationError::InvalidNegativeSampleHash {
                stable_id: sample.stable_id.clone(),
            });
        }
        if !content_hashes.insert(sample.content_sha256.as_str()) {
            return Err(ImageRuleVerificationError::DuplicateNegativeSampleContent {
                content_sha256: sample.content_sha256.clone(),
            });
        }
        if !verification::normalized_score(sample.measured_score) {
            return Err(ImageRuleVerificationError::InvalidNegativeScore);
        }
        if sample.evaluation != expected {
            return Err(
                ImageRuleVerificationError::NegativeSampleEvaluationMismatch {
                    stable_id: sample.stable_id.clone(),
                },
            );
        }
    }
    let best_score = canonical
        .iter()
        .map(|sample| sample.measured_score)
        .max_by(f32::total_cmp)
        .expect("non-empty corpus was checked above");
    let sample_count = u64::try_from(canonical.len())
        .map_err(|_| ImageRuleVerificationError::InvalidNegativeCorpus)?;
    let bytes = serde_json::to_vec(&CanonicalNegativeCorpus {
        version: 1,
        samples: &canonical,
    })
    .expect("negative-corpus inputs contain only serializable values");
    Ok(VerifiedNegativeCorpus {
        sha256: format!("{:x}", Sha256::digest(bytes)),
        sample_count,
        best_score,
    })
}

#[cfg(test)]
fn verify_negative_corpus_for_test(
    rule: &ImageRule,
    samples: &[NegativeCorpusSample],
) -> std::result::Result<(), ImageRuleVerificationError> {
    verify_negative_corpus(rule, 96, 13, (640, 360), samples).map(|_| ())
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ImageMatcher;

fn scaled_dimension(
    dimension: u32,
    scale_percent: u16,
) -> std::result::Result<u32, ImageMatchError> {
    let rounded = u64::from(dimension)
        .checked_mul(u64::from(scale_percent))
        .and_then(|value| value.checked_add(50))
        .map(|value| value / 100)
        .ok_or(ImageMatchError::ScaleDimensionOverflow { scale_percent })?;
    u32::try_from(rounded).map_err(|_| ImageMatchError::ScaleDimensionOverflow { scale_percent })
}

pub(super) fn validate_resource_dimensions(
    dimensions: (u32, u32),
    decoded_bytes_per_pixel: u64,
    resource: ImageResourceKind,
    maximum_pixels: u64,
    maximum_bytes: u64,
) -> std::result::Result<(u64, u64), ImageMatchError> {
    let pixels = u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .ok_or(ImageMatchError::ResourceDimensionOverflow { resource })?;
    if pixels > maximum_pixels {
        return Err(ImageMatchError::ResourcePixelLimit {
            resource,
            actual: pixels,
            maximum: maximum_pixels,
        });
    }
    let bytes = pixels
        .checked_mul(decoded_bytes_per_pixel)
        .ok_or(ImageMatchError::ResourceDimensionOverflow { resource })?;
    if bytes > maximum_bytes {
        return Err(ImageMatchError::ResourceByteLimit {
            resource,
            actual: bytes,
            maximum: maximum_bytes,
        });
    }
    Ok((pixels, bytes))
}

pub(super) fn validate_search_dimensions(
    dimensions: (u32, u32),
    policy: ImageWorkPolicy,
) -> std::result::Result<(), ImageMatchError> {
    validate_resource_dimensions(
        dimensions,
        4,
        ImageResourceKind::Search,
        policy.maximum_search_pixels,
        policy.maximum_search_bytes,
    )?;
    Ok(())
}

pub(super) fn validated_scale_plan(
    search_dimensions: (u32, u32),
    template: &GrayImage,
    mask: Option<&GrayImage>,
    scales_percent: &[u16],
    policy: ImageWorkPolicy,
) -> std::result::Result<ValidatedScalePlan, ImageMatchError> {
    validate_search_dimensions(search_dimensions, policy)?;
    validate_resource_dimensions(
        template.dimensions(),
        4,
        ImageResourceKind::Template,
        policy.maximum_template_pixels,
        policy.maximum_template_bytes,
    )?;
    if let Some(mask) = mask {
        validate_resource_dimensions(
            mask.dimensions(),
            4,
            ImageResourceKind::Mask,
            policy.maximum_mask_pixels,
            policy.maximum_mask_bytes,
        )?;
    }
    if scales_percent.is_empty() {
        return Err(ImageMatchError::NoFittingScale);
    }
    if scales_percent.len() > DEFAULT_MAX_SCALES {
        return Err(ImageMatchError::ScaleCountLimit {
            maximum: DEFAULT_MAX_SCALES,
        });
    }
    let mut seen = BTreeSet::new();
    let mut scales = Vec::with_capacity(scales_percent.len());
    let mut total_cells = 0_u64;
    let mut total_pixel_operations = 0_u64;
    let mut total_scaled_pixels = 0_u64;
    let mut total_scaled_bytes = 0_u64;
    for &percent in scales_percent {
        if percent == 0 {
            return Err(ImageMatchError::ZeroScale);
        }
        if !seen.insert(percent) {
            return Err(ImageMatchError::DuplicateScale {
                scale_percent: percent,
            });
        }
        let width = scaled_dimension(template.width(), percent)?;
        let height = scaled_dimension(template.height(), percent)?;
        if width == 0 || height == 0 || width > search_dimensions.0 || height > search_dimensions.1
        {
            return Err(ImageMatchError::ScaleDoesNotFit {
                scale_percent: percent,
            });
        }
        let score_cells = u64::from(search_dimensions.0 - width + 1)
            .checked_mul(u64::from(search_dimensions.1 - height + 1))
            .ok_or(ImageMatchError::ScoreCellLimit {
                actual: u64::MAX,
                maximum: policy.maximum_score_cells,
            })?;
        total_cells =
            total_cells
                .checked_add(score_cells)
                .ok_or(ImageMatchError::ScoreCellLimit {
                    actual: u64::MAX,
                    maximum: policy.maximum_score_cells,
                })?;
        if total_cells > policy.maximum_score_cells {
            return Err(ImageMatchError::ScoreCellLimit {
                actual: total_cells,
                maximum: policy.maximum_score_cells,
            });
        }
        let scaled_pixels = u64::from(width).checked_mul(u64::from(height)).ok_or(
            ImageMatchError::ResourceDimensionOverflow {
                resource: ImageResourceKind::ScaledTemplate,
            },
        )?;
        if scaled_pixels > policy.maximum_scaled_template_pixels {
            return Err(ImageMatchError::ResourcePixelLimit {
                resource: ImageResourceKind::ScaledTemplate,
                actual: scaled_pixels,
                maximum: policy.maximum_scaled_template_pixels,
            });
        }
        total_scaled_pixels = total_scaled_pixels.checked_add(scaled_pixels).ok_or(
            ImageMatchError::ResourceDimensionOverflow {
                resource: ImageResourceKind::TotalScaledTemplates,
            },
        )?;
        if total_scaled_pixels > policy.maximum_total_scaled_pixels {
            return Err(ImageMatchError::ResourcePixelLimit {
                resource: ImageResourceKind::TotalScaledTemplates,
                actual: total_scaled_pixels,
                maximum: policy.maximum_total_scaled_pixels,
            });
        }
        let scaled_bytes_per_pixel = if mask.is_some() { 2 } else { 1 };
        let scaled_bytes = scaled_pixels.checked_mul(scaled_bytes_per_pixel).ok_or(
            ImageMatchError::ResourceDimensionOverflow {
                resource: ImageResourceKind::TotalScaledTemplates,
            },
        )?;
        total_scaled_bytes = total_scaled_bytes.checked_add(scaled_bytes).ok_or(
            ImageMatchError::ResourceDimensionOverflow {
                resource: ImageResourceKind::TotalScaledTemplates,
            },
        )?;
        if total_scaled_bytes > policy.maximum_total_scaled_bytes {
            return Err(ImageMatchError::ResourceByteLimit {
                resource: ImageResourceKind::TotalScaledTemplates,
                actual: total_scaled_bytes,
                maximum: policy.maximum_total_scaled_bytes,
            });
        }
        let active_pixels = if let Some(mask) = mask {
            let base_active = u64::try_from(mask.pixels().filter(|pixel| pixel[0] != 0).count())
                .map_err(|_| ImageMatchError::ResourceDimensionOverflow {
                    resource: ImageResourceKind::Mask,
                })?;
            let horizontal_replication = width.div_ceil(template.width());
            let vertical_replication = height.div_ceil(template.height());
            base_active
                .checked_mul(u64::from(horizontal_replication))
                .and_then(|pixels| pixels.checked_mul(u64::from(vertical_replication)))
                .ok_or(ImageMatchError::ResourceDimensionOverflow {
                    resource: ImageResourceKind::ScaledTemplate,
                })?
                .min(scaled_pixels)
        } else {
            scaled_pixels
        };
        let pixel_operations =
            score_cells
                .checked_mul(active_pixels)
                .ok_or(ImageMatchError::PixelOperationLimit {
                    actual: u64::MAX,
                    maximum: policy.maximum_pixel_operations,
                })?;
        total_pixel_operations = total_pixel_operations.checked_add(pixel_operations).ok_or(
            ImageMatchError::PixelOperationLimit {
                actual: u64::MAX,
                maximum: policy.maximum_pixel_operations,
            },
        )?;
        if total_pixel_operations > policy.maximum_pixel_operations {
            return Err(ImageMatchError::PixelOperationLimit {
                actual: total_pixel_operations,
                maximum: policy.maximum_pixel_operations,
            });
        }
        scales.push(ValidatedScale {
            percent,
            width,
            height,
            score_cells,
        });
    }
    if scales.is_empty() {
        return Err(ImageMatchError::NoFittingScale);
    }
    Ok(ValidatedScalePlan {
        scales,
        score_cells: total_cells,
        pixel_operations: total_pixel_operations,
        scaled_template_pixels: total_scaled_pixels,
        scaled_template_bytes: total_scaled_bytes,
        maximum_candidates: policy.maximum_candidates,
    })
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
) -> std::result::Result<Vec<CandidateCluster>, ImageMatchError> {
    if peaks.len() > DEFAULT_MAX_CANDIDATES {
        return Err(ImageMatchError::CandidateLimit {
            maximum: DEFAULT_MAX_CANDIDATES,
        });
    }
    peaks.sort_by(candidate_order);
    let cell_size = peaks
        .iter()
        .map(|peak| peak.rect.width.max(peak.rect.height))
        .max()
        .unwrap_or(1)
        .max(policy.maximum_center_distance_px.ceil() as u32)
        .max(1);
    let mut clusters: Vec<CandidateCluster> = Vec::new();
    let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    let mut comparisons = 0_u64;
    for peak in peaks {
        let center_x = i64::from(peak.rect.x) + i64::from(peak.rect.width) / 2;
        let center_y = i64::from(peak.rect.y) + i64::from(peak.rect.height) / 2;
        let key = (
            center_x.div_euclid(i64::from(cell_size)),
            center_y.div_euclid(i64::from(cell_size)),
        );
        let mut nearby = BTreeSet::new();
        for y in key.1 - 1..=key.1 + 1 {
            for x in key.0 - 1..=key.0 + 1 {
                if let Some(indices) = grid.get(&(x, y)) {
                    nearby.extend(indices.iter().copied());
                }
            }
        }
        let mut matching = None;
        for index in nearby {
            comparisons += 1;
            if comparisons > DEFAULT_MAX_CLUSTER_COMPARISONS {
                return Err(ImageMatchError::ClusterComparisonLimit {
                    maximum: DEFAULT_MAX_CLUSTER_COMPARISONS,
                });
            }
            if candidates_represent_same_object(&clusters[index].best, &peak, policy) {
                matching = Some(index);
                break;
            }
        }
        if let Some(index) = matching {
            let cluster = &mut clusters[index];
            cluster.add(peak);
        } else {
            let index = clusters.len();
            clusters.push(CandidateCluster::from_peak(peak));
            grid.entry(key).or_default().push(index);
        }
    }
    clusters.sort_by(cluster_order);
    Ok(clusters)
}

fn extract_local_maxima(
    scores: &ImageBuffer<Luma<f32>, Vec<f32>>,
    template_dimensions: (u32, u32),
    scale_percent: u16,
    threshold: f32,
    existing_candidates: usize,
    maximum_candidates: usize,
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
                if existing_candidates + maxima.len() >= maximum_candidates {
                    return Err(ImageMatchError::CandidateLimit {
                        maximum: maximum_candidates,
                    }
                    .into());
                }
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

pub(super) fn validate_mask_reference<'a>(
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

pub(super) fn template_variance(template: &GrayImage, mask: Option<&GrayImage>) -> f32 {
    let mut count = 0_u64;
    let mut mean = 0.0_f64;
    let mut sum_squared_delta = 0.0_f64;
    for (x, y, pixel) in template.enumerate_pixels() {
        if mask.is_some_and(|mask| mask.get_pixel(x, y)[0] == 0) {
            continue;
        }
        count += 1;
        let value = f64::from(pixel[0]);
        let delta = value - mean;
        mean += delta / count as f64;
        sum_squared_delta += delta * (value - mean);
    }
    if count == 0 {
        return 0.0;
    }
    (sum_squared_delta / count as f64) as f32
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
        validate_search_dimensions(search.rgba.dimensions(), ImageWorkPolicy::production())?;
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
        let plan = validated_scale_plan(
            search.dimensions(),
            template,
            mask,
            &config.scales_percent,
            ImageWorkPolicy::production(),
        )?;
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
        for scale in plan.scales {
            let scale_percent = scale.percent;
            let width = scale.width;
            let height = scale.height;
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
                candidates.len(),
                plan.maximum_candidates,
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
        IMAGE_RULE_VERIFICATION_VERSION, ImageRule, Limit, MACRO_SCHEMA_VERSION, MacroDefinition,
        MatchSelectionPolicy, ObservationRequest, ObserveMode, PinnedAsset, RegionDefinition,
        SafetyPolicy, SavedRevision, TargetProfile,
    };
    use crate::engine::{
        automation::{CaptureFrameMetadata, CaptureSource, CapturedScreenFrame},
        types::{RectRatio, ScreenImage},
    };
    use image::{DynamicImage, GrayImage, ImageFormat, Luma, Rgba, RgbaImage};
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
                client_x: 0,
                client_y: 0,
                client_width: 64,
                client_height: 48,
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
            client_x: frame.client_x,
            client_y: frame.client_y,
            client_width: frame.client_width,
            client_height: frame.client_height,
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
            let negative_samples = vec![corpus_sample_for(
                &rule,
                "negative/a",
                NEGATIVE_CORPUS_SHA256,
                0.80,
                96,
                13,
                (64, 48),
            )];
            ImageRuleVerification::verify(ImageRuleVerificationInput {
                rule: &rule,
                template: &template,
                mask: None,
                captured_dpi: 96,
                current_dpi: 96,
                region_revision: 13,
                search_dimensions: (64, 48),
                negative_samples: &negative_samples,
                observed_clusters: &[],
                maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
            })
            .unwrap()
            .into_artifact()
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

        assert!(matches!(
            error,
            ImageMatchError::ScaleDimensionOverflow { .. }
        ));
    }

    #[test]
    fn validated_scale_plan_rejects_zero_duplicate_and_non_fitting_scales() {
        let template = fixture_icon();
        let limits = ImageWorkPolicy::production();

        assert_eq!(
            validated_scale_plan((64, 48), &template, None, &[0], limits).unwrap_err(),
            ImageMatchError::ZeroScale
        );
        assert_eq!(
            validated_scale_plan((64, 48), &template, None, &[100, 100], limits).unwrap_err(),
            ImageMatchError::DuplicateScale { scale_percent: 100 }
        );
        assert_eq!(
            validated_scale_plan((6, 4), &template, None, &[100], limits).unwrap_err(),
            ImageMatchError::ScaleDoesNotFit { scale_percent: 100 }
        );
    }

    #[test]
    fn validated_scale_plan_bounds_pixel_operations_independently_of_score_cells() {
        let template = fixture_icon();
        let limits = ImageWorkPolicy {
            maximum_score_cells: u64::MAX,
            maximum_pixel_operations: 10,
            maximum_candidates: DEFAULT_MAX_CANDIDATES,
            ..ImageWorkPolicy::production()
        };

        assert!(matches!(
            validated_scale_plan((64, 48), &template, None, &[100], limits).unwrap_err(),
            ImageMatchError::PixelOperationLimit { .. }
        ));
    }

    #[test]
    fn resource_policy_supports_4k_and_rejects_oversized_search_before_preprocess() {
        let policy = ImageWorkPolicy::production();

        validate_search_dimensions((4096, 4096), policy).unwrap();
        assert_eq!(
            validate_search_dimensions((4097, 4096), policy).unwrap_err(),
            ImageMatchError::ResourcePixelLimit {
                resource: ImageResourceKind::Search,
                actual: 16_781_312,
                maximum: DEFAULT_MAX_SEARCH_PIXELS,
            }
        );
    }

    #[test]
    fn scale_count_is_bounded_before_plan_reservation() {
        let template = fixture_icon();
        let scales = vec![100; DEFAULT_MAX_SCALES + 1];

        assert_eq!(
            validated_scale_plan(
                (64, 48),
                &template,
                None,
                &scales,
                ImageWorkPolicy::production(),
            )
            .unwrap_err(),
            ImageMatchError::ScaleCountLimit {
                maximum: DEFAULT_MAX_SCALES,
            }
        );
    }

    #[test]
    fn authoring_resource_preflight_precedes_corpus_and_variance_work() {
        let mut rule = fixture_rule(0.91);
        rule.scales_percent = vec![100];
        let template = fixture_icon();

        let error = ImageRuleVerification::verify(ImageRuleVerificationInput {
            rule: &rule,
            template: &template,
            mask: None,
            captured_dpi: 96,
            current_dpi: 96,
            region_revision: 13,
            search_dimensions: (4097, 4096),
            negative_samples: &[],
            observed_clusters: &[],
            maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
        })
        .unwrap_err();

        assert_eq!(
            error,
            ImageRuleVerificationError::InvalidWorkPlan(ImageMatchError::ResourcePixelLimit {
                resource: ImageResourceKind::Search,
                actual: 16_781_312,
                maximum: DEFAULT_MAX_SEARCH_PIXELS,
            })
        );
    }

    #[test]
    fn sparse_mask_cannot_bypass_scaled_template_total() {
        let template = fixture_icon();
        let mut mask = GrayImage::from_pixel(template.width(), template.height(), Luma([0]));
        mask.put_pixel(0, 0, Luma([255]));
        let policy = ImageWorkPolicy {
            maximum_score_cells: u64::MAX,
            maximum_pixel_operations: u64::MAX,
            maximum_total_scaled_pixels: 34,
            ..ImageWorkPolicy::production()
        };

        assert_eq!(
            validated_scale_plan((64, 48), &template, Some(&mask), &[100], policy).unwrap_err(),
            ImageMatchError::ResourcePixelLimit {
                resource: ImageResourceKind::TotalScaledTemplates,
                actual: 35,
                maximum: 34,
            }
        );
    }

    #[test]
    fn per_scaled_template_cap_is_independent_of_sparse_mask_activity() {
        let template = fixture_icon();
        let mut mask = GrayImage::from_pixel(template.width(), template.height(), Luma([0]));
        mask.put_pixel(0, 0, Luma([255]));
        let policy = ImageWorkPolicy {
            maximum_score_cells: u64::MAX,
            maximum_pixel_operations: u64::MAX,
            maximum_scaled_template_pixels: 34,
            ..ImageWorkPolicy::production()
        };

        assert_eq!(
            validated_scale_plan((64, 48), &template, Some(&mask), &[100], policy).unwrap_err(),
            ImageMatchError::ResourcePixelLimit {
                resource: ImageResourceKind::ScaledTemplate,
                actual: 35,
                maximum: 34,
            }
        );
    }

    #[test]
    fn decoded_byte_cap_is_checked_independently_of_pixel_cap() {
        assert_eq!(
            validate_resource_dimensions((2, 2), 4, ImageResourceKind::Mask, 10, 15).unwrap_err(),
            ImageMatchError::ResourceByteLimit {
                resource: ImageResourceKind::Mask,
                actual: 16,
                maximum: 15,
            }
        );
    }

    #[test]
    fn resource_byte_accounting_rejects_arithmetic_overflow() {
        assert_eq!(
            validate_resource_dimensions(
                (u32::MAX, u32::MAX),
                4,
                ImageResourceKind::Template,
                u64::MAX,
                u64::MAX,
            )
            .unwrap_err(),
            ImageMatchError::ResourceDimensionOverflow {
                resource: ImageResourceKind::Template,
            }
        );
    }

    #[test]
    fn local_maxima_fail_closed_at_candidate_capacity() {
        let mut scores = ImageBuffer::from_pixel(3, 1, Luma([0.1_f32]));
        scores.put_pixel(0, 0, Luma([0.99]));
        scores.put_pixel(2, 0, Luma([0.98]));

        let error = extract_local_maxima(&scores, (1, 1), 100, 0.9, 0, 1).unwrap_err();

        assert!(matches!(
            error.downcast_ref::<ImageMatchError>(),
            Some(ImageMatchError::CandidateLimit { maximum: 1 })
        ));
    }

    #[test]
    fn spatial_clustering_rejects_unbounded_candidate_input() {
        let peaks = (0..=DEFAULT_MAX_CANDIDATES)
            .map(|x| peak(x as i32 * 20, 0, 0.99, 100))
            .collect();

        assert_eq!(
            cluster_peaks(peaks, ClusterPolicy::default()).unwrap_err(),
            ImageMatchError::CandidateLimit {
                maximum: DEFAULT_MAX_CANDIDATES
            }
        );
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

        let clusters = cluster_peaks(peaks, ClusterPolicy::default()).unwrap();

        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].best.score, 0.97);
        assert_eq!(clusters[0].members.len(), 2);
    }

    #[test]
    fn same_object_across_scales_merges_before_exactly_one() {
        let peaks = vec![peak(20, 20, 0.97, 95), peak(20, 20, 0.98, 100)];
        let rule = fixture_rule(0.95);

        let result = ImageMatchResult::select(
            cluster_peaks(peaks, ClusterPolicy::default()).unwrap(),
            &rule,
        );

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

        let result = ImageMatchResult::select(
            cluster_peaks(peaks, ClusterPolicy::default()).unwrap(),
            &rule,
        );

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

        let maxima = extract_local_maxima(&scores, (7, 5), 100, 0.90, 0, 16).unwrap();

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
    fn process_restart_revision_cannot_continue_prior_process_stability() {
        let mut tracker = StabilityTracker::new(2, 40, 3);
        assert!(matches!(
            tracker.observe(frame(1, 100, 20, 100)),
            StabilityOutcome::Accepted {
                qualified: false,
                ..
            }
        ));
        let mut restarted_process = frame(2, 160, 20, 100);
        // Production derives this revision from HWND + PID + process creation FILETIME.
        restarted_process.frame.window_revision += 1;

        assert_eq!(
            tracker.observe(restarted_process),
            StabilityOutcome::Reset { stable_frames: 1 }
        );
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

    fn corpus_sample(
        rule: &ImageRule,
        stable_id: &str,
        content_sha256: &str,
        measured_score: f32,
    ) -> NegativeCorpusSample {
        corpus_sample_for(
            rule,
            stable_id,
            content_sha256,
            measured_score,
            96,
            13,
            (640, 360),
        )
    }

    fn corpus_sample_for(
        rule: &ImageRule,
        stable_id: &str,
        content_sha256: &str,
        measured_score: f32,
        captured_dpi: u32,
        region_revision: u64,
        search_dimensions: (u32, u32),
    ) -> NegativeCorpusSample {
        NegativeCorpusSample {
            stable_id: stable_id.to_string(),
            content_sha256: content_sha256.to_string(),
            measured_score,
            evaluation: NegativeSampleEvaluationInputs::for_rule(
                rule,
                captured_dpi,
                region_revision,
                search_dimensions,
            ),
        }
    }

    #[test]
    fn verification_derives_order_independent_negative_corpus_provenance() {
        let rule = fixture_rule(0.91);
        let template = fixture_icon();
        let a = corpus_sample(
            &rule,
            "negative/a",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            0.72,
        );
        let b = corpus_sample(
            &rule,
            "negative/b",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            0.80,
        );
        let verify = |samples: &[NegativeCorpusSample]| {
            ImageRuleVerification::verify(ImageRuleVerificationInput {
                rule: &rule,
                template: &template,
                mask: None,
                captured_dpi: 96,
                current_dpi: 96,
                region_revision: 13,
                search_dimensions: (640, 360),
                negative_samples: samples,
                observed_clusters: &[],
                maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
            })
            .unwrap()
        };

        let forward = verify(&[a.clone(), b.clone()]);
        let reverse = verify(&[b.clone(), a.clone()]);
        assert_eq!(
            forward.artifact().negative_corpus_sha256(),
            reverse.artifact().negative_corpus_sha256()
        );
        assert_eq!(forward.artifact().negative_sample_count(), 2);
        assert_eq!(forward.artifact().best_negative_score(), 0.80);

        let mut changed_content = b;
        changed_content.content_sha256 =
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".to_string();
        let changed = verify(&[a.clone(), changed_content]);
        assert_ne!(
            forward.artifact().negative_corpus_sha256(),
            changed.artifact().negative_corpus_sha256()
        );

        let duplicate = verify_negative_corpus_for_test(&rule, &[a.clone(), a.clone()]);
        assert_eq!(
            duplicate.unwrap_err(),
            ImageRuleVerificationError::DuplicateNegativeSample {
                stable_id: "negative/a".to_string()
            }
        );

        let mut same_content = a.clone();
        same_content.stable_id = "negative/same-bytes".to_string();
        assert_eq!(
            verify_negative_corpus_for_test(&rule, &[a.clone(), same_content]).unwrap_err(),
            ImageRuleVerificationError::DuplicateNegativeSampleContent {
                content_sha256: NEGATIVE_CORPUS_SHA256.to_string(),
            }
        );

        let mut malformed_hash = a.clone();
        malformed_hash.content_sha256 = "not-a-sha".to_string();
        assert!(matches!(
            verify_negative_corpus_for_test(&rule, &[malformed_hash]).unwrap_err(),
            ImageRuleVerificationError::InvalidNegativeSampleHash { .. }
        ));
        let mut invalid_score = a.clone();
        invalid_score.measured_score = f32::NAN;
        assert!(matches!(
            verify_negative_corpus_for_test(&rule, &[invalid_score]).unwrap_err(),
            ImageRuleVerificationError::InvalidNegativeScore
        ));
        let mut changed_input = a;
        changed_input.evaluation.search_width += 1;
        assert_eq!(
            verify_negative_corpus_for_test(&rule, &[changed_input]).unwrap_err(),
            ImageRuleVerificationError::NegativeSampleEvaluationMismatch {
                stable_id: "negative/a".to_string()
            }
        );
    }

    #[test]
    fn negative_corpus_count_is_bounded_before_canonicalization() {
        let rule = fixture_rule(0.91);
        let sample = corpus_sample(&rule, "negative/a", NEGATIVE_CORPUS_SHA256, 0.80);
        let samples = vec![sample; DEFAULT_MAX_NEGATIVE_SAMPLES + 1];

        assert_eq!(
            verify_negative_corpus_for_test(&rule, &samples).unwrap_err(),
            ImageRuleVerificationError::NegativeCorpusLimit {
                maximum: DEFAULT_MAX_NEGATIVE_SAMPLES,
            }
        );
    }

    #[test]
    fn verification_rejects_low_variance_stale_dpi_and_oversized_work() {
        let rule = fixture_rule(0.91);
        let flat = GrayImage::from_pixel(8, 8, Luma([128]));
        let clusters = vec![CandidateCluster::from_peak(peak(20, 20, 0.96, 100))];
        let samples = vec![corpus_sample(
            &rule,
            "negative/a",
            NEGATIVE_CORPUS_SHA256,
            0.80,
        )];
        let base = ImageRuleVerificationInput {
            rule: &rule,
            template: &flat,
            mask: None,
            captured_dpi: 96,
            current_dpi: 96,
            region_revision: 13,
            search_dimensions: (640, 360),
            negative_samples: &samples,
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

        let oversized_samples = vec![corpus_sample_for(
            &rule,
            "negative/a",
            NEGATIVE_CORPUS_SHA256,
            0.80,
            96,
            13,
            (4_000, 4_000),
        )];
        let oversized = ImageRuleVerificationInput {
            rule: &rule,
            template: &varied,
            mask: None,
            captured_dpi: 96,
            current_dpi: 96,
            region_revision: 13,
            search_dimensions: (4_000, 4_000),
            negative_samples: &oversized_samples,
            observed_clusters: &clusters,
            maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
        };
        assert!(matches!(
            ImageRuleVerification::verify(oversized).unwrap_err(),
            ImageRuleVerificationError::InvalidWorkPlan(ImageMatchError::ScoreCellLimit { .. })
        ));
    }

    #[test]
    fn verification_rejects_non_finite_or_out_of_range_margin_and_negative_scores() {
        let template = fixture_icon();
        for value in [f32::NAN, f32::INFINITY, -0.01, 1.01] {
            let mut rule = fixture_rule(0.91);
            rule.minimum_runner_up_margin = value;
            let samples = vec![corpus_sample(
                &rule,
                "negative/a",
                NEGATIVE_CORPUS_SHA256,
                0.80,
            )];
            let input = ImageRuleVerificationInput {
                rule: &rule,
                template: &template,
                mask: None,
                captured_dpi: 96,
                current_dpi: 96,
                region_revision: 13,
                search_dimensions: (640, 360),
                negative_samples: &samples,
                observed_clusters: &[],
                maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
            };
            assert_eq!(
                ImageRuleVerification::verify(input).unwrap_err(),
                ImageRuleVerificationError::InvalidRunnerUpMargin
            );

            let rule = fixture_rule(0.91);
            let samples = vec![corpus_sample(
                &rule,
                "negative/a",
                NEGATIVE_CORPUS_SHA256,
                value,
            )];
            let input = ImageRuleVerificationInput {
                rule: &rule,
                template: &template,
                mask: None,
                captured_dpi: 96,
                current_dpi: 96,
                region_revision: 13,
                search_dimensions: (640, 360),
                negative_samples: &samples,
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
    fn verification_rejects_empty_or_invalid_negative_corpus_provenance() {
        let rule = fixture_rule(0.91);
        let template = fixture_icon();
        let input = ImageRuleVerificationInput {
            rule: &rule,
            template: &template,
            mask: None,
            captured_dpi: 96,
            current_dpi: 96,
            region_revision: 13,
            search_dimensions: (640, 360),
            negative_samples: &[],
            observed_clusters: &[],
            maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
        };
        assert_eq!(
            ImageRuleVerification::verify(input).unwrap_err(),
            ImageRuleVerificationError::InvalidNegativeCorpus
        );

        let mut invalid_id = corpus_sample(&rule, " negative/a", NEGATIVE_CORPUS_SHA256, 0.80);
        invalid_id.stable_id.push(' ');
        assert!(matches!(
            verify_negative_corpus_for_test(&rule, &[invalid_id]).unwrap_err(),
            ImageRuleVerificationError::InvalidNegativeSampleId { .. }
        ));
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
        let high_negative_samples = vec![corpus_sample(
            &rule,
            "negative/a",
            NEGATIVE_CORPUS_SHA256,
            0.90,
        )];
        let input = ImageRuleVerificationInput {
            rule: &rule,
            template: &template,
            mask: None,
            captured_dpi: 96,
            current_dpi: 96,
            region_revision: 13,
            search_dimensions: (640, 360),
            negative_samples: &high_negative_samples,
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

        let transparent_white = RgbaImage::from_pixel(
            template.width(),
            template.height(),
            Rgba([255, 255, 255, 0]),
        );
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(transparent_white)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        let alpha_mask = ImageRuleVerification::decode_mask_png(&bytes.into_inner()).unwrap();
        let mut transparent = input.clone();
        transparent.mask = Some(&alpha_mask);
        assert!(matches!(
            ImageRuleVerification::verify(transparent).unwrap_err(),
            ImageRuleVerificationError::InvalidMask { .. }
        ));

        let valid_mask = GrayImage::from_pixel(template.width(), template.height(), Luma([255]));
        let mut negative = input.clone();
        negative.mask = Some(&valid_mask);
        assert!(matches!(
            ImageRuleVerification::verify(negative).unwrap_err(),
            ImageRuleVerificationError::InsufficientNegativeMargin { .. }
        ));

        let lower_negative_samples = vec![corpus_sample(
            &rule,
            "negative/a",
            NEGATIVE_CORPUS_SHA256,
            0.80,
        )];
        let ambiguous = ImageRuleVerificationInput {
            rule: &rule,
            template: &template,
            mask: Some(&valid_mask),
            captured_dpi: 96,
            current_dpi: 96,
            region_revision: 13,
            search_dimensions: (640, 360),
            negative_samples: &lower_negative_samples,
            observed_clusters: &clusters,
            maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
        };
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
        let samples = vec![corpus_sample(
            &rule,
            "negative/a",
            NEGATIVE_CORPUS_SHA256,
            0.80,
        )];

        let verified = ImageRuleVerification::verify(ImageRuleVerificationInput {
            rule: &rule,
            template: &template,
            mask: None,
            captured_dpi: 96,
            current_dpi: 96,
            region_revision: 13,
            search_dimensions: (640, 360),
            negative_samples: &samples,
            observed_clusters: &clusters,
            maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
        })
        .unwrap();

        assert_eq!(INITIAL_SIMILARITY_THRESHOLD, 0.95);
        assert_eq!(verified.threshold(), 0.91);
        assert!(verified.template_variance() >= MIN_TEMPLATE_VARIANCE);
        assert!((verified.negative_margin() - 0.11).abs() < 0.000_001);
        assert_eq!(verified.ambiguity_margin(), None);
        assert!(verified.score_cells() > 0);
        assert_eq!(verified.artifact().rule_id, rule.id);
        assert_eq!(verified.artifact().rule_revision, rule.revision);
        assert_eq!(verified.artifact().template, rule.template);
        assert_eq!(verified.artifact().region_revision, 13);
        assert_eq!(verified.artifact().scales_percent, vec![100]);
        assert_eq!(verified.artifact().best_negative_score(), 0.80);
    }

    #[test]
    fn verification_rejects_unbounded_observed_candidate_count() {
        let mut rule = fixture_rule(0.91);
        rule.scales_percent = vec![100];
        let template = fixture_icon();
        let samples = vec![corpus_sample(
            &rule,
            "negative/a",
            NEGATIVE_CORPUS_SHA256,
            0.80,
        )];
        let clusters = (0..=DEFAULT_MAX_CANDIDATES)
            .map(|x| CandidateCluster::from_peak(peak(x as i32, 0, 0.96, 100)))
            .collect::<Vec<_>>();

        let error = ImageRuleVerification::verify(ImageRuleVerificationInput {
            rule: &rule,
            template: &template,
            mask: None,
            captured_dpi: 96,
            current_dpi: 96,
            region_revision: 13,
            search_dimensions: (640, 360),
            negative_samples: &samples,
            observed_clusters: &clusters,
            maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
        })
        .unwrap_err();

        assert_eq!(
            error,
            ImageRuleVerificationError::InvalidWorkPlan(ImageMatchError::CandidateLimit {
                maximum: DEFAULT_MAX_CANDIDATES,
            })
        );

        let one_unbounded_cluster = CandidateCluster {
            best: peak(0, 0, 0.96, 100),
            members: (0..=DEFAULT_MAX_CANDIDATES)
                .map(|x| peak(x as i32, 0, 0.96, 100))
                .collect(),
        };
        let error = ImageRuleVerification::verify(ImageRuleVerificationInput {
            rule: &rule,
            template: &template,
            mask: None,
            captured_dpi: 96,
            current_dpi: 96,
            region_revision: 13,
            search_dimensions: (640, 360),
            negative_samples: &samples,
            observed_clusters: &[one_unbounded_cluster],
            maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
        })
        .unwrap_err();
        assert_eq!(
            error,
            ImageRuleVerificationError::InvalidWorkPlan(ImageMatchError::CandidateLimit {
                maximum: DEFAULT_MAX_CANDIDATES,
            })
        );
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
    fn image_detector_rejects_frame_from_different_client_dimensions() {
        let compiled = compiled_image_macro(fixture_icon());
        let BlockKind::Observe { condition } = &compiled.definition().blocks[0].kind else {
            unreachable!()
        };
        let mut stale = frame(1, 100, 23, 100).frame;
        stale.client_width += 1;
        let capture = FixtureCapture {
            frames: Mutex::new(vec![captured_frame(
                fixture_search_with_icon_at(23, 17),
                stale,
            )]),
        };

        let error = ImageDetector::new()
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
            .unwrap_err();

        assert!(error.to_string().contains("client geometry"));
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
    fn stability_capacity_is_released_by_completed_run_generation_only() {
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
                    frame(3, 210, 23, 100).frame,
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
        detector.run_finished("run-a", &[1]);
        assert!(observe("run-b", 120).is_ok());
    }

    #[test]
    fn more_than_default_capacity_completed_image_runs_do_not_exhaust_state() {
        let compiled = compiled_image_macro(fixture_icon());
        let BlockKind::Observe { condition } = &compiled.definition().blocks[0].kind else {
            unreachable!()
        };
        let frames = (0..300)
            .map(|index| {
                captured_frame(
                    fixture_search_with_icon_at(23, 17),
                    frame(index + 1, 100 + index, 23, 100).frame,
                )
            })
            .collect();
        let capture = FixtureCapture {
            frames: Mutex::new(frames),
        };
        let detector = ImageDetector::new();

        for index in 0..300 {
            let run_id = format!("completed-{index}");
            detector
                .observe(
                    &ObservationRequest {
                        run_id: &run_id,
                        generation: 1,
                        condition,
                        compiled: &compiled,
                        observed_at_ms: index,
                    },
                    &capture,
                )
                .unwrap();
            detector.run_finished(&run_id, &[1]);
        }
    }

    #[test]
    fn finishing_one_generation_keeps_same_run_other_generation_active() {
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
                    frame(3, 210, 23, 100).frame,
                ),
            ]),
        };
        let detector = ImageDetector::with_stability_capacity(2);
        let observe = |generation, observed_at_ms| {
            detector
                .observe(
                    &ObservationRequest {
                        run_id: "same-run",
                        generation,
                        condition,
                        compiled: &compiled,
                        observed_at_ms,
                    },
                    &capture,
                )
                .unwrap()
        };

        assert!(!observe(1, 100).matched);
        assert!(!observe(2, 110).matched);
        detector.run_finished("same-run", &[1]);
        assert!(observe(2, 210).matched);
    }
}
