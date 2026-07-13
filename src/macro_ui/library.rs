use eframe::egui::{self, Color32, Frame, RichText, Stroke, Ui};

use crate::engine::macro_engine::{MacroDefinition, RunStatus, ValidationProblem};

use super::monitor::MonitorProjection;

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
            Self::NeedsRevalidation => "Needs Revalidation",
            Self::Running => "Running",
            Self::StoppedWithError => "Stopped with Error",
            Self::Disabled => "Disabled",
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
) -> MacroLibraryRow {
    let status = if !enabled {
        MacroLibraryStatus::Disabled
    } else if matches!(
        monitor.status,
        RunStatus::Running | RunStatus::Paused | RunStatus::Stopping
    ) && monitor.running_revision.is_some()
    {
        MacroLibraryStatus::Running
    } else if monitor.error.is_some()
        || monitor.stop_reason.as_deref().is_some_and(|reason| {
            reason.contains("failure") || reason.contains("error") || reason.contains("Safety")
        })
    {
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
        last_run: monitor
            .stop_reason
            .clone()
            .unwrap_or_else(|| "No completed run".to_string()),
    }
}

pub fn show(ui: &mut Ui, rows: &[MacroLibraryRow], selected_id: Option<&str>) {
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
                        .size(11.0)
                        .color(Color32::from_gray(120)),
                );
            });
        return;
    }

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
                ui.label(
                    RichText::new(&row.name)
                        .strong()
                        .color(Color32::from_gray(218)),
                );
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(row.status.label())
                            .monospace()
                            .size(10.0)
                            .color(status_color(row.status)),
                    );
                    ui.label(
                        RichText::new(format!("REV {}", row.revision))
                            .monospace()
                            .size(10.0)
                            .color(Color32::from_gray(108)),
                    );
                });
                ui.label(
                    RichText::new(format!("{} · {} DPI", row.target, row.dpi))
                        .size(11.0)
                        .color(Color32::from_gray(135)),
                );
                ui.label(
                    RichText::new(format!("{} · {}", row.last_validation, row.last_run))
                        .size(10.0)
                        .color(Color32::from_gray(105)),
                );
            });
        ui.add_space(6.0);
    }
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
