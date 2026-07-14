use eframe::egui::{self, Color32, Frame, RichText, Stroke, Ui};

use crate::engine::macro_engine::{MacroDefinition, RunStatus, ValidationProblem};
use crate::ui_theme::text;

use super::MacroIntent;
use super::monitor::{MonitorProjection, StopOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroLibraryStatus {
    Draft,
    Ready,
    NeedsRevalidation,
    Running,
    StoppedWithError,
    Disabled,
}

impl MacroLibraryStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Draft => "Draft",
            Self::Ready => "Ready",
            Self::NeedsRevalidation => "Needs revalidation",
            Self::Running => "Running",
            Self::StoppedWithError => "Last run failed",
            Self::Disabled => "Disabled",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Draft => "This definition has unsaved edits or has not been saved yet.",
            Self::Ready => "The saved revision is valid and enabled.",
            Self::NeedsRevalidation => "Changes require validation before this draft can be saved.",
            Self::Running => "This saved macro currently owns the active run.",
            Self::StoppedWithError => "The most recent completed run stopped with an error.",
            Self::Disabled => "This macro is saved but cannot be started until it is enabled.",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroLibraryRow {
    pub id: String,
    pub name: String,
    pub revision: u64,
    pub status: MacroLibraryStatus,
    pub target: String,
    pub dpi: u32,
    pub last_validation: String,
    pub last_run: String,
}

pub fn project_definition(
    definition: &MacroDefinition,
    saved_revision: Option<u64>,
    enabled: bool,
    problems: &[ValidationProblem],
    monitor: &MonitorProjection,
    last_completion: Option<&StopOutcome>,
) -> MacroLibraryRow {
    let status = if !enabled {
        MacroLibraryStatus::Disabled
    } else if matches!(
        monitor.status,
        RunStatus::Running | RunStatus::Paused | RunStatus::Stopping
    ) && monitor.running_revision.is_some()
    {
        MacroLibraryStatus::Running
    } else if monitor.error.is_some() || last_completion.is_some_and(StopOutcome::is_error) {
        MacroLibraryStatus::StoppedWithError
    } else if !problems.is_empty() {
        MacroLibraryStatus::NeedsRevalidation
    } else if saved_revision != Some(definition.revision) {
        MacroLibraryStatus::Draft
    } else {
        MacroLibraryStatus::Ready
    };
    MacroLibraryRow {
        id: definition.id.clone(),
        name: definition.name.clone(),
        revision: definition.revision,
        status,
        target: if definition.target.title_contains.trim().is_empty() {
            definition.target.window_class.clone()
        } else {
            definition.target.title_contains.clone()
        },
        dpi: definition.target.captured_dpi,
        last_validation: if problems.is_empty() {
            "Valid".to_string()
        } else {
            format!("{} issues", problems.len())
        },
        last_run: last_completion
            .map(StopOutcome::label)
            .unwrap_or_else(|| "No completed run".to_string()),
    }
}

pub fn show(
    ui: &mut Ui,
    rows: &[MacroLibraryRow],
    selected_id: Option<&str>,
) -> Option<MacroIntent> {
    if rows.is_empty() {
        Frame::none()
            .fill(Color32::from_rgb(14, 16, 18))
            .stroke(Stroke::new(1.0, Color32::from_rgb(47, 49, 52)))
            .rounding(6.0)
            .inner_margin(egui::Margin::same(12.0))
            .show(ui, |ui| {
                ui.label(
                    RichText::new("No macros yet")
                        .strong()
                        .color(Color32::from_gray(205)),
                );
                ui.label(
                    RichText::new("The guided creator arrives in the next phase.")
                        .size(text::SUPPORTING)
                        .color(Color32::from_gray(120)),
                );
            });
        return None;
    }

    let mut intent = None;
    for row in rows {
        let selected = selected_id == Some(row.id.as_str());
        Frame::none()
            .fill(if selected {
                Color32::from_rgb(43, 31, 23)
            } else {
                Color32::from_rgb(19, 22, 25)
            })
            .stroke(Stroke::new(
                1.0,
                if selected {
                    Color32::from_rgb(174, 91, 43)
                } else {
                    Color32::from_rgb(46, 50, 54)
                },
            ))
            .rounding(5.0)
            .inner_margin(egui::Margin::same(10.0))
            .show(ui, |ui| {
                if ui
                    .add(egui::Button::new(
                        RichText::new(&row.name)
                            .strong()
                            .color(Color32::from_gray(218)),
                    ))
                    .clicked()
                {
                    intent = Some(MacroIntent::Select {
                        macro_id: row.id.clone(),
                    });
                }
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(row.status.label())
                            .monospace()
                            .size(text::META)
                            .color(status_color(row.status)),
                    )
                    .on_hover_text(row.status.description());
                    ui.label(
                        RichText::new(format!("REV {}", row.revision))
                            .monospace()
                            .size(text::META)
                            .color(Color32::from_gray(165)),
                    );
                });
                ui.label(
                    RichText::new(format!("{} · {} DPI", row.target, row.dpi))
                        .size(text::META)
                        .color(Color32::from_gray(174)),
                );
                ui.label(
                    RichText::new(format!("{} · {}", row.last_validation, row.last_run))
                        .size(text::META)
                        .color(Color32::from_gray(164)),
                );
            });
        ui.add_space(6.0);
    }
    intent
}

fn status_color(status: MacroLibraryStatus) -> Color32 {
    match status {
        MacroLibraryStatus::Ready | MacroLibraryStatus::Running => Color32::from_rgb(104, 201, 126),
        MacroLibraryStatus::Draft => Color32::from_rgb(221, 161, 82),
        MacroLibraryStatus::NeedsRevalidation | MacroLibraryStatus::StoppedWithError => {
            Color32::from_rgb(226, 105, 77)
        }
        MacroLibraryStatus::Disabled => Color32::from_gray(105),
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::macro_engine::{
        FocusLossPolicy, Limit, MACRO_SCHEMA_VERSION, SafetyPolicy, StopReason, TargetProfile,
    };

    use super::*;
    use crate::macro_ui::monitor::{StopClassification, StopOutcome};

    fn definition() -> MacroDefinition {
        MacroDefinition {
            schema_version: MACRO_SCHEMA_VERSION,
            id: "alpha".to_string(),
            name: "Alpha".to_string(),
            revision: 2,
            target: TargetProfile {
                process_path: "Diablo IV.exe".to_string(),
                window_class: "Diablo".to_string(),
                title_contains: "Diablo IV".to_string(),
                captured_client_width: 1920,
                captured_client_height: 1080,
                captured_dpi: 96,
            },
            regions: Vec::new(),
            points: Vec::new(),
            text_rules: Vec::new(),
            image_rules: Vec::new(),
            blocks: Vec::new(),
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

    #[test]
    fn current_running_status_keeps_previous_completed_result() {
        let mut monitor = MonitorProjection::default();
        monitor.status = RunStatus::Running;
        monitor.running_revision = Some(2);
        let completion = StopOutcome {
            reason: StopReason::Completed,
            classification: StopClassification::Success,
        };

        let row = project_definition(
            &definition(),
            Some(2),
            true,
            &[],
            &monitor,
            Some(&completion),
        );

        assert_eq!(row.status, MacroLibraryStatus::Running);
        assert_eq!(row.last_run, "Macro completed");
    }

    #[test]
    fn typed_unsupported_block_outcome_is_an_error_without_string_matching() {
        let completion = StopOutcome {
            reason: StopReason::UnsupportedBlock {
                block_id: "future".to_string(),
            },
            classification: StopClassification::Error,
        };

        let row = project_definition(
            &definition(),
            Some(2),
            true,
            &[],
            &MonitorProjection::default(),
            Some(&completion),
        );

        assert_eq!(row.status, MacroLibraryStatus::StoppedWithError);
        assert_eq!(row.last_run, "Unsupported block: future");
    }

    #[test]
    fn lifecycle_badges_keep_revalidation_and_prior_failure_distinct() {
        assert_ne!(
            MacroLibraryStatus::NeedsRevalidation.label(),
            MacroLibraryStatus::StoppedWithError.label()
        );
        assert!(!MacroLibraryStatus::Disabled.description().is_empty());
    }
}
