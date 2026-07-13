use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Result, anyhow};
use windows::Win32::{
    Foundation::POINT,
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEINPUT, SendInput,
            VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
        },
        WindowsAndMessaging::{GetCursorPos, SetCursorPos},
    },
};

use crate::engine::{
    automation::{InputSink, MouseButton, StopSource},
    config::MouseMovementProfile,
    macro_engine::{InputDispatchOutcome, LiveActionInput, MovementOutcome},
    types::Point,
};

const RUNTIME_INPUT_MARKER: usize = 0x4D_41_43_52_4F; // "MACRO"
const DEFAULT_MANUAL_MOVEMENT_THRESHOLD_PX: i32 = 4;

trait InputProbe: Send + Sync {
    fn cursor_position(&self) -> Result<Point>;
    fn any_mouse_button_down(&self) -> bool;
}

#[derive(Debug, Default)]
struct WindowsInputProbe;

impl InputProbe for WindowsInputProbe {
    fn cursor_position(&self) -> Result<Point> {
        let mut point = POINT::default();
        unsafe { GetCursorPos(&mut point) }?;
        Ok(Point::new(point.x, point.y))
    }

    fn any_mouse_button_down(&self) -> bool {
        let mut observed = false;
        for button in [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2] {
            // Check both the high "currently down" bit and the low "pressed since last query"
            // bit so a short physical click between polling samples is still takeover.
            observed |= unsafe { GetAsyncKeyState(button.0 as i32) } != 0;
        }
        observed
    }
}

/// Polling manual-takeover detector shared by one run-owned macro input sink.
pub struct ManualInputMonitor {
    probe: Arc<dyn InputProbe>,
    baseline: Mutex<Point>,
    runtime_owned: AtomicBool,
    movement_threshold_px: i32,
}

impl std::fmt::Debug for ManualInputMonitor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManualInputMonitor")
            .field("runtime_owned", &self.runtime_owned.load(Ordering::Acquire))
            .field("movement_threshold_px", &self.movement_threshold_px)
            .finish_non_exhaustive()
    }
}

impl ManualInputMonitor {
    pub fn new() -> Result<Self> {
        Self::with_probe(
            Arc::new(WindowsInputProbe),
            DEFAULT_MANUAL_MOVEMENT_THRESHOLD_PX,
        )
    }

    fn with_probe(probe: Arc<dyn InputProbe>, movement_threshold_px: i32) -> Result<Self> {
        anyhow::ensure!(
            movement_threshold_px > 0,
            "manual movement threshold must be positive"
        );
        let baseline = probe.cursor_position()?;
        Ok(Self {
            probe,
            baseline: Mutex::new(baseline),
            runtime_owned: AtomicBool::new(false),
            movement_threshold_px,
        })
    }

    pub fn manual_takeover_detected(&self) -> Result<bool> {
        if self.runtime_owned.load(Ordering::Acquire) {
            return Ok(false);
        }
        self.takeover_from(
            *self
                .baseline
                .lock()
                .expect("manual input baseline poisoned"),
        )
    }

    pub fn reset_baseline(&self) -> Result<()> {
        let current = self.probe.cursor_position()?;
        self.record_runtime_position(current);
        Ok(())
    }

    fn takeover_from(&self, expected_cursor: Point) -> Result<bool> {
        if self.probe.any_mouse_button_down() {
            return Ok(true);
        }
        let current = self.probe.cursor_position()?;
        let dx = i64::from(current.x) - i64::from(expected_cursor.x);
        let dy = i64::from(current.y) - i64::from(expected_cursor.y);
        let threshold = i64::from(self.movement_threshold_px);
        Ok(dx * dx + dy * dy >= threshold * threshold)
    }

    fn begin_runtime_input(&self) -> RuntimeInputOwnership<'_> {
        self.runtime_owned.store(true, Ordering::Release);
        RuntimeInputOwnership { monitor: self }
    }

    fn record_runtime_position(&self, point: Point) {
        *self
            .baseline
            .lock()
            .expect("manual input baseline poisoned") = point;
    }
}

struct RuntimeInputOwnership<'a> {
    monitor: &'a ManualInputMonitor,
}

impl Drop for RuntimeInputOwnership<'_> {
    fn drop(&mut self) {
        self.monitor.runtime_owned.store(false, Ordering::Release);
    }
}

#[derive(Debug)]
pub struct WindowsInputSink {
    monitor: Arc<ManualInputMonitor>,
}

impl WindowsInputSink {
    pub fn new() -> Result<Self> {
        Ok(Self {
            monitor: Arc::new(ManualInputMonitor::new()?),
        })
    }

    pub fn with_monitor(monitor: Arc<ManualInputMonitor>) -> Self {
        Self { monitor }
    }

    pub fn reset_manual_baseline(&self) -> Result<()> {
        self.monitor.reset_baseline()
    }

    fn move_cursor(
        &self,
        destination: Point,
        movement: Option<&MouseMovementProfile>,
        stop: &dyn StopSource,
    ) -> Result<MovementOutcome> {
        let start = self.monitor.probe.cursor_position()?;
        let _ownership = self.monitor.begin_runtime_input();
        self.monitor.record_runtime_position(start);
        let duration_ms = movement
            .filter(|profile| profile.is_usable())
            .map_or(0, |profile| profile.duration_ms.clamp(1, 2_000));
        let segments = if duration_ms == 0 {
            1
        } else {
            (duration_ms / 8).clamp(2, 90)
        };
        let mut previous = start;
        for segment in 1..=segments {
            if stop.is_stopped() {
                return Ok(MovementOutcome::Cancelled);
            }
            if self.monitor.takeover_from(previous)? {
                return Ok(MovementOutcome::ManualTakeover);
            }
            let progress = segment as f64 / segments as f64;
            let dx = i64::from(destination.x) - i64::from(start.x);
            let dy = i64::from(destination.y) - i64::from(start.y);
            let next = Point::new(
                (f64::from(start.x) + dx as f64 * progress).round() as i32,
                (f64::from(start.y) + dy as f64 * progress).round() as i32,
            );
            unsafe { SetCursorPos(next.x, next.y) }?;
            self.monitor.record_runtime_position(next);
            previous = next;
            if duration_ms > 0 && segment < segments {
                thread::sleep(Duration::from_millis((duration_ms / segments).max(1)));
            }
        }
        Ok(MovementOutcome::Reached)
    }

    fn send_click(
        &self,
        point: Point,
        button: MouseButton,
        stop: &dyn StopSource,
    ) -> InputDispatchOutcome {
        let _ownership = self.monitor.begin_runtime_input();
        if stop.is_stopped() {
            return InputDispatchOutcome::BlockedStopped;
        }
        match self.monitor.takeover_from(point) {
            Ok(true) => return InputDispatchOutcome::BlockedManualTakeover,
            Ok(false) => {}
            Err(error) => {
                return InputDispatchOutcome::BlockedInputFailure {
                    message: error.to_string(),
                };
            }
        }
        if stop.is_stopped() {
            return InputDispatchOutcome::BlockedStopped;
        }
        let (down, up) = match button {
            MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        };
        let inputs = [marked_mouse_input(down), marked_mouse_input(up)];
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent != inputs.len() as u32 {
            return InputDispatchOutcome::UncertainDispatch {
                message: format!("SendInput sent {sent}/{} mouse events", inputs.len()),
            };
        }
        // Consume button transition bits while runtime ownership is marked so our own injected
        // click cannot be mistaken for a physical takeover on the next prepared action.
        let _ = self.monitor.probe.any_mouse_button_down();
        InputDispatchOutcome::Dispatched
    }
}

impl LiveActionInput for WindowsInputSink {
    fn manual_takeover_detected(&self) -> Result<bool> {
        self.monitor.manual_takeover_detected()
    }

    fn move_to(
        &self,
        point: Point,
        movement: Option<&MouseMovementProfile>,
        stop: &dyn StopSource,
    ) -> Result<MovementOutcome> {
        self.move_cursor(point, movement, stop)
    }

    fn dispatch_click(
        &self,
        point: Point,
        button: MouseButton,
        stop: &dyn StopSource,
    ) -> InputDispatchOutcome {
        self.send_click(point, button, stop)
    }
}

impl InputSink for WindowsInputSink {
    fn move_and_click(
        &self,
        point: Point,
        button: MouseButton,
        movement: Option<&MouseMovementProfile>,
        stop: Option<&dyn StopSource>,
    ) -> Result<()> {
        struct NeverStop;
        impl StopSource for NeverStop {
            fn is_stopped(&self) -> bool {
                false
            }
        }
        let never = NeverStop;
        let stop = stop.unwrap_or(&never);
        if matches!(
            self.move_cursor(point, movement, stop)?,
            MovementOutcome::Reached
        ) && !stop.is_stopped()
        {
            match self.send_click(point, button, stop) {
                InputDispatchOutcome::Dispatched => {}
                InputDispatchOutcome::BlockedStopped => return Ok(()),
                InputDispatchOutcome::BlockedManualTakeover => {
                    return Err(anyhow!("manual mouse takeover blocked input"));
                }
                InputDispatchOutcome::BlockedInputFailure { message }
                | InputDispatchOutcome::UncertainDispatch { message } => {
                    return Err(anyhow!(message));
                }
            }
        }
        Ok(())
    }
}

fn marked_mouse_input(
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: RUNTIME_INPUT_MARKER,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use anyhow::{Result, bail};

    use super::{InputProbe, ManualInputMonitor};

    use crate::engine::{
        automation::{Clock, MouseButton, StopSource, TargetGuard, TargetSnapshot},
        macro_engine::{
            ActionCommitter, ActionOutcome, ActionPrepareRequest, ActionState, BlockReason,
            CommitContext, InputDispatchOutcome, LiveActionInput, MovementOutcome,
            ObservationToken, TakeoverPolicy,
        },
        types::{Point, Rect},
    };

    #[derive(Default)]
    struct Stop(AtomicBool);

    impl StopSource for Stop {
        fn is_stopped(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }

    struct FixedClock(u64);
    impl Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            self.0
        }
    }

    struct ScriptedTarget(Mutex<Vec<TargetSnapshot>>);
    impl TargetGuard for ScriptedTarget {
        fn snapshot(&self) -> Result<TargetSnapshot> {
            let mut snapshots = self.0.lock().unwrap();
            Ok(if snapshots.len() > 1 {
                snapshots.remove(0)
            } else {
                snapshots[0].clone()
            })
        }
        fn validate(&self, expected: &TargetSnapshot) -> Result<()> {
            let current = self.snapshot()?;
            if &current == expected {
                Ok(())
            } else {
                bail!("target changed")
            }
        }
    }

    #[derive(Default)]
    struct RecordingInput {
        calls: Mutex<Vec<&'static str>>,
        takeover: AtomicBool,
        fail_dispatch: AtomicBool,
        stop_during_dispatch: Mutex<Option<Arc<Stop>>>,
        movement_outcome: Mutex<Option<MovementOutcome>>,
    }

    impl LiveActionInput for RecordingInput {
        fn manual_takeover_detected(&self) -> Result<bool> {
            Ok(self.takeover.load(Ordering::Acquire))
        }
        fn move_to(
            &self,
            _point: Point,
            _movement: Option<&crate::engine::config::MouseMovementProfile>,
            stop: &dyn StopSource,
        ) -> Result<MovementOutcome> {
            self.calls.lock().unwrap().push("move");
            Ok(self
                .movement_outcome
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| {
                    if stop.is_stopped() {
                        MovementOutcome::Cancelled
                    } else {
                        MovementOutcome::Reached
                    }
                }))
        }
        fn dispatch_click(
            &self,
            _point: Point,
            _button: MouseButton,
            _stop: &dyn StopSource,
        ) -> InputDispatchOutcome {
            self.calls.lock().unwrap().push("dispatch");
            if let Some(stop) = self.stop_during_dispatch.lock().unwrap().as_ref() {
                stop.0.store(true, Ordering::Release);
            }
            if self.fail_dispatch.load(Ordering::Acquire) {
                return InputDispatchOutcome::UncertainDispatch {
                    message: "uncertain send".to_string(),
                };
            }
            InputDispatchOutcome::Dispatched
        }
    }

    fn target() -> TargetSnapshot {
        TargetSnapshot {
            window_id: 91,
            process_id: 7,
            process_started_at_100ns: 100,
            process_path: "game.exe".to_string(),
            client_rect: Rect::new(100, 100, 800, 600),
            window_revision: 1,
            geometry_revision: 2,
            dpi: 144,
            display_profile: "display-a".to_string(),
            display_profile_revision: 3,
            is_visible: true,
            is_minimized: false,
            is_foreground: true,
        }
    }

    fn token() -> ObservationToken {
        ObservationToken {
            run_id: "run-1".to_string(),
            generation: 4,
            source_block_id: "observe".to_string(),
            detector: crate::engine::macro_engine::DetectorKind::Image,
            region_id: "region".to_string(),
            region_revision: 1,
            rule_id: "rule".to_string(),
            rule_revision: 1,
            frame_id: 8,
            captured_at_ms: 10,
            match_rect: Some(Rect::new(120, 120, 20, 20)),
            score: Some(0.99),
            match_count: 1,
            stable_frames: 2,
            frame_metadata: Some(crate::engine::macro_engine::ImageFrameMetadata {
                frame_id: 8,
                captured_at_ms: 10,
                window_id: 91,
                window_revision: 1,
                client_width: 800,
                client_height: 600,
                geometry_revision: 2,
                display_profile_revision: 3,
                dpi: 144,
                region_revision: 1,
                rule_revision: 1,
            }),
            evidence: serde_json::Value::Null,
        }
    }

    fn request() -> ActionPrepareRequest {
        ActionPrepareRequest {
            block_id: "click".to_string(),
            expected_target: target(),
            destination: Point::new(130, 130),
            button: MouseButton::Left,
            observation: Some(token()),
            observation_target: Some(target()),
            movement: None,
            run_id: "run-1".to_string(),
            generation: 4,
            maximum_observation_age_ms: 1_000,
            minimum_click_interval_ms: 50,
            takeover_policy: TakeoverPolicy::Pause,
        }
    }

    fn committer(targets: Vec<TargetSnapshot>, input: Arc<RecordingInput>) -> ActionCommitter {
        ActionCommitter::new(
            Arc::new(ScriptedTarget(Mutex::new(targets))),
            input,
            Arc::new(FixedClock(100)),
        )
    }

    #[test]
    fn focus_loss_before_commit_blocks_input() {
        let input = Arc::new(RecordingInput::default());
        let mut invalid = target();
        invalid.is_foreground = false;
        let committer = committer(vec![target(), invalid], input.clone());
        let prepared = committer.prepare(request()).unwrap();
        let stop = Stop::default();

        let result = committer.commit(
            prepared,
            &stop,
            CommitContext::new("run-1", 4, Some(token())),
        );

        assert!(matches!(
            result,
            ActionOutcome::Blocked {
                reason: BlockReason::TargetChanged,
                ..
            }
        ));
        assert!(input.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn stop_before_commit_blocks_without_input() {
        let input = Arc::new(RecordingInput::default());
        let committer = committer(vec![target()], input.clone());
        let prepared = committer.prepare(request()).unwrap();
        let stop = Stop::default();
        stop.0.store(true, Ordering::Release);

        let result = committer.commit(
            prepared,
            &stop,
            CommitContext::new("run-1", 4, Some(token())),
        );

        assert!(matches!(
            result,
            ActionOutcome::Blocked {
                reason: BlockReason::Stopped,
                ..
            }
        ));
        assert!(input.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn stop_after_commit_is_uncertain_and_never_retried() {
        let stop = Arc::new(Stop::default());
        let input = Arc::new(RecordingInput::default());
        *input.stop_during_dispatch.lock().unwrap() = Some(stop.clone());
        let committer = committer(vec![target(), target()], input.clone());
        let prepared = committer.prepare(request()).unwrap();

        let result = committer.commit(
            prepared,
            stop.as_ref(),
            CommitContext::new("run-1", 4, Some(token())),
        );

        assert!(matches!(result, ActionOutcome::UncertainDispatch { .. }));
        assert_eq!(*input.calls.lock().unwrap(), vec!["move", "dispatch"]);
        assert_eq!(
            result.transitions(),
            &[
                ActionState::Prepared,
                ActionState::Committed,
                ActionState::UncertainDispatch
            ]
        );
    }

    #[test]
    fn stale_token_and_out_of_bounds_destination_fail_closed() {
        let input = Arc::new(RecordingInput::default());
        let committer = committer(vec![target()], input.clone());
        let prepared = committer.prepare(request()).unwrap();
        let mut stale = token();
        stale.generation += 1;
        let stop = Stop::default();
        assert!(matches!(
            committer.commit(prepared, &stop, CommitContext::new("run-1", 4, Some(stale))),
            ActionOutcome::Blocked {
                reason: BlockReason::StaleObservation,
                ..
            }
        ));

        let mut outside = request();
        outside.destination = Point::new(900, 130);
        let prepared = committer.prepare(outside).unwrap();
        assert!(matches!(
            committer.commit(
                prepared,
                &stop,
                CommitContext::new("run-1", 4, Some(token()))
            ),
            ActionOutcome::Blocked {
                reason: BlockReason::DestinationOutOfBounds,
                ..
            }
        ));
        assert!(input.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn old_observation_and_destination_outside_match_geometry_fail_closed() {
        let input = Arc::new(RecordingInput::default());
        let committer = committer(vec![target()], input.clone());
        let mut old = request();
        old.maximum_observation_age_ms = 50;
        let prepared = committer.prepare(old).unwrap();
        assert!(matches!(
            committer.commit(
                prepared,
                &Stop::default(),
                CommitContext::new("run-1", 4, Some(token()))
            ),
            ActionOutcome::Blocked {
                reason: BlockReason::StaleObservation,
                ..
            }
        ));

        let mut outside_match = request();
        outside_match.destination = Point::new(500, 500);
        let prepared = committer.prepare(outside_match).unwrap();
        assert!(matches!(
            committer.commit(
                prepared,
                &Stop::default(),
                CommitContext::new("run-1", 4, Some(token()))
            ),
            ActionOutcome::Blocked {
                reason: BlockReason::DestinationOutOfBounds,
                ..
            }
        ));
        assert!(input.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn observation_time_target_must_match_commit_target() {
        let input = Arc::new(RecordingInput::default());
        let committer = committer(vec![target()], input.clone());
        let mut request = request();
        request.observation_target.as_mut().unwrap().window_id += 1;
        let prepared = committer.prepare(request).unwrap();
        assert!(matches!(
            committer.commit(
                prepared,
                &Stop::default(),
                CommitContext::new("run-1", 4, Some(token()))
            ),
            ActionOutcome::Blocked {
                reason: BlockReason::StaleObservation,
                ..
            }
        ));
        assert!(input.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn manual_takeover_blocks_by_policy() {
        let input = Arc::new(RecordingInput::default());
        input.takeover.store(true, Ordering::Release);
        let committer = committer(vec![target()], input.clone());
        let prepared = committer.prepare(request()).unwrap();
        let result = committer.commit(
            prepared,
            &Stop::default(),
            CommitContext::new("run-1", 4, Some(token())),
        );
        assert!(matches!(
            result,
            ActionOutcome::Blocked {
                reason: BlockReason::ManualTakeover(TakeoverPolicy::Pause),
                ..
            }
        ));
        assert!(input.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn target_change_after_movement_still_blocks_before_dispatch() {
        let input = Arc::new(RecordingInput::default());
        let mut changed = target();
        changed.geometry_revision += 1;
        let committer = committer(vec![target(), target(), changed], input.clone());
        let prepared = committer.prepare(request()).unwrap();

        let result = committer.commit(
            prepared,
            &Stop::default(),
            CommitContext::new("run-1", 4, Some(token())),
        );

        assert!(matches!(
            result,
            ActionOutcome::Blocked {
                reason: BlockReason::TargetChanged,
                ..
            }
        ));
        assert_eq!(*input.calls.lock().unwrap(), vec!["move"]);
    }

    #[test]
    fn successful_dispatch_and_uncertain_error_have_ordered_terminal_states() {
        let input = Arc::new(RecordingInput::default());
        let successful_committer = committer(vec![target(), target()], input.clone());
        let prepared = successful_committer.prepare(request()).unwrap();
        let result = successful_committer.commit(
            prepared,
            &Stop::default(),
            CommitContext::new("run-1", 4, Some(token())),
        );
        assert!(matches!(result, ActionOutcome::Dispatched { .. }));
        assert_eq!(
            result.transitions(),
            &[
                ActionState::Prepared,
                ActionState::Committed,
                ActionState::Dispatched
            ]
        );

        let error_input = Arc::new(RecordingInput::default());
        error_input.fail_dispatch.store(true, Ordering::Release);
        let error_committer = committer(vec![target(), target()], error_input.clone());
        let prepared = error_committer.prepare(request()).unwrap();
        let result = error_committer.commit(
            prepared,
            &Stop::default(),
            CommitContext::new("run-1", 4, Some(token())),
        );
        assert!(matches!(result, ActionOutcome::UncertainDispatch { .. }));
        assert_eq!(*error_input.calls.lock().unwrap(), vec!["move", "dispatch"]);
    }

    #[test]
    fn action_lock_and_click_pacing_fail_closed() {
        let input = Arc::new(RecordingInput::default());
        let committer = committer(vec![target(), target()], input);
        let first = committer.prepare(request()).unwrap();
        assert!(matches!(
            committer.prepare(request()),
            Err(BlockReason::ActionLockBusy)
        ));
        let result = committer.commit(
            first,
            &Stop::default(),
            CommitContext::new("run-1", 4, Some(token())),
        );
        assert!(matches!(result, ActionOutcome::Dispatched { .. }));

        let second = committer.prepare(request()).unwrap();
        let result = committer.commit(
            second,
            &Stop::default(),
            CommitContext::new("run-1", 4, Some(token())),
        );
        assert!(matches!(
            result,
            ActionOutcome::Blocked {
                reason: BlockReason::ClickPacing,
                ..
            }
        ));
    }

    #[test]
    fn failed_prepare_releases_the_action_lock() {
        let input = Arc::new(RecordingInput::default());
        let committer = committer(vec![target()], input);
        let mut invalid = request();
        invalid.expected_target.is_foreground = false;
        assert!(matches!(
            committer.prepare(invalid),
            Err(BlockReason::TargetChanged)
        ));
        assert!(committer.prepare(request()).is_ok());
    }

    #[test]
    fn takeover_during_movement_uses_selected_policy() {
        let input = Arc::new(RecordingInput::default());
        *input.movement_outcome.lock().unwrap() = Some(MovementOutcome::ManualTakeover);
        let committer = committer(vec![target()], input.clone());
        let mut stop_request = request();
        stop_request.takeover_policy = TakeoverPolicy::Stop;
        let prepared = committer.prepare(stop_request).unwrap();

        let result = committer.commit(
            prepared,
            &Stop::default(),
            CommitContext::new("run-1", 4, Some(token())),
        );

        assert!(matches!(
            result,
            ActionOutcome::Blocked {
                reason: BlockReason::ManualTakeover(TakeoverPolicy::Stop),
                ..
            }
        ));
        assert_eq!(*input.calls.lock().unwrap(), vec!["move"]);
    }

    struct FakeProbe {
        cursor: Mutex<Point>,
        button: AtomicBool,
    }

    impl Default for FakeProbe {
        fn default() -> Self {
            Self {
                cursor: Mutex::new(Point::new(0, 0)),
                button: AtomicBool::new(false),
            }
        }
    }

    impl InputProbe for FakeProbe {
        fn cursor_position(&self) -> Result<Point> {
            Ok(*self.cursor.lock().unwrap())
        }
        fn any_mouse_button_down(&self) -> bool {
            self.button.load(Ordering::Acquire)
        }
    }

    #[test]
    fn manual_monitor_detects_physical_motion_and_buttons_but_owns_runtime_motion() {
        let probe = Arc::new(FakeProbe::default());
        let monitor = ManualInputMonitor::with_probe(probe.clone(), 4).unwrap();
        *probe.cursor.lock().unwrap() = Point::new(5, 0);
        assert!(monitor.manual_takeover_detected().unwrap());
        monitor.reset_baseline().unwrap();
        assert!(!monitor.manual_takeover_detected().unwrap());

        probe.button.store(true, Ordering::Release);
        assert!(monitor.manual_takeover_detected().unwrap());
        probe.button.store(false, Ordering::Release);
        {
            let _owned = monitor.begin_runtime_input();
            *probe.cursor.lock().unwrap() = Point::new(20, 20);
            monitor.record_runtime_position(Point::new(20, 20));
            assert!(!monitor.manual_takeover_detected().unwrap());
        }
        assert!(!monitor.manual_takeover_detected().unwrap());
    }
}
