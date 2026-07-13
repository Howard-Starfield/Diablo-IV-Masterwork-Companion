use std::sync::Arc;

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
        Self::captured_match(
            matched,
            frame,
            match_rect,
            score,
            match_count,
            stable_frames,
            details,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn captured_match(
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
    #[serde(default)]
    pub side_effect_epoch: u64,
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
    pub side_effect_epoch: u64,
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

    /// Invalidates detector-owned temporal evidence immediately after an action boundary.
    fn side_effect_boundary(&self, _run_id: &str, _generation: u64, _next_epoch: u64) {}
}

/// Routes typed conditions to the detector that owns that evidence family while forwarding
/// lifecycle invalidation to both stateful engines.
pub struct ConditionDetectorRouter {
    text: Arc<dyn ConditionDetector>,
    image: Arc<dyn ConditionDetector>,
}

impl ConditionDetectorRouter {
    pub fn new(text: Arc<dyn ConditionDetector>, image: Arc<dyn ConditionDetector>) -> Self {
        Self { text, image }
    }
}

impl ConditionDetector for ConditionDetectorRouter {
    fn observe(
        &self,
        request: &ObservationRequest<'_>,
        capture: &(dyn CaptureSource + Send + Sync),
    ) -> Result<DetectorEvidence> {
        match request.condition {
            Condition::Text { .. } => self.text.observe(request, capture),
            Condition::Image { .. } => self.image.observe(request, capture),
        }
    }

    fn run_finished(&self, run_id: &str, generations: &[u64]) {
        self.text.run_finished(run_id, generations);
        self.image.run_finished(run_id, generations);
    }

    fn side_effect_boundary(&self, run_id: &str, generation: u64, next_epoch: u64) {
        self.text
            .side_effect_boundary(run_id, generation, next_epoch);
        self.image
            .side_effect_boundary(run_id, generation, next_epoch);
    }
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
    use crate::engine::{
        automation::CaptureSource,
        macro_engine::{
            Block, BlockKind, FocusLossPolicy, ImageFrameMetadata, Limit, MACRO_SCHEMA_VERSION,
            MacroDefinition, ObserveMode, SafetyPolicy, SavedRevision, TargetProfile,
        },
    };
    use sha2::{Digest, Sha256};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct TaggedDetector {
        tag: &'static str,
        observed: Mutex<Vec<&'static str>>,
        finished: Mutex<Vec<String>>,
        boundaries: Mutex<Vec<(String, u64, u64)>>,
    }

    impl TaggedDetector {
        fn new(tag: &'static str) -> Self {
            Self {
                tag,
                ..Self::default()
            }
        }
    }

    impl ConditionDetector for TaggedDetector {
        fn observe(
            &self,
            request: &ObservationRequest<'_>,
            _capture: &(dyn CaptureSource + Send + Sync),
        ) -> Result<DetectorEvidence> {
            self.observed.lock().unwrap().push(self.tag);
            Ok(DetectorEvidence::new(
                true,
                request.observed_at_ms,
                request.observed_at_ms,
                Some(Rect::new(1, 2, 3, 4)),
                Some(if self.tag == "text" { 0.9 } else { 0.8 }),
                1,
                1,
                serde_json::json!({"tag": self.tag}),
            ))
        }

        fn run_finished(&self, run_id: &str, _generations: &[u64]) {
            self.finished.lock().unwrap().push(run_id.to_string());
        }

        fn side_effect_boundary(&self, run_id: &str, generation: u64, next_epoch: u64) {
            self.boundaries
                .lock()
                .unwrap()
                .push((run_id.to_string(), generation, next_epoch));
        }
    }

    #[derive(Default)]
    struct EmptyCapture;

    impl CaptureSource for EmptyCapture {
        fn capture(&self, _rect: Rect) -> Result<ScreenImage> {
            anyhow::bail!("router test detector must not capture")
        }
    }

    fn compiled_for_router_test() -> CompiledMacro {
        let definition = MacroDefinition {
            schema_version: MACRO_SCHEMA_VERSION,
            id: "router".to_string(),
            name: "Router".to_string(),
            revision: 1,
            target: TargetProfile {
                process_path: "game.exe".to_string(),
                window_class: "game".to_string(),
                title_contains: "Diablo".to_string(),
                captured_client_width: 64,
                captured_client_height: 48,
                captured_dpi: 96,
            },
            regions: vec![],
            points: vec![],
            text_rules: vec![],
            image_rules: vec![],
            blocks: vec![Block {
                id: "comment".to_string(),
                enabled: true,
                kind: BlockKind::Comment {
                    text: "router fixture".to_string(),
                },
            }],
            safety: SafetyPolicy {
                max_runtime_ms: Limit::Finite(1_000),
                max_clicks: Limit::Finite(1),
                max_observation_retries: Limit::Finite(1),
                max_observations_per_second: 1,
                minimum_click_interval_ms: 1,
                focus_loss: FocusLossPolicy::Stop,
            },
        };
        let bytes = serde_json::to_vec_pretty(&definition).unwrap();
        CompiledMacro::compile(SavedRevision {
            definition,
            definition_hash: format!("{:x}", Sha256::digest(bytes)),
            pinned_assets: vec![],
        })
        .unwrap()
    }

    #[test]
    fn detector_router_routes_by_typed_condition_and_forwards_lifecycle() {
        let text = Arc::new(TaggedDetector::new("text"));
        let image = Arc::new(TaggedDetector::new("image"));
        let router = ConditionDetectorRouter::new(text.clone(), image.clone());
        let compiled = compiled_for_router_test();
        let capture = EmptyCapture;
        let text_condition = Condition::Text {
            source_block_id: "observe-text".to_string(),
            rule_id: "text".to_string(),
            mode: ObserveMode::CheckNow,
        };
        let image_condition = Condition::Image {
            source_block_id: "observe-image".to_string(),
            rule_id: "image".to_string(),
            mode: ObserveMode::CheckNow,
        };

        for (condition, expected) in [(&text_condition, "text"), (&image_condition, "image")] {
            let evidence = router
                .observe(
                    &ObservationRequest {
                        run_id: "run",
                        generation: 2,
                        side_effect_epoch: 3,
                        condition,
                        compiled: &compiled,
                        observed_at_ms: 4,
                    },
                    &capture,
                )
                .unwrap();
            assert_eq!(evidence.details["tag"], expected);
        }
        router.side_effect_boundary("run", 2, 4);
        router.run_finished("run", &[2]);

        assert_eq!(*text.observed.lock().unwrap(), vec!["text"]);
        assert_eq!(*image.observed.lock().unwrap(), vec!["image"]);
        assert_eq!(*text.finished.lock().unwrap(), vec!["run"]);
        assert_eq!(*image.finished.lock().unwrap(), vec!["run"]);
        assert_eq!(
            *text.boundaries.lock().unwrap(),
            vec![("run".to_string(), 2, 4)]
        );
        assert_eq!(
            *image.boundaries.lock().unwrap(),
            vec![("run".to_string(), 2, 4)]
        );
    }

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
            process_id: 10,
            process_started_at_100ns: 11,
            client_x: 0,
            client_y: 0,
            client_width: 64,
            client_height: 48,
            geometry_revision: 3,
            display_id: 5,
            display_profile_revision: 4,
            dpi: 96,
            is_visible: true,
            is_minimized: false,
            is_foreground: true,
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
