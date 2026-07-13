use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use windows::{
    Win32::{
        Foundation::{CloseHandle, FILETIME, HWND, POINT, RECT},
        Graphics::Gdi::{
            ClientToScreen, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO,
            MonitorFromWindow,
        },
        System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
        UI::{
            HiDpi::GetDpiForWindow,
            WindowsAndMessaging::{
                GetClassNameW, GetClientRect, GetForegroundWindow, GetWindowTextW,
                GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
            },
        },
    },
    core::PWSTR,
};

use crate::engine::{
    automation::{TargetGuard, TargetSnapshot},
    types::Rect,
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
    raw_hwnd: isize,
    hints: DurableTargetHints,
}

impl WindowsTargetGuard {
    pub fn from_window_id(window_id: u64, hints: DurableTargetHints) -> Result<Self> {
        let raw_hwnd = isize::try_from(window_id).context("window handle exceeds pointer range")?;
        anyhow::ensure!(raw_hwnd != 0, "window handle must not be null");
        Ok(Self { raw_hwnd, hints })
    }

    pub fn from_xcap_window_id(window_id: u32, hints: DurableTargetHints) -> Self {
        Self {
            raw_hwnd: window_id as i32 as isize,
            hints,
        }
    }

    pub fn durable_hints(&self) -> &DurableTargetHints {
        &self.hints
    }

    fn hwnd(&self) -> HWND {
        HWND(self.raw_hwnd as *mut core::ffi::c_void)
    }

    #[cfg(test)]
    fn from_raw_hwnd_for_test(raw_hwnd: isize, hints: DurableTargetHints) -> Self {
        Self { raw_hwnd, hints }
    }

    #[cfg(test)]
    fn raw_hwnd_for_test(&self) -> isize {
        self.raw_hwnd
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
                window_class(self.hwnd())? == self.hints.window_class,
                "target window class no longer matches the saved hint"
            );
        }
        if !self.hints.title_contains.is_empty() {
            anyhow::ensure!(
                window_title(self.hwnd())?.contains(&self.hints.title_contains),
                "target title no longer contains the saved hint"
            );
        }
        Ok(())
    }
}

impl TargetGuard for WindowsTargetGuard {
    fn snapshot(&self) -> Result<TargetSnapshot> {
        snapshot_target(self.hwnd())
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

fn snapshot_target(hwnd: HWND) -> Result<TargetSnapshot> {
    anyhow::ensure!(
        unsafe { IsWindow(Some(hwnd)) }.as_bool(),
        "target HWND is invalid"
    );
    let mut process_id = 0_u32;
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    anyhow::ensure!(
        thread_id != 0 && process_id != 0,
        "failed to query target process id"
    );
    let (process_started_at_100ns, process_path) = process_identity(process_id)?;
    let client_rect = client_rect_in_screen(hwnd)?;
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    anyhow::ensure!(dpi != 0, "failed to query target DPI");
    let (display_profile, display_profile_revision) = display_profile(hwnd)?;
    let window_id = hwnd.0 as usize as u64;
    let window_revision = stable_revision(&(window_id, process_id, process_started_at_100ns));
    let geometry_revision = stable_revision(&(
        client_rect.x,
        client_rect.y,
        client_rect.width,
        client_rect.height,
        dpi,
    ));

    Ok(TargetSnapshot {
        window_id,
        process_id,
        process_started_at_100ns,
        process_path,
        client_rect,
        window_revision,
        geometry_revision,
        dpi,
        display_profile,
        display_profile_revision,
        is_visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
        is_minimized: unsafe { IsIconic(hwnd) }.as_bool(),
        is_foreground: unsafe { GetForegroundWindow() } == hwnd,
    })
}

fn process_identity(process_id: u32) -> Result<(u64, String)> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
        .with_context(|| format!("failed to open target process {process_id}"))?;
    let result = (|| {
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }
            .with_context(|| {
                format!("failed to query target process {process_id} creation time")
            })?;
        let created = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);

        let mut path = vec![0_u16; 32_768];
        let mut path_len = path.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut path_len,
            )
        }
        .with_context(|| format!("failed to query target process {process_id} image path"))?;
        path.truncate(path_len as usize);
        Ok((
            created,
            String::from_utf16(&path).context("target process path is invalid UTF-16")?,
        ))
    })();
    let close = unsafe { CloseHandle(process) };
    if let Err(error) = close {
        return Err(error).context("failed to close target process identity handle");
    }
    result
}

fn client_rect_in_screen(hwnd: HWND) -> Result<Rect> {
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect) }.context("failed to query target client rect")?;
    let width = rect
        .right
        .checked_sub(rect.left)
        .and_then(|value| u32::try_from(value).ok())
        .context("target client width is invalid")?;
    let height = rect
        .bottom
        .checked_sub(rect.top)
        .and_then(|value| u32::try_from(value).ok())
        .context("target client height is invalid")?;
    anyhow::ensure!(width > 0 && height > 0, "target client area is empty");
    let mut origin = POINT {
        x: rect.left,
        y: rect.top,
    };
    anyhow::ensure!(
        unsafe { ClientToScreen(hwnd, &mut origin) }.as_bool(),
        "failed to map target client rect"
    );
    Ok(Rect::new(origin.x, origin.y, width, height))
}

fn display_profile(hwnd: HWND) -> Result<(String, u64)> {
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    anyhow::ensure!(!monitor.is_invalid(), "failed to locate target monitor");
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    anyhow::ensure!(
        unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool(),
        "failed to query target monitor profile"
    );
    let values = (
        info.rcMonitor.left,
        info.rcMonitor.top,
        info.rcMonitor.right,
        info.rcMonitor.bottom,
        info.rcWork.left,
        info.rcWork.top,
        info.rcWork.right,
        info.rcWork.bottom,
        info.dwFlags,
    );
    Ok((
        format!(
            "monitor=({},{}..{},{});work=({},{}..{},{});flags={}",
            values.0,
            values.1,
            values.2,
            values.3,
            values.4,
            values.5,
            values.6,
            values.7,
            values.8
        ),
        stable_revision(&values),
    ))
}

fn window_class(hwnd: HWND) -> Result<String> {
    let mut buffer = vec![0_u16; 256];
    let length = unsafe { GetClassNameW(hwnd, &mut buffer) };
    anyhow::ensure!(length > 0, "failed to query target window class");
    buffer.truncate(length as usize);
    String::from_utf16(&buffer).context("target window class is invalid UTF-16")
}

fn window_title(hwnd: HWND) -> Result<String> {
    let mut buffer = vec![0_u16; 2_048];
    let length = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    anyhow::ensure!(length >= 0, "failed to query target window title");
    buffer.truncate(length as usize);
    String::from_utf16(&buffer).context("target window title is invalid UTF-16")
}

fn stable_revision(value: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
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
