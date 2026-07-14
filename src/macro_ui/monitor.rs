use std::collections::{BTreeMap, HashMap, HashSet};

use eframe::egui::{self, Color32, Frame, Grid, RichText, Stroke, Ui};

use crate::engine::macro_engine::{
    ActionAttemptId, ActionState, Block, BlockKind, ControllerLifecycleProjection,
    ControllerSemanticProjection, MacroDefinition, RunEvent, RunMode, RunStatus, SavedRevision,
    StopReason,
};

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorProjection {
    pub run_id: Option<String>,
    pub definition_hash: Option<String>,
    pub running_revision: Option<u64>,
    pub mode: Option<RunMode>,
    pub status: RunStatus,
    pub elapsed_ms: u64,
    pub active_block: Option<String>,
    pub active_branch: Option<String>,
    pub active_loop: Option<String>,
    pub loop_iterations: Option<u64>,
    pub candidate_count: Option<u32>,
    pub candidate_score: Option<f64>,
    pub runner_up_score: Option<f64>,
    pub scale_percent: Option<u64>,
    pub stable_frames: Option<u8>,
    pub observation_matched: Option<bool>,
    pub action_state: Option<ActionState>,
    pub action_block_id: Option<String>,
    pub action_attempt_id: Option<ActionAttemptId>,
    pub stop_outcome: Option<StopOutcome>,
    pub error: Option<String>,
    loop_iterations_by_id: BTreeMap<String, u64>,
}

impl Default for MonitorProjection {
    fn default() -> Self {
        Self {
            run_id: None,
            definition_hash: None,
            running_revision: None,
            mode: None,
            status: RunStatus::Idle,
            elapsed_ms: 0,
            active_block: None,
            active_branch: None,
            active_loop: None,
            loop_iterations: None,
            candidate_count: None,
            candidate_score: None,
            runner_up_score: None,
            scale_percent: None,
            stable_frames: None,
            observation_matched: None,
            action_state: None,
            action_block_id: None,
            action_attempt_id: None,
            stop_outcome: None,
            error: None,
            loop_iterations_by_id: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopClassification {
    Success,
    UserStopped,
    SafetyStopped,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopOutcome {
    pub reason: StopReason,
    pub classification: StopClassification,
}

impl StopOutcome {
    fn new(reason: &StopReason) -> Self {
        Self {
            reason: reason.clone(),
            classification: classify_stop(reason),
        }
    }

    pub fn label(&self) -> String {
        format_stop_reason(&self.reason)
    }

    pub fn is_error(&self) -> bool {
        matches!(
            self.classification,
            StopClassification::Error | StopClassification::SafetyStopped
        )
    }
}

pub fn classify_stop(reason: &StopReason) -> StopClassification {
    match reason {
        StopReason::Completed | StopReason::StopSuccess => StopClassification::Success,
        StopReason::UserStopped => StopClassification::UserStopped,
        StopReason::EmergencyStopped | StopReason::SafetyLimit { .. } => {
            StopClassification::SafetyStopped
        }
        StopReason::StopError { .. }
        | StopReason::TechnicalFailure { .. }
        | StopReason::UnsupportedBlock { .. } => StopClassification::Error,
    }
}

#[derive(Debug, Clone)]
pub struct RunDefinitionSnapshot {
    run_id: String,
    definition_hash: String,
    definition: MacroDefinition,
}

impl RunDefinitionSnapshot {
    pub fn from_saved(run_id: impl Into<String>, saved: SavedRevision) -> Self {
        Self {
            run_id: run_id.into(),
            definition_hash: saved.definition_hash,
            definition: saved.definition,
        }
    }

    fn definition_for<'a>(&'a self, run: &SelectedRun<'_>) -> Option<&'a MacroDefinition> {
        (self.run_id == run.run_id
            && self.definition.id == run.macro_id
            && self.definition.revision == run.revision
            && self.definition_hash == run.definition_hash)
            .then_some(&self.definition)
    }
}

#[derive(Debug, Clone, Copy)]
struct SelectedRun<'a> {
    start_index: usize,
    run_id: &'a str,
    macro_id: &'a str,
    revision: u64,
    definition_hash: &'a str,
    mode: RunMode,
}

pub fn project_monitor(
    selected_macro_id: Option<&str>,
    run_snapshot: Option<&RunDefinitionSnapshot>,
    events: &[RunEvent],
) -> MonitorProjection {
    let Some(selected_macro_id) = selected_macro_id else {
        return MonitorProjection::default();
    };
    let Some(run) = latest_selected_run(events, selected_macro_id) else {
        return MonitorProjection::default();
    };
    let mut projection = MonitorProjection::default();
    projection.run_id = Some(run.run_id.to_string());
    projection.definition_hash = Some(run.definition_hash.to_string());
    projection.running_revision = Some(run.revision);
    projection.mode = Some(run.mode);
    projection.status = RunStatus::Running;
    let run_definition = run_snapshot.and_then(|snapshot| snapshot.definition_for(&run));
    let mut loop_container_ids = HashSet::new();
    if let Some(definition) = run_definition {
        collect_loop_container_ids(&definition.blocks, &mut loop_container_ids);
    }

    let mut scoped_events = events[run.start_index..]
        .iter()
        .filter(|event| event_run_id(event) == run.run_id)
        .collect::<Vec<_>>();
    scoped_events.sort_by_key(|event| event.sequence());

    for event in scoped_events {
        projection.elapsed_ms = event.elapsed_ms();
        match event {
            RunEvent::RunStarted { .. } => {}
            RunEvent::StatusChanged { status, .. } => projection.status = *status,
            RunEvent::BlockEntered { block_id, .. } => {
                if loop_container_ids.contains(block_id.as_str()) {
                    projection.loop_iterations_by_id.remove(block_id);
                }
                projection.active_block = Some(block_id.clone());
            }
            RunEvent::ActionPlanned {
                block_id, state, ..
            } => {
                projection.action_state = Some(*state);
                projection.action_block_id = Some(block_id.clone());
                projection.action_attempt_id = None;
            }
            RunEvent::ActionBlocked {
                block_id,
                attempt_id,
                state,
                ..
            } => {
                projection.action_state = Some(*state);
                projection.action_block_id = Some(block_id.clone());
                projection.action_attempt_id = attempt_id.clone();
            }
            RunEvent::ActionStateChanged {
                block_id,
                attempt_id,
                state,
                ..
            } => {
                projection.action_state = Some(*state);
                projection.action_block_id = Some(block_id.clone());
                projection.action_attempt_id = Some(attempt_id.clone());
            }
            RunEvent::ObservationCompleted { evidence, .. } => {
                projection.observation_matched = Some(evidence.matched);
                projection.candidate_count = Some(evidence.match_count);
                projection.candidate_score = evidence.score;
                projection.runner_up_score = evidence
                    .details
                    .get("runner_up_score")
                    .and_then(serde_json::Value::as_f64);
                projection.scale_percent = evidence
                    .details
                    .get("selected_scale_percent")
                    .and_then(serde_json::Value::as_u64);
                projection.stable_frames = Some(evidence.stable_frames);
            }
            RunEvent::LoopYielded {
                block_id,
                completed_iterations,
                ..
            } => {
                projection
                    .loop_iterations_by_id
                    .insert(block_id.clone(), *completed_iterations);
            }
            RunEvent::Error { message, .. } => projection.error = Some(message.clone()),
            RunEvent::RunStopped { status, reason, .. } => {
                projection.status = *status;
                projection.stop_outcome = Some(StopOutcome::new(reason));
            }
            RunEvent::ConditionEvaluated { .. }
            | RunEvent::ObservationProgress { .. }
            | RunEvent::ArbitrationCompleted { .. }
            | RunEvent::PollingDelayed { .. } => {}
        }
    }

    if let (Some(definition), Some(active_block)) =
        (run_definition, projection.active_block.as_deref())
    {
        if let Some(context) =
            find_block_context(&definition.blocks, active_block, &ContextPath::default())
        {
            projection.active_branch = context.branch;
            projection.active_loop = context.loop_id;
            projection.loop_iterations = projection
                .active_loop
                .as_ref()
                .and_then(|loop_id| projection.loop_iterations_by_id.get(loop_id).copied());
        }
    }
    projection
}

/// Builds a monitor from the bounded replay buffer plus the controller's durable lifecycle and
/// latest semantic projections. The projections retain run ownership and terminal state after the
/// raw replay buffer evicts a `RunStarted` event.
pub fn project_monitor_with_controller_projections(
    selected_macro_id: Option<&str>,
    run_snapshot: Option<&RunDefinitionSnapshot>,
    replay_events: &[RunEvent],
    lifecycle: &ControllerLifecycleProjection,
    semantic: &ControllerSemanticProjection,
) -> MonitorProjection {
    let events = controller_projection_events(replay_events, lifecycle, semantic);
    project_monitor(selected_macro_id, run_snapshot, &events)
}

pub fn project_last_completion(events: &[RunEvent], macro_id: &str) -> Option<StopOutcome> {
    let mut run_owners = HashMap::new();
    let mut latest = None;
    for event in events {
        match event {
            RunEvent::RunStarted {
                run_id,
                macro_id: started_macro_id,
                ..
            } => {
                run_owners.insert(run_id.as_str(), started_macro_id.as_str());
            }
            RunEvent::RunStopped { run_id, reason, .. }
                if run_owners.get(run_id.as_str()).copied() == Some(macro_id) =>
            {
                latest = Some(StopOutcome::new(reason));
            }
            _ => {}
        }
    }
    latest
}

pub fn project_last_completion_with_controller_projections(
    replay_events: &[RunEvent],
    macro_id: &str,
    lifecycle: &ControllerLifecycleProjection,
    semantic: &ControllerSemanticProjection,
) -> Option<StopOutcome> {
    let events = controller_projection_events(replay_events, lifecycle, semantic);
    project_last_completion(&events, macro_id)
}

fn controller_projection_events(
    replay_events: &[RunEvent],
    lifecycle: &ControllerLifecycleProjection,
    semantic: &ControllerSemanticProjection,
) -> Vec<RunEvent> {
    let mut events = replay_events.to_vec();
    if let Some(run_started) = lifecycle
        .run_started
        .as_ref()
        .or(semantic.run_started.as_ref())
        .filter(|event| !events.contains(event))
    {
        let run_id = event_run_id(run_started);
        let insertion_index = events
            .iter()
            .position(|event| event_run_id(event) == run_id)
            .unwrap_or(events.len());
        events.insert(insertion_index, run_started.clone());
    }
    for event in [
        lifecycle.run_stopped.as_ref(),
        semantic.status.as_ref(),
        semantic.active_block.as_ref(),
        semantic.latest_observation.as_ref(),
        semantic.latest_condition.as_ref(),
        semantic.latest_progress.as_ref(),
        semantic.latest_action.as_ref(),
        semantic.latest_loop.as_ref(),
        semantic.latest_arbitration.as_ref(),
        semantic.latest_polling_delay.as_ref(),
        semantic.latest_error.as_ref(),
        semantic.run_stopped.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if !events.contains(event) {
            events.push(event.clone());
        }
    }
    events
}

fn latest_selected_run<'a>(events: &'a [RunEvent], macro_id: &str) -> Option<SelectedRun<'a>> {
    events
        .iter()
        .enumerate()
        .rev()
        .find_map(|(start_index, event)| match event {
            RunEvent::RunStarted {
                run_id,
                macro_id: started_macro_id,
                revision,
                definition_hash,
                mode,
                ..
            } if started_macro_id == macro_id => Some(SelectedRun {
                start_index,
                run_id,
                macro_id: started_macro_id,
                revision: *revision,
                definition_hash,
                mode: *mode,
            }),
            _ => None,
        })
}

fn event_run_id(event: &RunEvent) -> &str {
    match event {
        RunEvent::RunStarted { run_id, .. }
        | RunEvent::StatusChanged { run_id, .. }
        | RunEvent::BlockEntered { run_id, .. }
        | RunEvent::ActionPlanned { run_id, .. }
        | RunEvent::ActionBlocked { run_id, .. }
        | RunEvent::ActionStateChanged { run_id, .. }
        | RunEvent::ObservationCompleted { run_id, .. }
        | RunEvent::ConditionEvaluated { run_id, .. }
        | RunEvent::ObservationProgress { run_id, .. }
        | RunEvent::LoopYielded { run_id, .. }
        | RunEvent::ArbitrationCompleted { run_id, .. }
        | RunEvent::PollingDelayed { run_id, .. }
        | RunEvent::Error { run_id, .. }
        | RunEvent::RunStopped { run_id, .. } => run_id,
    }
}

fn collect_loop_container_ids<'a>(blocks: &'a [Block], ids: &mut HashSet<&'a str>) {
    for block in blocks {
        match &block.kind {
            BlockKind::If {
                then_body,
                else_body,
                ..
            } => {
                collect_loop_container_ids(then_body, ids);
                collect_loop_container_ids(else_body, ids);
            }
            BlockKind::RepeatN { body, .. }
            | BlockKind::RepeatUntil { body, .. }
            | BlockKind::Continuous { body } => {
                ids.insert(block.id.as_str());
                collect_loop_container_ids(body, ids);
            }
            BlockKind::WatchGroup { group } => {
                for lane in &group.lanes {
                    collect_loop_container_ids(&lane.then_body, ids);
                }
                if let crate::engine::macro_engine::TimeoutOutcome::RunBody { body } =
                    &group.timeout_outcome
                {
                    collect_loop_container_ids(body, ids);
                }
            }
            BlockKind::Observe { .. }
            | BlockKind::Action { .. }
            | BlockKind::Wait { .. }
            | BlockKind::StopSuccess
            | BlockKind::StopError { .. }
            | BlockKind::Comment { .. } => {}
        }
    }
}

fn format_stop_reason(reason: &StopReason) -> String {
    match reason {
        StopReason::Completed => "Macro completed".into(),
        StopReason::StopSuccess => "Stopped successfully".into(),
        StopReason::StopError { message } => format!("Stopped with error: {message}"),
        StopReason::UserStopped => "User stopped".into(),
        StopReason::EmergencyStopped => "Emergency stop".into(),
        StopReason::TechnicalFailure { message } => format!("Technical failure: {message}"),
        StopReason::SafetyLimit { message } => format!("Safety limit: {message}"),
        StopReason::UnsupportedBlock { block_id } => format!("Unsupported block: {block_id}"),
    }
}

#[derive(Debug, Clone, Default)]
struct ContextPath {
    branch: Option<String>,
    loop_id: Option<String>,
}

fn find_block_context(
    blocks: &[Block],
    target: &str,
    inherited: &ContextPath,
) -> Option<ContextPath> {
    for block in blocks {
        if block.id == target {
            return Some(inherited.clone());
        }
        match &block.kind {
            BlockKind::If {
                then_body,
                else_body,
                ..
            } => {
                let then_context = ContextPath {
                    branch: Some(format!("{} · THEN", block.id)),
                    loop_id: inherited.loop_id.clone(),
                };
                if let Some(found) = find_block_context(then_body, target, &then_context) {
                    return Some(found);
                }
                let else_context = ContextPath {
                    branch: Some(format!("{} · ELSE", block.id)),
                    loop_id: inherited.loop_id.clone(),
                };
                if let Some(found) = find_block_context(else_body, target, &else_context) {
                    return Some(found);
                }
            }
            BlockKind::RepeatN { body, .. }
            | BlockKind::RepeatUntil { body, .. }
            | BlockKind::Continuous { body } => {
                let context = ContextPath {
                    branch: inherited.branch.clone(),
                    loop_id: Some(block.id.clone()),
                };
                if let Some(found) = find_block_context(body, target, &context) {
                    return Some(found);
                }
            }
            BlockKind::WatchGroup { group } => {
                for (index, lane) in group.lanes.iter().enumerate() {
                    let context = ContextPath {
                        branch: Some(format!("{} · Priority {} THEN", block.id, index + 1)),
                        loop_id: inherited.loop_id.clone(),
                    };
                    if let Some(found) = find_block_context(&lane.then_body, target, &context) {
                        return Some(found);
                    }
                }
                if let crate::engine::macro_engine::TimeoutOutcome::RunBody { body } =
                    &group.timeout_outcome
                {
                    let context = ContextPath {
                        branch: Some(format!("{} · ON TIMEOUT", block.id)),
                        loop_id: inherited.loop_id.clone(),
                    };
                    if let Some(found) = find_block_context(body, target, &context) {
                        return Some(found);
                    }
                }
            }
            BlockKind::Observe { .. }
            | BlockKind::Action { .. }
            | BlockKind::Wait { .. }
            | BlockKind::StopSuccess
            | BlockKind::StopError { .. }
            | BlockKind::Comment { .. } => {}
        }
    }
    None
}

pub fn show(ui: &mut Ui, monitor: &MonitorProjection) {
    Frame::none()
        .fill(Color32::from_rgb(13, 15, 17))
        .stroke(Stroke::new(1.0, Color32::from_rgb(49, 52, 55)))
        .rounding(6.0)
        .inner_margin(egui::Margin::same(12.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("RUN MONITOR")
                        .monospace()
                        .strong()
                        .size(11.0)
                        .color(Color32::from_rgb(196, 147, 91)),
                );
                ui.label(
                    RichText::new(format!("{:?}", monitor.status))
                        .strong()
                        .color(status_color(monitor.status)),
                );
                ui.label(
                    RichText::new(format_elapsed(monitor.elapsed_ms))
                        .monospace()
                        .color(Color32::from_gray(143)),
                );
            });
            ui.add_space(8.0);
            Grid::new("macro_monitor_grid")
                .num_columns(4)
                .min_col_width(105.0)
                .spacing([12.0, 5.0])
                .show(ui, |ui| {
                    monitor_cell(ui, "Active block", monitor.active_block.as_deref());
                    monitor_cell(ui, "Branch", monitor.active_branch.as_deref());
                    monitor_cell(ui, "Loop", monitor.active_loop.as_deref());
                    monitor_cell(
                        ui,
                        "Iteration",
                        monitor
                            .loop_iterations
                            .map(|value| value.to_string())
                            .as_deref(),
                    );
                    ui.end_row();
                    monitor_cell(
                        ui,
                        "Candidates",
                        monitor
                            .candidate_count
                            .map(|value| value.to_string())
                            .as_deref(),
                    );
                    monitor_cell(ui, "Best / runner-up", score_pair(monitor).as_deref());
                    monitor_cell(ui, "Scale / stability", scale_stability(monitor).as_deref());
                    monitor_cell(ui, "Action state", action_state(monitor).as_deref());
                    ui.end_row();
                });
            ui.add_space(7.0);
            ui.separator();
            ui.add_space(5.0);
            ui.label(
                RichText::new(
                    monitor
                        .stop_outcome
                        .as_ref()
                        .map(StopOutcome::label)
                        .or_else(|| monitor.error.clone())
                        .unwrap_or_else(|| "No stop reason reported".to_string()),
                )
                .size(12.0)
                .color(
                    if monitor.stop_outcome.is_some() || monitor.error.is_some() {
                        Color32::from_rgb(231, 137, 102)
                    } else {
                        Color32::from_gray(120)
                    },
                ),
            );
        });
}

fn monitor_cell(ui: &mut Ui, label: &str, value: Option<&str>) {
    ui.vertical(|ui| {
        ui.label(
            RichText::new(label)
                .size(10.0)
                .color(Color32::from_gray(112)),
        );
        ui.label(
            RichText::new(value.unwrap_or("--"))
                .size(12.0)
                .color(Color32::from_gray(205)),
        );
    });
}

fn score_pair(monitor: &MonitorProjection) -> Option<String> {
    match (monitor.candidate_score, monitor.runner_up_score) {
        (Some(best), Some(runner_up)) => Some(format!("{best:.3} / {runner_up:.3}")),
        (Some(best), None) => Some(format!("{best:.3} / --")),
        _ => None,
    }
}

fn scale_stability(monitor: &MonitorProjection) -> Option<String> {
    match (monitor.scale_percent, monitor.stable_frames) {
        (Some(scale), Some(frames)) => Some(format!("{scale}% / {frames} frames")),
        (None, Some(frames)) => Some(format!("-- / {frames} frames")),
        _ => None,
    }
}

fn action_state(monitor: &MonitorProjection) -> Option<String> {
    monitor.action_state.map(|state| {
        let mut summary = match &monitor.action_block_id {
            Some(block_id) => format!("{state:?} · {block_id}"),
            None => format!("{state:?}"),
        };
        if let Some(attempt_id) = &monitor.action_attempt_id {
            summary.push_str(&format!(" · {attempt_id:?}"));
        }
        summary
    })
}

fn status_color(status: RunStatus) -> Color32 {
    match status {
        RunStatus::Running => Color32::from_rgb(101, 202, 126),
        RunStatus::Paused | RunStatus::Validating => Color32::from_rgb(231, 164, 79),
        RunStatus::Stopping => Color32::from_rgb(231, 116, 79),
        RunStatus::Idle | RunStatus::Stopped => Color32::from_gray(145),
    }
}

fn format_elapsed(elapsed_ms: u64) -> String {
    let seconds = elapsed_ms / 1_000;
    format!(
        "{:02}:{:02}.{:03}",
        seconds / 60,
        seconds % 60,
        elapsed_ms % 1_000
    )
}

#[cfg(test)]
mod tests {
    use crate::engine::macro_engine::{
        ControllerLifecycleProjection, ControllerSemanticProjection,
    };

    use serde_json::json;

    use crate::engine::macro_engine::{
        Action, ActionAttemptId, ActionState, Block, BlockKind, Condition, DetectorEvidence,
        FocusLossPolicy, Limit, MACRO_SCHEMA_VERSION, MouseButton, ObserveMode, RunEvent, RunMode,
        RunStatus, SafetyPolicy, StopReason, TargetProfile,
    };

    use super::*;

    fn definition(id: &str, revision: u64, blocks: Vec<Block>) -> MacroDefinition {
        MacroDefinition {
            schema_version: MACRO_SCHEMA_VERSION,
            id: id.to_string(),
            name: id.to_string(),
            revision,
            target: TargetProfile {
                process_path: "Diablo IV.exe".to_string(),
                window_class: "Diablo IV Main Window".to_string(),
                title_contains: "Diablo IV".to_string(),
                captured_client_width: 1920,
                captured_client_height: 1080,
                captured_dpi: 96,
            },
            regions: Vec::new(),
            points: Vec::new(),
            text_rules: Vec::new(),
            image_rules: Vec::new(),
            blocks,
            safety: SafetyPolicy {
                max_runtime_ms: Limit::Unlimited,
                max_clicks: Limit::Unlimited,
                max_observation_retries: Limit::Unlimited,
                max_observations_per_second: 10,
                minimum_click_interval_ms: 100,
                focus_loss: FocusLossPolicy::Stop,
            },
        }
    }

    fn started(sequence: u64, run_id: &str, macro_id: &str, revision: u64, hash: &str) -> RunEvent {
        RunEvent::RunStarted {
            sequence,
            elapsed_ms: 0,
            run_id: run_id.to_string(),
            macro_id: macro_id.to_string(),
            revision,
            definition_hash: hash.to_string(),
            mode: RunMode::DryRun,
        }
    }

    fn entered(sequence: u64, run_id: &str, block_id: &str) -> RunEvent {
        RunEvent::BlockEntered {
            sequence,
            elapsed_ms: sequence * 10,
            run_id: run_id.to_string(),
            block_id: block_id.to_string(),
        }
    }

    fn yielded(sequence: u64, run_id: &str, block_id: &str, count: u64) -> RunEvent {
        RunEvent::LoopYielded {
            sequence,
            elapsed_ms: sequence * 10,
            run_id: run_id.to_string(),
            block_id: block_id.to_string(),
            completed_iterations: count,
        }
    }

    fn comment(id: &str) -> Block {
        Block {
            id: id.to_string(),
            enabled: true,
            kind: BlockKind::Comment {
                text: id.to_string(),
            },
        }
    }

    fn continuous(id: &str, body: Vec<Block>) -> Block {
        Block {
            id: id.to_string(),
            enabled: true,
            kind: BlockKind::Continuous { body },
        }
    }

    fn repeat_n(id: &str, count: u32, body: Vec<Block>) -> Block {
        Block {
            id: id.to_string(),
            enabled: true,
            kind: BlockKind::RepeatN { count, body },
        }
    }

    fn snapshot(run_id: &str, hash: &str, definition: MacroDefinition) -> RunDefinitionSnapshot {
        RunDefinitionSnapshot::from_saved(
            run_id,
            SavedRevision {
                definition,
                definition_hash: hash.to_string(),
                pinned_assets: Vec::new(),
            },
        )
    }

    #[test]
    fn monitor_projects_revision_observation_action_and_stop_details() {
        let events = vec![
            RunEvent::RunStarted {
                sequence: 1,
                elapsed_ms: 0,
                run_id: "run-1".to_string(),
                macro_id: "forge-loop".to_string(),
                revision: 7,
                definition_hash: "hash".to_string(),
                mode: RunMode::DryRun,
            },
            RunEvent::BlockEntered {
                sequence: 2,
                elapsed_ms: 10,
                run_id: "run-1".to_string(),
                block_id: "find-icon".to_string(),
            },
            RunEvent::ObservationCompleted {
                sequence: 3,
                elapsed_ms: 20,
                run_id: "run-1".to_string(),
                block_id: "find-icon".to_string(),
                evidence: DetectorEvidence::new(
                    true,
                    5,
                    19,
                    None,
                    Some(0.96),
                    3,
                    2,
                    json!({
                        "runner_up_score": 0.91,
                        "selected_scale_percent": 105
                    }),
                ),
                token: None,
            },
            RunEvent::ActionStateChanged {
                sequence: 4,
                elapsed_ms: 25,
                run_id: "run-1".to_string(),
                block_id: "click-icon".to_string(),
                action: Action::ClickImageMatch {
                    source_block_id: "find-icon".to_string(),
                    button: MouseButton::Left,
                },
                attempt_id: ActionAttemptId::for_test("run-1", 1),
                state: ActionState::Prepared,
            },
            RunEvent::RunStopped {
                sequence: 5,
                elapsed_ms: 30,
                run_id: "run-1".to_string(),
                status: RunStatus::Stopped,
                reason: StopReason::UserStopped,
            },
        ];

        let monitor = project_monitor(Some("forge-loop"), None, &events);

        assert_eq!(monitor.running_revision, Some(7));
        assert_eq!(monitor.active_block.as_deref(), Some("find-icon"));
        assert_eq!(monitor.candidate_count, Some(3));
        assert_eq!(monitor.runner_up_score, Some(0.91));
        assert_eq!(monitor.scale_percent, Some(105));
        assert_eq!(monitor.stable_frames, Some(2));
        assert_eq!(monitor.action_state, Some(ActionState::Prepared));
        assert_eq!(
            monitor.action_attempt_id,
            Some(ActionAttemptId::for_test("run-1", 1))
        );
        assert_eq!(
            monitor.stop_outcome,
            Some(StopOutcome {
                reason: StopReason::UserStopped,
                classification: StopClassification::UserStopped,
            })
        );
        assert_eq!(monitor.elapsed_ms, 30);
    }

    #[test]
    fn action_summary_includes_a_blocked_attempt_identifier() {
        let monitor = MonitorProjection {
            action_state: Some(ActionState::Blocked),
            action_block_id: Some("repeat-click".to_string()),
            action_attempt_id: Some(ActionAttemptId::for_test("run-1", 2)),
            ..MonitorProjection::default()
        };

        assert!(
            action_state(&monitor)
                .expect("blocked action must render a summary")
                .contains("ActionAttemptId")
        );
    }

    #[test]
    fn active_block_context_reports_owning_branch_and_loop() {
        let target = Block {
            id: "target".to_string(),
            enabled: true,
            kind: BlockKind::Comment {
                text: "inside".to_string(),
            },
        };
        let blocks = vec![Block {
            id: "loop".to_string(),
            enabled: true,
            kind: BlockKind::Continuous {
                body: vec![Block {
                    id: "if".to_string(),
                    enabled: true,
                    kind: BlockKind::If {
                        condition: Condition::Text {
                            source_block_id: "source".to_string(),
                            rule_id: "text".to_string(),
                            mode: ObserveMode::WaitForTrue {
                                timeout_ms: Limit::Finite(100),
                                timeout_outcome:
                                    crate::engine::macro_engine::TimeoutOutcome::Continue,
                            },
                        },
                        then_body: Vec::new(),
                        else_body: vec![target],
                    },
                }],
            },
        }];

        let context = find_block_context(&blocks, "target", &ContextPath::default()).unwrap();

        assert_eq!(context.branch.as_deref(), Some("if · ELSE"));
        assert_eq!(context.loop_id.as_deref(), Some("loop"));
    }

    #[test]
    fn a_new_run_clears_prior_run_projection_details() {
        let events = vec![
            RunEvent::RunStarted {
                sequence: 1,
                elapsed_ms: 0,
                run_id: "old-run".to_string(),
                macro_id: "macro".to_string(),
                revision: 1,
                definition_hash: "old".to_string(),
                mode: RunMode::DryRun,
            },
            RunEvent::ObservationCompleted {
                sequence: 2,
                elapsed_ms: 500,
                run_id: "old-run".to_string(),
                block_id: "old-observation".to_string(),
                evidence: DetectorEvidence::new(
                    true,
                    1,
                    500,
                    None,
                    Some(0.99),
                    4,
                    3,
                    json!({"runner_up_score": 0.80}),
                ),
                token: None,
            },
            RunEvent::RunStarted {
                sequence: 1,
                elapsed_ms: 0,
                run_id: "new-run".to_string(),
                macro_id: "macro".to_string(),
                revision: 2,
                definition_hash: "new".to_string(),
                mode: RunMode::ObservationOnly,
            },
        ];

        let monitor = project_monitor(Some("macro"), None, &events);

        assert_eq!(monitor.running_revision, Some(2));
        assert_eq!(monitor.elapsed_ms, 0);
        assert_eq!(monitor.candidate_count, None);
        assert_eq!(monitor.runner_up_score, None);
        assert_eq!(monitor.stable_frames, None);
    }

    #[test]
    fn every_stop_reason_has_a_typed_exhaustive_classification() {
        let cases = vec![
            (StopReason::Completed, StopClassification::Success),
            (StopReason::StopSuccess, StopClassification::Success),
            (
                StopReason::StopError {
                    message: "failed".to_string(),
                },
                StopClassification::Error,
            ),
            (StopReason::UserStopped, StopClassification::UserStopped),
            (
                StopReason::EmergencyStopped,
                StopClassification::SafetyStopped,
            ),
            (
                StopReason::TechnicalFailure {
                    message: "broken".to_string(),
                },
                StopClassification::Error,
            ),
            (
                StopReason::SafetyLimit {
                    message: "limit".to_string(),
                },
                StopClassification::SafetyStopped,
            ),
            (
                StopReason::UnsupportedBlock {
                    block_id: "future".to_string(),
                },
                StopClassification::Error,
            ),
        ];

        for (reason, expected) in cases {
            assert_eq!(classify_stop(&reason), expected, "{reason:?}");
        }
    }

    #[test]
    fn selected_macro_ignores_foreign_events_and_sequence_orders_its_run() {
        let events = vec![
            started(1, "alpha-run", "alpha", 4, "alpha-hash"),
            started(1, "beta-run", "beta", 9, "beta-hash"),
            entered(2, "beta-run", "foreign"),
            entered(4, "alpha-run", "alpha-latest"),
            entered(2, "alpha-run", "alpha-earlier"),
        ];

        let monitor = project_monitor(Some("alpha"), None, &events);

        assert_eq!(monitor.run_id.as_deref(), Some("alpha-run"));
        assert_eq!(monitor.running_revision, Some(4));
        assert_eq!(monitor.active_block.as_deref(), Some("alpha-latest"));
    }

    #[test]
    fn latest_same_macro_run_rejects_late_events_from_prior_run() {
        let events = vec![
            started(1, "old", "alpha", 1, "old-hash"),
            entered(2, "old", "old-before"),
            started(1, "new", "alpha", 2, "new-hash"),
            entered(99, "old", "old-late"),
            entered(2, "new", "new-active"),
        ];

        let monitor = project_monitor(Some("alpha"), None, &events);

        assert_eq!(monitor.run_id.as_deref(), Some("new"));
        assert_eq!(monitor.active_block.as_deref(), Some("new-active"));
        assert_eq!(monitor.running_revision, Some(2));
    }

    #[test]
    fn previous_completion_survives_while_new_run_is_running() {
        let events = vec![
            started(1, "completed", "alpha", 1, "old-hash"),
            RunEvent::RunStopped {
                sequence: 2,
                elapsed_ms: 40,
                run_id: "completed".to_string(),
                status: RunStatus::Stopped,
                reason: StopReason::Completed,
            },
            started(1, "running", "alpha", 2, "new-hash"),
            entered(2, "running", "now"),
        ];

        let monitor = project_monitor(Some("alpha"), None, &events);
        let completion = project_last_completion(&events, "alpha").unwrap();

        assert_eq!(monitor.status, RunStatus::Running);
        assert_eq!(monitor.stop_outcome, None);
        assert_eq!(completion.reason, StopReason::Completed);
        assert_eq!(completion.classification, StopClassification::Success);
    }

    #[test]
    fn completion_before_its_matching_start_is_not_associated() {
        let events = vec![
            RunEvent::RunStopped {
                sequence: 2,
                elapsed_ms: 40,
                run_id: "reused".to_string(),
                status: RunStatus::Stopped,
                reason: StopReason::TechnicalFailure {
                    message: "foreign early stop".to_string(),
                },
            },
            started(1, "reused", "alpha", 1, "hash"),
        ];

        assert_eq!(project_last_completion(&events, "alpha"), None);
    }

    #[test]
    fn controller_projections_preserve_terminal_outcome_after_started_event_eviction() {
        let stopped = RunEvent::RunStopped {
            sequence: 3,
            elapsed_ms: 40,
            run_id: "evicted-run".to_string(),
            status: RunStatus::Stopped,
            reason: StopReason::EmergencyStopped,
        };
        let lifecycle = ControllerLifecycleProjection {
            run_started: Some(started(1, "evicted-run", "alpha", 1, "hash")),
            run_stopped: Some(stopped.clone()),
        };
        let semantic = ControllerSemanticProjection {
            run_started: lifecycle.run_started.clone(),
            run_stopped: Some(stopped.clone()),
            status: Some(stopped.clone()),
            ..ControllerSemanticProjection::default()
        };

        let monitor = project_monitor_with_controller_projections(
            Some("alpha"),
            None,
            &[stopped],
            &lifecycle,
            &semantic,
        );
        let completion = project_last_completion_with_controller_projections(
            &[],
            "alpha",
            &lifecycle,
            &semantic,
        )
        .unwrap();

        assert_eq!(monitor.status, RunStatus::Stopped);
        assert_eq!(
            monitor.stop_outcome.as_ref().map(|outcome| &outcome.reason),
            Some(&StopReason::EmergencyStopped)
        );
        assert_eq!(completion.reason, StopReason::EmergencyStopped);
    }

    #[test]
    fn controller_projection_keeps_newer_short_run_after_older_long_run() {
        let older_stopped = RunEvent::RunStopped {
            sequence: 100,
            elapsed_ms: 1_000,
            run_id: "older-run".to_string(),
            status: RunStatus::Stopped,
            reason: StopReason::Completed,
        };
        let newer_stopped = RunEvent::RunStopped {
            sequence: 3,
            elapsed_ms: 30,
            run_id: "newer-run".to_string(),
            status: RunStatus::Stopped,
            reason: StopReason::EmergencyStopped,
        };
        let lifecycle = ControllerLifecycleProjection {
            run_started: Some(started(1, "newer-run", "alpha", 2, "newer-hash")),
            run_stopped: Some(newer_stopped.clone()),
        };
        let semantic = ControllerSemanticProjection {
            run_started: lifecycle.run_started.clone(),
            run_stopped: Some(newer_stopped.clone()),
            status: Some(newer_stopped.clone()),
            ..ControllerSemanticProjection::default()
        };
        let replay = vec![
            started(1, "older-run", "alpha", 1, "older-hash"),
            older_stopped,
            newer_stopped,
        ];

        let monitor = project_monitor_with_controller_projections(
            Some("alpha"),
            None,
            &replay,
            &lifecycle,
            &semantic,
        );
        let completion = project_last_completion_with_controller_projections(
            &replay, "alpha", &lifecycle, &semantic,
        )
        .unwrap();

        assert_eq!(monitor.run_id.as_deref(), Some("newer-run"));
        assert_eq!(completion.reason, StopReason::EmergencyStopped);
    }

    #[test]
    fn loop_count_belongs_to_the_active_sequential_loop() {
        let saved = definition(
            "alpha",
            7,
            vec![
                continuous("loop-a", vec![comment("a-body")]),
                continuous("loop-b", vec![comment("b-body")]),
            ],
        );
        let snapshot = snapshot("run", "hash", saved);
        let events = vec![
            started(1, "run", "alpha", 7, "hash"),
            yielded(2, "run", "loop-a", 2),
            yielded(3, "run", "loop-b", 8),
            entered(4, "run", "b-body"),
        ];

        let monitor = project_monitor(Some("alpha"), Some(&snapshot), &events);

        assert_eq!(monitor.active_loop.as_deref(), Some("loop-b"));
        assert_eq!(monitor.loop_iterations, Some(8));
    }

    #[test]
    fn innermost_nested_loop_uses_its_own_iteration_count() {
        let saved = definition(
            "alpha",
            7,
            vec![continuous(
                "outer",
                vec![continuous("inner", vec![comment("nested-target")])],
            )],
        );
        let snapshot = snapshot("run", "hash", saved);
        let events = vec![
            started(1, "run", "alpha", 7, "hash"),
            yielded(2, "run", "outer", 3),
            yielded(3, "run", "inner", 11),
            entered(4, "run", "nested-target"),
        ];

        let monitor = project_monitor(Some("alpha"), Some(&snapshot), &events);

        assert_eq!(monitor.active_loop.as_deref(), Some("inner"));
        assert_eq!(monitor.loop_iterations, Some(11));
    }

    #[test]
    fn top_level_block_after_loop_clears_loop_and_iteration() {
        let saved = definition(
            "alpha",
            7,
            vec![
                continuous("loop", vec![comment("body")]),
                comment("after-loop"),
            ],
        );
        let snapshot = snapshot("run", "hash", saved);
        let events = vec![
            started(1, "run", "alpha", 7, "hash"),
            yielded(2, "run", "loop", 5),
            entered(3, "run", "body"),
            entered(4, "run", "after-loop"),
        ];

        let monitor = project_monitor(Some("alpha"), Some(&snapshot), &events);

        assert_eq!(monitor.active_loop, None);
        assert_eq!(monitor.loop_iterations, None);
    }

    #[test]
    fn active_context_uses_matching_run_snapshot_not_mutable_draft() {
        let saved = definition(
            "alpha",
            7,
            vec![continuous("saved-loop", vec![comment("active")])],
        );
        let draft = definition("alpha", 8, vec![comment("different-draft-block")]);
        let snapshot = snapshot("run", "saved-hash", saved);
        let events = vec![
            started(1, "run", "alpha", 7, "saved-hash"),
            yielded(2, "run", "saved-loop", 6),
            entered(3, "run", "active"),
        ];

        let monitor = project_monitor(Some(&draft.id), Some(&snapshot), &events);

        assert_eq!(monitor.active_loop.as_deref(), Some("saved-loop"));
        assert_eq!(monitor.loop_iterations, Some(6));
    }

    #[test]
    fn mismatched_run_snapshot_keys_cannot_supply_active_context() {
        let saved = definition(
            "alpha",
            7,
            vec![continuous("saved-loop", vec![comment("active")])],
        );
        let events = vec![
            started(1, "run", "alpha", 7, "saved-hash"),
            entered(2, "run", "active"),
        ];
        let wrong_run = snapshot("other-run", "saved-hash", saved.clone());
        let wrong_hash = snapshot("run", "other-hash", saved.clone());
        let wrong_revision = snapshot(
            "run",
            "saved-hash",
            definition("alpha", 8, saved.blocks.clone()),
        );

        for snapshot in [&wrong_run, &wrong_hash, &wrong_revision] {
            let monitor = project_monitor(Some("alpha"), Some(snapshot), &events);
            assert_eq!(monitor.active_loop, None);
            assert_eq!(monitor.active_branch, None);
        }
    }

    #[test]
    fn nested_loop_reentry_clears_only_that_invocations_prior_count() {
        let saved = definition(
            "alpha",
            7,
            vec![
                repeat_n("sibling", 9, vec![comment("sibling-body")]),
                continuous(
                    "outer",
                    vec![repeat_n("inner", 2, vec![comment("inner-body")])],
                ),
            ],
        );
        let snapshot = snapshot("run", "hash", saved);
        let before_first_new_yield = vec![
            started(1, "run", "alpha", 7, "hash"),
            yielded(2, "run", "sibling", 7),
            entered(3, "run", "outer"),
            entered(4, "run", "inner"),
            entered(5, "run", "inner-body"),
            yielded(6, "run", "inner", 2),
            yielded(7, "run", "outer", 1),
            entered(8, "run", "inner"),
            entered(9, "run", "inner-body"),
        ];

        let before = project_monitor(Some("alpha"), Some(&snapshot), &before_first_new_yield);

        assert_eq!(before.active_loop.as_deref(), Some("inner"));
        assert_eq!(before.loop_iterations, None);
        assert_eq!(before.loop_iterations_by_id.get("inner"), None);
        assert_eq!(before.loop_iterations_by_id.get("outer"), Some(&1));
        assert_eq!(before.loop_iterations_by_id.get("sibling"), Some(&7));

        let mut after_first_new_yield = before_first_new_yield;
        after_first_new_yield.push(yielded(10, "run", "inner", 1));
        let after = project_monitor(Some("alpha"), Some(&snapshot), &after_first_new_yield);

        assert_eq!(after.active_loop.as_deref(), Some("inner"));
        assert_eq!(after.loop_iterations, Some(1));
        assert_eq!(after.loop_iterations_by_id.get("inner"), Some(&1));
        assert_eq!(after.loop_iterations_by_id.get("outer"), Some(&1));
        assert_eq!(after.loop_iterations_by_id.get("sibling"), Some(&7));
    }
}
