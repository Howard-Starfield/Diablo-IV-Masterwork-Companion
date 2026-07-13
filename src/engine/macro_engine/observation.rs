use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::engine::{
    automation::CaptureSource,
    types::{Rect, ScreenImage},
};

use super::{CompiledMacro, Condition, ImageFrameMetadata};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorKind {
    Text,
    Image,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetectorEvidence {
    pub matched: bool,
    pub frame_id: u64,
    pub captured_at_ms: u64,
    pub match_rect: Option<Rect>,
    pub score: Option<f64>,
    pub match_count: u32,
    pub stable_frames: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_metadata: Option<ImageFrameMetadata>,
    #[serde(default)]
    pub details: serde_json::Value,
}

impl DetectorEvidence {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        matched: bool,
        frame_id: u64,
        captured_at_ms: u64,
        match_rect: Option<Rect>,
        score: Option<f64>,
        match_count: u32,
        stable_frames: u8,
        details: serde_json::Value,
    ) -> Self {
        Self {
            matched,
            frame_id,
            captured_at_ms,
            match_rect: matched.then_some(match_rect).flatten(),
            score,
            match_count,
            stable_frames,
            frame_metadata: None,
            details,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn image_match(
        matched: bool,
        frame: ImageFrameMetadata,
        match_rect: Option<Rect>,
        score: Option<f64>,
        match_count: u32,
        stable_frames: u8,
        details: serde_json::Value,
    ) -> Self {
        Self {
            matched,
            frame_id: frame.frame_id,
            captured_at_ms: frame.captured_at_ms,
            match_rect: matched.then_some(match_rect).flatten(),
            score,
            match_count,
            stable_frames,
            frame_metadata: Some(frame),
            details,
        }
    }

    pub fn unmatched(frame_id: u64, captured_at_ms: u64) -> Self {
        Self {
            matched: false,
            frame_id,
            captured_at_ms,
            match_rect: None,
            score: None,
            match_count: 0,
            stable_frames: 0,
            frame_metadata: None,
            details: serde_json::Value::Null,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationToken {
    pub run_id: String,
    pub generation: u64,
    pub source_block_id: String,
    pub detector: DetectorKind,
    pub region_id: String,
    pub region_revision: u64,
    pub rule_id: String,
    pub rule_revision: u64,
    pub frame_id: u64,
    pub captured_at_ms: u64,
    pub match_rect: Option<Rect>,
    pub score: Option<f64>,
    pub match_count: u32,
    pub stable_frames: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_metadata: Option<ImageFrameMetadata>,
    pub evidence: serde_json::Value,
}

impl ObservationToken {
    pub fn is_current(&self, run_id: &str, generation: u64) -> bool {
        self.run_id == run_id && self.generation == generation
    }
}

pub struct ObservationRequest<'a> {
    pub run_id: &'a str,
    pub generation: u64,
    pub condition: &'a Condition,
    pub compiled: &'a CompiledMacro,
    pub observed_at_ms: u64,
}

pub trait ConditionDetector: Send + Sync {
    fn observe(
        &self,
        request: &ObservationRequest<'_>,
        capture: &(dyn CaptureSource + Send + Sync),
    ) -> Result<DetectorEvidence>;

    /// Releases detector state owned by every generation actually observed by one completed run.
    /// Implementations must not affect other runs or generations absent from this slice.
    fn run_finished(&self, _run_id: &str, _generations: &[u64]) {}
}

#[derive(Debug, Default)]
pub struct UnavailableDetector;

impl ConditionDetector for UnavailableDetector {
    fn observe(
        &self,
        _request: &ObservationRequest<'_>,
        _capture: &(dyn CaptureSource + Send + Sync),
    ) -> Result<DetectorEvidence> {
        anyhow::bail!("no detector is configured")
    }
}

#[derive(Debug, Default)]
pub struct UnavailableCapture;

impl CaptureSource for UnavailableCapture {
    fn capture(&self, _rect: Rect) -> Result<ScreenImage> {
        anyhow::bail!("no capture source is configured")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::macro_engine::ImageFrameMetadata;

    #[test]
    fn negative_evidence_cannot_retain_click_geometry() {
        let evidence = DetectorEvidence::new(
            false,
            1,
            2,
            Some(Rect::new(10, 20, 30, 40)),
            Some(0.9),
            1,
            1,
            serde_json::Value::Null,
        );

        assert!(evidence.match_rect.is_none());
    }

    #[test]
    fn image_evidence_keeps_typed_frame_identity_but_not_unqualified_geometry() {
        let frame = ImageFrameMetadata {
            frame_id: 4,
            captured_at_ms: 120,
            window_id: 9,
            window_revision: 2,
            client_width: 64,
            client_height: 48,
            geometry_revision: 3,
            display_profile_revision: 4,
            dpi: 96,
            region_revision: 5,
            rule_revision: 6,
        };

        let evidence = DetectorEvidence::image_match(
            false,
            frame,
            Some(Rect::new(10, 20, 30, 40)),
            Some(0.98),
            1,
            1,
            serde_json::json!({"ambiguity_margin": 0.04}),
        );

        assert_eq!(evidence.frame_metadata, Some(frame));
        assert!(evidence.match_rect.is_none());
        assert_eq!(evidence.details["ambiguity_margin"], 0.04);
    }
}
