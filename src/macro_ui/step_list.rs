use eframe::egui::{self, Color32, Frame, RichText, Sense, Stroke, Ui};

use crate::engine::macro_engine::{Block, BlockKind, Condition, MacroDefinition, TimeoutOutcome};
use crate::macro_ui::canvas_model::{CanvasSelection, block_category, block_presentation};
use crate::ui_theme::{BlockCategory, category_style, colors, text};

const INDENT_PX: f32 = 20.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepListRow {
    Block {
        id: String,
        indent: usize,
        title: String,
        summary: String,
        enabled: bool,
        category: BlockCategory,
        selection: CanvasSelection,
    },
    Section {
        title: String,
        indent: usize,
        selection: Option<CanvasSelection>,
        empty: bool,
    },
}

impl StepListRow {
    pub fn indent(&self) -> usize {
        match self {
            Self::Block { indent, .. } | Self::Section { indent, .. } => *indent,
        }
    }

    #[cfg(test)]
    pub fn title(&self) -> &str {
        match self {
            Self::Block { title, .. } | Self::Section { title, .. } => title,
        }
    }

    #[cfg(test)]
    pub fn block_id(&self) -> Option<&str> {
        match self {
            Self::Block { id, .. } => Some(id.as_str()),
            Self::Section { .. } => None,
        }
    }

    pub fn selection(&self) -> Option<&CanvasSelection> {
        match self {
            Self::Block { selection, .. } => Some(selection),
            Self::Section { selection, .. } => selection.as_ref(),
        }
    }
}

pub fn project_step_list(definition: &MacroDefinition) -> Vec<StepListRow> {
    let mut rows = Vec::new();
    append_blocks(&definition.blocks, 0, &mut rows);
    rows
}

fn append_blocks(blocks: &[Block], indent: usize, rows: &mut Vec<StepListRow>) {
    for block in blocks {
        let (title, summary) = block_presentation(&block.kind);
        rows.push(StepListRow::Block {
            id: block.id.clone(),
            indent,
            title,
            summary,
            enabled: block.enabled,
            category: block_category(&block.kind),
            selection: CanvasSelection::Block(block.id.clone()),
        });
        append_owned(block, indent + 1, rows);
    }
}

fn append_owned(block: &Block, indent: usize, rows: &mut Vec<StepListRow>) {
    match &block.kind {
        BlockKind::If {
            condition,
            then_body,
            else_body,
        } => {
            push_section(
                "THEN",
                indent,
                Some(CanvasSelection::IfThen {
                    if_id: block.id.clone(),
                }),
                then_body.is_empty(),
                rows,
            );
            append_blocks(then_body, indent + 1, rows);
            push_section(
                "ELSE",
                indent,
                Some(CanvasSelection::IfElse {
                    if_id: block.id.clone(),
                }),
                else_body.is_empty(),
                rows,
            );
            append_blocks(else_body, indent + 1, rows);
            append_timeout(condition, &block.id, indent, rows);
        }
        BlockKind::RepeatN { body, .. } | BlockKind::Continuous { body } => {
            push_section("LOOP BODY", indent, None, body.is_empty(), rows);
            append_blocks(body, indent + 1, rows);
        }
        BlockKind::RepeatUntil {
            condition, body, ..
        } => {
            push_section("LOOP BODY", indent, None, body.is_empty(), rows);
            append_blocks(body, indent + 1, rows);
            append_timeout(condition, &block.id, indent, rows);
        }
        BlockKind::WatchGroup { group } => {
            for lane in &group.lanes {
                push_section(
                    &format!("LANE {}", lane.id),
                    indent,
                    Some(CanvasSelection::Lane {
                        group_id: block.id.clone(),
                        lane_id: lane.id.clone(),
                    }),
                    lane.then_body.is_empty(),
                    rows,
                );
                append_blocks(&lane.then_body, indent + 1, rows);
            }
            if let TimeoutOutcome::RunBody { body } = &group.timeout_outcome {
                push_section(
                    "ON TIMEOUT",
                    indent,
                    Some(CanvasSelection::TimeoutBody {
                        owner_id: block.id.clone(),
                    }),
                    body.is_empty(),
                    rows,
                );
                append_blocks(body, indent + 1, rows);
            }
        }
        BlockKind::Observe { condition } => {
            append_timeout(condition, &block.id, indent, rows);
        }
        BlockKind::Action { .. }
        | BlockKind::Wait { .. }
        | BlockKind::StopSuccess
        | BlockKind::StopError { .. }
        | BlockKind::Comment { .. } => {}
    }
}

fn append_timeout(
    condition: &Condition,
    owner_id: &str,
    indent: usize,
    rows: &mut Vec<StepListRow>,
) {
    let Some(body) = timeout_body(condition) else {
        return;
    };
    push_section(
        "ON TIMEOUT",
        indent,
        Some(CanvasSelection::TimeoutBody {
            owner_id: owner_id.to_string(),
        }),
        body.is_empty(),
        rows,
    );
    append_blocks(body, indent + 1, rows);
}

fn timeout_body(condition: &Condition) -> Option<&[Block]> {
    match condition {
        Condition::Text { mode, .. } | Condition::Image { mode, .. } => match mode {
            crate::engine::macro_engine::ObserveMode::WaitForTrue {
                timeout_outcome, ..
            }
            | crate::engine::macro_engine::ObserveMode::WaitForFalse {
                timeout_outcome, ..
            } => match timeout_outcome {
                TimeoutOutcome::RunBody { body } => Some(body.as_slice()),
                TimeoutOutcome::StopError { .. } | TimeoutOutcome::Continue => None,
            },
            crate::engine::macro_engine::ObserveMode::CheckNow => None,
        },
    }
}

fn push_section(
    title: &str,
    indent: usize,
    selection: Option<CanvasSelection>,
    empty: bool,
    rows: &mut Vec<StepListRow>,
) {
    rows.push(StepListRow::Section {
        title: title.to_string(),
        indent,
        selection,
        empty,
    });
}

pub fn show(
    ui: &mut Ui,
    rows: &[StepListRow],
    current: Option<&CanvasSelection>,
    active_block: Option<&str>,
) -> Option<CanvasSelection> {
    if rows.is_empty() {
        ui.label(
            RichText::new("Create or select a macro to inspect its steps.")
                .size(text::SUPPORTING)
                .color(Color32::from_gray(150)),
        );
        return None;
    }
    let mut selection = None;
    egui::ScrollArea::vertical()
        .id_source("macro-step-list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for row in rows {
                if let Some(next) = show_row(ui, row, current, active_block) {
                    selection = Some(next);
                }
                ui.add_space(4.0);
            }
        });
    selection
}

fn show_row(
    ui: &mut Ui,
    row: &StepListRow,
    current: Option<&CanvasSelection>,
    active_block: Option<&str>,
) -> Option<CanvasSelection> {
    let selected = row.selection() == current;
    let is_section = matches!(row, StepListRow::Section { .. });
    let indent = row.indent() as f32 * INDENT_PX;
    let mut chosen = None;
    ui.horizontal(|ui| {
        ui.add_space(indent);
        let fill = if selected {
            Color32::from_rgb(43, 31, 23)
        } else if is_section {
            Color32::from_rgb(15, 17, 19)
        } else {
            Color32::from_rgb(19, 22, 25)
        };
        let stroke = if selected {
            Color32::from_rgb(174, 91, 43)
        } else {
            Color32::from_rgb(46, 50, 54)
        };
        let response = Frame::none()
            .fill(fill)
            .stroke(Stroke::new(1.0, stroke))
            .rounding(5.0)
            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
            .show(ui, |ui| {
                ui.set_width((ui.available_width() - 4.0).max(80.0));
                match row {
                    StepListRow::Block {
                        id,
                        title,
                        summary,
                        enabled,
                        category,
                        ..
                    } => {
                        let style = category_style(*category);
                        let title_color = if *enabled {
                            Color32::from_gray(218)
                        } else {
                            Color32::from_gray(130)
                        };
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(style.icon)
                                    .size(text::BODY)
                                    .color(style.accent),
                            );
                            ui.label(
                                RichText::new(title)
                                    .size(text::BODY)
                                    .strong()
                                    .color(title_color),
                            );
                            if !*enabled {
                                ui.label(
                                    RichText::new("Disabled")
                                        .size(text::META)
                                        .color(Color32::from_gray(130)),
                                );
                            }
                            if active_block == Some(id.as_str()) {
                                ui.label(
                                    RichText::new("Running")
                                        .size(text::META)
                                        .color(colors::SUCCESS),
                                );
                            }
                        });
                        ui.label(
                            RichText::new(summary)
                                .size(text::SUPPORTING)
                                .color(Color32::from_gray(174)),
                        );
                    }
                    StepListRow::Section { title, empty, .. } => {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(title)
                                    .monospace()
                                    .size(text::SUPPORTING)
                                    .strong()
                                    .color(Color32::from_rgb(186, 143, 96)),
                            );
                            if *empty {
                                ui.label(
                                    RichText::new("empty")
                                        .size(text::META)
                                        .color(Color32::from_gray(130)),
                                );
                            }
                        });
                    }
                }
            })
            .response;
        let clickable = response.interact(Sense::click());
        if clickable.clicked() {
            if let Some(selection) = row.selection() {
                chosen = Some(selection.clone());
            }
        }
    });
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::macro_engine::{Block, BlockKind};
    use crate::macro_ui::canvas_model::CanvasSelection;
    use crate::macro_ui::test_support::fixture_if;
    use crate::macro_ui::{
        ContainerPath, EditorCommand, EditorDraft, InsertionTarget, apply_editor_command,
        locate_block_path,
    };

    fn wait_block(id: &str) -> Block {
        Block {
            id: id.into(),
            enabled: true,
            kind: BlockKind::Wait { duration_ms: 250 },
        }
    }

    fn row_titles(rows: &[StepListRow]) -> Vec<(usize, String, Option<String>)> {
        rows.iter()
            .map(|row| {
                (
                    row.indent(),
                    row.title().to_string(),
                    row.block_id().map(str::to_string),
                )
            })
            .collect()
    }

    #[test]
    fn step_list_if_then_else_indent() {
        let rows = project_step_list(&fixture_if());
        assert_eq!(
            row_titles(&rows),
            vec![
                (0, "If".into(), Some("if-1".into())),
                (1, "THEN".into(), None),
                (2, "Check text".into(), Some("then-observe".into())),
                (1, "ELSE".into(), None),
                (2, "Note".into(), Some("else-note".into())),
            ]
        );
        assert_eq!(
            rows[1].selection(),
            Some(&CanvasSelection::IfThen {
                if_id: "if-1".into()
            })
        );
        assert_eq!(
            rows[3].selection(),
            Some(&CanvasSelection::IfElse {
                if_id: "if-1".into()
            })
        );
    }

    #[test]
    fn step_list_insert_into_else() {
        let mut definition = fixture_if();
        let BlockKind::If { else_body, .. } = &mut definition.blocks[0].kind else {
            panic!("fixture_if must start with If");
        };
        else_body.clear();
        let mut draft = EditorDraft::new(definition);
        apply_editor_command(
            &mut draft,
            EditorCommand::InsertBlock {
                target: InsertionTarget {
                    container: ContainerPath::IfElse {
                        if_id: "if-1".into(),
                    },
                    index: 0,
                },
                block: wait_block("wait-else"),
            },
        )
        .unwrap();

        let rows = project_step_list(&draft.definition);
        let else_index = rows
            .iter()
            .position(|row| row.title() == "ELSE")
            .expect("ELSE section");
        assert_eq!(rows[else_index].indent(), 1);
        assert_eq!(rows[else_index + 1].block_id(), Some("wait-else"));
        assert_eq!(rows[else_index + 1].indent(), 2);
        assert_eq!(
            rows[else_index].selection(),
            Some(&CanvasSelection::IfElse {
                if_id: "if-1".into()
            })
        );
        let path = locate_block_path(&draft, "wait-else").unwrap();
        assert_eq!(
            path.container,
            ContainerPath::IfElse {
                if_id: "if-1".into()
            }
        );
    }

    #[test]
    fn step_list_reorder_else_siblings() {
        let mut definition = fixture_if();
        let BlockKind::If { else_body, .. } = &mut definition.blocks[0].kind else {
            panic!("fixture_if must start with If");
        };
        *else_body = vec![wait_block("wait-a"), wait_block("wait-b")];
        let mut draft = EditorDraft::new(definition);
        let path = locate_block_path(&draft, "wait-b").unwrap();
        apply_editor_command(
            &mut draft,
            EditorCommand::ReorderSibling {
                path,
                to_index: 0,
            },
        )
        .unwrap();

        let rows = project_step_list(&draft.definition);
        let else_index = rows
            .iter()
            .position(|row| row.title() == "ELSE")
            .expect("ELSE section");
        assert_eq!(rows[else_index + 1].block_id(), Some("wait-b"));
        assert_eq!(rows[else_index + 2].block_id(), Some("wait-a"));
        assert_eq!(rows[else_index + 1].indent(), 2);
        assert_eq!(rows[else_index + 2].indent(), 2);
    }
}
