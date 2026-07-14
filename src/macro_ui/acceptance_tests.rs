use crate::engine::macro_engine::{
    Block, BlockKind, Limit, PassiveCondition, RunEvent, RunMode, SavedRevision, TimeoutOutcome,
    WatchGroup, WatchLane, validate_macro,
};
use crate::macro_ui::canvas::{CanvasHit, finish_connection};
use crate::macro_ui::canvas_model::{
    CanvasConnectionError, CanvasEdgeKind, CanvasGroupKind, OutputPort, connection_command,
    project_canvas,
};
use crate::macro_ui::monitor::{RunDefinitionSnapshot, project_monitor};
use crate::macro_ui::test_support::{
    corrupt_layout, fixture_continuous_with_observe_and_action, fixture_definition,
    fixture_nested_loop_draft, fixture_ready_state, fixture_with_pinned_run,
};
use crate::macro_ui::{
    CanvasViewport, MacroIntent, MacroPageState, SavedMacroIdentity, node_rect, reconcile_layout,
    reveal_node, run_control_availability, select_canvas, visible_nodes,
};
use crate::ui_state::UiStateStore;

#[test]
fn layout_edits_do_not_change_saved_or_running_identity() {
    let mut state = fixture_with_pinned_run();
    state.set_selected_saved(SavedMacroIdentity {
        macro_id: "macro".into(),
        revision: 1,
        definition_hash: "fixture-hash".into(),
    });
    let saved = state.selected_saved.clone();
    let running = format!("{:?}", state.running_snapshot.as_ref().unwrap());

    state.move_canvas_node("observe", [440.0, 210.0]).unwrap();

    assert_eq!(state.selected_saved, saved);
    assert_eq!(
        format!("{:?}", state.running_snapshot.as_ref().unwrap()),
        running
    );
}

#[test]
fn persisted_layout_round_trip_leaves_definition_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let (mut store, _) = UiStateStore::open(temp.path().join("ui-state.json"));
    let definition = fixture_definition();
    let definition_before = definition.clone();
    let saved = SavedMacroIdentity {
        macro_id: definition.id.clone(),
        revision: definition.revision,
        definition_hash: "definition-hash".into(),
    };
    let mut state = MacroPageState::default();
    state.load_saved_draft(definition.clone(), saved.clone());
    state.move_canvas_node("observe", [440.0, 210.0]).unwrap();
    state.persist_canvas_layout(&mut store);

    let mut reopened = MacroPageState::default();
    reopened.load_saved_draft(definition, saved);
    reopened.hydrate_canvas_layout(&store);

    assert_eq!(
        reopened.canvas_layout.node_positions["observe"],
        [440.0, 210.0]
    );
    assert_eq!(
        reopened.draft.as_ref().unwrap().definition,
        definition_before
    );
}

#[test]
fn corrupt_layout_recovery_preserves_canonical_definition() {
    let definition = fixture_definition();
    let recovered = reconcile_layout(&project_canvas(&definition), corrupt_layout());

    assert!(validate_macro(&definition).is_empty());
    assert!(recovered.is_finite());
    assert!(recovered.node_positions.contains_key("observe"));
}

#[test]
fn stop_replaces_oldest_non_stop_intent_when_queue_is_full() {
    let mut state = fixture_ready_state(RunMode::ObservationOnly);
    for index in 0..64 {
        state.push_intent(MacroIntent::Rename {
            saved: SavedMacroIdentity {
                macro_id: "macro".into(),
                revision: 1,
                definition_hash: "fixture-hash".into(),
            },
            name: format!("name-{index}"),
        });
    }
    state.push_intent(MacroIntent::Stop);

    assert_eq!(state.pending_intent_count(), 64);
    let intents = state.drain_intents().collect::<Vec<_>>();
    assert!(matches!(intents.last(), Some(MacroIntent::Stop)));
    assert_eq!(intents.len(), 64);
    for (expected, intent) in (1..64).zip(&intents[..63]) {
        assert!(matches!(
            intent,
            MacroIntent::Rename { name, .. } if name == &format!("name-{expected}")
        ));
    }
}

#[test]
fn invalid_cross_branch_link_and_generated_loop_return_cannot_mutate_the_draft() {
    let draft = fixture_nested_loop_draft();
    let before = draft.definition.clone();
    assert_eq!(
        connection_command(&draft, OutputPort::Next("child".into()), "loop"),
        Err(CanvasConnectionError::IllegalDescendant)
    );
    assert_eq!(draft.definition, before);

    let loop_draft =
        crate::macro_ui::EditorDraft::new(fixture_continuous_with_observe_and_action());
    let response = finish_connection(
        &loop_draft,
        OutputPort::LoopReturn("loop".into()),
        CanvasHit::Node("observe".into()),
    );
    assert!(response.editor_command.is_none());
    assert_eq!(
        loop_draft.definition,
        fixture_continuous_with_observe_and_action()
    );
}

#[test]
fn watch_lane_projection_keeps_canonical_lane_order() {
    let mut definition = fixture_definition();
    let lane = |id: &str| WatchLane {
        id: id.into(),
        enabled: true,
        condition: PassiveCondition::Text {
            source_block_id: "observe".into(),
            rule_id: "rule".into(),
        },
        then_body: vec![],
    };
    definition.blocks = vec![Block {
        id: "watch".into(),
        enabled: true,
        kind: BlockKind::WatchGroup {
            group: WatchGroup {
                lanes: vec![lane("first"), lane("second"), lane("third")],
                timeout_ms: Limit::Unlimited,
                timeout_outcome: TimeoutOutcome::Continue,
                cooldown_ms: 1,
            },
        },
    }];

    let priorities = project_canvas(&definition)
        .groups
        .iter()
        .filter(|group| group.id.kind == CanvasGroupKind::WatchLaneThen)
        .map(|group| group.lane_priority)
        .collect::<Vec<_>>();

    assert_eq!(priorities, vec![Some(1), Some(2), Some(3)]);
}

#[test]
fn fit_view_and_active_node_reveal_are_presentation_only() {
    let mut state = fixture_ready_state(RunMode::ObservationOnly);
    state.canvas_layout = corrupt_layout();
    state.load_canvas_layout(state.canvas_layout.clone());
    state.fit_canvas_view([900.0, 700.0]);
    let layout_after_fit = state.canvas_layout.clone();

    select_canvas(
        &mut state,
        crate::macro_ui::canvas_model::CanvasSelection::Block("observe".into()),
    );

    assert!(state.canvas_layout.is_finite());
    assert_eq!(state.selected_block_id.as_deref(), Some("observe"));
    assert_eq!(state.canvas_layout, layout_after_fit);
}

#[test]
fn monitor_active_block_reveals_an_offscreen_node_without_mutating_saved_layout() {
    let definition = fixture_continuous_with_observe_and_action();
    let graph = project_canvas(&definition);
    let mut layout = reconcile_layout(&graph, Default::default());
    layout
        .node_positions
        .insert("observe".into(), [4_000.0, 2_500.0]);
    let saved_layout = layout.clone();
    let canvas = eframe::egui::Rect::from_min_size(
        eframe::egui::Pos2::ZERO,
        eframe::egui::Vec2::new(900.0, 430.0),
    );
    let mut viewport = CanvasViewport::from_layout(&layout, [canvas.width(), canvas.height()]);
    let snapshot = RunDefinitionSnapshot::from_saved(
        "run-1",
        SavedRevision {
            definition: definition.clone(),
            definition_hash: "fixture-hash".into(),
            pinned_assets: vec![],
        },
    );
    let events = vec![
        RunEvent::RunStarted {
            sequence: 1,
            elapsed_ms: 0,
            run_id: "run-1".into(),
            macro_id: definition.id.clone(),
            revision: definition.revision,
            definition_hash: "fixture-hash".into(),
            mode: RunMode::ObservationOnly,
        },
        RunEvent::BlockEntered {
            sequence: 2,
            elapsed_ms: 20,
            run_id: "run-1".into(),
            block_id: "observe".into(),
        },
    ];
    let monitor = project_monitor(Some(&definition.id), Some(&snapshot), &events);
    let active = monitor.active_block.as_deref().unwrap();
    let node = graph.node(active).unwrap();

    assert_eq!(monitor.active_loop.as_deref(), Some("loop"));
    assert!(
        !visible_nodes(&graph, &layout, &viewport, canvas)
            .iter()
            .any(|node| node.id == active)
    );
    assert!(reveal_node(&mut viewport, canvas, node_rect(node, &layout)));
    assert!(
        visible_nodes(&graph, &layout, &viewport, canvas)
            .iter()
            .any(|node| node.id == active)
    );
    assert_eq!(layout, saved_layout);
}

#[test]
fn disabled_run_controls_explain_why_the_action_is_unavailable() {
    let state = fixture_ready_state(RunMode::ObservationOnly);
    let controls = run_control_availability(&state);

    assert!(!controls.can_dry_run);
    assert!(!controls.can_run_live);
    assert_eq!(
        controls.disabled_reason.as_deref(),
        Some("Save and select a validated macro before running.")
    );
}

#[test]
fn continuous_loop_return_remains_a_generated_noneditable_edge() {
    let graph = project_canvas(&fixture_continuous_with_observe_and_action());
    assert!(graph.edges.iter().any(|edge| {
        edge.kind == CanvasEdgeKind::LoopReturn
            && edge.from == OutputPort::LoopReturn("loop".into())
            && !edge.editable
    }));
}
