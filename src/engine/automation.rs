use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{
    config::MouseMovementProfile,
    types::{Point, Rect, ScreenImage},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureFrameMetadata {
    pub frame_id: u64,
    pub captured_at_ms: u64,
    pub window_id: u64,
    pub window_revision: u64,
    #[serde(default)]
    pub process_id: u32,
    #[serde(default)]
    pub process_started_at_100ns: u64,
    /// Physical client-area dimensions used to validate and translate the captured crop.
    #[serde(default)]
    pub client_x: i32,
    #[serde(default)]
    pub client_y: i32,
    #[serde(default)]
    pub client_width: u32,
    #[serde(default)]
    pub client_height: u32,
    pub geometry_revision: u64,
    /// Process-local Win32 monitor handle identity for the display containing the target.
    #[serde(default)]
    pub display_id: u64,
    pub display_profile_revision: u64,
    pub dpi: u32,
    #[serde(default)]
    pub is_visible: bool,
    #[serde(default)]
    pub is_minimized: bool,
    #[serde(default)]
    pub is_foreground: bool,
}

#[derive(Debug, Clone)]
pub struct CapturedScreenFrame {
    pub image: ScreenImage,
    pub metadata: CaptureFrameMetadata,
}

/// The target facts sampled immediately around a raw screen capture.
///
/// Keeping the requested region in the snapshot makes a frame stale when a buggy or delayed
/// snapshot provider describes a different crop than the pixels that were requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomicCaptureSnapshot {
    pub requested_region: Rect,
    pub window_id: u64,
    pub window_revision: u64,
    pub process_id: u32,
    /// Raw Win32 process creation `FILETIME`, in 100-nanosecond ticks.
    /// Together with PID this distinguishes a restarted process after PID/HWND reuse.
    pub process_started_at_100ns: u64,
    /// Screen-space Win32 client area, excluding the non-client frame and title bar.
    pub client_rect: Rect,
    pub geometry_revision: u64,
    pub display_id: u64,
    pub display_profile_revision: u64,
    pub dpi: u32,
    pub is_visible: bool,
    pub is_minimized: bool,
    pub is_foreground: bool,
}

pub trait AtomicFrameSnapshotSource {
    fn snapshot(&self, requested_region: Rect) -> Result<AtomicCaptureSnapshot>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("screen frame became stale while pixels were being captured")]
pub struct StaleCapturedFrameError;

/// Brackets a reusable raw capture source with before/after target snapshots.
///
/// `capture` deliberately remains a direct raw path for OCR utilities and the existing Enchant
/// flow. Executable text and image detection use `capture_frame`, which fails closed if any
/// target fact drifts.
#[derive(Debug)]
pub struct AtomicFrameCapture<S, R, C> {
    snapshots: S,
    raw: R,
    clock: C,
    next_frame_id: AtomicU64,
}

impl<S, R, C> AtomicFrameCapture<S, R, C> {
    pub fn new(snapshots: S, raw: R, clock: C) -> Self {
        Self {
            snapshots,
            raw,
            clock,
            next_frame_id: AtomicU64::new(1),
        }
    }
}

impl<S, R, C> CaptureSource for AtomicFrameCapture<S, R, C>
where
    S: AtomicFrameSnapshotSource,
    R: CaptureSource,
    C: Clock,
{
    fn capture(&self, rect: Rect) -> Result<ScreenImage> {
        self.raw.capture(rect)
    }

    fn capture_frame(&self, rect: Rect) -> Result<CapturedScreenFrame> {
        let before = self.snapshots.snapshot(rect)?;
        let image = self.raw.capture(rect)?;
        let after = self.snapshots.snapshot(rect)?;
        if before != after || before.requested_region != rect {
            return Err(StaleCapturedFrameError.into());
        }
        Ok(CapturedScreenFrame {
            image,
            metadata: CaptureFrameMetadata {
                frame_id: self.next_frame_id.fetch_add(1, Ordering::Relaxed),
                captured_at_ms: self.clock.now_ms(),
                window_id: before.window_id,
                window_revision: before.window_revision,
                process_id: before.process_id,
                process_started_at_100ns: before.process_started_at_100ns,
                client_x: before.client_rect.x,
                client_y: before.client_rect.y,
                client_width: before.client_rect.width,
                client_height: before.client_rect.height,
                geometry_revision: before.geometry_revision,
                display_id: before.display_id,
                display_profile_revision: before.display_profile_revision,
                dpi: before.dpi,
                is_visible: before.is_visible,
                is_minimized: before.is_minimized,
                is_foreground: before.is_foreground,
            },
        })
    }

    fn validate_frame(&self, rect: Rect, metadata: &CaptureFrameMetadata) -> Result<()> {
        let current = self.snapshots.snapshot(rect)?;
        if current.requested_region != rect
            || current.window_id != metadata.window_id
            || current.window_revision != metadata.window_revision
            || current.process_id != metadata.process_id
            || current.process_started_at_100ns != metadata.process_started_at_100ns
            || current.client_rect.x != metadata.client_x
            || current.client_rect.y != metadata.client_y
            || current.client_rect.width != metadata.client_width
            || current.client_rect.height != metadata.client_height
            || current.geometry_revision != metadata.geometry_revision
            || current.display_id != metadata.display_id
            || current.display_profile_revision != metadata.display_profile_revision
            || current.dpi != metadata.dpi
            || current.is_visible != metadata.is_visible
            || current.is_minimized != metadata.is_minimized
            || current.is_foreground != metadata.is_foreground
            || !current.is_visible
            || current.is_minimized
            || !current.is_foreground
        {
            return Err(StaleCapturedFrameError.into());
        }
        Ok(())
    }
}

pub trait CaptureSource {
    fn capture(&self, rect: Rect) -> Result<ScreenImage>;

    fn capture_frame(&self, _rect: Rect) -> Result<CapturedScreenFrame> {
        anyhow::bail!("capture source does not provide atomic pixels and frame metadata")
    }

    fn validate_frame(&self, _rect: Rect, _metadata: &CaptureFrameMetadata) -> Result<()> {
        anyhow::bail!("capture source does not support executable frame freshness validation")
    }
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
    /// Raw Win32 process creation `FILETIME`, in 100-nanosecond ticks.
    pub process_started_at_100ns: u64,
    pub process_path: String,
    pub client_rect: Rect,
    pub window_revision: u64,
    pub geometry_revision: u64,
    pub dpi: u32,
    pub display_profile: String,
    pub display_profile_revision: u64,
    pub is_visible: bool,
    pub is_minimized: bool,
    pub is_foreground: bool,
}

impl Default for TargetSnapshot {
    fn default() -> Self {
        Self {
            window_id: 0,
            process_id: 0,
            process_started_at_100ns: 0,
            process_path: String::new(),
            client_rect: Rect::new(0, 0, 0, 0),
            window_revision: 0,
            geometry_revision: 0,
            dpi: 0,
            display_profile: String::new(),
            display_profile_revision: 0,
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
    use std::{collections::VecDeque, sync::Mutex};

    use anyhow::Result;
    use image::RgbaImage;

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

    #[derive(Debug)]
    struct FakeSnapshots(Mutex<VecDeque<AtomicCaptureSnapshot>>);

    impl AtomicFrameSnapshotSource for FakeSnapshots {
        fn snapshot(&self, _requested_region: Rect) -> Result<AtomicCaptureSnapshot> {
            Ok(self.0.lock().unwrap().pop_front().unwrap())
        }
    }

    #[derive(Debug, Default)]
    struct FakeRawCapture;

    impl CaptureSource for FakeRawCapture {
        fn capture(&self, rect: Rect) -> Result<ScreenImage> {
            Ok(ScreenImage::new(RgbaImage::new(rect.width, rect.height)))
        }
    }

    fn atomic_snapshot(region: Rect) -> AtomicCaptureSnapshot {
        AtomicCaptureSnapshot {
            requested_region: region,
            window_id: 91,
            window_revision: 7,
            process_id: 4,
            process_started_at_100ns: 6,
            client_rect: Rect::new(10, 20, 800, 600),
            geometry_revision: 8,
            display_id: 10,
            display_profile_revision: 9,
            dpi: 144,
            is_visible: true,
            is_minimized: false,
            is_foreground: true,
        }
    }

    #[test]
    fn bracketed_capture_returns_pixels_only_when_target_snapshot_stays_atomic() {
        let region = Rect::new(50, 60, 20, 10);
        let snapshot = atomic_snapshot(region);
        let capture = AtomicFrameCapture::new(
            FakeSnapshots(Mutex::new(VecDeque::from([snapshot.clone(), snapshot]))),
            FakeRawCapture,
            FakeClock(123),
        );

        let frame = capture.capture_frame(region).unwrap();

        assert_eq!(frame.image.rgba.dimensions(), (20, 10));
        assert_eq!(frame.metadata.frame_id, 1);
        assert_eq!(frame.metadata.captured_at_ms, 123);
        assert_eq!(frame.metadata.window_id, 91);
        assert_eq!(frame.metadata.process_id, 4);
        assert_eq!(frame.metadata.process_started_at_100ns, 6);
        assert_eq!((frame.metadata.client_x, frame.metadata.client_y), (10, 20));
        assert_eq!(frame.metadata.client_width, 800);
        assert_eq!(frame.metadata.client_height, 600);
        assert_eq!(frame.metadata.geometry_revision, 8);
        assert_eq!(frame.metadata.display_id, 10);
        assert_eq!(frame.metadata.display_profile_revision, 9);
        assert_eq!(frame.metadata.dpi, 144);
    }

    #[test]
    fn captured_frame_can_be_revalidated_against_current_canonical_target() {
        let region = Rect::new(50, 60, 20, 10);
        let snapshot = atomic_snapshot(region);
        let capture = AtomicFrameCapture::new(
            FakeSnapshots(Mutex::new(VecDeque::from([
                snapshot.clone(),
                snapshot.clone(),
                snapshot,
            ]))),
            FakeRawCapture,
            FakeClock(123),
        );
        let frame = capture.capture_frame(region).unwrap();

        capture.validate_frame(region, &frame.metadata).unwrap();
    }

    #[test]
    fn frame_revalidation_rejects_process_or_geometry_drift_after_capture() {
        let region = Rect::new(50, 60, 20, 10);
        let snapshot = atomic_snapshot(region);
        let mut drifted = snapshot.clone();
        drifted.process_started_at_100ns += 1;
        drifted.geometry_revision += 1;
        let capture = AtomicFrameCapture::new(
            FakeSnapshots(Mutex::new(VecDeque::from([
                snapshot.clone(),
                snapshot,
                drifted,
            ]))),
            FakeRawCapture,
            FakeClock(123),
        );
        let frame = capture.capture_frame(region).unwrap();

        assert!(capture.validate_frame(region, &frame.metadata).is_err());
    }

    #[test]
    fn frame_revalidation_rejects_monitor_identity_drift_after_capture() {
        let region = Rect::new(50, 60, 20, 10);
        let snapshot = atomic_snapshot(region);
        let mut drifted = snapshot.clone();
        drifted.display_id += 1;
        let capture = AtomicFrameCapture::new(
            FakeSnapshots(Mutex::new(VecDeque::from([
                snapshot.clone(),
                snapshot,
                drifted,
            ]))),
            FakeRawCapture,
            FakeClock(123),
        );
        let frame = capture.capture_frame(region).unwrap();

        let error = capture.validate_frame(region, &frame.metadata).unwrap_err();

        assert!(error.downcast_ref::<StaleCapturedFrameError>().is_some());
    }

    #[test]
    fn bracketed_capture_rejects_client_geometry_or_requested_region_drift() {
        let region = Rect::new(50, 60, 20, 10);
        let before = atomic_snapshot(region);
        let mut after = before.clone();
        after.client_rect.width -= 1;
        let capture = AtomicFrameCapture::new(
            FakeSnapshots(Mutex::new(VecDeque::from([before, after]))),
            FakeRawCapture,
            FakeClock(123),
        );

        let error = capture.capture_frame(region).unwrap_err();

        assert!(error.downcast_ref::<StaleCapturedFrameError>().is_some());
    }

    #[test]
    fn frame_revalidation_rejects_focus_or_visibility_loss_after_capture() {
        let region = Rect::new(50, 60, 20, 10);
        let snapshot = atomic_snapshot(region);
        let mut drifted = snapshot.clone();
        drifted.is_foreground = false;
        drifted.is_visible = false;
        let capture = AtomicFrameCapture::new(
            FakeSnapshots(Mutex::new(VecDeque::from([
                snapshot.clone(),
                snapshot,
                drifted,
            ]))),
            FakeRawCapture,
            FakeClock(123),
        );
        let frame = capture.capture_frame(region).unwrap();

        let error = capture.validate_frame(region, &frame.metadata).unwrap_err();

        assert!(error.downcast_ref::<StaleCapturedFrameError>().is_some());
    }

    #[test]
    fn bracketed_capture_rejects_process_identity_reuse_during_capture() {
        let region = Rect::new(50, 60, 20, 10);
        let before = atomic_snapshot(region);
        let mut after = before.clone();
        after.process_started_at_100ns += 1;
        let capture = AtomicFrameCapture::new(
            FakeSnapshots(Mutex::new(VecDeque::from([before, after]))),
            FakeRawCapture,
            FakeClock(123),
        );

        let error = capture.capture_frame(region).unwrap_err();

        assert!(error.downcast_ref::<StaleCapturedFrameError>().is_some());
    }

    #[test]
    fn bracketed_capture_preserves_raw_capture_for_ocr_utilities_and_enchant() {
        let capture = AtomicFrameCapture::new(
            FakeSnapshots(Mutex::new(VecDeque::new())),
            FakeRawCapture,
            FakeClock(123),
        );

        assert_eq!(
            capture
                .capture(Rect::new(0, 0, 3, 2))
                .unwrap()
                .rgba
                .dimensions(),
            (3, 2)
        );
    }
}
