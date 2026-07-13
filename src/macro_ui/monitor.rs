use eframe::egui::{self, Color32, Frame, Grid, RichText, Stroke, Ui};

use crate::engine::macro_engine::{
    ActionState, Block, BlockKind, MacroDefinition, RunEvent, RunMode, RunStatus, StopReason,
};

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorProjection {
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
    pub stop_reason: Option<String>,
    pub error: Option<String>,
}

impl Default for MonitorProjection {
    fn default() -> Self {
        Self {
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
            stop_reason: None,
            error: None,
        }
    }
}

pub fn project_monitor(
    definition: Option<&MacroDefinition>,
    events: &[RunEvent],
) -> MonitorProjection {
    let mut projection = MonitorProjection::default();
    for event in events {
        if matches!(event, RunEvent::RunStarted { .. }) {
            projection = MonitorProjection::default();
        }
        projection.elapsed_ms = event.elapsed_ms();
        match event {
            RunEvent::RunStarted { revision, mode, .. } => {
                projection.running_revision = Some(*revision);
                projection.mode = Some(*mode);
                projection.status = RunStatus::Running;
                projection.stop_reason = None;
            }
            RunEvent::StatusChanged { status, .. } => projection.status = *status,
            RunEvent::BlockEntered { block_id, .. } => {
                projection.active_block = Some(block_id.clone());
            }
            RunEvent::ActionPlanned {
                block_id, state, ..
            }
            | RunEvent::ActionBlocked {
                block_id, state, ..
            } => {
                projection.action_state = Some(*state);
                projection.action_block_id = Some(block_id.clone());
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
                projection.active_loop = Some(block_id.clone());
                projection.loop_iterations = Some(*completed_iterations);
            }
            RunEvent::Error { message, .. } => projection.error = Some(message.clone()),
            RunEvent::RunStopped { status, reason, .. } => {
                projection.status = *status;
                projection.stop_reason = Some(format_stop_reason(reason));
            }
            RunEvent::ConditionEvaluated { .. }
            | RunEvent::ObservationProgress { .. }
            | RunEvent::ArbitrationCompleted { .. }
            | RunEvent::PollingDelayed { .. } => {}
        }
    }

    if let (Some(definition), Some(active_block)) = (definition, projection.active_block.as_deref())
    {
        if let Some(context) =
            find_block_context(&definition.blocks, active_block, &ContextPath::default())
        {
            projection.active_branch = context.branch;
            projection.active_loop = context.loop_id.or(projection.active_loop);
        }
    }
    projection
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
                        .stop_reason
                        .as_deref()
                        .or(monitor.error.as_deref())
                        .unwrap_or("No stop reason reported"),
                )
                .size(12.0)
                .color(
                    if monitor.stop_reason.is_some() || monitor.error.is_some() {
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
    monitor
        .action_state
        .map(|state| match &monitor.action_block_id {
            Some(block_id) => format!("{state:?} · {block_id}"),
            None => format!("{state:?}"),
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
    use serde_json::json;

    use crate::engine::macro_engine::{
        Action, ActionState, Block, BlockKind, Condition, DetectorEvidence, Limit, MouseButton,
        ObserveMode, RunEvent, RunMode, RunStatus, StopReason,
    };

    use super::*;

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
            RunEvent::ActionPlanned {
                sequence: 4,
                elapsed_ms: 25,
                run_id: "run-1".to_string(),
                block_id: "click-icon".to_string(),
                action: Action::ClickImageMatch {
                    source_block_id: "find-icon".to_string(),
                    button: MouseButton::Left,
                },
                state: ActionState::Prepared,
                token: None,
            },
            RunEvent::RunStopped {
                sequence: 5,
                elapsed_ms: 30,
                run_id: "run-1".to_string(),
                status: RunStatus::Stopped,
                reason: StopReason::UserStopped,
            },
        ];

        let monitor = project_monitor(None, &events);

        assert_eq!(monitor.running_revision, Some(7));
        assert_eq!(monitor.active_block.as_deref(), Some("find-icon"));
        assert_eq!(monitor.candidate_count, Some(3));
        assert_eq!(monitor.runner_up_score, Some(0.91));
        assert_eq!(monitor.scale_percent, Some(105));
        assert_eq!(monitor.stable_frames, Some(2));
        assert_eq!(monitor.action_state, Some(ActionState::Prepared));
        assert_eq!(monitor.stop_reason.as_deref(), Some("User stopped"));
        assert_eq!(monitor.elapsed_ms, 30);
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

        let monitor = project_monitor(None, &events);

        assert_eq!(monitor.running_revision, Some(2));
        assert_eq!(monitor.elapsed_ms, 0);
        assert_eq!(monitor.candidate_count, None);
        assert_eq!(monitor.runner_up_score, None);
        assert_eq!(monitor.stable_frames, None);
    }
}
