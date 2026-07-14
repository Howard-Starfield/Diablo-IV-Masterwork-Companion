use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::PathBuf,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

pub const UI_STATE_SCHEMA_VERSION: u32 = 1;
pub const MAX_UI_STATE_BYTES: u64 = 1_048_576;
pub const MAX_LAYOUT_MACROS: usize = 128;
pub const MAX_LAYOUT_NODES: usize = 2_048;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppUiState {
    pub schema_version: u32,
    #[serde(default)]
    pub always_on_top: bool,
    #[serde(default)]
    pub macro_layouts: BTreeMap<String, MacroCanvasLayout>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MacroCanvasLayout {
    pub node_positions: BTreeMap<String, [f32; 2]>,
    pub pan: [f32; 2],
    pub zoom: f32,
    pub library_width: f32,
    pub inspector_width: f32,
}

impl Default for AppUiState {
    fn default() -> Self {
        Self {
            schema_version: UI_STATE_SCHEMA_VERSION,
            always_on_top: false,
            macro_layouts: BTreeMap::new(),
        }
    }
}

impl Default for MacroCanvasLayout {
    fn default() -> Self {
        Self {
            node_positions: BTreeMap::new(),
            pan: [0.0, 0.0],
            zoom: 1.0,
            library_width: 220.0,
            inspector_width: 320.0,
        }
    }
}

impl MacroCanvasLayout {
    pub fn is_finite(&self) -> bool {
        self.pan.iter().all(|value| value.is_finite())
            && self.zoom.is_finite()
            && self
                .node_positions
                .values()
                .flatten()
                .all(|value| value.is_finite())
    }

    fn sanitize(&mut self) -> bool {
        let mut changed = false;
        if !self.pan.iter().all(|value| value.is_finite()) {
            self.pan = [0.0, 0.0];
            changed = true;
        }
        let zoom = if self.zoom.is_finite() {
            self.zoom.clamp(0.5, 1.75)
        } else {
            1.0
        };
        if self.zoom != zoom {
            self.zoom = zoom;
            changed = true;
        }
        if !self.library_width.is_finite() {
            self.library_width = Self::default().library_width;
            changed = true;
        }
        if !self.inspector_width.is_finite() {
            self.inspector_width = Self::default().inspector_width;
            changed = true;
        }
        let before = self.node_positions.len();
        self.node_positions
            .retain(|_, position| position.iter().all(|value| value.is_finite()));
        if self.node_positions.len() != before {
            changed = true;
        }
        if self.node_positions.len() > MAX_LAYOUT_NODES {
            self.node_positions = self
                .node_positions
                .iter()
                .take(MAX_LAYOUT_NODES)
                .map(|(id, position)| (id.clone(), *position))
                .collect();
            changed = true;
        }
        changed
    }
}

pub struct UiStateStore {
    path: PathBuf,
    pub state: AppUiState,
    dirty: bool,
}

impl UiStateStore {
    pub fn open(path: PathBuf) -> (Self, Option<String>) {
        let mut warning = None;
        let mut dirty = false;
        let state = match read_ui_state(&path) {
            Ok(None) => AppUiState::default(),
            Ok(Some(mut state)) if state.schema_version == UI_STATE_SCHEMA_VERSION => {
                if sanitize_state(&mut state) {
                    dirty = true;
                    warning = Some("Recovered invalid UI layout values.".to_string());
                }
                state
            }
            Ok(Some(_)) => {
                dirty = true;
                warning = Some("UI state version is unsupported; using defaults.".to_string());
                AppUiState::default()
            }
            Err(error) => {
                dirty = true;
                warning = Some(format!("Could not load UI state; using defaults: {error}"));
                AppUiState::default()
            }
        };
        (Self { path, state, dirty }, warning)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Returns the selected macro's non-executable presentation state, creating only a bounded
    /// default entry when the caller is already editing that macro.
    pub fn macro_layout_mut(&mut self, macro_id: &str) -> &mut MacroCanvasLayout {
        self.state
            .macro_layouts
            .entry(macro_id.to_owned())
            .or_default()
    }

    pub fn save_if_dirty(&mut self) -> anyhow::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let parent = self
            .path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create UI state directory {}", parent.display()))?;
        let bytes = serde_json::to_vec_pretty(&self.state)?;
        let mut temp = NamedTempFile::new_in(parent).with_context(|| {
            format!(
                "failed to create UI state temp file in {}",
                parent.display()
            )
        })?;
        temp.write_all(&bytes)?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path)
            .map_err(|error| error.error)
            .with_context(|| format!("failed to replace UI state {}", self.path.display()))?;
        self.dirty = false;
        Ok(())
    }
}

fn read_ui_state(path: &PathBuf) -> anyhow::Result<Option<AppUiState>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_UI_STATE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_UI_STATE_BYTES {
        anyhow::bail!("UI state exceeds the 1 MiB limit");
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn sanitize_state(state: &mut AppUiState) -> bool {
    let mut changed = false;
    if state.macro_layouts.len() > MAX_LAYOUT_MACROS {
        state.macro_layouts = state
            .macro_layouts
            .iter()
            .take(MAX_LAYOUT_MACROS)
            .map(|(id, layout)| (id.clone(), layout.clone()))
            .collect();
        changed = true;
    }
    state
        .macro_layouts
        .values_mut()
        .fold(changed, |changed, layout| layout.sanitize() || changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_state_defaults_always_on_top_off() {
        assert!(!AppUiState::default().always_on_top);
    }

    #[test]
    fn corrupt_ui_state_recovers_without_macro_data() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("ui-state.json"), b"not json").unwrap();
        let (store, warning) = UiStateStore::open(temp.path().join("ui-state.json"));
        assert_eq!(store.state, AppUiState::default());
        assert!(warning.is_some());
    }

    #[test]
    fn invalid_layout_values_are_sanitized_before_use() {
        let mut state = AppUiState {
            macro_layouts: BTreeMap::from([(
                "macro".to_string(),
                MacroCanvasLayout {
                    pan: [f32::NAN, 0.0],
                    zoom: 9.0,
                    node_positions: BTreeMap::from([
                        ("valid".to_string(), [1.0, 2.0]),
                        ("invalid".to_string(), [f32::INFINITY, 2.0]),
                    ]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        assert!(sanitize_state(&mut state));
        let layout = &state.macro_layouts["macro"];
        assert_eq!(layout.pan, [0.0, 0.0]);
        assert_eq!(layout.zoom, 1.75);
        assert_eq!(layout.node_positions.len(), 1);
        assert!(layout.is_finite());
    }

    #[test]
    fn dirty_state_round_trips_to_its_dedicated_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ui-state.json");
        let (mut store, _) = UiStateStore::open(path.clone());
        store.state.always_on_top = true;
        store.mark_dirty();
        store.save_if_dirty().unwrap();

        let (reopened, warning) = UiStateStore::open(path);
        assert!(warning.is_none());
        assert!(reopened.state.always_on_top);
    }
}
