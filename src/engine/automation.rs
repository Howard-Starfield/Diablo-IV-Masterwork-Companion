use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{
    config::MouseMovementProfile,
    types::{Point, Rect, ScreenImage},
};

pub trait CaptureSource {
    fn capture(&self, rect: Rect) -> Result<ScreenImage>;
}

pub trait InputSink {
    fn move_and_click(
        &self,
        point: Point,
        button: MouseButton,
        movement: Option<&MouseMovementProfile>,
        stop: Option<&dyn StopSource>,
    ) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
}

pub trait StopSource {
    fn is_stopped(&self) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSnapshot {
    pub window_id: u64,
    pub process_id: u32,
    pub process_started_at_ms: u64,
    pub process_path: String,
    pub client_rect: Rect,
    pub dpi: u32,
    pub display_profile: String,
    pub is_visible: bool,
    pub is_minimized: bool,
    pub is_foreground: bool,
}

impl Default for TargetSnapshot {
    fn default() -> Self {
        Self {
            window_id: 0,
            process_id: 0,
            process_started_at_ms: 0,
            process_path: String::new(),
            client_rect: Rect::new(0, 0, 0, 0),
            dpi: 0,
            display_profile: String::new(),
            is_visible: false,
            is_minimized: false,
            is_foreground: false,
        }
    }
}

pub trait TargetGuard {
    fn snapshot(&self) -> Result<TargetSnapshot>;
    fn validate(&self, expected: &TargetSnapshot) -> Result<()>;
}

pub trait Clock {
    fn now_ms(&self) -> u64;
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SystemClock {
    started: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::*;

    struct FakeClock(u64);

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0
        }
    }

    struct FakeTargetGuard(TargetSnapshot);

    impl TargetGuard for FakeTargetGuard {
        fn snapshot(&self) -> Result<TargetSnapshot> {
            Ok(self.0.clone())
        }

        fn validate(&self, expected: &TargetSnapshot) -> Result<()> {
            anyhow::ensure!(&self.0 == expected, "target changed");
            Ok(())
        }
    }

    #[test]
    fn clock_and_target_contracts_are_mockable() {
        let snapshot = TargetSnapshot::default();
        let target = FakeTargetGuard(snapshot.clone());

        assert_eq!(FakeClock(42).now_ms(), 42);
        assert_eq!(target.snapshot().unwrap(), snapshot);
        target.validate(&snapshot).unwrap();
    }

    #[test]
    fn mouse_buttons_keep_stable_serialized_names() {
        assert_eq!(
            serde_json::to_string(&MouseButton::Left).unwrap(),
            r#""left""#
        );
        assert_eq!(
            serde_json::to_string(&MouseButton::Right).unwrap(),
            r#""right""#
        );
    }
}
