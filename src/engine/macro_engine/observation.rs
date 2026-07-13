use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::engine::{
    automation::CaptureSource,
    types::{Rect, ScreenImage},
};

use super::{CompiledMacro, Condition};

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
    #[serde(default)]
    pub details: serde_json::Value,
}

impl DetectorEvidence {
    pub fn unmatched(frame_id: u64, captured_at_ms: u64) -> Self {
        Self {
            matched: false,
            frame_id,
            captured_at_ms,
            match_rect: None,
            score: None,
            match_count: 0,
            stable_frames: 0,
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
