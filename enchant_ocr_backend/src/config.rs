use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::types::{PointRatio, Rect, RectRatio};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MouseMovementProfile {
    pub duration_ms: u64,
    pub distance_px: f32,
    #[serde(default)]
    pub model: Option<MouseMovementModel>,
    #[serde(default)]
    pub movement_steps: Vec<MouseMovementStep>,
    #[serde(default)]
    pub samples: Vec<MouseMovementSample>,
}

impl MouseMovementProfile {
    pub fn is_usable(&self) -> bool {
        self.duration_ms > 0
            && self.distance_px >= 1.0
            && (self.model.is_some() || !self.movement_steps.is_empty() || self.samples.len() >= 2)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MouseMovementModel {
    pub point_count: u32,
    pub avg_step_ms: u64,
    pub curve_lateral: f32,
    pub curve_peak_progress: f32,
    pub target_width_px: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MouseMovementStep {
    pub delay_ms: u64,
    pub progress_delta: f32,
    pub lateral_delta: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MouseMovementSample {
    pub at_ms: u64,
    pub progress: f32,
    pub lateral: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnchantConfig {
    pub targets: Vec<String>,
    pub fuzzy_threshold: f64,
    pub max_attempts: u32,
    pub enchant_window: Rect,
    pub ocr_region: RectRatio,
    pub enchant_button: PointRatio,
    pub replace_button: PointRatio,
    pub close_button: PointRatio,
    #[serde(default)]
    pub mouse_movement: Option<MouseMovementProfile>,
    pub wait_after_enchant_ms: u64,
    pub wait_after_replace_ms: u64,
    pub wait_after_close_ms: u64,
}

impl EnchantConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse config {}", path.display()))
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }
        let contents = serde_json::to_string_pretty(self)?;
        fs::write(path, contents)
            .with_context(|| format!("failed to write config {}", path.display()))
    }

    pub fn sample() -> Self {
        Self {
            targets: vec!["Max Health".to_string()],
            fuzzy_threshold: 0.78,
            max_attempts: 0,
            enchant_window: Rect::new(100, 100, 900, 700),
            ocr_region: RectRatio {
                x: 0.35,
                y: 0.25,
                width: 0.30,
                height: 0.12,
            },
            enchant_button: PointRatio { x: 0.50, y: 0.86 },
            replace_button: PointRatio { x: 0.42, y: 0.72 },
            close_button: PointRatio { x: 0.55, y: 0.72 },
            mouse_movement: Some(default_mouse_movement_profile()),
            wait_after_enchant_ms: 1_000,
            wait_after_replace_ms: 350,
            wait_after_close_ms: 350,
        }
    }
}

pub fn default_mouse_movement_profile() -> MouseMovementProfile {
    MouseMovementProfile {
        duration_ms: 74,
        distance_px: 61.269894,
        model: Some(MouseMovementModel {
            point_count: 10,
            avg_step_ms: 8,
            curve_lateral: -0.07725096,
            curve_peak_progress: 0.5833777,
            target_width_px: 48.0,
        }),
        movement_steps: Vec::new(),
        samples: Vec::new(),
    }
}
