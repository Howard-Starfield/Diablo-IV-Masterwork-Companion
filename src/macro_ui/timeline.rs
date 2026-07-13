use eframe::egui::{self, Color32, Frame, RichText, Stroke, Ui};

use crate::engine::macro_engine::{
    Action, Block, BlockKind, Condition, Limit, ObserveMode, PassiveCondition, TimeoutOutcome,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineSelection {
    Identity(String),
    TimeoutBody { owner_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineRow {
    pub identity: Option<String>,
    pub selection: Option<TimelineSelection>,
    pub depth: usize,
    pub label: String,
    pub summary: String,
    pub lane_priority: Option<usize>,
    pub enabled: bool,
    pub is_loop_marker: bool,
    pub is_selectable: bool,
}

pub fn project_timeline(blocks: &[Block]) -> Vec<TimelineRow> {
    let mut rows = Vec::new();
    append_blocks(blocks, 0, &mut rows);
    rows
}

fn append_blocks(blocks: &[Block], depth: usize, rows: &mut Vec<TimelineRow>) {
    for block in blocks {
        let (label, summary) = block_summary(&block.kind);
        rows.push(TimelineRow {
            identity: Some(block.id.clone()),
            selection: Some(TimelineSelection::Identity(block.id.clone())),
            depth,
            label,
            summary,
            lane_priority: None,
            enabled: block.enabled,
            is_loop_marker: false,
            is_selectable: true,
        });

        match &block.kind {
            BlockKind::If {
                then_body,
                else_body,
                condition,
            } => {
                append_container(depth + 1, "THEN", rows);
                append_blocks(then_body, depth + 2, rows);
                append_container(depth + 1, "ELSE", rows);
                append_blocks(else_body, depth + 2, rows);
                append_condition_timeout(condition, block, depth, rows);
            }
            BlockKind::RepeatN { body, .. } | BlockKind::Continuous { body } => {
                append_blocks(body, depth + 1, rows);
                append_loop_marker(block, depth + 1, rows);
            }
            BlockKind::RepeatUntil {
                condition, body, ..
            } => {
                append_blocks(body, depth + 1, rows);
                append_loop_marker(block, depth + 1, rows);
                append_condition_timeout(condition, block, depth, rows);
            }
            BlockKind::WatchGroup { group } => {
                for (index, lane) in group.lanes.iter().enumerate() {
                    rows.push(TimelineRow {
                        identity: Some(lane.id.clone()),
                        selection: Some(TimelineSelection::Identity(lane.id.clone())),
                        depth: depth + 1,
                        label: format!("Priority {}", index + 1),
                        summary: passive_condition_summary(&lane.condition),
                        lane_priority: Some(index + 1),
                        enabled: lane.enabled,
                        is_loop_marker: false,
                        is_selectable: true,
                    });
                    append_container(depth + 2, "THEN", rows);
                    append_blocks(&lane.then_body, depth + 3, rows);
                }
                if let TimeoutOutcome::RunBody { body } = &group.timeout_outcome {
                    append_timeout_container(&block.id, depth + 1, "ON TIMEOUT", rows);
                    append_blocks(body, depth + 2, rows);
                }
            }
            BlockKind::Observe { condition } => {
                append_condition_timeout(condition, block, depth, rows);
            }
            BlockKind::Action { .. }
            | BlockKind::Wait { .. }
            | BlockKind::StopSuccess
            | BlockKind::StopError { .. }
            | BlockKind::Comment { .. } => {}
        }
    }
}

fn append_condition_timeout(
    condition: &Condition,
    owner: &Block,
    depth: usize,
    rows: &mut Vec<TimelineRow>,
) {
    if let Some(body) = condition_timeout_body(condition) {
        append_timeout_container(&owner.id, depth + 1, "ON TIMEOUT", rows);
        append_blocks(body, depth + 2, rows);
    }
}

fn append_timeout_container(
    owner_id: &str,
    depth: usize,
    label: &str,
    rows: &mut Vec<TimelineRow>,
) {
    rows.push(TimelineRow {
        identity: None,
        selection: Some(TimelineSelection::TimeoutBody {
            owner_id: owner_id.to_string(),
        }),
        depth,
        label: label.to_string(),
        summary: "Owned timeout branch".to_string(),
        lane_priority: None,
        enabled: true,
        is_loop_marker: false,
        is_selectable: true,
    });
}

fn condition_timeout_body(condition: &Condition) -> Option<&[Block]> {
    let mode = match condition {
        Condition::Text { mode, .. } | Condition::Image { mode, .. } => mode,
    };
    match mode {
        ObserveMode::WaitForTrue {
            timeout_outcome: TimeoutOutcome::RunBody { body },
            ..
        }
        | ObserveMode::WaitForFalse {
            timeout_outcome: TimeoutOutcome::RunBody { body },
            ..
        } => Some(body),
        _ => None,
    }
}

fn append_container(depth: usize, label: &str, rows: &mut Vec<TimelineRow>) {
    rows.push(TimelineRow {
        identity: None,
        selection: None,
        depth,
        label: label.to_string(),
        summary: "Owned branch".to_string(),
        lane_priority: None,
        enabled: true,
        is_loop_marker: false,
        is_selectable: false,
    });
}

fn append_loop_marker(block: &Block, depth: usize, rows: &mut Vec<TimelineRow>) {
    rows.push(TimelineRow {
        identity: None,
        selection: None,
        depth,
        label: "LOOP".to_string(),
        summary: "Return to loop start".to_string(),
        lane_priority: None,
        enabled: block.enabled,
        is_loop_marker: true,
        is_selectable: false,
    });
}

fn block_summary(kind: &BlockKind) -> (String, String) {
    match kind {
        BlockKind::Observe { condition } => ("OBSERVE".into(), condition_summary(condition)),
        BlockKind::Action { action } => ("ACTION".into(), action_summary(action)),
        BlockKind::If { condition, .. } => ("IF".into(), condition_summary(condition)),
        BlockKind::Wait { duration_ms } => ("WAIT".into(), format_duration(*duration_ms)),
        BlockKind::RepeatN { count, .. } => ("REPEAT".into(), format!("{count} iterations")),
        BlockKind::RepeatUntil {
            condition,
            max_iterations,
            ..
        } => (
            "REPEAT UNTIL".into(),
            format!(
                "{} | {} iterations",
                condition_summary(condition),
                format_limit(max_iterations)
            ),
        ),
        BlockKind::Continuous { .. } => ("CONTINUOUS LOOP".into(), "Until stopped".into()),
        BlockKind::WatchGroup { group } => (
            "WATCH GROUP".into(),
            format!(
                "{} lanes | {} timeout",
                group.lanes.len(),
                format_limit(&group.timeout_ms)
            ),
        ),
        BlockKind::StopSuccess => ("STOP".into(), "Complete successfully".into()),
        BlockKind::StopError { message } => ("STOP ERROR".into(), message.clone()),
        BlockKind::Comment { text } => ("NOTE".into(), text.clone()),
    }
}

fn condition_summary(condition: &Condition) -> String {
    match condition {
        Condition::Text { rule_id, mode, .. } => {
            format!("{} text | {rule_id}", observe_verb(mode))
        }
        Condition::Image { rule_id, mode, .. } => {
            format!("{} image | {rule_id}", observe_verb(mode))
        }
    }
}

fn passive_condition_summary(condition: &PassiveCondition) -> String {
    match condition {
        PassiveCondition::Text { rule_id, .. } => format!("Watch text | {rule_id}"),
        PassiveCondition::Image { rule_id, .. } => format!("Watch image | {rule_id}"),
    }
}

fn observe_verb(mode: &ObserveMode) -> &'static str {
    match mode {
        ObserveMode::CheckNow => "Check",
        ObserveMode::WaitForTrue { .. } => "Wait for",
        ObserveMode::WaitForFalse { .. } => "Wait until absent",
    }
}

fn action_summary(action: &Action) -> String {
    match action {
        Action::ClickTextMatch { button, .. } => format!("{button:?}-click text match"),
        Action::ClickImageMatch { button, .. } => format!("{button:?}-click image match"),
        Action::ClickPoint { point_id, button } => format!("{button:?}-click point | {point_id}"),
        Action::ClickRegion { region_id, button } => {
            format!("{button:?}-click region | {region_id}")
        }
        Action::MoveOnly { .. } => "Move pointer without clicking".into(),
    }
}

fn format_limit<T: std::fmt::Display>(limit: &Limit<T>) -> String {
    match limit {
        Limit::Finite(value) => value.to_string(),
        Limit::Unlimited => "Unlimited".to_string(),
    }
}

fn format_duration(duration_ms: u64) -> String {
    if duration_ms >= 1_000 && duration_ms.is_multiple_of(1_000) {
        format!("{} seconds", duration_ms / 1_000)
    } else {
        format!("{duration_ms} ms")
    }
}

pub fn show(
    ui: &mut Ui,
    rows: &[TimelineRow],
    active_block: Option<&str>,
    current_selection: Option<&TimelineSelection>,
) -> Option<TimelineSelection> {
    if rows.is_empty() {
        Frame::none()
            .fill(Color32::from_rgb(14, 16, 18))
            .stroke(Stroke::new(1.0, Color32::from_rgb(47, 49, 52)))
            .rounding(6.0)
            .inner_margin(egui::Margin::same(18.0))
            .show(ui, |ui| {
                ui.label(
                    RichText::new("No sequence selected")
                        .strong()
                        .color(Color32::from_gray(208)),
                );
                ui.label(
                    RichText::new("Create or select a macro to inspect its canonical blocks.")
                        .size(12.0)
                        .color(Color32::from_gray(132)),
                );
            });
        return None;
    }

    let mut clicked_selection = None;
    for row in rows {
        let active =
            row.identity.as_deref() == active_block || row.selection.as_ref() == current_selection;
        let fill = if active {
            Color32::from_rgb(58, 37, 24)
        } else if row.is_loop_marker {
            Color32::from_rgb(17, 19, 21)
        } else {
            Color32::from_rgb(21, 24, 27)
        };
        let stroke = if active {
            Stroke::new(1.0, Color32::from_rgb(220, 104, 42))
        } else {
            Stroke::new(1.0, Color32::from_rgb(47, 51, 55))
        };
        let response = ui
            .horizontal(|ui| {
                ui.add_space(row.depth as f32 * 14.0);
                Frame::none()
                    .fill(fill)
                    .stroke(stroke)
                    .rounding(5.0)
                    .inner_margin(egui::Margin::symmetric(9.0, 7.0))
                    .show(ui, |ui| {
                        ui.set_min_width((ui.available_width() - 8.0).max(120.0));
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(&row.label)
                                    .monospace()
                                    .size(10.0)
                                    .strong()
                                    .color(if active {
                                        Color32::from_rgb(255, 172, 102)
                                    } else {
                                        Color32::from_rgb(176, 142, 104)
                                    }),
                            );
                            ui.label(RichText::new(&row.summary).color(if row.enabled {
                                Color32::from_gray(211)
                            } else {
                                Color32::from_gray(100)
                            }));
                        });
                    });
            })
            .response;
        if row.is_selectable && response.interact(egui::Sense::click()).clicked() {
            clicked_selection = row.selection.clone();
        }
        ui.add_space(4.0);
    }
    clicked_selection
}

#[cfg(test)]
mod tests {
    use crate::engine::macro_engine::{
        Block, BlockKind, Condition, Limit, ObserveMode, PassiveCondition, TimeoutOutcome,
        WatchGroup, WatchLane,
    };

    use super::*;

    fn fixture_watch_group_definition() -> Vec<Block> {
        let lane = |id: &str| WatchLane {
            id: id.to_string(),
            enabled: true,
            condition: PassiveCondition::Text {
                source_block_id: format!("observe-{id}"),
                rule_id: "text-rule".to_string(),
            },
            then_body: vec![Block {
                id: format!("comment-{id}"),
                enabled: true,
                kind: BlockKind::Comment {
                    text: format!("Handle {id}"),
                },
            }],
        };
        vec![Block {
            id: "watch".to_string(),
            enabled: true,
            kind: BlockKind::WatchGroup {
                group: WatchGroup {
                    lanes: vec![lane("salvage"), lane("retry"), lane("stop")],
                    timeout_ms: Limit::Finite(5_000),
                    timeout_outcome: TimeoutOutcome::Continue,
                    cooldown_ms: 100,
                },
            },
        }]
    }

    #[test]
    fn watch_group_rows_show_lane_order_as_priority() {
        let rows = project_timeline(&fixture_watch_group_definition());

        assert_eq!(
            rows.iter()
                .filter_map(|row| row.lane_priority)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn continuous_loop_projects_a_non_editable_return_marker() {
        let rows = project_timeline(&[Block {
            id: "loop".to_string(),
            enabled: true,
            kind: BlockKind::Continuous {
                body: vec![Block {
                    id: "inside".to_string(),
                    enabled: true,
                    kind: BlockKind::Comment {
                        text: "Still watching".to_string(),
                    },
                }],
            },
        }]);

        let marker = rows.last().expect("loop marker");
        assert_eq!(marker.summary, "Return to loop start");
        assert!(marker.is_loop_marker);
        assert!(!marker.is_selectable);
    }

    #[test]
    fn condition_timeout_bodies_project_as_owned_selectable_rows() {
        let condition = |owner: &str, child: &str| Condition::Text {
            source_block_id: owner.into(),
            rule_id: "rule".into(),
            mode: ObserveMode::WaitForTrue {
                timeout_ms: Limit::Finite(100),
                timeout_outcome: TimeoutOutcome::RunBody {
                    body: vec![Block {
                        id: child.into(),
                        enabled: true,
                        kind: BlockKind::Comment {
                            text: "fallback".into(),
                        },
                    }],
                },
            },
        };
        let rows = project_timeline(&[
            Block {
                id: "observe".into(),
                enabled: true,
                kind: BlockKind::Observe {
                    condition: condition("observe", "observe-timeout-child"),
                },
            },
            Block {
                id: "if".into(),
                enabled: true,
                kind: BlockKind::If {
                    condition: condition("observe", "if-timeout-child"),
                    then_body: vec![],
                    else_body: vec![],
                },
            },
            Block {
                id: "repeat".into(),
                enabled: true,
                kind: BlockKind::RepeatUntil {
                    condition: condition("observe", "repeat-timeout-child"),
                    max_iterations: Limit::Finite(2),
                    body: vec![],
                },
            },
        ]);

        for (owner, child) in [
            ("observe", "observe-timeout-child"),
            ("if", "if-timeout-child"),
            ("repeat", "repeat-timeout-child"),
        ] {
            let marker = rows
                .iter()
                .find(|row| {
                    row.selection
                        == Some(TimelineSelection::TimeoutBody {
                            owner_id: owner.into(),
                        })
                })
                .unwrap();
            assert_eq!(marker.label, "ON TIMEOUT");
            assert!(marker.is_selectable);
            let child = rows
                .iter()
                .find(|row| row.identity.as_deref() == Some(child))
                .unwrap();
            assert!(child.is_selectable);
            assert!(child.depth > marker.depth);
        }
    }

    #[test]
    fn typed_timeout_marker_does_not_collide_with_real_suffix_identity() {
        let rows = project_timeline(&[
            Block {
                id: "owner".into(),
                enabled: true,
                kind: BlockKind::Observe {
                    condition: Condition::Text {
                        source_block_id: "owner".into(),
                        rule_id: "rule".into(),
                        mode: ObserveMode::WaitForTrue {
                            timeout_ms: Limit::Finite(100),
                            timeout_outcome: TimeoutOutcome::RunBody { body: vec![] },
                        },
                    },
                },
            },
            Block {
                id: "owner-timeout".into(),
                enabled: true,
                kind: BlockKind::Comment {
                    text: "real block".into(),
                },
            },
        ]);
        assert!(rows.iter().any(|row| {
            row.selection == Some(TimelineSelection::Identity("owner-timeout".into()))
        }));
        assert!(rows.iter().any(|row| {
            row.selection
                == Some(TimelineSelection::TimeoutBody {
                    owner_id: "owner".into(),
                })
        }));
    }
}
