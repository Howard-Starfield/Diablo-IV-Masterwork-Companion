use std::{
    collections::hash_map::DefaultHasher,
    fmt::Debug,
    hash::{Hash, Hasher},
};

use anyhow::{Context, Result};
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
    automation::{AtomicCaptureSnapshot, TargetSnapshot},
    types::Rect,
};

/// One canonical representation of a Windows user handle.
///
/// xcap exposes the low 32 bits as `u32`. Win32 expects bit 31 to be sign-extended when that
/// value is reconstructed as a pointer-sized `HWND`. Metadata always carries the zero-extended
/// low 32 bits so the guard and capture path cannot disagree on high-bit handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CanonicalWindowIdentity(u32);

impl CanonicalWindowIdentity {
    pub(crate) const fn from_xcap_window_id(window_id: u32) -> Self {
        Self(window_id)
    }

    pub(crate) fn from_window_id(window_id: u64) -> Result<Self> {
        let low = window_id as u32;
        let canonical = u64::from(low);
        let sign_extended = (low as i32 as isize) as usize as u64;
        anyhow::ensure!(
            window_id == canonical || window_id == sign_extended,
            "window handle exceeds the Windows user-handle range"
        );
        anyhow::ensure!(low != 0, "window handle must not be null");
        Ok(Self(low))
    }

    pub(crate) fn from_raw_hwnd(raw_hwnd: isize) -> Result<Self> {
        let low = raw_hwnd as u32;
        anyhow::ensure!(
            raw_hwnd == low as i32 as isize,
            "window handle is not a canonical sign-extended Windows user handle"
        );
        Self::from_window_id(u64::from(low))
    }

    pub(crate) const fn window_id(self) -> u64 {
        self.0 as u64
    }

    pub(crate) fn hwnd(self) -> HWND {
        HWND((self.0 as i32 as isize) as *mut core::ffi::c_void)
    }
}

/// The complete, typed Win32 monitor profile used by both target and frame revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DisplayProfileInputs {
    pub(crate) display_id: u64,
    pub(crate) monitor_rect: Rect,
    pub(crate) work_rect: Rect,
    pub(crate) flags: u32,
}

impl DisplayProfileInputs {
    fn label(self) -> String {
        format!(
            "id={};monitor=({},{}..{},{});work=({},{}..{},{});flags={}",
            self.display_id,
            self.monitor_rect.x,
            self.monitor_rect.y,
            i64::from(self.monitor_rect.x) + i64::from(self.monitor_rect.width),
            i64::from(self.monitor_rect.y) + i64::from(self.monitor_rect.height),
            self.work_rect.x,
            self.work_rect.y,
            i64::from(self.work_rect.x) + i64::from(self.work_rect.width),
            i64::from(self.work_rect.y) + i64::from(self.work_rect.height),
            self.flags
        )
    }

    fn revision(self) -> u64 {
        stable_revision(&(
            self.display_id,
            self.monitor_rect.x,
            self.monitor_rect.y,
            self.monitor_rect.width,
            self.monitor_rect.height,
            self.work_rect.x,
            self.work_rect.y,
            self.work_rect.width,
            self.work_rect.height,
            self.flags,
        ))
    }
}

/// Raw facts sampled in one Win32 call path and projected into guard/capture views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CanonicalWindowsSnapshot {
    pub(crate) identity: CanonicalWindowIdentity,
    pub(crate) process_id: u32,
    pub(crate) process_started_at_100ns: u64,
    pub(crate) process_path: String,
    pub(crate) client_rect: Rect,
    pub(crate) dpi: u32,
    pub(crate) display: DisplayProfileInputs,
    pub(crate) is_visible: bool,
    pub(crate) is_minimized: bool,
    pub(crate) is_foreground: bool,
}

impl CanonicalWindowsSnapshot {
    pub(crate) fn window_revision(&self) -> u64 {
        stable_revision(&(
            self.identity.window_id(),
            self.process_id,
            self.process_started_at_100ns,
        ))
    }

    pub(crate) fn geometry_revision(&self) -> u64 {
        stable_revision(&(
            self.client_rect.x,
            self.client_rect.y,
            self.client_rect.width,
            self.client_rect.height,
            self.dpi,
        ))
    }

    pub(crate) fn target_snapshot(&self) -> TargetSnapshot {
        TargetSnapshot {
            window_id: self.identity.window_id(),
            process_id: self.process_id,
            process_started_at_100ns: self.process_started_at_100ns,
            process_path: self.process_path.clone(),
            client_rect: self.client_rect,
            window_revision: self.window_revision(),
            geometry_revision: self.geometry_revision(),
            dpi: self.dpi,
            display_profile: self.display.label(),
            display_profile_revision: self.display.revision(),
            is_visible: self.is_visible,
            is_minimized: self.is_minimized,
            is_foreground: self.is_foreground,
        }
    }

    pub(crate) fn atomic_capture_snapshot(&self, requested_region: Rect) -> AtomicCaptureSnapshot {
        AtomicCaptureSnapshot {
            requested_region,
            window_id: self.identity.window_id(),
            window_revision: self.window_revision(),
            process_id: self.process_id,
            process_started_at_100ns: self.process_started_at_100ns,
            client_rect: self.client_rect,
            geometry_revision: self.geometry_revision(),
            display_id: self.display.display_id,
            display_profile_revision: self.display.revision(),
            dpi: self.dpi,
            is_visible: self.is_visible,
            is_minimized: self.is_minimized,
            is_foreground: self.is_foreground,
        }
    }
}

pub(crate) trait WindowsSnapshotSource: Debug + Send + Sync {
    fn snapshot(&self, identity: CanonicalWindowIdentity) -> Result<CanonicalWindowsSnapshot>;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Win32WindowsSnapshotSource;

impl WindowsSnapshotSource for Win32WindowsSnapshotSource {
    fn snapshot(&self, identity: CanonicalWindowIdentity) -> Result<CanonicalWindowsSnapshot> {
        snapshot_window(identity)
    }
}

fn snapshot_window(identity: CanonicalWindowIdentity) -> Result<CanonicalWindowsSnapshot> {
    let hwnd = identity.hwnd();
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
    let client_rect = client_rect_in_screen(identity)?;
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    anyhow::ensure!(dpi != 0, "failed to query target DPI");

    Ok(CanonicalWindowsSnapshot {
        identity,
        process_id,
        process_started_at_100ns,
        process_path,
        client_rect,
        dpi,
        display: display_profile_inputs(hwnd)?,
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

pub(crate) fn client_rect_in_screen(identity: CanonicalWindowIdentity) -> Result<Rect> {
    let hwnd = identity.hwnd();
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

fn display_profile_inputs(hwnd: HWND) -> Result<DisplayProfileInputs> {
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
    Ok(DisplayProfileInputs {
        display_id: monitor.0 as usize as u64,
        monitor_rect: rect_from_edges(info.rcMonitor)?,
        work_rect: rect_from_edges(info.rcWork)?,
        flags: info.dwFlags,
    })
}

fn rect_from_edges(rect: RECT) -> Result<Rect> {
    let width = rect
        .right
        .checked_sub(rect.left)
        .and_then(|value| u32::try_from(value).ok())
        .context("monitor width is invalid")?;
    let height = rect
        .bottom
        .checked_sub(rect.top)
        .and_then(|value| u32::try_from(value).ok())
        .context("monitor height is invalid")?;
    Ok(Rect::new(rect.left, rect.top, width, height))
}

pub(crate) fn window_class(identity: CanonicalWindowIdentity) -> Result<String> {
    let mut buffer = vec![0_u16; 256];
    let length = unsafe { GetClassNameW(identity.hwnd(), &mut buffer) };
    anyhow::ensure!(length > 0, "failed to query target window class");
    buffer.truncate(length as usize);
    String::from_utf16(&buffer).context("target window class is invalid UTF-16")
}

pub(crate) fn window_title(identity: CanonicalWindowIdentity) -> Result<String> {
    let mut buffer = vec![0_u16; 2_048];
    let length = unsafe { GetWindowTextW(identity.hwnd(), &mut buffer) };
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
    use crate::engine::types::Rect;

    use super::*;

    fn fixture(identity: CanonicalWindowIdentity) -> CanonicalWindowsSnapshot {
        CanonicalWindowsSnapshot {
            identity,
            process_id: 7,
            process_started_at_100ns: 100,
            process_path: r#"C:\games\Diablo IV.exe"#.to_string(),
            client_rect: Rect::new(1_208, -269, 1_008, 729),
            dpi: 144,
            display: DisplayProfileInputs {
                display_id: 11,
                monitor_rect: Rect::new(0, -1_080, 1_920, 1_080),
                work_rect: Rect::new(0, -1_080, 1_920, 1_040),
                flags: 1,
            },
            is_visible: true,
            is_minimized: false,
            is_foreground: true,
        }
    }

    #[test]
    fn high_bit_xcap_id_has_one_metadata_identity_and_a_sign_extended_hwnd() {
        let identity = CanonicalWindowIdentity::from_xcap_window_id(0x8000_0001);

        assert_eq!(identity.window_id(), 0x8000_0001);
        assert_eq!(identity.hwnd().0 as isize, i32::MIN as isize + 1);
        assert_eq!(
            CanonicalWindowIdentity::from_window_id(identity.window_id())
                .unwrap()
                .hwnd()
                .0 as isize,
            i32::MIN as isize + 1
        );
        let sign_extended_metadata_id = (0x8000_0001_u32 as i32 as isize) as usize as u64;
        assert_eq!(
            CanonicalWindowIdentity::from_window_id(sign_extended_metadata_id)
                .unwrap()
                .window_id(),
            0x8000_0001
        );
    }

    #[test]
    fn target_and_atomic_views_share_canonical_identity_and_revision_inputs() {
        let snapshot = fixture(CanonicalWindowIdentity::from_xcap_window_id(0x8000_0001));
        let target = snapshot.target_snapshot();
        let atomic = snapshot.atomic_capture_snapshot(Rect::new(25, 40, 100, 80));

        assert_eq!(atomic.window_id, target.window_id);
        assert_eq!(atomic.window_revision, target.window_revision);
        assert_eq!(atomic.client_rect, target.client_rect);
        assert_eq!(atomic.geometry_revision, target.geometry_revision);
        assert_eq!(atomic.dpi, target.dpi);
        assert_eq!(atomic.display_id, snapshot.display.display_id);
        assert_eq!(target.display_profile, snapshot.display.label());
        assert_eq!(
            atomic.display_profile_revision,
            target.display_profile_revision
        );
    }

    #[test]
    fn geometry_revision_includes_client_screen_rect_and_dpi() {
        let snapshot = fixture(CanonicalWindowIdentity::from_xcap_window_id(42));
        let original = snapshot.geometry_revision();

        let mut moved = snapshot.clone();
        moved.client_rect.x += 1;
        assert_ne!(moved.geometry_revision(), original);

        let mut rescaled = snapshot;
        rescaled.dpi += 1;
        assert_ne!(rescaled.geometry_revision(), original);
    }
}
