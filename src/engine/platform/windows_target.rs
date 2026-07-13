use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::engine::automation::{TargetGuard, TargetSnapshot};

use super::windows_snapshot::{
    CanonicalWindowIdentity, Win32WindowsSnapshotSource, WindowsSnapshotSource, window_class,
    window_title,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableTargetHints {
    pub process_path: String,
    pub window_class: String,
    pub title_contains: String,
}

/// Guard for one concrete HWND selected for the current run.
///
/// Only `DurableTargetHints` belongs in a saved macro. The raw HWND is deliberately private and
/// remains run-local because Windows may reuse it after a target restart.
#[derive(Debug, Clone)]
pub struct WindowsTargetGuard {
    identity: CanonicalWindowIdentity,
    hints: DurableTargetHints,
    snapshots: Arc<dyn WindowsSnapshotSource>,
}

impl WindowsTargetGuard {
    pub fn from_window_id(window_id: u64, hints: DurableTargetHints) -> Result<Self> {
        Ok(Self::new(
            CanonicalWindowIdentity::from_window_id(window_id)?,
            hints,
            Win32WindowsSnapshotSource,
        ))
    }

    pub fn from_xcap_window_id(window_id: u32, hints: DurableTargetHints) -> Self {
        Self::new(
            CanonicalWindowIdentity::from_xcap_window_id(window_id),
            hints,
            Win32WindowsSnapshotSource,
        )
    }

    pub fn durable_hints(&self) -> &DurableTargetHints {
        &self.hints
    }

    fn new(
        identity: CanonicalWindowIdentity,
        hints: DurableTargetHints,
        snapshots: impl WindowsSnapshotSource + 'static,
    ) -> Self {
        Self {
            identity,
            hints,
            snapshots: Arc::new(snapshots),
        }
    }

    #[cfg(test)]
    fn from_raw_hwnd_for_test(raw_hwnd: isize, hints: DurableTargetHints) -> Self {
        Self::new(
            CanonicalWindowIdentity::from_raw_hwnd(raw_hwnd).unwrap(),
            hints,
            Win32WindowsSnapshotSource,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_snapshot_source_for_test(
        identity: CanonicalWindowIdentity,
        hints: DurableTargetHints,
        snapshots: impl WindowsSnapshotSource + 'static,
    ) -> Self {
        Self::new(identity, hints, snapshots)
    }

    #[cfg(test)]
    fn raw_hwnd_for_test(&self) -> isize {
        self.identity.hwnd().0 as isize
    }

    fn validate_hints(&self, snapshot: &TargetSnapshot) -> Result<()> {
        if !self.hints.process_path.is_empty() {
            anyhow::ensure!(
                snapshot.process_path.to_lowercase() == self.hints.process_path.to_lowercase(),
                "target executable path no longer matches the saved hint"
            );
        }
        if !self.hints.window_class.is_empty() {
            anyhow::ensure!(
                window_class(self.identity)? == self.hints.window_class,
                "target window class no longer matches the saved hint"
            );
        }
        if !self.hints.title_contains.is_empty() {
            anyhow::ensure!(
                window_title(self.identity)?.contains(&self.hints.title_contains),
                "target title no longer contains the saved hint"
            );
        }
        Ok(())
    }
}

impl TargetGuard for WindowsTargetGuard {
    fn snapshot(&self) -> Result<TargetSnapshot> {
        Ok(self.snapshots.snapshot(self.identity)?.target_snapshot())
    }

    fn validate(&self, expected: &TargetSnapshot) -> Result<()> {
        let before = self.snapshot()?;
        self.validate_hints(&before)?;
        validate_exact_target(expected, &before)?;
        let after = self.snapshot()?;
        self.validate_hints(&after)?;
        validate_exact_target(expected, &after)
    }
}

pub fn validate_exact_target(expected: &TargetSnapshot, current: &TargetSnapshot) -> Result<()> {
    anyhow::ensure!(current == expected, "target identity or geometry changed");
    anyhow::ensure!(current.is_visible, "target is not visible");
    anyhow::ensure!(!current.is_minimized, "target is minimized");
    anyhow::ensure!(current.is_foreground, "target is not foreground");
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::engine::{automation::TargetSnapshot, types::Rect};

    use super::*;

    #[test]
    fn exact_target_validation_rejects_process_reuse() {
        let expected = TargetSnapshot {
            window_id: 91,
            process_id: 7,
            process_started_at_100ns: 100,
            process_path: r#"C:\games\Diablo IV.exe"#.to_string(),
            client_rect: Rect::new(10, 20, 800, 600),
            window_revision: 1,
            geometry_revision: 2,
            dpi: 144,
            display_profile: "display-a".to_string(),
            display_profile_revision: 3,
            is_visible: true,
            is_minimized: false,
            is_foreground: true,
        };
        let mut restarted = expected.clone();
        restarted.process_started_at_100ns += 1;

        assert!(validate_exact_target(&expected, &restarted).is_err());

        let mut changed_dpi = expected.clone();
        changed_dpi.dpi += 1;
        assert!(validate_exact_target(&expected, &changed_dpi).is_err());

        let mut wrong_hwnd = expected.clone();
        wrong_hwnd.window_id += 1;
        assert!(validate_exact_target(&expected, &wrong_hwnd).is_err());

        let mut changed_geometry = expected.clone();
        changed_geometry.client_rect.x += 1;
        assert!(validate_exact_target(&expected, &changed_geometry).is_err());

        let mut changed_display = expected.clone();
        changed_display.display_profile_revision += 1;
        assert!(validate_exact_target(&expected, &changed_display).is_err());

        let mut background = expected.clone();
        background.is_foreground = false;
        assert!(validate_exact_target(&background, &background).is_err());
    }

    #[test]
    fn live_hwnd_is_run_local_and_not_a_durable_hint() {
        let hints = DurableTargetHints {
            process_path: r#"C:\games\Diablo IV.exe"#.to_string(),
            window_class: "Diablo IV Main Window Class".to_string(),
            title_contains: "Diablo IV".to_string(),
        };
        let guard = WindowsTargetGuard::from_raw_hwnd_for_test(91, hints.clone());

        assert_eq!(guard.durable_hints(), &hints);
        assert_eq!(guard.raw_hwnd_for_test(), 91);
        let json = serde_json::to_string(&hints).unwrap();
        assert!(!json.contains("91"));
    }
}
