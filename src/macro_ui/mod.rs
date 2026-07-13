mod library;
mod monitor;
mod timeline;

use eframe::egui::{self, Align, Button, Color32, Frame, Layout, RichText, Stroke, Ui};

use crate::engine::macro_engine::{MacroDefinition, RunEvent, ValidationProblem, validate_macro};

use library::{MacroLibraryRow, project_definition};
use monitor::{MonitorProjection, project_monitor};
use timeline::{TimelineRow, project_timeline};

/// Read-only data accepted by the Macro page. It intentionally owns no runtime command sender,
/// capture service, mouse controller, or platform input handle.
#[derive(Debug)]
pub struct MacroPageState {
    pub definition: Option<MacroDefinition>,
    pub saved_revision: Option<u64>,
    pub enabled: bool,
    pub runtime_events: Vec<RunEvent>,
}

impl Default for MacroPageState {
    fn default() -> Self {
        Self {
            definition: None,
            saved_revision: None,
            enabled: true,
            runtime_events: Vec::new(),
        }
    }
}

#[derive(Debug, Default)]
pub struct MacroPage;

impl MacroPage {
    pub fn show(ui: &mut Ui, state: &MacroPageState) {
        let monitor = project_monitor(state.definition.as_ref(), &state.runtime_events);
        let problems = state
            .definition
            .as_ref()
            .map(validate_macro)
            .unwrap_or_default();
        let library_rows = state
            .definition
            .as_ref()
            .map(|definition| {
                vec![project_definition(
                    definition,
                    state.saved_revision,
                    state.enabled,
                    &problems,
                    &monitor,
                )]
            })
            .unwrap_or_default();
        let timeline_rows = state
            .definition
            .as_ref()
            .map(|definition| project_timeline(&definition.blocks))
            .unwrap_or_default();

        ui.set_width(ui.available_width());
        ui.vertical(|ui| {
            title(ui);
            ui.add_space(8.0);
            status_strip(ui, state, &monitor, &problems);
            ui.add_space(8.0);
            workspace(ui, state, &library_rows, &timeline_rows, &monitor);
            ui.add_space(8.0);
            monitor::show(ui, &monitor);
        });
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
                            "Observe, arbitrate, then act — one immutable revision at a time",
                        )
                        .size(12.0)
                        .color(Color32::from_gray(138)),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new("READ-ONLY SHELL")
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
    state: &MacroPageState,
    monitor: &MonitorProjection,
    problems: &[ValidationProblem],
) {
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
                        .definition
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
                        .definition
                        .as_ref()
                        .map(|definition| {
                            format!(
                                "{}×{} · {} DPI",
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
                status_fact(
                    ui,
                    "VALIDATION",
                    if state.definition.is_none() {
                        "No definition"
                    } else if problems.is_empty() {
                        "Valid"
                    } else {
                        "Needs revalidation"
                    },
                );
            });
            ui.add_space(9.0);
            ui.horizontal_wrapped(|ui| {
                for label in ["Validate", "Dry Run", "Run Once", "Run", "Pause", "Stop"] {
                    ui.add_enabled(false, Button::new(label));
                }
                ui.label(
                    RichText::new("Controls unlock only after editor/runtime wiring")
                        .size(10.0)
                        .color(Color32::from_gray(107)),
                );
            });
        });
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
        .definition
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
    format!("draft {draft} · saved {saved} · running {running}")
}

fn workspace(
    ui: &mut Ui,
    state: &MacroPageState,
    library_rows: &[MacroLibraryRow],
    timeline_rows: &[TimelineRow],
    monitor: &MonitorProjection,
) {
    if ui.available_width() >= 900.0 {
        ui.columns(3, |columns| {
            section(&mut columns[0], "LIBRARY", |ui| {
                library::show(
                    ui,
                    library_rows,
                    state
                        .definition
                        .as_ref()
                        .map(|definition| definition.id.as_str()),
                );
            });
            section(&mut columns[1], "EVENT TIMELINE", |ui| {
                timeline::show(ui, timeline_rows, monitor.active_block.as_deref());
            });
            section(&mut columns[2], "INSPECTOR", inspector_empty);
        });
    } else {
        section(ui, "LIBRARY", |ui| {
            library::show(
                ui,
                library_rows,
                state
                    .definition
                    .as_ref()
                    .map(|definition| definition.id.as_str()),
            );
        });
        ui.add_space(8.0);
        section(ui, "EVENT TIMELINE", |ui| {
            timeline::show(ui, timeline_rows, monitor.active_block.as_deref());
        });
        ui.add_space(8.0);
        section(ui, "INSPECTOR", inspector_empty);
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

fn inspector_empty(ui: &mut Ui) {
    ui.label(
        RichText::new("No block selected")
            .strong()
            .color(Color32::from_gray(204)),
    );
    ui.label(
        RichText::new("Select a timeline block to inspect settings and validation results.")
            .size(11.0)
            .color(Color32::from_gray(119)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_macro_page_has_no_runtime_or_input_authority() {
        let state = MacroPageState::default();
        assert!(state.definition.is_none());
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
            "draft -- · saved 2 · running 3"
        );
    }
}
