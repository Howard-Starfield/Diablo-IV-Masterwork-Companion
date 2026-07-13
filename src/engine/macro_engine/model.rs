use crate::engine::types::{PointRatio, RectRatio};
use serde::{Deserialize, Serialize};

pub use crate::engine::automation::MouseButton;

pub const MACRO_SCHEMA_VERSION: u32 = 1;
pub const IMAGE_RULE_VERIFICATION_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AssetRef {
    pub id: String,
    pub revision: u64,
    pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Limit<T> {
    Finite(T),
    Unlimited,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MacroDefinition {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub revision: u64,
    pub target: TargetProfile,
    pub regions: Vec<RegionDefinition>,
    pub points: Vec<PointDefinition>,
    pub text_rules: Vec<TextRule>,
    pub image_rules: Vec<ImageRule>,
    pub blocks: Vec<Block>,
    pub safety: SafetyPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetProfile {
    pub process_path: String,
    pub window_class: String,
    pub title_contains: String,
    pub captured_client_width: u32,
    pub captured_client_height: u32,
    pub captured_dpi: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegionDefinition {
    pub id: String,
    pub revision: u64,
    pub rect: RectRatio,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PointDefinition {
    pub id: String,
    pub revision: u64,
    pub point: PointRatio,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextRule {
    pub id: String,
    pub revision: u64,
    pub region_id: String,
    pub language: String,
    pub preprocess: PreprocessProfile,
    pub expected: String,
    pub match_mode: TextMatchMode,
    pub threshold: f64,
    pub case_sensitive: bool,
    pub allow_cross_line: bool,
    pub match_policy: MatchSelectionPolicy,
    pub poll_interval_ms: u64,
    pub timeout_ms: Limit<u64>,
    pub stable_frames: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageRule {
    pub id: String,
    pub revision: u64,
    pub region_id: String,
    pub template: AssetRef,
    pub transparent_mask: Option<AssetRef>,
    pub threshold: f32,
    pub scales_percent: Vec<u16>,
    pub stable_frames: u8,
    pub maximum_center_drift_px: u32,
    pub minimum_runner_up_margin: f32,
    pub verification: Option<ImageRuleVerificationArtifact>,
    pub match_policy: MatchSelectionPolicy,
    pub poll_interval_ms: u64,
    pub timeout_ms: Limit<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageVerificationPreprocess {
    GrayscaleNormalizedCrossCorrelation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageRuleVerificationArtifact {
    pub version: u32,
    pub preprocess: ImageVerificationPreprocess,
    pub rule_id: String,
    pub rule_revision: u64,
    pub template: AssetRef,
    pub transparent_mask: Option<AssetRef>,
    pub captured_dpi: u32,
    pub region_id: String,
    pub region_revision: u64,
    pub search_width: u32,
    pub search_height: u32,
    pub scales_percent: Vec<u16>,
    pub threshold: f32,
    pub minimum_runner_up_margin: f32,
    /// SHA-256 of canonical ordered negative sample identities/content hashes and evaluation inputs.
    pub negative_corpus_sha256: String,
    pub negative_sample_count: u64,
    pub best_negative_score: f32,
    pub active_mask_variance: f32,
    /// SHA-256 over every persisted binding and result field above.
    pub verification_fingerprint_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SafetyPolicy {
    pub max_runtime_ms: Limit<u64>,
    pub max_clicks: Limit<u64>,
    pub max_observation_retries: Limit<u64>,
    pub max_observations_per_second: u32,
    pub minimum_click_interval_ms: u64,
    pub focus_loss: FocusLossPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextMatchMode {
    Exact,
    Contains,
    Fuzzy,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreprocessProfile {
    Original,
    Grayscale,
    HighContrast,
    SmallText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchSelectionPolicy {
    ExactlyOne,
    HighestScore,
    FirstReadingOrder,
    Topmost,
    Bottommost,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionTarget {
    TextMatch { source_block_id: String },
    ImageMatch { source_block_id: String },
    Point { point_id: String },
    Region { region_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusLossPolicy {
    Pause,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub id: String,
    pub enabled: bool,
    pub kind: BlockKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockKind {
    Observe {
        condition: Condition,
    },
    Action {
        action: Action,
    },
    If {
        condition: Condition,
        then_body: Vec<Block>,
        else_body: Vec<Block>,
    },
    Wait {
        duration_ms: u64,
    },
    RepeatN {
        count: u32,
        body: Vec<Block>,
    },
    RepeatUntil {
        condition: Condition,
        max_iterations: Limit<u64>,
        body: Vec<Block>,
    },
    Continuous {
        body: Vec<Block>,
    },
    WatchGroup {
        group: WatchGroup,
    },
    StopSuccess,
    StopError {
        message: String,
    },
    Comment {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "detector", rename_all = "snake_case")]
pub enum Condition {
    Text {
        source_block_id: String,
        rule_id: String,
        mode: ObserveMode,
    },
    Image {
        source_block_id: String,
        rule_id: String,
        mode: ObserveMode,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "detector", rename_all = "snake_case", deny_unknown_fields)]
pub enum PassiveCondition {
    Text {
        source_block_id: String,
        rule_id: String,
    },
    Image {
        source_block_id: String,
        rule_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ObserveMode {
    CheckNow,
    WaitForTrue {
        timeout_ms: Limit<u64>,
        timeout_outcome: TimeoutOutcome,
    },
    WaitForFalse {
        timeout_ms: Limit<u64>,
        timeout_outcome: TimeoutOutcome,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    ClickTextMatch {
        source_block_id: String,
        button: MouseButton,
    },
    ClickImageMatch {
        source_block_id: String,
        button: MouseButton,
    },
    ClickPoint {
        point_id: String,
        button: MouseButton,
    },
    ClickRegion {
        region_id: String,
        button: MouseButton,
    },
    MoveOnly {
        target: ActionTarget,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchGroup {
    pub lanes: Vec<WatchLane>,
    pub timeout_ms: Limit<u64>,
    pub timeout_outcome: TimeoutOutcome,
    pub cooldown_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchLane {
    pub id: String,
    pub enabled: bool,
    pub condition: PassiveCondition,
    pub then_body: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimeoutOutcome {
    StopError { message: String },
    Continue,
    RunBody { body: Vec<Block> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_button_round_trip_remains_stable() {
        let button: crate::engine::macro_engine::MouseButton = MouseButton::Right;
        let json = serde_json::to_string(&button).unwrap();

        assert_eq!(json, r#""right""#);
        assert_eq!(serde_json::from_str::<MouseButton>(&json).unwrap(), button);
    }

    #[test]
    fn limit_round_trips_with_explicit_unlimited_tag() {
        let json = serde_json::to_string(&Limit::<u64>::Unlimited).unwrap();
        assert_eq!(json, r#"{"kind":"unlimited"}"#);
        assert_eq!(
            serde_json::from_str::<Limit<u64>>(&json).unwrap(),
            Limit::Unlimited
        );
    }

    #[test]
    fn standalone_wait_modes_round_trip_timeout_and_outcome() {
        let cases = [
            ObserveMode::WaitForTrue {
                timeout_ms: Limit::Finite(2_500),
                timeout_outcome: TimeoutOutcome::StopError {
                    message: "not found".to_string(),
                },
            },
            ObserveMode::WaitForFalse {
                timeout_ms: Limit::Unlimited,
                timeout_outcome: TimeoutOutcome::Continue,
            },
            ObserveMode::WaitForTrue {
                timeout_ms: Limit::Finite(100),
                timeout_outcome: TimeoutOutcome::RunBody {
                    body: vec![Block {
                        id: "timeout".to_string(),
                        enabled: true,
                        kind: BlockKind::Comment {
                            text: "fallback".to_string(),
                        },
                    }],
                },
            },
        ];

        for mode in cases {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(serde_json::from_str::<ObserveMode>(&json).unwrap(), mode);
        }
    }

    #[test]
    fn passive_watch_condition_has_no_wait_mode_or_timeout_body() {
        let condition = PassiveCondition::Text {
            source_block_id: "observe".to_string(),
            rule_id: "text-rule".to_string(),
        };

        assert_eq!(
            serde_json::to_string(&condition).unwrap(),
            r#"{"detector":"text","source_block_id":"observe","rule_id":"text-rule"}"#
        );
        assert_eq!(
            serde_json::from_str::<PassiveCondition>(
                r#"{"detector":"text","source_block_id":"observe","rule_id":"text-rule"}"#
            )
            .unwrap(),
            condition
        );
    }

    #[test]
    fn passive_watch_condition_rejects_wait_fields() {
        let json = r#"{
            "detector":"text",
            "source_block_id":"observe",
            "rule_id":"text-rule",
            "mode":{"type":"wait_for_true","timeout_ms":{"kind":"unlimited"},"timeout_outcome":{"type":"continue"}}
        }"#;

        assert!(serde_json::from_str::<PassiveCondition>(json).is_err());
    }

    #[test]
    fn image_verification_artifact_round_trips_all_binding_fields() {
        let template = AssetRef {
            id: "template".to_string(),
            revision: 3,
            content_hash: "template-hash".to_string(),
        };
        let artifact = ImageRuleVerificationArtifact {
            version: IMAGE_RULE_VERIFICATION_VERSION,
            preprocess: ImageVerificationPreprocess::GrayscaleNormalizedCrossCorrelation,
            rule_id: "image".to_string(),
            rule_revision: 7,
            template,
            transparent_mask: None,
            captured_dpi: 96,
            region_id: "region".to_string(),
            region_revision: 5,
            search_width: 640,
            search_height: 360,
            scales_percent: vec![95, 100, 105],
            threshold: 0.91,
            minimum_runner_up_margin: 0.03,
            negative_corpus_sha256:
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            negative_sample_count: 100_000,
            best_negative_score: 0.80,
            active_mask_variance: 42.5,
            verification_fingerprint_sha256:
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
        };

        let json = serde_json::to_string(&artifact).unwrap();
        assert_eq!(
            serde_json::from_str::<ImageRuleVerificationArtifact>(&json).unwrap(),
            artifact
        );
    }
}
