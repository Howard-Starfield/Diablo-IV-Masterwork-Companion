mod editor;
mod inspector;
mod library;
mod monitor;
mod timeline;

pub use editor::*;

use eframe::egui::{self, Align, Button, Color32, Frame, Layout, RichText, Stroke, Ui};

use crate::engine::macro_engine::{RunEvent, ValidationProblem};

use library::{MacroLibraryRow, project_definition};
use monitor::{MonitorProjection, RunDefinitionSnapshot, project_last_completion, project_monitor};
use timeline::{TimelineRow, TimelineSelection, project_timeline};

/// Canonical editor state. It intentionally owns no runtime command sender, capture service,
/// mouse controller, or platform input handle.
#[derive(Debug)]
pub struct MacroPageState {
    pub draft: Option<EditorDraft>,
    pub saved_revision: Option<u64>,
    pub enabled: bool,
    pub running_snapshot: Option<RunDefinitionSnapshot>,
    pub runtime_events: Vec<RunEvent>,
    pub selected_block_id: Option<String>,
    selected_timeline: Option<TimelineSelection>,
    pub pending_inspector_intent: Option<inspector::InspectorIntent>,
    pub editor_feedback: Option<String>,
    pending_conversion: Option<PendingConversion>,
}

impl Default for MacroPageState {
    fn default() -> Self {
        Self {
            draft: None,
            saved_revision: None,
            enabled: true,
            running_snapshot: None,
            runtime_events: Vec::new(),
            selected_block_id: None,
            selected_timeline: None,
            pending_inspector_intent: None,
            editor_feedback: None,
            pending_conversion: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct PendingConversion {
    block_id: String,
    preview: ConversionPreview,
    required_values: Vec<(String, String)>,
    command: EditorCommand,
    structural_children: bool,
}

#[derive(Debug, Default)]
pub struct MacroPage;

impl MacroPage {
    pub const MONITOR_HEIGHT: f32 = 176.0;

    pub fn show(ui: &mut Ui, state: &mut MacroPageState) {
        if let Some(draft) = &mut state.draft {
            draft.editability = if state.running_snapshot.is_some() {
                DraftEditability::Running {
                    revision: draft.definition.revision,
                }
            } else {
                DraftEditability::Editable
            };
        }
        let selected_macro_id = state
            .draft
            .as_ref()
            .map(|definition| definition.id.as_str());
        let monitor = project_monitor(
            selected_macro_id,
            state.running_snapshot.as_ref(),
            &state.runtime_events,
        );
        let last_completion = selected_macro_id
            .and_then(|macro_id| project_last_completion(&state.runtime_events, macro_id));
        let problems = state
            .draft
            .as_ref()
            .map(editor_validation_problems)
            .unwrap_or_default();
        let library_rows = state
            .draft
            .as_ref()
            .map(|definition| {
                vec![project_definition(
                    definition,
                    state.saved_revision,
                    state.enabled,
                    &problems,
                    &monitor,
                    last_completion.as_ref(),
                )]
            })
            .unwrap_or_default();
        let timeline_rows = state
            .draft
            .as_ref()
            .map(|definition| project_timeline(&definition.blocks))
            .unwrap_or_default();

        ui.set_width(ui.available_width());
        ui.vertical(|ui| {
            title(ui);
            ui.add_space(8.0);
            if let Some(target) = status_strip(ui, state, &monitor, &problems) {
                select_timeline(state, TimelineSelection::Identity(target));
            }
            ui.add_space(8.0);
            if let Some(target) = workspace(
                ui,
                state,
                &library_rows,
                &timeline_rows,
                &monitor,
                &problems,
            ) {
                select_timeline(state, target);
            }
        });
    }

    pub fn show_monitor(ui: &mut Ui, state: &MacroPageState) {
        let selected_macro_id = state
            .draft
            .as_ref()
            .map(|definition| definition.id.as_str());
        let monitor = project_monitor(
            selected_macro_id,
            state.running_snapshot.as_ref(),
            &state.runtime_events,
        );
        monitor::show(ui, &monitor);
    }
}

fn select_timeline(state: &mut MacroPageState, selection: TimelineSelection) {
    state.selected_block_id = match &selection {
        TimelineSelection::Identity(id) => Some(id.clone()),
        TimelineSelection::TimeoutBody { .. } => None,
    };
    state.selected_timeline = Some(selection);
}

fn current_timeline_selection(state: &MacroPageState) -> Option<TimelineSelection> {
    match (&state.selected_block_id, &state.selected_timeline) {
        (Some(id), _) => Some(TimelineSelection::Identity(id.clone())),
        (None, Some(selection @ TimelineSelection::TimeoutBody { .. })) => Some(selection.clone()),
        _ => None,
    }
}

fn title(ui: &mut Ui) {
    Frame::none()
        .fill(Color32::from_rgb(18, 18, 19))
        .stroke(Stroke::new(1.0, Color32::from_rgb(57, 48, 41)))
        .rounding(6.0)
        .inner_margin(egui::Margin::same(14.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("MACRO FORGE")
                            .monospace()
                            .size(21.0)
                            .strong()
                            .color(Color32::from_rgb(224, 119, 53)),
                    );
                    ui.label(
                        RichText::new(
                            "Observe, arbitrate, then act - one immutable revision at a time",
                        )
                        .size(12.0)
                        .color(Color32::from_gray(138)),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new("STRUCTURED EDITOR")
                            .monospace()
                            .size(10.0)
                            .strong()
                            .color(Color32::from_rgb(174, 142, 102)),
                    );
                });
            });
        });
}

fn status_strip(
    ui: &mut Ui,
    state: &mut MacroPageState,
    monitor: &MonitorProjection,
    problems: &[ValidationProblem],
) -> Option<String> {
    let mut navigate = None;
    Frame::none()
        .fill(Color32::from_rgb(17, 20, 22))
        .stroke(Stroke::new(1.0, Color32::from_rgb(48, 52, 55)))
        .rounding(6.0)
        .inner_margin(egui::Margin::same(11.0))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                status_fact(
                    ui,
                    "TARGET",
                    state
                        .draft
                        .as_ref()
                        .map(|definition| definition.target.title_contains.as_str())
                        .filter(|title| !title.is_empty())
                        .unwrap_or("No target selected"),
                );
                status_fact(ui, "WINDOW", "Not connected");
                status_fact(ui, "FOREGROUND", "Unknown");
                status_fact(
                    ui,
                    "DISPLAY",
                    state
                        .draft
                        .as_ref()
                        .map(|definition| {
                            format!(
                                "{}x{} | {} DPI",
                                definition.target.captured_client_width,
                                definition.target.captured_client_height,
                                definition.target.captured_dpi
                            )
                        })
                        .as_deref()
                        .unwrap_or("--"),
                );
                status_fact(ui, "GEOMETRY", "Snapshot only");
                status_fact(ui, "REVISION", revision_summary(state, monitor).as_str());
                status_fact(ui, "VALIDATION", validation_summary(state, problems));
            });
            ui.add_space(9.0);
            ui.horizontal_wrapped(|ui| {
                if state.draft.is_none() && ui.button("Create starter draft").clicked() {
                    state.draft = Some(EditorDraft::new(starter_macro_definition()));
                    state.selected_block_id = Some("observe-1".into());
                    state.selected_timeline = Some(TimelineSelection::Identity("observe-1".into()));
                    state.editor_feedback =
                        Some("Created an unsaved starter draft for editor authoring.".into());
                }
                let can_edit = state
                    .draft
                    .as_ref()
                    .is_some_and(|draft| matches!(draft.editability, DraftEditability::Editable));
                if ui.add_enabled(can_edit, Button::new("Validate")).clicked() {
                    let _ = dispatch_editor_command(state, EditorCommand::MarkValidated);
                }
                for label in ["Dry Run", "Run Once", "Run", "Pause", "Stop"] {
                    ui.add_enabled(false, Button::new(label));
                }
                ui.label(
                    RichText::new("Controls unlock only after editor/runtime wiring")
                        .size(10.0)
                        .color(Color32::from_gray(107)),
                );
                if let Some(target) = inspector::problem_navigation_target(problems, 0) {
                    if ui.small_button("Open first problem").clicked() {
                        navigate = Some(target);
                    }
                }
            });
        });
    navigate
}

fn validation_summary(state: &MacroPageState, problems: &[ValidationProblem]) -> &'static str {
    match state.draft.as_ref() {
        None => "No definition",
        Some(_) if !problems.is_empty() => "Needs revalidation",
        Some(draft) if draft.status == DraftStatus::NeedsValidation => "Needs revalidation",
        Some(_) => "Valid",
    }
}

fn starter_macro_definition() -> crate::engine::macro_engine::MacroDefinition {
    use crate::engine::macro_engine::*;
    use crate::engine::types::RectRatio;
    MacroDefinition {
        schema_version: MACRO_SCHEMA_VERSION,
        id: "starter-macro".into(),
        name: "Starter Macro".into(),
        revision: 1,
        target: TargetProfile {
            process_path: String::new(),
            window_class: String::new(),
            title_contains: "Diablo IV".into(),
            captured_client_width: 1280,
            captured_client_height: 720,
            captured_dpi: 96,
        },
        regions: vec![RegionDefinition {
            id: "starter-region".into(),
            revision: 1,
            rect: RectRatio {
                x: 0.25,
                y: 0.25,
                width: 0.5,
                height: 0.2,
            },
        }],
        points: vec![],
        text_rules: vec![TextRule {
            id: "starter-text".into(),
            revision: 1,
            region_id: "starter-region".into(),
            language: "en-US".into(),
            preprocess: PreprocessProfile::Grayscale,
            expected: "Edit expected text".into(),
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
        blocks: vec![Block {
            id: "observe-1".into(),
            enabled: true,
            kind: BlockKind::Observe {
                condition: Condition::Text {
                    source_block_id: "observe-1".into(),
                    rule_id: "starter-text".into(),
                    mode: ObserveMode::CheckNow,
                },
            },
        }],
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

fn status_fact(ui: &mut Ui, label: &str, value: &str) {
    ui.vertical(|ui| {
        ui.label(
            RichText::new(label)
                .monospace()
                .size(9.0)
                .strong()
                .color(Color32::from_rgb(164, 127, 88)),
        );
        ui.label(
            RichText::new(value)
                .size(11.0)
                .color(Color32::from_gray(202)),
        );
    });
    ui.add_space(10.0);
}

fn revision_summary(state: &MacroPageState, monitor: &MonitorProjection) -> String {
    let draft = state
        .draft
        .as_ref()
        .map(|definition| definition.revision.to_string())
        .unwrap_or_else(|| "--".to_string());
    let saved = state
        .saved_revision
        .map(|revision| revision.to_string())
        .unwrap_or_else(|| "--".to_string());
    let running = monitor
        .running_revision
        .map(|revision| revision.to_string())
        .unwrap_or_else(|| "--".to_string());
    format!("draft {draft} | saved {saved} | running {running}")
}

fn workspace(
    ui: &mut Ui,
    state: &mut MacroPageState,
    library_rows: &[MacroLibraryRow],
    timeline_rows: &[TimelineRow],
    monitor: &MonitorProjection,
    problems: &[ValidationProblem],
) -> Option<TimelineSelection> {
    let mut selection = None;
    let selected_owned = state.selected_block_id.clone();
    let selected = selected_owned.as_deref();
    let selected_timeline = current_timeline_selection(state);
    let projection = state
        .draft
        .as_ref()
        .and_then(|definition| {
            selected.map(|id| inspector::project_inspector(definition, id, problems))
        })
        .unwrap_or(inspector::InspectorProjection::Empty);
    let editable = state.running_snapshot.is_none();
    if ui.available_width() >= 900.0 {
        ui.columns(3, |columns| {
            section(&mut columns[0], "LIBRARY", |ui| {
                library::show(
                    ui,
                    library_rows,
                    state
                        .draft
                        .as_ref()
                        .map(|definition| definition.id.as_str()),
                );
            });
            section(&mut columns[1], "EVENT TIMELINE", |ui| {
                editor_toolbar(ui, state);
                selection = timeline::show(
                    ui,
                    timeline_rows,
                    monitor.active_block.as_deref(),
                    selected_timeline.as_ref(),
                );
            });
            section(&mut columns[2], "INSPECTOR", |ui| {
                if let Some(intent) = inspector::show(ui, &projection, editable) {
                    handle_inspector_intent(state, intent);
                }
            });
        });
    } else {
        section(ui, "LIBRARY", |ui| {
            library::show(
                ui,
                library_rows,
                state
                    .draft
                    .as_ref()
                    .map(|definition| definition.id.as_str()),
            );
        });
        ui.add_space(8.0);
        section(ui, "EVENT TIMELINE", |ui| {
            editor_toolbar(ui, state);
            selection = timeline::show(
                ui,
                timeline_rows,
                monitor.active_block.as_deref(),
                selected_timeline.as_ref(),
            );
        });
        ui.add_space(8.0);
        section(ui, "INSPECTOR", |ui| {
            if let Some(intent) = inspector::show(ui, &projection, editable) {
                handle_inspector_intent(state, intent);
            }
        });
    }
    selection
}

fn dispatch_editor_command(
    state: &mut MacroPageState,
    command: EditorCommand,
) -> Result<EditOutcome, EditorError> {
    let result = state
        .draft
        .as_mut()
        .ok_or_else(|| EditorError::MissingBlock("no draft".into()))
        .and_then(|draft| apply_editor_command(draft, command));
    state.editor_feedback = Some(match &result {
        Ok(EditOutcome::Changed) => "Draft updated; validation required.".into(),
        Ok(EditOutcome::Validated) => "Draft validated.".into(),
        Ok(EditOutcome::NoChange) => "No change.".into(),
        Err(error) => format!("Edit rejected: {error:?}"),
    });
    result
}

fn handle_inspector_intent(state: &mut MacroPageState, intent: inspector::InspectorIntent) {
    match inspector_editor_command(state.draft.as_ref(), &intent) {
        Ok(Some(command)) => {
            let _ = dispatch_editor_command(state, command);
        }
        Ok(None) => state.pending_inspector_intent = Some(intent),
        Err(message) => state.editor_feedback = Some(message),
    }
}

fn inspector_editor_command(
    draft: Option<&EditorDraft>,
    intent: &inspector::InspectorIntent,
) -> Result<Option<EditorCommand>, String> {
    use inspector::InspectorIntent;
    let path = |block_id: &str| {
        draft
            .and_then(|draft| locate_block_path(draft, block_id))
            .ok_or_else(|| format!("Selected block '{block_id}' is no longer available."))
    };
    let command = match intent {
        InspectorIntent::TestOcr { .. }
        | InspectorIntent::TestImage { .. }
        | InspectorIntent::RecaptureRegion { .. } => return Ok(None),
        InspectorIntent::InvalidEdit { message } => return Err(message.clone()),
        InspectorIntent::ReplaceTextRule { rule } => {
            EditorCommand::ReplaceTextRule { rule: rule.clone() }
        }
        InspectorIntent::ReplaceImageRule { rule } => {
            EditorCommand::ReplaceImageRule { rule: rule.clone() }
        }
        InspectorIntent::SetConditionMode { block_id, mode } => EditorCommand::SetConditionMode {
            path: path(block_id)?,
            mode: mode.clone(),
        },
        InspectorIntent::SetRepeatUntilMax { block_id, max } => EditorCommand::SetRepeatUntilMax {
            path: path(block_id)?,
            max_iterations: max.clone(),
        },
        InspectorIntent::SetWaitDuration {
            block_id,
            duration_ms,
        } => EditorCommand::SetWaitDuration {
            path: path(block_id)?,
            duration_ms: *duration_ms,
        },
        InspectorIntent::SetRepeatCount { block_id, count } => EditorCommand::SetRepeatCount {
            path: path(block_id)?,
            count: *count,
        },
        InspectorIntent::SetWatchSettings {
            block_id,
            timeout_ms,
            cooldown_ms,
        } => EditorCommand::SetWatchSettings {
            path: path(block_id)?,
            timeout_ms: timeout_ms.clone(),
            cooldown_ms: *cooldown_ms,
        },
    };
    Ok(Some(command))
}

fn editor_toolbar(ui: &mut Ui, state: &mut MacroPageState) {
    let editable = state
        .draft
        .as_ref()
        .is_some_and(|draft| matches!(draft.editability, DraftEditability::Editable));
    let selected = state.selected_block_id.clone();
    let selected_timeline = current_timeline_selection(state);
    if let Some(feedback) = &state.editor_feedback {
        ui.label(
            RichText::new(feedback)
                .size(10.0)
                .color(Color32::from_rgb(196, 154, 106)),
        );
    }
    if let Some(intent) = state.pending_inspector_intent.clone() {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!("Pending observation intent: {intent:?}"))
                    .size(10.0)
                    .color(Color32::from_gray(150)),
            );
            if ui.small_button("Clear").clicked() {
                state.pending_inspector_intent = None;
            }
        });
    }
    ui.horizontal_wrapped(|ui| {
        if ui.add_enabled(editable, Button::new("Undo")).clicked() {
            let _ = dispatch_editor_command(state, EditorCommand::Undo);
        }
        if ui.add_enabled(editable, Button::new("+ Note")).clicked() {
            if let Some(draft) = &state.draft {
                let block = crate::engine::macro_engine::Block {
                    id: next_unique_id(draft, "note"),
                    enabled: true,
                    kind: crate::engine::macro_engine::BlockKind::Comment {
                        text: "New step".into(),
                    },
                };
                let target = InsertionTarget {
                    container: ContainerPath::Root,
                    index: draft.blocks.len(),
                };
                let _ =
                    dispatch_editor_command(state, EditorCommand::InsertBlock { target, block });
            }
        }
    });
    ui.horizontal_wrapped(|ui| {
        for (label, kind) in [
            ("+ Observe", PaletteKind::Observe),
            ("+ Action", PaletteKind::Action),
            ("+ IF", PaletteKind::If),
            ("+ Repeat", PaletteKind::Repeat),
            ("+ Watch", PaletteKind::Watch),
            ("+ Wait", PaletteKind::Wait),
            ("+ Stop", PaletteKind::Stop),
        ] {
            if ui.add_enabled(editable, Button::new(label)).clicked() {
                let command = state
                    .draft
                    .as_ref()
                    .ok_or_else(|| "No draft is open.".to_string())
                    .and_then(|draft| {
                        palette_command_for_selection(draft, selected_timeline.as_ref(), kind)
                    });
                match command {
                    Ok(command) => {
                        let _ = dispatch_editor_command(state, command);
                    }
                    Err(message) => state.editor_feedback = Some(message),
                }
            }
        }
    });
    let Some(selected) = selected else { return };
    if let Some((group_id, index, len, enabled)) = state
        .draft
        .as_ref()
        .and_then(|draft| locate_watch_lane(draft, &selected))
    {
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(editable && index > 0, Button::new("Lane up"))
                .clicked()
            {
                let _ = dispatch_editor_command(
                    state,
                    EditorCommand::MoveLane {
                        group_id: group_id.clone(),
                        lane_id: selected.clone(),
                        to_index: index - 1,
                    },
                );
            }
            if ui
                .add_enabled(editable && index + 1 < len, Button::new("Lane down"))
                .clicked()
            {
                let _ = dispatch_editor_command(
                    state,
                    EditorCommand::MoveLane {
                        group_id: group_id.clone(),
                        lane_id: selected.clone(),
                        to_index: index + 1,
                    },
                );
            }
            if ui
                .add_enabled(
                    editable,
                    Button::new(if enabled {
                        "Disable lane"
                    } else {
                        "Enable lane"
                    }),
                )
                .clicked()
            {
                let _ = dispatch_editor_command(
                    state,
                    EditorCommand::SetLaneEnabled {
                        group_id,
                        lane_id: selected.clone(),
                        enabled: !enabled,
                    },
                );
            }
        });
        return;
    }
    let Some(path) = state
        .draft
        .as_ref()
        .and_then(|draft| locate_block_path(draft, &selected))
    else {
        return;
    };
    let Some((index, len)) = state
        .draft
        .as_ref()
        .and_then(|draft| sibling_position(draft, &path))
    else {
        return;
    };
    let block = state
        .draft
        .as_ref()
        .and_then(|draft| block_at_path(draft, &path))
        .cloned();
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(editable && index > 0, Button::new("Up"))
            .clicked()
        {
            let _ = dispatch_editor_command(
                state,
                EditorCommand::ReorderSibling {
                    path: path.clone(),
                    to_index: index - 1,
                },
            );
        }
        if ui
            .add_enabled(editable && index + 1 < len, Button::new("Down"))
            .clicked()
        {
            let _ = dispatch_editor_command(
                state,
                EditorCommand::ReorderSibling {
                    path: path.clone(),
                    to_index: index + 1,
                },
            );
        }
        if let Some(block) = &block {
            if ui
                .add_enabled(
                    editable,
                    Button::new(if block.enabled { "Disable" } else { "Enable" }),
                )
                .clicked()
            {
                let _ = dispatch_editor_command(
                    state,
                    EditorCommand::SetBlockEnabled {
                        path: path.clone(),
                        enabled: !block.enabled,
                    },
                );
            }
        }
        if ui.add_enabled(editable, Button::new("Duplicate")).clicked() {
            let _ = dispatch_editor_command(
                state,
                EditorCommand::DuplicateBlock {
                    source: path.clone(),
                    target: InsertionTarget {
                        container: path.container.clone(),
                        index: index + 1,
                    },
                },
            );
        }
        if !matches!(path.container, ContainerPath::Root)
            && ui
                .add_enabled(editable, Button::new("Move to root"))
                .clicked()
        {
            let root = state
                .draft
                .as_ref()
                .map(|draft| draft.blocks.len())
                .unwrap_or(0);
            let _ = dispatch_editor_command(
                state,
                EditorCommand::MoveBlock {
                    source: path.clone(),
                    target: InsertionTarget {
                        container: ContainerPath::Root,
                        index: root,
                    },
                },
            );
        }
    });
    if let Some(block) = block {
        ui.horizontal_wrapped(|ui| match block.kind {
            crate::engine::macro_engine::BlockKind::RepeatN { .. }
            | crate::engine::macro_engine::BlockKind::RepeatUntil { .. }
            | crate::engine::macro_engine::BlockKind::Continuous { .. } => {
                if ui
                    .add_enabled(editable, Button::new("Delete loop + contents"))
                    .clicked()
                {
                    let _ = dispatch_editor_command(
                        state,
                        EditorCommand::RemoveBlock {
                            path: path.clone(),
                            loop_choice: Some(LoopDeletionChoice::DeleteWithContents),
                        },
                    );
                }
                if ui
                    .add_enabled(editable, Button::new("Keep contents"))
                    .clicked()
                {
                    let _ = dispatch_editor_command(
                        state,
                        EditorCommand::RemoveBlock {
                            path: path.clone(),
                            loop_choice: Some(LoopDeletionChoice::KeepContents),
                        },
                    );
                }
            }
            _ => {
                if ui.add_enabled(editable, Button::new("Delete")).clicked() {
                    let _ = dispatch_editor_command(
                        state,
                        EditorCommand::RemoveBlock {
                            path: path.clone(),
                            loop_choice: None,
                        },
                    );
                }
            }
        });
        if let ContainerPath::IfThen { ref if_id } | ContainerPath::IfElse { ref if_id } =
            path.container
        {
            let branch = if matches!(path.container, ContainerPath::IfThen { .. }) {
                IfBranch::Then
            } else {
                IfBranch::Else
            };
            if ui
                .add_enabled(editable, Button::new("Transfer THEN / ELSE"))
                .clicked()
            {
                let _ = dispatch_editor_command(
                    state,
                    EditorCommand::TransferIfBranch {
                        if_id: if_id.clone(),
                        branch,
                        block_id: path.block_id.clone(),
                        to_index: 0,
                    },
                );
            }
        }
        if state
            .pending_conversion
            .as_ref()
            .is_some_and(|pending| pending.block_id == block.id)
        {
            let mut pending = state.pending_conversion.take().expect("checked pending");
            conversion_preview_ui(ui, &mut pending);
            let valid = state
                .draft
                .as_ref()
                .is_some_and(|draft| pending_conversion_valid(draft, &pending));
            let mut apply = false;
            let mut cancel = false;
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(editable && valid, Button::new("Apply conversion"))
                    .clicked()
                {
                    apply = true;
                }
                if ui.small_button("Cancel conversion").clicked() {
                    cancel = true;
                }
            });
            if !valid {
                ui.label(
                    RichText::new("Complete valid required values before applying.")
                        .color(Color32::from_rgb(224, 112, 75)),
                );
            }
            state.pending_conversion = Some(pending);
            if apply {
                let _ = confirm_pending_conversion(state);
            } else if cancel {
                cancel_pending_conversion(state);
            }
        } else {
            let choices = state
                .draft
                .as_ref()
                .map(|draft| conversion_choices(draft, &block, &path))
                .unwrap_or_default();
            ui.horizontal_wrapped(|ui| {
                for (label, preview) in choices {
                    if ui.add_enabled(editable, Button::new(label)).clicked() {
                        state.pending_conversion = Some(preview);
                    }
                }
            });
            let replacements = state
                .draft
                .as_ref()
                .map(|draft| replacement_choices(draft, &block, &path))
                .unwrap_or_default();
            ui.horizontal_wrapped(|ui| {
                for (label, preview) in replacements {
                    if ui.add_enabled(editable, Button::new(label)).clicked() {
                        state.pending_conversion = Some(preview);
                    }
                }
            });
        }
    }
}

fn conversion_preview_ui(ui: &mut Ui, pending: &mut PendingConversion) {
    let (preserved, required, removed, reason) = match &pending.preview {
        ConversionPreview::Compatible {
            preserved_fields,
            required_fields,
            removed_fields,
        } => (
            preserved_fields.join(", "),
            required_fields.join(", "),
            removed_fields.join(", "),
            None,
        ),
        ConversionPreview::ReplaceRequired { reason, .. } => {
            (String::new(), String::new(), String::new(), Some(*reason))
        }
    };
    if let Some(reason) = reason {
        ui.label(RichText::new(reason).color(Color32::from_rgb(224, 112, 75)));
    }
    if !preserved.is_empty() {
        ui.label(format!("Preserved: {preserved}"));
    }
    if !required.is_empty() {
        ui.label(format!("Required: {required}"));
    }
    if !removed.is_empty() {
        ui.label(format!("Removed (undo only): {removed}"));
    }
    for (field, value) in &pending.required_values {
        ui.label(format!("{field}: {value}"));
    }
    match &mut pending.command {
        EditorCommand::ConvertBlock {
            target: ConversionTarget::RepeatN { count },
            ..
        } => {
            ui.horizontal(|ui| {
                ui.label("Count");
                ui.add(egui::DragValue::new(count).clamp_range(0..=u32::MAX));
            });
            pending.required_values = vec![("count".into(), count.to_string())];
        }
        EditorCommand::ConvertBlock {
            target:
                ConversionTarget::RepeatUntil {
                    max_iterations: crate::engine::macro_engine::Limit::Finite(maximum),
                    ..
                },
            ..
        } => {
            ui.horizontal(|ui| {
                ui.label("Max iterations");
                ui.add(egui::DragValue::new(maximum).clamp_range(0..=u64::MAX));
            });
            pending.required_values = vec![("max_iterations".into(), maximum.to_string())];
        }
        EditorCommand::ReplaceBlock { replacement, .. } => match &mut replacement.kind {
            crate::engine::macro_engine::BlockKind::RepeatN { count, .. } => {
                ui.horizontal(|ui| {
                    ui.label("Count");
                    ui.add(egui::DragValue::new(count).clamp_range(0..=u32::MAX));
                });
                pending.required_values = vec![("count".into(), count.to_string())];
            }
            crate::engine::macro_engine::BlockKind::Wait { duration_ms } => {
                ui.horizontal(|ui| {
                    ui.label("Duration ms");
                    ui.add(egui::DragValue::new(duration_ms).clamp_range(0..=u64::MAX));
                });
                pending.required_values = vec![("duration_ms".into(), duration_ms.to_string())];
            }
            crate::engine::macro_engine::BlockKind::Comment { text } => {
                ui.horizontal(|ui| {
                    ui.label("Text");
                    ui.text_edit_singleline(text);
                });
                pending.required_values = vec![("text".into(), text.clone())];
            }
            _ => {}
        },
        _ => {}
    }
    if pending.structural_children {
        let disposition = match &pending.command {
            EditorCommand::ReplaceBlock {
                children: ChildDisposition::KeepOwnedContents,
                ..
            } => "Keep",
            EditorCommand::ReplaceBlock {
                children: ChildDisposition::DeleteOwnedContents,
                ..
            } => "Delete",
            _ => "Not applicable",
        };
        ui.label(format!("Selected child disposition: {disposition}"));
        ui.horizontal_wrapped(|ui| {
            ui.label("Owned children");
            if ui.button("Keep").clicked() {
                if let EditorCommand::ReplaceBlock { children, .. } = &mut pending.command {
                    *children = ChildDisposition::KeepOwnedContents;
                }
            }
            if ui.button("Delete").clicked() {
                if let EditorCommand::ReplaceBlock { children, .. } = &mut pending.command {
                    *children = ChildDisposition::DeleteOwnedContents;
                }
            }
        });
    }
}

fn pending_conversion_valid(draft: &EditorDraft, pending: &PendingConversion) -> bool {
    use crate::engine::macro_engine::{BlockKind, Limit};
    let required_values_valid = match &pending.command {
        EditorCommand::ConvertBlock { target, .. } => match target {
            ConversionTarget::ClickPoint { point_id, .. } => {
                draft.points.iter().any(|point| point.id == *point_id)
            }
            ConversionTarget::ClickRegion { region_id, .. } => {
                draft.regions.iter().any(|region| region.id == *region_id)
            }
            ConversionTarget::RepeatN { count } => *count > 0,
            ConversionTarget::RepeatUntil {
                condition,
                max_iterations,
            } => {
                let max_valid = !matches!(max_iterations, Limit::Finite(0));
                let source_id = match condition {
                    crate::engine::macro_engine::Condition::Text {
                        source_block_id, ..
                    }
                    | crate::engine::macro_engine::Condition::Image {
                        source_block_id, ..
                    } => source_block_id,
                };
                max_valid
                    && locate_block_path(draft, source_id)
                        .and_then(|path| block_at_path(draft, &path))
                        .is_some_and(|block| matches!(block.kind, BlockKind::Observe { .. }))
            }
            _ => true,
        },
        EditorCommand::ReplaceBlock { replacement, .. } => match &replacement.kind {
            BlockKind::Wait { duration_ms } => *duration_ms > 0,
            BlockKind::Comment { text } => !text.trim().is_empty(),
            BlockKind::RepeatN { count, .. } => *count > 0,
            BlockKind::Observe {
                condition: crate::engine::macro_engine::Condition::Text { rule_id, .. },
            } => draft.text_rules.iter().any(|rule| rule.id == *rule_id),
            BlockKind::Observe {
                condition: crate::engine::macro_engine::Condition::Image { rule_id, .. },
            } => draft.image_rules.iter().any(|rule| rule.id == *rule_id),
            BlockKind::Action {
                action:
                    crate::engine::macro_engine::Action::ClickTextMatch {
                        source_block_id, ..
                    },
            } => locate_block_path(draft, source_block_id)
                .and_then(|path| block_at_path(draft, &path))
                .is_some_and(|block| {
                    matches!(
                        block.kind,
                        BlockKind::Observe {
                            condition: crate::engine::macro_engine::Condition::Text { .. }
                        }
                    )
                }),
            BlockKind::Action {
                action:
                    crate::engine::macro_engine::Action::ClickImageMatch {
                        source_block_id, ..
                    },
            } => locate_block_path(draft, source_block_id)
                .and_then(|path| block_at_path(draft, &path))
                .is_some_and(|block| {
                    matches!(
                        block.kind,
                        BlockKind::Observe {
                            condition: crate::engine::macro_engine::Condition::Image { .. }
                        }
                    )
                }),
            _ => true,
        },
        _ => true,
    };
    if !required_values_valid {
        return false;
    }
    let mut candidate = draft.clone();
    apply_editor_command(&mut candidate, pending.command.clone()).is_ok()
}

fn confirm_pending_conversion(state: &mut MacroPageState) -> Result<EditOutcome, EditorError> {
    let pending = state
        .pending_conversion
        .take()
        .ok_or_else(|| EditorError::MissingBlock("no conversion preview".into()))?;
    if !state
        .draft
        .as_ref()
        .is_some_and(|draft| pending_conversion_valid(draft, &pending))
    {
        state.editor_feedback = Some("Conversion has invalid required values.".into());
        state.pending_conversion = Some(pending);
        return Err(EditorError::IncompatibleConversion);
    }
    dispatch_editor_command(state, pending.command)
}

fn cancel_pending_conversion(state: &mut MacroPageState) {
    state.pending_conversion = None;
    state.editor_feedback = Some("Conversion canceled; draft unchanged.".into());
}

fn compatible_conversion_preview(
    block: &crate::engine::macro_engine::Block,
    path: BlockPath,
    target: ConversionTarget,
) -> PendingConversion {
    let family = conversion_target_family(&target);
    PendingConversion {
        block_id: block.id.clone(),
        preview: preview_conversion(block, family),
        required_values: conversion_required_values(&target),
        command: EditorCommand::ConvertBlock { path, target },
        structural_children: false,
    }
}

fn conversion_choices(
    draft: &EditorDraft,
    block: &crate::engine::macro_engine::Block,
    path: &BlockPath,
) -> Vec<(String, PendingConversion)> {
    use crate::engine::macro_engine::{
        Action, BlockKind, Condition, Limit, MouseButton, ObserveMode, TimeoutOutcome,
    };
    let wait_true = |current: &ObserveMode| {
        if matches!(current, ObserveMode::WaitForTrue { .. }) {
            current.clone()
        } else {
            editor::transition_observe_mode(
                current,
                ObserveMode::WaitForTrue {
                    timeout_ms: Limit::Unlimited,
                    timeout_outcome: TimeoutOutcome::Continue,
                },
            )
        }
    };
    let wait_false = |current: &ObserveMode| {
        if matches!(current, ObserveMode::WaitForFalse { .. }) {
            current.clone()
        } else {
            editor::transition_observe_mode(
                current,
                ObserveMode::WaitForFalse {
                    timeout_ms: Limit::Unlimited,
                    timeout_outcome: TimeoutOutcome::Continue,
                },
            )
        }
    };
    let mut targets = Vec::new();
    match &block.kind {
        BlockKind::Observe {
            condition: Condition::Text { mode, .. },
        } => targets.extend([
            (
                "Text: Check now".into(),
                ConversionTarget::TextObservation {
                    mode: ObserveMode::CheckNow,
                },
            ),
            (
                "Text: Wait true".into(),
                ConversionTarget::TextObservation {
                    mode: wait_true(mode),
                },
            ),
            (
                "Text: Wait false".into(),
                ConversionTarget::TextObservation {
                    mode: wait_false(mode),
                },
            ),
        ]),
        BlockKind::Observe {
            condition: Condition::Image { mode, .. },
        } => targets.extend([
            (
                "Image: Check now".into(),
                ConversionTarget::ImageObservation {
                    mode: ObserveMode::CheckNow,
                },
            ),
            (
                "Image: Wait true".into(),
                ConversionTarget::ImageObservation {
                    mode: wait_true(mode),
                },
            ),
            (
                "Image: Wait false".into(),
                ConversionTarget::ImageObservation {
                    mode: wait_false(mode),
                },
            ),
        ]),
        BlockKind::Action {
            action: Action::ClickTextMatch { .. },
        } => targets.extend([
            (
                "Text click: Left".into(),
                ConversionTarget::ClickTextMatch {
                    button: MouseButton::Left,
                },
            ),
            (
                "Text click: Right".into(),
                ConversionTarget::ClickTextMatch {
                    button: MouseButton::Right,
                },
            ),
        ]),
        BlockKind::Action {
            action: Action::ClickImageMatch { .. },
        } => targets.extend([
            (
                "Image click: Left".into(),
                ConversionTarget::ClickImageMatch {
                    button: MouseButton::Left,
                },
            ),
            (
                "Image click: Right".into(),
                ConversionTarget::ClickImageMatch {
                    button: MouseButton::Right,
                },
            ),
        ]),
        BlockKind::Action {
            action: Action::ClickPoint { .. } | Action::ClickRegion { .. },
        } => {
            for button in [MouseButton::Left, MouseButton::Right] {
                targets.extend(draft.points.iter().map(|point| {
                    (
                        format!("Point: {} ({button:?})", point.id),
                        ConversionTarget::ClickPoint {
                            point_id: point.id.clone(),
                            button,
                        },
                    )
                }));
                targets.extend(draft.regions.iter().map(|region| {
                    (
                        format!("Region: {} ({button:?})", region.id),
                        ConversionTarget::ClickRegion {
                            region_id: region.id.clone(),
                            button,
                        },
                    )
                }));
            }
        }
        BlockKind::RepeatN { .. } => {
            for (source_id, condition) in observation_sources(draft) {
                targets.push((
                    format!("Repeat Until: {source_id}"),
                    ConversionTarget::RepeatUntil {
                        condition: condition_with_source(condition, source_id),
                        max_iterations: Limit::Finite(100),
                    },
                ));
            }
        }
        BlockKind::RepeatUntil { .. } => {
            targets.push(("Repeat N".into(), ConversionTarget::RepeatN { count: 2 }))
        }
        _ => {}
    }
    targets
        .into_iter()
        .filter(|(_, target)| conversion_target_changes(block, target))
        .map(|(label, target)| {
            (
                label,
                compatible_conversion_preview(block, path.clone(), target),
            )
        })
        .collect()
}

fn conversion_target_changes(
    block: &crate::engine::macro_engine::Block,
    target: &ConversionTarget,
) -> bool {
    use crate::engine::macro_engine::{Action, BlockKind, Condition};
    match (&block.kind, target) {
        (
            BlockKind::Observe {
                condition: Condition::Text { mode, .. },
            },
            ConversionTarget::TextObservation { mode: target },
        )
        | (
            BlockKind::Observe {
                condition: Condition::Image { mode, .. },
            },
            ConversionTarget::ImageObservation { mode: target },
        ) => mode != target,
        (
            BlockKind::Action {
                action: Action::ClickTextMatch { button, .. },
            },
            ConversionTarget::ClickTextMatch { button: target },
        )
        | (
            BlockKind::Action {
                action: Action::ClickImageMatch { button, .. },
            },
            ConversionTarget::ClickImageMatch { button: target },
        ) => button != target,
        (
            BlockKind::Action {
                action: Action::ClickPoint { point_id, button },
            },
            ConversionTarget::ClickPoint {
                point_id: target_id,
                button: target_button,
            },
        ) => point_id != target_id || button != target_button,
        (
            BlockKind::Action {
                action: Action::ClickRegion { region_id, button },
            },
            ConversionTarget::ClickRegion {
                region_id: target_id,
                button: target_button,
            },
        ) => region_id != target_id || button != target_button,
        _ => true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReplacementKind {
    DetectorText(String),
    DetectorImage(String),
    ActionText(String),
    ActionImage(String),
    Loop,
    Wait,
    Note,
    Stop,
}

fn replace_block_preview(
    block: &crate::engine::macro_engine::Block,
    path: BlockPath,
    replacement_kind: ReplacementKind,
) -> PendingConversion {
    use crate::engine::macro_engine::{
        Action, Block, BlockKind, Condition, MouseButton, ObserveMode,
    };
    let (kind, required_values) = match replacement_kind {
        ReplacementKind::DetectorText(rule_id) => (
            BlockKind::Observe {
                condition: Condition::Text {
                    source_block_id: block.id.clone(),
                    rule_id: rule_id.clone(),
                    mode: ObserveMode::CheckNow,
                },
            },
            vec![("text_rule".into(), rule_id)],
        ),
        ReplacementKind::DetectorImage(rule_id) => (
            BlockKind::Observe {
                condition: Condition::Image {
                    source_block_id: block.id.clone(),
                    rule_id: rule_id.clone(),
                    mode: ObserveMode::CheckNow,
                },
            },
            vec![("image_rule".into(), rule_id)],
        ),
        ReplacementKind::ActionText(source_id) => (
            BlockKind::Action {
                action: Action::ClickTextMatch {
                    source_block_id: source_id.clone(),
                    button: MouseButton::Left,
                },
            },
            vec![("text_source".into(), source_id)],
        ),
        ReplacementKind::ActionImage(source_id) => (
            BlockKind::Action {
                action: Action::ClickImageMatch {
                    source_block_id: source_id.clone(),
                    button: MouseButton::Left,
                },
            },
            vec![("image_source".into(), source_id)],
        ),
        ReplacementKind::Loop => (
            BlockKind::RepeatN {
                count: 2,
                body: vec![],
            },
            vec![("count".into(), "2".into())],
        ),
        ReplacementKind::Wait => (
            BlockKind::Wait { duration_ms: 250 },
            vec![("duration_ms".into(), "250".into())],
        ),
        ReplacementKind::Note => (
            BlockKind::Comment {
                text: "Replaced step".into(),
            },
            vec![("text".into(), "Replaced step".into())],
        ),
        ReplacementKind::Stop => (BlockKind::StopSuccess, vec![]),
    };
    PendingConversion {
        block_id: block.id.clone(),
        preview: ConversionPreview::ReplaceRequired {
            from: block_family_for_ui(block),
            to: BlockFamily::Other,
            reason: "Replacing an unrelated type requires confirmation; removed settings remain available only through undo.",
        },
        required_values,
        command: EditorCommand::ReplaceBlock {
            path,
            replacement: Block {
                id: block.id.clone(),
                enabled: block.enabled,
                kind,
            },
            children: ChildDisposition::KeepOwnedContents,
        },
        structural_children: has_replaceable_children(block),
    }
}

fn has_replaceable_children(block: &crate::engine::macro_engine::Block) -> bool {
    use crate::engine::macro_engine::{BlockKind, Condition, ObserveMode, TimeoutOutcome};
    match &block.kind {
        BlockKind::Observe { condition } => {
            let mode = match condition {
                Condition::Text { mode, .. } | Condition::Image { mode, .. } => mode,
            };
            matches!(
                mode,
                ObserveMode::WaitForTrue {
                    timeout_outcome: TimeoutOutcome::RunBody { .. },
                    ..
                } | ObserveMode::WaitForFalse {
                    timeout_outcome: TimeoutOutcome::RunBody { .. },
                    ..
                }
            )
        }
        BlockKind::If { .. }
        | BlockKind::RepeatN { .. }
        | BlockKind::RepeatUntil { .. }
        | BlockKind::Continuous { .. }
        | BlockKind::WatchGroup { .. } => true,
        _ => false,
    }
}

fn replacement_choices(
    draft: &EditorDraft,
    block: &crate::engine::macro_engine::Block,
    path: &BlockPath,
) -> Vec<(String, PendingConversion)> {
    let mut kinds = Vec::new();
    kinds.extend(draft.text_rules.iter().map(|rule| {
        (
            format!("Detector: Text {}", rule.id),
            ReplacementKind::DetectorText(rule.id.clone()),
        )
    }));
    kinds.extend(draft.image_rules.iter().map(|rule| {
        (
            format!("Detector: Image {}", rule.id),
            ReplacementKind::DetectorImage(rule.id.clone()),
        )
    }));
    kinds.extend(
        observation_sources(draft)
            .into_iter()
            .map(|(id, condition)| match condition {
                crate::engine::macro_engine::Condition::Text { .. } => (
                    format!("Action: Text source {id}"),
                    ReplacementKind::ActionText(id),
                ),
                crate::engine::macro_engine::Condition::Image { .. } => (
                    format!("Action: Image source {id}"),
                    ReplacementKind::ActionImage(id),
                ),
            }),
    );
    kinds.extend(
        [
            ("Replace as Loop", ReplacementKind::Loop),
            ("Replace as Wait", ReplacementKind::Wait),
            ("Replace as Note", ReplacementKind::Note),
            ("Replace as Stop", ReplacementKind::Stop),
        ]
        .into_iter()
        .map(|(label, kind)| (label.into(), kind)),
    );
    kinds
        .into_iter()
        .map(|(label, kind)| (label, replace_block_preview(block, path.clone(), kind)))
        .collect()
}

fn conversion_target_family(target: &ConversionTarget) -> BlockFamily {
    match target {
        ConversionTarget::TextObservation { .. } => BlockFamily::TextObservation,
        ConversionTarget::ImageObservation { .. } => BlockFamily::ImageObservation,
        ConversionTarget::ClickTextMatch { .. } => BlockFamily::TextMatchedClick,
        ConversionTarget::ClickImageMatch { .. } => BlockFamily::ImageMatchedClick,
        ConversionTarget::ClickPoint { .. } | ConversionTarget::ClickRegion { .. } => {
            BlockFamily::SavedLocationClick
        }
        ConversionTarget::RepeatN { .. } | ConversionTarget::RepeatUntil { .. } => {
            BlockFamily::Loop
        }
    }
}

fn conversion_required_values(target: &ConversionTarget) -> Vec<(String, String)> {
    match target {
        ConversionTarget::ClickPoint { point_id, .. } => {
            vec![("point_id".into(), point_id.clone())]
        }
        ConversionTarget::ClickRegion { region_id, .. } => {
            vec![("region_id".into(), region_id.clone())]
        }
        ConversionTarget::RepeatN { count } => vec![("count".into(), count.to_string())],
        ConversionTarget::RepeatUntil { max_iterations, .. } => {
            vec![("max_iterations".into(), format!("{max_iterations:?}"))]
        }
        _ => vec![],
    }
}

fn block_family_for_ui(block: &crate::engine::macro_engine::Block) -> BlockFamily {
    use crate::engine::macro_engine::{Action, BlockKind, Condition};
    match &block.kind {
        BlockKind::Observe {
            condition: Condition::Text { .. },
        } => BlockFamily::TextObservation,
        BlockKind::Observe {
            condition: Condition::Image { .. },
        } => BlockFamily::ImageObservation,
        BlockKind::Action {
            action: Action::ClickTextMatch { .. },
        } => BlockFamily::TextMatchedClick,
        BlockKind::Action {
            action: Action::ClickImageMatch { .. },
        } => BlockFamily::ImageMatchedClick,
        BlockKind::Action {
            action: Action::ClickPoint { .. } | Action::ClickRegion { .. },
        } => BlockFamily::SavedLocationClick,
        BlockKind::RepeatN { .. } | BlockKind::RepeatUntil { .. } => BlockFamily::Loop,
        _ => BlockFamily::Other,
    }
}

fn next_unique_id(draft: &EditorDraft, prefix: &str) -> String {
    for ordinal in 1_u64.. {
        let id = format!("{prefix}-{ordinal}");
        if locate_block_path(draft, &id).is_none() && locate_watch_lane(draft, &id).is_none() {
            return id;
        }
    }
    unreachable!()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaletteKind {
    Observe,
    Action,
    If,
    Repeat,
    Watch,
    Wait,
    Stop,
}

#[cfg(test)]
fn palette_command(
    draft: &EditorDraft,
    selected_id: Option<&str>,
    kind: PaletteKind,
) -> Result<EditorCommand, String> {
    let selection = selected_id.map(|id| TimelineSelection::Identity(id.to_string()));
    palette_command_for_selection(draft, selection.as_ref(), kind)
}

fn palette_command_for_selection(
    draft: &EditorDraft,
    selected: Option<&TimelineSelection>,
    kind: PaletteKind,
) -> Result<EditorCommand, String> {
    use crate::engine::macro_engine::{
        Action, Block, BlockKind, Condition, Limit, MouseButton, ObserveMode, PassiveCondition,
        TimeoutOutcome, WatchGroup, WatchLane,
    };

    let target = insertion_target(draft, selected);
    let selected_id = selected.and_then(|selection| match selection {
        TimelineSelection::Identity(id) => Some(id.as_str()),
        TimelineSelection::TimeoutBody { .. } => None,
    });
    let id = next_unique_id(
        draft,
        match kind {
            PaletteKind::Observe => "observe",
            PaletteKind::Action => "action",
            PaletteKind::If => "if",
            PaletteKind::Repeat => "repeat",
            PaletteKind::Watch => "watch",
            PaletteKind::Wait => "wait",
            PaletteKind::Stop => "stop",
        },
    );
    let source = observation_source(draft, selected_id);
    let kind = match kind {
        PaletteKind::Observe => {
            if let Some(rule) = draft.text_rules.first() {
                BlockKind::Observe {
                    condition: Condition::Text {
                        source_block_id: id.clone(),
                        rule_id: rule.id.clone(),
                        mode: ObserveMode::CheckNow,
                    },
                }
            } else if let Some(rule) = draft.image_rules.first() {
                BlockKind::Observe {
                    condition: Condition::Image {
                        source_block_id: id.clone(),
                        rule_id: rule.id.clone(),
                        mode: ObserveMode::CheckNow,
                    },
                }
            } else {
                return Err("Add a text or image rule before inserting an observation.".into());
            }
        }
        PaletteKind::Action => match source {
            Some((source_id, Condition::Text { .. })) => BlockKind::Action {
                action: Action::ClickTextMatch {
                    source_block_id: source_id,
                    button: MouseButton::Left,
                },
            },
            Some((source_id, Condition::Image { .. })) => BlockKind::Action {
                action: Action::ClickImageMatch {
                    source_block_id: source_id,
                    button: MouseButton::Left,
                },
            },
            None => return Err("Add or select an observation before inserting an action.".into()),
        },
        PaletteKind::If => {
            let Some((source_id, condition)) = source else {
                return Err("Add or select an observation before inserting an IF.".into());
            };
            BlockKind::If {
                condition: condition_with_source(condition, source_id),
                then_body: vec![],
                else_body: vec![],
            }
        }
        PaletteKind::Repeat => BlockKind::RepeatN {
            count: 2,
            body: vec![],
        },
        PaletteKind::Watch => {
            let Some((source_id, condition)) = source else {
                return Err("Add or select an observation before inserting a Watch Group.".into());
            };
            let lane_id = next_unique_id(draft, "lane");
            let passive = match condition {
                Condition::Text { rule_id, .. } => PassiveCondition::Text {
                    source_block_id: source_id,
                    rule_id,
                },
                Condition::Image { rule_id, .. } => PassiveCondition::Image {
                    source_block_id: source_id,
                    rule_id,
                },
            };
            BlockKind::WatchGroup {
                group: WatchGroup {
                    lanes: vec![WatchLane {
                        id: lane_id,
                        enabled: true,
                        condition: passive,
                        then_body: vec![],
                    }],
                    timeout_ms: Limit::Unlimited,
                    timeout_outcome: TimeoutOutcome::Continue,
                    cooldown_ms: 250,
                },
            }
        }
        PaletteKind::Wait => BlockKind::Wait { duration_ms: 250 },
        PaletteKind::Stop => BlockKind::StopSuccess,
    };
    Ok(EditorCommand::InsertBlock {
        target,
        block: Block {
            id,
            enabled: true,
            kind,
        },
    })
}

fn insertion_target(draft: &EditorDraft, selected: Option<&TimelineSelection>) -> InsertionTarget {
    let Some(selected) = selected else {
        return InsertionTarget {
            container: ContainerPath::Root,
            index: draft.blocks.len(),
        };
    };
    if let TimelineSelection::TimeoutBody { owner_id } = selected {
        let container = ContainerPath::TimeoutBody {
            owner_id: owner_id.clone(),
        };
        if let Some(index) = container_len(draft, &container) {
            return InsertionTarget { container, index };
        }
    }
    let TimelineSelection::Identity(selected_id) = selected else {
        return InsertionTarget {
            container: ContainerPath::Root,
            index: draft.blocks.len(),
        };
    };
    if let Some((watch_id, _, _, _)) = locate_watch_lane(draft, selected_id) {
        let container = ContainerPath::WatchLaneBody {
            watch_id,
            lane_id: selected_id.into(),
        };
        let index = container_len(draft, &container).unwrap_or(0);
        return InsertionTarget { container, index };
    }
    let Some(path) = locate_block_path(draft, selected_id) else {
        return InsertionTarget {
            container: ContainerPath::Root,
            index: draft.blocks.len(),
        };
    };
    let child_container =
        block_at_path(draft, &path).and_then(|block| match &block.kind {
            crate::engine::macro_engine::BlockKind::If { .. } => Some(ContainerPath::IfThen {
                if_id: block.id.clone(),
            }),
            crate::engine::macro_engine::BlockKind::RepeatN { .. }
            | crate::engine::macro_engine::BlockKind::RepeatUntil { .. }
            | crate::engine::macro_engine::BlockKind::Continuous { .. } => {
                Some(ContainerPath::LoopBody {
                    loop_id: block.id.clone(),
                })
            }
            crate::engine::macro_engine::BlockKind::WatchGroup { group } => group
                .lanes
                .first()
                .map(|lane| ContainerPath::WatchLaneBody {
                    watch_id: block.id.clone(),
                    lane_id: lane.id.clone(),
                }),
            _ => None,
        });
    if let Some(container) = child_container {
        let index = container_len(draft, &container).unwrap_or(0);
        InsertionTarget { container, index }
    } else {
        let index = sibling_position(draft, &path)
            .map(|(index, _)| index + 1)
            .unwrap_or(0);
        InsertionTarget {
            container: path.container,
            index,
        }
    }
}

fn observation_source(
    draft: &EditorDraft,
    selected_id: Option<&str>,
) -> Option<(String, crate::engine::macro_engine::Condition)> {
    use crate::engine::macro_engine::BlockKind;
    let selected = selected_id
        .and_then(|id| locate_block_path(draft, id))
        .and_then(|path| block_at_path(draft, &path));
    if let Some(block) = selected {
        if let BlockKind::Observe { condition } = &block.kind {
            return Some((block.id.clone(), condition.clone()));
        }
    }
    fn find(
        blocks: &[crate::engine::macro_engine::Block],
    ) -> Option<(String, crate::engine::macro_engine::Condition)> {
        for block in blocks {
            if let BlockKind::Observe { condition } = &block.kind {
                return Some((block.id.clone(), condition.clone()));
            }
            for child in palette_child_slices(block) {
                if let Some(found) = find(child) {
                    return Some(found);
                }
            }
        }
        None
    }
    find(&draft.blocks)
}

fn observation_sources(
    draft: &EditorDraft,
) -> Vec<(String, crate::engine::macro_engine::Condition)> {
    use crate::engine::macro_engine::BlockKind;
    fn collect(
        blocks: &[crate::engine::macro_engine::Block],
        out: &mut Vec<(String, crate::engine::macro_engine::Condition)>,
    ) {
        for block in blocks {
            if let BlockKind::Observe { condition } = &block.kind {
                out.push((block.id.clone(), condition.clone()));
            }
            for child in palette_child_slices(block) {
                collect(child, out);
            }
        }
    }
    let mut out = Vec::new();
    collect(&draft.blocks, &mut out);
    out
}

fn palette_child_slices(
    block: &crate::engine::macro_engine::Block,
) -> Vec<&[crate::engine::macro_engine::Block]> {
    use crate::engine::macro_engine::BlockKind;
    match &block.kind {
        BlockKind::If {
            then_body,
            else_body,
            ..
        } => vec![then_body, else_body],
        BlockKind::RepeatN { body, .. }
        | BlockKind::RepeatUntil { body, .. }
        | BlockKind::Continuous { body } => vec![body],
        BlockKind::WatchGroup { group } => group
            .lanes
            .iter()
            .map(|lane| lane.then_body.as_slice())
            .collect(),
        _ => vec![],
    }
}

fn condition_with_source(
    condition: crate::engine::macro_engine::Condition,
    source_block_id: String,
) -> crate::engine::macro_engine::Condition {
    use crate::engine::macro_engine::{Condition, ObserveMode};
    match condition {
        Condition::Text { rule_id, .. } => Condition::Text {
            source_block_id,
            rule_id,
            mode: ObserveMode::CheckNow,
        },
        Condition::Image { rule_id, .. } => Condition::Image {
            source_block_id,
            rule_id,
            mode: ObserveMode::CheckNow,
        },
    }
}

fn section(ui: &mut Ui, label: &str, contents: impl FnOnce(&mut Ui)) {
    Frame::none()
        .fill(Color32::from_rgb(17, 19, 21))
        .stroke(Stroke::new(1.0, Color32::from_rgb(43, 47, 50)))
        .rounding(6.0)
        .inner_margin(egui::Margin::same(10.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(label)
                    .monospace()
                    .size(10.0)
                    .strong()
                    .color(Color32::from_rgb(186, 143, 96)),
            );
            ui.add_space(7.0);
            contents(ui);
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::macro_engine::*;

    fn fixture() -> EditorDraft {
        EditorDraft::new(MacroDefinition {
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
            regions: vec![],
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
            blocks: vec![Block {
                id: "observe-1".into(),
                enabled: true,
                kind: BlockKind::Observe {
                    condition: Condition::Text {
                        source_block_id: "observe-1".into(),
                        rule_id: "rule".into(),
                        mode: ObserveMode::CheckNow,
                    },
                },
            }],
            safety: SafetyPolicy {
                max_runtime_ms: Limit::Unlimited,
                max_clicks: Limit::Unlimited,
                max_observation_retries: Limit::Unlimited,
                max_observations_per_second: 20,
                minimum_click_interval_ms: 100,
                focus_loss: FocusLossPolicy::Stop,
            },
        })
    }

    #[test]
    fn empty_macro_page_has_no_runtime_or_input_authority() {
        let state = MacroPageState::default();
        assert!(state.draft.is_none());
        assert!(state.runtime_events.is_empty());
        assert!(state.enabled);
    }

    #[test]
    fn revision_summary_distinguishes_draft_saved_and_running() {
        let mut monitor = MonitorProjection::default();
        monitor.running_revision = Some(3);
        let state = MacroPageState {
            saved_revision: Some(2),
            ..MacroPageState::default()
        };

        assert_eq!(
            revision_summary(&state, &monitor),
            "draft -- | saved 2 | running 3"
        );
    }

    #[test]
    fn palette_covers_core_types_and_inserts_nested_containers() {
        let mut draft = fixture();
        assert_eq!(next_unique_id(&draft, "observe"), "observe-2");
        for kind in [
            PaletteKind::Observe,
            PaletteKind::Action,
            PaletteKind::If,
            PaletteKind::Repeat,
            PaletteKind::Watch,
            PaletteKind::Wait,
            PaletteKind::Stop,
        ] {
            assert!(palette_command(&draft, Some("observe-1"), kind).is_ok());
        }

        let command = palette_command(&draft, Some("observe-1"), PaletteKind::If).unwrap();
        let EditorCommand::InsertBlock { block, .. } = &command else {
            panic!()
        };
        let if_id = block.id.clone();
        apply_editor_command(&mut draft, command).unwrap();
        let command = palette_command(&draft, Some(&if_id), PaletteKind::Repeat).unwrap();
        let EditorCommand::InsertBlock { target, block } = &command else {
            panic!()
        };
        assert_eq!(
            target.container,
            ContainerPath::IfThen {
                if_id: if_id.clone()
            }
        );
        let repeat_id = block.id.clone();
        apply_editor_command(&mut draft, command).unwrap();
        let EditorCommand::InsertBlock { target, .. } =
            palette_command(&draft, Some(&repeat_id), PaletteKind::Wait).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            target.container,
            ContainerPath::LoopBody { loop_id: repeat_id }
        );

        let BlockKind::Observe { condition } = &mut draft.blocks[0].kind else {
            panic!()
        };
        let Condition::Text { mode, .. } = condition else {
            panic!()
        };
        *mode = ObserveMode::WaitForTrue {
            timeout_ms: Limit::Finite(100),
            timeout_outcome: TimeoutOutcome::RunBody { body: vec![] },
        };
        let timeout = TimelineSelection::TimeoutBody {
            owner_id: "observe-1".into(),
        };
        let EditorCommand::InsertBlock { target, .. } =
            palette_command_for_selection(&draft, Some(&timeout), PaletteKind::Wait).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            target.container,
            ContainerPath::TimeoutBody {
                owner_id: "observe-1".into()
            }
        );
    }

    #[test]
    fn real_block_id_with_timeout_suffix_is_not_treated_as_a_timeout_marker() {
        let mut draft = fixture();
        let BlockKind::Observe { condition } = &mut draft.blocks[0].kind else {
            panic!()
        };
        let Condition::Text { mode, .. } = condition else {
            panic!()
        };
        *mode = ObserveMode::WaitForTrue {
            timeout_ms: Limit::Finite(100),
            timeout_outcome: TimeoutOutcome::RunBody { body: vec![] },
        };
        draft.blocks.push(Block {
            id: "observe-1-timeout".into(),
            enabled: true,
            kind: BlockKind::Comment {
                text: "real imported block".into(),
            },
        });

        let EditorCommand::InsertBlock { target, .. } =
            palette_command(&draft, Some("observe-1-timeout"), PaletteKind::Wait).unwrap()
        else {
            panic!()
        };
        assert_eq!(target.container, ContainerPath::Root);
        assert_eq!(target.index, draft.blocks.len());
    }

    #[test]
    fn inspector_intents_share_transactional_dispatch_and_surface_run_lock() {
        let mut state = MacroPageState {
            draft: Some(fixture()),
            ..MacroPageState::default()
        };
        let mut rule = state.draft.as_ref().unwrap().text_rules[0].clone();
        rule.expected = "Retry".into();
        let command = inspector_editor_command(
            state.draft.as_ref(),
            &inspector::InspectorIntent::ReplaceTextRule { rule },
        )
        .unwrap()
        .unwrap();
        dispatch_editor_command(&mut state, command).unwrap();
        assert_eq!(
            state.draft.as_ref().unwrap().text_rules[0].expected,
            "Retry"
        );

        state.draft.as_mut().unwrap().editability = DraftEditability::Running { revision: 2 };
        let path = locate_block_path(state.draft.as_ref().unwrap(), "observe-1").unwrap();
        let result = dispatch_editor_command(
            &mut state,
            EditorCommand::SetConditionMode {
                path,
                mode: ObserveMode::CheckNow,
            },
        );
        assert_eq!(result, Err(EditorError::RunInProgress));
        assert!(
            state
                .editor_feedback
                .as_deref()
                .unwrap()
                .contains("RunInProgress")
        );
    }

    #[test]
    fn inspector_dispatch_persists_required_text_and_image_rule_fields() {
        let mut state = MacroPageState {
            draft: Some(fixture()),
            ..MacroPageState::default()
        };
        let mut text = state.draft.as_ref().unwrap().text_rules[0].clone();
        text.match_mode = TextMatchMode::Absent;
        text.case_sensitive = true;
        text.allow_cross_line = true;
        text.preprocess = PreprocessProfile::HighContrast;
        text.match_policy = MatchSelectionPolicy::Topmost;
        text.timeout_ms = Limit::Finite(7_500);
        text.stable_frames = 4;
        let command = inspector_editor_command(
            state.draft.as_ref(),
            &inspector::InspectorIntent::ReplaceTextRule { rule: text },
        )
        .unwrap()
        .unwrap();
        dispatch_editor_command(&mut state, command).unwrap();
        let text = &state.draft.as_ref().unwrap().text_rules[0];
        assert_eq!(text.match_mode, TextMatchMode::Absent);
        assert!(text.case_sensitive && text.allow_cross_line);
        assert_eq!(text.preprocess, PreprocessProfile::HighContrast);
        assert_eq!(text.match_policy, MatchSelectionPolicy::Topmost);
        assert_eq!(text.timeout_ms, Limit::Finite(7_500));
        assert_eq!(text.stable_frames, 4);

        let template = |id: &str| AssetRef {
            id: id.into(),
            revision: 1,
            content_hash: format!("hash-{id}"),
        };
        state.draft.as_mut().unwrap().image_rules.push(ImageRule {
            id: "image-rule".into(),
            revision: 1,
            region_id: "scan".into(),
            template: template("old"),
            transparent_mask: None,
            threshold: 0.9,
            scales_percent: vec![100],
            stable_frames: 2,
            maximum_center_drift_px: 5,
            minimum_runner_up_margin: 0.05,
            verification: None,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 250,
            timeout_ms: Limit::Unlimited,
        });
        let mut image = state.draft.as_ref().unwrap().image_rules[0].clone();
        image.template = template("new");
        image.match_policy = MatchSelectionPolicy::Bottommost;
        image.timeout_ms = Limit::Finite(9_000);
        let command = inspector_editor_command(
            state.draft.as_ref(),
            &inspector::InspectorIntent::ReplaceImageRule { rule: image },
        )
        .unwrap()
        .unwrap();
        dispatch_editor_command(&mut state, command).unwrap();
        let image = &state.draft.as_ref().unwrap().image_rules[0];
        assert_eq!(image.template.id, "new");
        assert_eq!(image.match_policy, MatchSelectionPolicy::Bottommost);
        assert_eq!(image.timeout_ms, Limit::Finite(9_000));
        assert_eq!(image.verification, None);
    }

    #[test]
    fn conversion_previews_expose_replace_confirmation_and_required_values() {
        let draft = fixture();
        let block = &draft.blocks[0];
        let path = locate_block_path(&draft, &block.id).unwrap();
        let compatible = conversion_choices(&draft, block, &path)
            .into_iter()
            .next()
            .unwrap()
            .1;
        assert!(matches!(
            compatible.preview,
            ConversionPreview::Compatible { .. }
        ));

        let replacement = replace_block_preview(block, path, ReplacementKind::Wait);
        assert!(matches!(
            replacement.preview,
            ConversionPreview::ReplaceRequired { .. }
        ));
        assert_eq!(
            replacement.required_values,
            vec![("duration_ms".into(), "250".into())]
        );
    }

    #[test]
    fn observe_timeout_body_requires_explicit_replacement_disposition() {
        let mut draft = fixture();
        let BlockKind::Observe { condition } = &mut draft.blocks[0].kind else {
            panic!()
        };
        let Condition::Text { mode, .. } = condition else {
            panic!()
        };
        *mode = ObserveMode::WaitForTrue {
            timeout_ms: Limit::Finite(100),
            timeout_outcome: TimeoutOutcome::RunBody {
                body: vec![Block {
                    id: "fallback".into(),
                    enabled: true,
                    kind: BlockKind::Comment {
                        text: "keep".into(),
                    },
                }],
            },
        };
        let block = &draft.blocks[0];
        let path = locate_block_path(&draft, &block.id).unwrap();
        let pending = replace_block_preview(block, path, ReplacementKind::Wait);
        assert!(pending.structural_children);
    }

    #[test]
    fn conversion_choices_cover_saved_targets_buttons_and_replacement_families() {
        use crate::engine::types::{PointRatio, RectRatio};
        let mut draft = fixture();
        draft.points.push(PointDefinition {
            id: "point".into(),
            revision: 1,
            point: PointRatio { x: 0.5, y: 0.5 },
        });
        draft.regions.push(RegionDefinition {
            id: "region".into(),
            revision: 1,
            rect: RectRatio {
                x: 0.1,
                y: 0.1,
                width: 0.2,
                height: 0.2,
            },
        });
        draft.image_rules.push(ImageRule {
            id: "image-rule".into(),
            revision: 1,
            region_id: "region".into(),
            template: AssetRef {
                id: "template".into(),
                revision: 1,
                content_hash: "hash".into(),
            },
            transparent_mask: None,
            threshold: 0.9,
            scales_percent: vec![100],
            stable_frames: 2,
            maximum_center_drift_px: 5,
            minimum_runner_up_margin: 0.05,
            verification: None,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 250,
            timeout_ms: Limit::Unlimited,
        });
        draft.blocks.push(Block {
            id: "observe-image".into(),
            enabled: true,
            kind: BlockKind::Observe {
                condition: Condition::Image {
                    source_block_id: "observe-image".into(),
                    rule_id: "image-rule".into(),
                    mode: ObserveMode::CheckNow,
                },
            },
        });
        draft.blocks.push(Block {
            id: "saved-click".into(),
            enabled: true,
            kind: BlockKind::Action {
                action: Action::ClickPoint {
                    point_id: "point".into(),
                    button: MouseButton::Left,
                },
            },
        });
        let block = draft.blocks.last().unwrap();
        let path = locate_block_path(&draft, &block.id).unwrap();
        let labels = conversion_choices(&draft, block, &path)
            .into_iter()
            .map(|(label, _)| label)
            .collect::<Vec<_>>();
        assert!(!labels.contains(&"Point: point (Left)".into()));
        assert!(labels.contains(&"Point: point (Right)".into()));
        assert!(labels.contains(&"Region: region (Left)".into()));
        assert!(labels.contains(&"Region: region (Right)".into()));

        let replacements = replacement_choices(&draft, block, &path)
            .into_iter()
            .map(|(label, _)| label)
            .collect::<Vec<_>>();
        assert!(replacements.contains(&"Detector: Text rule".into()));
        assert!(replacements.contains(&"Detector: Image image-rule".into()));
        assert!(replacements.contains(&"Action: Text source observe-1".into()));
        assert!(replacements.contains(&"Action: Image source observe-image".into()));
        assert!(replacements.contains(&"Replace as Loop".into()));
    }

    #[test]
    fn observation_conversion_choices_preserve_configured_wait_outcomes() {
        let mut draft = fixture();
        let BlockKind::Observe { condition } = &mut draft.blocks[0].kind else {
            panic!()
        };
        let Condition::Text { mode, .. } = condition else {
            panic!()
        };
        *mode = ObserveMode::WaitForTrue {
            timeout_ms: Limit::Finite(812),
            timeout_outcome: TimeoutOutcome::RunBody {
                body: vec![Block {
                    id: "fallback".into(),
                    enabled: true,
                    kind: BlockKind::Comment {
                        text: "keep".into(),
                    },
                }],
            },
        };
        let block = &draft.blocks[0];
        let path = locate_block_path(&draft, &block.id).unwrap();
        let (_, pending) = conversion_choices(&draft, block, &path)
            .into_iter()
            .find(|(label, _)| label == "Text: Wait false")
            .unwrap();
        assert!(matches!(
            pending.command,
            EditorCommand::ConvertBlock {
                target: ConversionTarget::TextObservation {
                    mode: ObserveMode::WaitForFalse {
                        timeout_ms: Limit::Finite(812),
                        timeout_outcome: TimeoutOutcome::RunBody { ref body },
                    },
                },
                ..
            } if body[0].id == "fallback"
        ));
    }

    #[test]
    fn conversion_confirm_cancel_and_invalid_requirements_are_transactional() {
        let mut state = MacroPageState {
            draft: Some(fixture()),
            ..MacroPageState::default()
        };
        let before = state.draft.as_ref().unwrap().definition.clone();
        let block = state.draft.as_ref().unwrap().blocks[0].clone();
        let path = locate_block_path(state.draft.as_ref().unwrap(), &block.id).unwrap();
        state.pending_conversion = Some(replace_block_preview(
            &block,
            path.clone(),
            ReplacementKind::Wait,
        ));
        cancel_pending_conversion(&mut state);
        assert_eq!(state.draft.as_ref().unwrap().definition, before);
        assert!(state.pending_conversion.is_none());

        state.pending_conversion = Some(replace_block_preview(
            &block,
            path.clone(),
            ReplacementKind::Wait,
        ));
        confirm_pending_conversion(&mut state).unwrap();
        assert!(matches!(
            state.draft.as_ref().unwrap().blocks[0].kind,
            BlockKind::Wait { duration_ms: 250 }
        ));

        let mut state = MacroPageState {
            draft: Some(fixture()),
            ..MacroPageState::default()
        };
        state.pending_conversion = Some(PendingConversion {
            block_id: "observe-1".into(),
            preview: ConversionPreview::Compatible {
                preserved_fields: vec![],
                required_fields: vec!["count"],
                removed_fields: vec![],
            },
            required_values: vec![("count".into(), "0".into())],
            command: EditorCommand::ConvertBlock {
                path,
                target: ConversionTarget::RepeatN { count: 0 },
            },
            structural_children: false,
        });
        assert_eq!(
            confirm_pending_conversion(&mut state),
            Err(EditorError::IncompatibleConversion)
        );
        assert!(state.pending_conversion.is_some());
        assert_eq!(state.draft.as_ref().unwrap().definition, before);
    }

    #[test]
    fn replacement_preview_validates_sources_against_the_post_replacement_candidate() {
        let draft = fixture();
        let block = &draft.blocks[0];
        let path = locate_block_path(&draft, &block.id).unwrap();
        let pending =
            replace_block_preview(block, path, ReplacementKind::ActionText(block.id.clone()));
        assert!(!pending_conversion_valid(&draft, &pending));
    }

    #[test]
    fn starter_draft_opens_a_native_editor_surface() {
        let draft = EditorDraft::new(starter_macro_definition());
        assert_eq!(draft.blocks.len(), 1);
        assert!(locate_block_path(&draft, "observe-1").is_some());
        assert!(!draft.text_rules.is_empty());
    }

    #[test]
    fn validation_status_honors_editor_draft_staleness() {
        let mut state = MacroPageState {
            draft: Some(fixture()),
            ..MacroPageState::default()
        };
        assert_eq!(validation_summary(&state, &[]), "Valid");
        dispatch_editor_command(
            &mut state,
            EditorCommand::InsertBlock {
                target: InsertionTarget {
                    container: ContainerPath::Root,
                    index: 1,
                },
                block: Block {
                    id: "note".into(),
                    enabled: true,
                    kind: BlockKind::Comment {
                        text: "changed".into(),
                    },
                },
            },
        )
        .unwrap();
        assert_eq!(validation_summary(&state, &[]), "Needs revalidation");
        state.draft.as_mut().unwrap().status = DraftStatus::Ready;
        assert_eq!(validation_summary(&state, &[]), "Valid");
    }
}
