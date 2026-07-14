#![cfg(test)]

use crate::engine::macro_engine::{
    Action, Block, BlockKind, Condition, FocusLossPolicy, Limit, MACRO_SCHEMA_VERSION,
    MacroDefinition, MatchSelectionPolicy, MouseButton, ObserveMode, PreprocessProfile,
    RegionDefinition, SafetyPolicy, TargetProfile, TextMatchMode, TextRule,
};
use crate::engine::types::RectRatio;
use crate::macro_ui::{EditorDraft, MacroPageState};

pub fn fixture_definition() -> MacroDefinition {
    MacroDefinition {
        schema_version: MACRO_SCHEMA_VERSION,
        id: "macro".into(),
        name: "Macro".into(),
        revision: 1,
        target: TargetProfile {
            process_path: "game.exe".into(),
            window_class: "d4".into(),
            title_contains: "Diablo".into(),
            captured_client_width: 1280,
            captured_client_height: 720,
            captured_dpi: 96,
        },
        regions: vec![RegionDefinition {
            id: "scan".into(),
            revision: 1,
            rect: RectRatio {
                x: 0.1,
                y: 0.1,
                width: 0.3,
                height: 0.2,
            },
        }],
        points: vec![],
        text_rules: vec![TextRule {
            id: "rule".into(),
            revision: 1,
            region_id: "scan".into(),
            language: "en-US".into(),
            preprocess: PreprocessProfile::Grayscale,
            expected: "Salvage".into(),
            match_mode: TextMatchMode::Contains,
            threshold: 0.9,
            case_sensitive: false,
            allow_cross_line: false,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 250,
            timeout_ms: Limit::Unlimited,
            stable_frames: 2,
        }],
        image_rules: vec![],
        blocks: vec![observe_block("observe")],
        safety: SafetyPolicy {
            max_runtime_ms: Limit::Unlimited,
            max_clicks: Limit::Unlimited,
            max_observation_retries: Limit::Unlimited,
            max_observations_per_second: 20,
            minimum_click_interval_ms: 100,
            focus_loss: FocusLossPolicy::Stop,
        },
    }
}

pub fn fixture_draft() -> EditorDraft {
    EditorDraft::new(fixture_definition())
}

pub fn fixture_if() -> MacroDefinition {
    let mut definition = fixture_definition();
    definition.blocks = vec![Block {
        id: "if-1".into(),
        enabled: true,
        kind: BlockKind::If {
            condition: text_condition("if-1"),
            then_body: vec![observe_block("then-observe")],
            else_body: vec![comment_block("else-note")],
        },
    }];
    definition
}

pub fn fixture_continuous_with_observe_and_action() -> MacroDefinition {
    let mut definition = fixture_definition();
    definition.blocks = vec![Block {
        id: "loop".into(),
        enabled: true,
        kind: BlockKind::Continuous {
            body: vec![
                observe_block("observe"),
                Block {
                    id: "action".into(),
                    enabled: true,
                    kind: BlockKind::Action {
                        action: Action::ClickTextMatch {
                            source_block_id: "observe".into(),
                            button: MouseButton::Left,
                        },
                    },
                },
            ],
        },
    }];
    definition
}

pub fn fixture_nested_loop_draft() -> EditorDraft {
    let mut definition = fixture_definition();
    definition.blocks = vec![Block {
        id: "loop".into(),
        enabled: true,
        kind: BlockKind::Continuous {
            body: vec![comment_block("child")],
        },
    }];
    EditorDraft::new(definition)
}

pub fn fixture_large_definition() -> MacroDefinition {
    let mut definition = fixture_definition();
    definition.blocks = (0..500)
        .map(|index| comment_block(&format!("comment-{index:03}")))
        .collect();
    definition
}

pub fn fixture_ready_state(_mode: crate::engine::macro_engine::RunMode) -> MacroPageState {
    MacroPageState {
        draft: Some(fixture_draft()),
        ..MacroPageState::default()
    }
}

pub fn fixture_with_pinned_run() -> MacroPageState {
    let definition = fixture_definition();
    let mut state = fixture_ready_state(crate::engine::macro_engine::RunMode::ObservationOnly);
    state.running_snapshot = Some(crate::macro_ui::RunDefinitionSnapshot::from_saved(
        "run-1",
        crate::engine::macro_engine::SavedRevision {
            definition,
            definition_hash: "fixture-hash".into(),
            pinned_assets: vec![],
        },
    ));
    state
}

pub fn corrupt_layout() -> crate::ui_state::MacroCanvasLayout {
    let mut layout = crate::ui_state::MacroCanvasLayout::default();
    layout
        .node_positions
        .insert("observe".into(), [f32::NAN, f32::INFINITY]);
    layout
}

fn observe_block(id: &str) -> Block {
    Block {
        id: id.into(),
        enabled: true,
        kind: BlockKind::Observe {
            condition: text_condition(id),
        },
    }
}

fn comment_block(id: &str) -> Block {
    Block {
        id: id.into(),
        enabled: true,
        kind: BlockKind::Comment {
            text: "Note".into(),
        },
    }
}

fn text_condition(source_block_id: &str) -> Condition {
    Condition::Text {
        source_block_id: source_block_id.into(),
        rule_id: "rule".into(),
        mode: ObserveMode::CheckNow,
    }
}
