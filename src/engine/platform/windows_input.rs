use std::{sync::Arc, thread, time::Duration};

use anyhow::{Result, anyhow};
use windows::Win32::{
    Foundation::{GetLastError, POINT, SetLastError, WIN32_ERROR},
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
            MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput,
        },
        WindowsAndMessaging::{
            GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
            SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        },
    },
};

use super::windows_mouse_hook::{
    ManualMouseActivityObserver, MouseEventSource, MousePoint, SessionInputMarker,
    WindowsMouseHookEventSource,
};
use crate::engine::{
    automation::{InputSink, MouseButton, StopSource},
    config::MouseMovementProfile,
    macro_engine::{
        BlockReason, InputDispatchOutcome, LiveActionInput, MovementOutcome, SendInputFailure,
    },
    types::Point,
};

const DEFAULT_MANUAL_MOVEMENT_THRESHOLD_PX: i32 = 4;

/// Sequenced low-level-hook takeover detector shared by one run-owned macro input sink.
pub struct ManualInputMonitor {
    observer: ManualMouseActivityObserver,
    marker: SessionInputMarker,
}

impl std::fmt::Debug for ManualInputMonitor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManualInputMonitor")
            .finish_non_exhaustive()
    }
}

impl ManualInputMonitor {
    pub fn new(marker: SessionInputMarker) -> Result<Self> {
        let baseline = cursor_position()?;
        let source = Arc::new(WindowsMouseHookEventSource::install(
            marker,
            MousePoint {
                x: baseline.x,
                y: baseline.y,
            },
            DEFAULT_MANUAL_MOVEMENT_THRESHOLD_PX,
        )?);
        Ok(Self::with_event_source(source))
    }

    pub fn with_event_source(source: Arc<dyn MouseEventSource>) -> Self {
        let marker = source.session_marker();
        Self {
            observer: ManualMouseActivityObserver::new(source),
            marker,
        }
    }

    pub fn manual_takeover_detected(&self) -> Result<bool> {
        Ok(self.observer.takeover_detected())
    }

    pub fn reset_baseline(&self) -> Result<()> {
        let baseline = cursor_position()?;
        self.observer.reset_baseline(MousePoint {
            x: baseline.x,
            y: baseline.y,
        });
        Ok(())
    }

    fn marker(&self) -> SessionInputMarker {
        self.marker
    }
}

#[derive(Debug)]
pub struct WindowsInputSink {
    monitor: Arc<ManualInputMonitor>,
    marker: SessionInputMarker,
}

impl WindowsInputSink {
    pub fn new() -> Result<Self> {
        let marker = SessionInputMarker::generate()?;
        let monitor = Arc::new(ManualInputMonitor::new(marker)?);
        Ok(Self::with_monitor(monitor))
    }

    pub fn with_monitor(monitor: Arc<ManualInputMonitor>) -> Self {
        let marker = monitor.marker();
        Self { monitor, marker }
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
        let start = cursor_position()?;
        let duration_ms = movement
            .filter(|profile| profile.is_usable())
            .map_or(0, |profile| profile.duration_ms.clamp(1, 2_000));
        let segments = if duration_ms == 0 {
            1
        } else {
            (duration_ms / 8).clamp(2, 90)
        };
        for segment in 1..=segments {
            if stop.is_stopped() {
                return Ok(MovementOutcome::Cancelled);
            }
            if self.monitor.manual_takeover_detected()? {
                return Ok(MovementOutcome::ManualTakeover);
            }
            let progress = segment as f64 / segments as f64;
            let dx = i64::from(destination.x) - i64::from(start.x);
            let dy = i64::from(destination.y) - i64::from(start.y);
            let next = Point::new(
                (f64::from(start.x) + dx as f64 * progress).round() as i32,
                (f64::from(start.y) + dy as f64 * progress).round() as i32,
            );
            send_marked_move(next, self.marker).map_err(|failure| anyhow!(failure.to_string()))?;
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
        commit: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
    ) -> InputDispatchOutcome {
        if stop.is_stopped() {
            return InputDispatchOutcome::BlockedStopped;
        }
        match self.monitor.manual_takeover_detected() {
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
        let current = match cursor_position() {
            Ok(current) => current,
            Err(error) => {
                return InputDispatchOutcome::BlockedInputFailure {
                    message: error.to_string(),
                };
            }
        };
        if point_distance_squared(current, point)
            >= i64::from(DEFAULT_MANUAL_MOVEMENT_THRESHOLD_PX).pow(2)
        {
            return InputDispatchOutcome::BlockedManualTakeover;
        }
        if let Err(reason) = commit() {
            return InputDispatchOutcome::BlockedCommit { reason };
        }
        let (down, up) = match button {
            MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        };
        let inputs = [
            marked_mouse_input(down, self.marker),
            marked_mouse_input(up, self.marker),
        ];
        match dispatch_inputs(&WindowsSendInputApi, &inputs) {
            Ok(()) => InputDispatchOutcome::Dispatched,
            Err(failure) => InputDispatchOutcome::UncertainDispatch { failure },
        }
    }
}

impl LiveActionInput for WindowsInputSink {
    fn reset_manual_baseline(&self) -> Result<()> {
        WindowsInputSink::reset_manual_baseline(self)
    }

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
        commit: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
    ) -> InputDispatchOutcome {
        self.send_click(point, button, stop, commit)
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
            let mut commit = || Ok(());
            match self.send_click(point, button, stop, &mut commit) {
                InputDispatchOutcome::Dispatched => {}
                InputDispatchOutcome::BlockedStopped => return Ok(()),
                InputDispatchOutcome::BlockedManualTakeover => {
                    return Err(anyhow!("manual mouse takeover blocked input"));
                }
                InputDispatchOutcome::BlockedInputFailure { message } => {
                    return Err(anyhow!(message));
                }
                InputDispatchOutcome::BlockedCommit { reason } => {
                    return Err(anyhow!("input commit was blocked: {reason:?}"));
                }
                InputDispatchOutcome::UncertainDispatch { failure } => {
                    return Err(anyhow!(failure.to_string()));
                }
            }
        }
        Ok(())
    }
}

fn cursor_position() -> Result<Point> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }?;
    Ok(Point::new(point.x, point.y))
}

fn point_distance_squared(left: Point, right: Point) -> i64 {
    let dx = i64::from(left.x) - i64::from(right.x);
    let dy = i64::from(left.y) - i64::from(right.y);
    dx * dx + dy * dy
}

trait SendInputApi {
    fn send(&self, inputs: &[INPUT]) -> (u32, u32);
}

struct WindowsSendInputApi;

impl SendInputApi for WindowsSendInputApi {
    fn send(&self, inputs: &[INPUT]) -> (u32, u32) {
        unsafe {
            SetLastError(WIN32_ERROR(0));
            let inserted = SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
            let error_code = GetLastError().0;
            (inserted, error_code)
        }
    }
}

fn dispatch_inputs(
    api: &dyn SendInputApi,
    inputs: &[INPUT],
) -> std::result::Result<(), SendInputFailure> {
    let expected = u32::try_from(inputs.len()).unwrap_or(u32::MAX);
    let (inserted, error_code) = api.send(inputs);
    if inserted == expected {
        Ok(())
    } else if inserted == 0 {
        Err(SendInputFailure::ZeroInsertion {
            expected,
            error_code,
        })
    } else {
        Err(SendInputFailure::PartialInsertion {
            inserted,
            expected,
            error_code,
        })
    }
}

fn send_marked_move(
    point: Point,
    marker: SessionInputMarker,
) -> std::result::Result<(), SendInputFailure> {
    let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if width <= 1 || height <= 1 {
        return Err(SendInputFailure::ZeroInsertion {
            expected: 1,
            error_code: 0,
        });
    }
    let normalized_x = ((i64::from(point.x) - i64::from(x)) * 65_535)
        .checked_div(i64::from(width - 1))
        .and_then(|value| i32::try_from(value.clamp(0, 65_535)).ok())
        .unwrap_or(0);
    let normalized_y = ((i64::from(point.y) - i64::from(y)) * 65_535)
        .checked_div(i64::from(height - 1))
        .and_then(|value| i32::try_from(value.clamp(0, 65_535)).ok())
        .unwrap_or(0);
    let mut input = marked_mouse_input(
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        marker,
    );
    input.Anonymous.mi.dx = normalized_x;
    input.Anonymous.mi.dy = normalized_y;
    dispatch_inputs(&WindowsSendInputApi, &[input])
}

fn marked_mouse_input(
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    marker: SessionInputMarker,
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
                dwExtraInfo: marker.get(),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };

    use anyhow::{Result, bail};

    use super::{
        ManualInputMonitor, SendInputApi, WindowsInputSink, dispatch_inputs, marked_mouse_input,
    };
    use crate::engine::platform::windows_mouse_hook::{
        MouseActivitySnapshot, MouseEventSource, MousePoint, SessionInputMarker,
    };

    use crate::engine::{
        automation::{Clock, MouseButton, StopSource, TargetGuard, TargetSnapshot},
        macro_engine::{
            ActionAttemptId, ActionAuthorization, ActionCommitter, ActionOutcome,
            ActionPrepareRequest, ActionState, BlockReason, CommitContext, InputDispatchOutcome,
            Limit, LiveActionInput, LiveActionSession, LiveControlSink, MovementOutcome,
            ObservationToken, ResumeAuthorization, SendInputFailure, TakeoverPolicy,
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
        baseline_resets: AtomicU64,
        takeover: AtomicBool,
        fail_dispatch: AtomicBool,
        stop_during_dispatch: Mutex<Option<Arc<Stop>>>,
        movement_outcome: Mutex<Option<MovementOutcome>>,
    }

    impl LiveActionInput for RecordingInput {
        fn reset_manual_baseline(&self) -> Result<()> {
            self.baseline_resets.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

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
            commit: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
        ) -> InputDispatchOutcome {
            if let Err(reason) = commit() {
                return InputDispatchOutcome::BlockedCommit { reason };
            }
            self.calls.lock().unwrap().push("dispatch");
            if let Some(stop) = self.stop_during_dispatch.lock().unwrap().as_ref() {
                stop.0.store(true, Ordering::Release);
            }
            if self.fail_dispatch.load(Ordering::Acquire) {
                return InputDispatchOutcome::UncertainDispatch {
                    failure: SendInputFailure::ZeroInsertion {
                        expected: 2,
                        error_code: 5,
                    },
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
            match_rect: Some(Rect::new(20, 20, 20, 20)),
            score: Some(0.99),
            match_count: 1,
            stable_frames: 2,
            frame_metadata: Some(crate::engine::macro_engine::ImageFrameMetadata {
                frame_id: 8,
                captured_at_ms: 10,
                window_id: 91,
                window_revision: 1,
                client_x: 100,
                client_y: 100,
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
        static NEXT_ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let authorization = ActionAuthorization::for_test(
            ActionAttemptId::for_test("run-1", NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed)),
            target(),
            Point::new(130, 130),
            MouseButton::Left,
            Some(token()),
            4,
            1_000,
        );
        ActionPrepareRequest::new(
            authorization,
            None,
            50,
            TakeoverPolicy::Pause,
            ResumeAuthorization::for_test(target()),
        )
    }

    #[derive(Default)]
    struct RecordingControl {
        paused: AtomicBool,
        stopped: AtomicBool,
    }

    impl LiveControlSink for RecordingControl {
        fn pause_for_manual_takeover(&self) {
            self.paused.store(true, Ordering::Release);
        }
        fn stop_for_manual_takeover(&self) {
            self.stopped.store(true, Ordering::Release);
        }
    }

    fn committer(targets: Vec<TargetSnapshot>, input: Arc<RecordingInput>) -> ActionCommitter {
        committer_with_limits(targets, input, Limit::Finite(100), 128)
    }

    fn committer_with_limits(
        targets: Vec<TargetSnapshot>,
        input: Arc<RecordingInput>,
        maximum_clicks: Limit<u64>,
        maximum_attempts: usize,
    ) -> ActionCommitter {
        let session = LiveActionSession::new(
            Arc::new(ScriptedTarget(Mutex::new(targets))),
            input,
            Arc::new(RecordingControl::default()),
        );
        session.activate_for_test(target());
        ActionCommitter::new(
            session,
            Arc::new(FixedClock(100)),
            "run-1",
            maximum_clicks,
            maximum_attempts,
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
        outside.set_destination_for_test(Point::new(900, 130));
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
        old.set_maximum_observation_age_for_test(50);
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
        outside_match.set_destination_for_test(Point::new(500, 500));
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
    fn observation_time_target_mismatch_is_rejected_before_prepare() {
        let input = Arc::new(RecordingInput::default());
        let committer = committer(vec![target()], input.clone());
        let mut request = request();
        request.alter_observation_target_for_test();
        assert!(matches!(
            committer.prepare(request),
            Err(BlockReason::ResumeRequired)
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
        invalid.set_expected_foreground_for_test(false);
        assert!(matches!(
            committer.prepare(invalid),
            Err(BlockReason::ResumeRequired)
        ));
        assert!(committer.prepare(request()).is_ok());
    }

    #[test]
    fn takeover_during_movement_uses_selected_policy() {
        let input = Arc::new(RecordingInput::default());
        *input.movement_outcome.lock().unwrap() = Some(MovementOutcome::ManualTakeover);
        let committer = committer(vec![target()], input.clone());
        let mut stop_request = request();
        stop_request.set_takeover_policy_for_test(TakeoverPolicy::Stop);
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

    #[test]
    fn committed_attempt_ids_are_permanently_replay_protected() {
        let input = Arc::new(RecordingInput::default());
        let successful_committer = committer(vec![target()], input);
        let mut original = request();
        original.set_minimum_click_interval_for_test(0);
        let replay = original.clone();
        let prepared = successful_committer.prepare(original).unwrap();

        assert!(matches!(
            successful_committer.commit(
                prepared,
                &Stop::default(),
                CommitContext::new("run-1", 4, Some(token()))
            ),
            ActionOutcome::Dispatched { .. }
        ));
        assert_eq!(successful_committer.committed_clicks(), 1);
        assert!(matches!(
            successful_committer.prepare(replay),
            Err(BlockReason::AttemptReplay)
        ));

        let uncertain_input = Arc::new(RecordingInput::default());
        uncertain_input.fail_dispatch.store(true, Ordering::Release);
        let uncertain_committer = committer(vec![target()], uncertain_input);
        let mut original = request();
        original.set_minimum_click_interval_for_test(0);
        let replay = original.clone();
        let prepared = uncertain_committer.prepare(original).unwrap();
        assert!(matches!(
            uncertain_committer.commit(
                prepared,
                &Stop::default(),
                CommitContext::new("run-1", 4, Some(token()))
            ),
            ActionOutcome::UncertainDispatch { .. }
        ));
        assert_eq!(uncertain_committer.committed_clicks(), 1);
        assert!(matches!(
            uncertain_committer.prepare(replay),
            Err(BlockReason::AttemptReplay)
        ));
    }

    #[test]
    fn click_budget_is_consumed_only_at_commit_boundary() {
        let input = Arc::new(RecordingInput::default());
        let committer = committer_with_limits(vec![target()], input.clone(), Limit::Finite(1), 8);
        let mut first = request();
        first.set_minimum_click_interval_for_test(0);
        let prepared = committer.prepare(first).unwrap();
        assert!(matches!(
            committer.commit(
                prepared,
                &Stop::default(),
                CommitContext::new("run-1", 4, Some(token()))
            ),
            ActionOutcome::Dispatched { .. }
        ));

        let mut second = request();
        second.set_minimum_click_interval_for_test(0);
        let prepared = committer.prepare(second).unwrap();
        assert!(matches!(
            committer.commit(
                prepared,
                &Stop::default(),
                CommitContext::new("run-1", 4, Some(token()))
            ),
            ActionOutcome::Blocked {
                reason: BlockReason::ClickBudgetExceeded,
                ..
            }
        ));
        assert_eq!(committer.committed_clicks(), 1);
        assert_eq!(
            input
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| **call == "dispatch")
                .count(),
            1
        );
    }

    #[test]
    fn precommit_block_does_not_consume_click_budget() {
        let input = Arc::new(RecordingInput::default());
        let committer = committer_with_limits(vec![target()], input, Limit::Finite(1), 8);
        let original = request();
        let replay = original.clone();
        let prepared = committer.prepare(original).unwrap();
        let stop = Stop::default();
        stop.0.store(true, Ordering::Release);
        assert!(matches!(
            committer.commit(
                prepared,
                &stop,
                CommitContext::new("run-1", 4, Some(token()))
            ),
            ActionOutcome::Blocked {
                reason: BlockReason::Stopped,
                ..
            }
        ));
        assert_eq!(committer.committed_clicks(), 0);
        assert!(matches!(
            committer.prepare(replay),
            Err(BlockReason::AttemptReplay)
        ));
    }

    #[test]
    fn prepared_actions_cannot_cross_committer_ownership() {
        let input_a = Arc::new(RecordingInput::default());
        let committer_a = committer(vec![target()], input_a);
        let prepared = committer_a.prepare(request()).unwrap();
        let input_b = Arc::new(RecordingInput::default());
        let committer_b = committer(vec![target()], input_b.clone());

        assert!(matches!(
            committer_b.commit(
                prepared,
                &Stop::default(),
                CommitContext::new("run-1", 4, Some(token()))
            ),
            ActionOutcome::Blocked {
                reason: BlockReason::CrossCommitter,
                ..
            }
        ));
        assert_eq!(committer_a.committed_clicks(), 0);
        assert_eq!(committer_b.committed_clicks(), 0);
        assert!(input_b.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn attempt_ledger_is_bounded_and_only_cleared_when_run_finishes() {
        let input = Arc::new(RecordingInput::default());
        let committer = committer_with_limits(vec![target()], input, Limit::Unlimited, 1);
        let mut first = request();
        first.set_minimum_click_interval_for_test(0);
        let prepared = committer.prepare(first).unwrap();
        assert!(matches!(
            committer.finish_run(),
            Err(BlockReason::ActionLockBusy)
        ));
        assert!(matches!(
            committer.commit(
                prepared,
                &Stop::default(),
                CommitContext::new("run-1", 4, Some(token()))
            ),
            ActionOutcome::Dispatched { .. }
        ));
        assert!(matches!(
            committer.prepare(request()),
            Err(BlockReason::AttemptLedgerFull)
        ));
        committer.finish_run().unwrap();
        assert!(matches!(
            committer.prepare(request()),
            Err(BlockReason::RunFinished)
        ));
    }

    #[test]
    fn resume_revalidates_target_resets_baseline_and_invalidates_old_epoch() {
        let input = Arc::new(RecordingInput::default());
        let session = LiveActionSession::new(
            Arc::new(ScriptedTarget(Mutex::new(vec![
                target(),
                target(),
                target(),
                target(),
            ]))),
            input.clone(),
            Arc::new(RecordingControl::default()),
        );
        let old = session.resume().unwrap();
        let current = session.resume().unwrap();
        assert_eq!(input.baseline_resets.load(Ordering::Acquire), 2);
        let committer = ActionCommitter::new(
            session,
            Arc::new(FixedClock(100)),
            "run-1",
            Limit::Finite(10),
            8,
        );
        let mut stale = request();
        stale.set_resume_for_test(old);
        assert!(matches!(
            committer.prepare(stale),
            Err(BlockReason::ResumeRequired)
        ));
        let mut fresh = request();
        fresh.set_resume_for_test(current);
        assert!(committer.prepare(fresh).is_ok());
    }

    #[test]
    fn resume_fails_if_target_changes_during_the_gate() {
        let input = Arc::new(RecordingInput::default());
        let mut changed = target();
        changed.geometry_revision += 1;
        let session = LiveActionSession::new(
            Arc::new(ScriptedTarget(Mutex::new(vec![target(), changed]))),
            input.clone(),
            Arc::new(RecordingControl::default()),
        );

        assert!(session.resume().is_err());
        assert_eq!(input.baseline_resets.load(Ordering::Acquire), 1);
    }

    #[test]
    fn takeover_applies_pause_policy_and_requires_explicit_resume() {
        let input = Arc::new(RecordingInput::default());
        input.takeover.store(true, Ordering::Release);
        let control = Arc::new(RecordingControl::default());
        let session = LiveActionSession::new(
            Arc::new(ScriptedTarget(Mutex::new(vec![target()]))),
            input,
            control.clone(),
        );
        session.activate_for_test(target());
        let committer = ActionCommitter::new(
            session,
            Arc::new(FixedClock(100)),
            "run-1",
            Limit::Finite(10),
            8,
        );
        let prepared = committer.prepare(request()).unwrap();
        assert!(matches!(
            committer.commit(
                prepared,
                &Stop::default(),
                CommitContext::new("run-1", 4, Some(token()))
            ),
            ActionOutcome::Blocked {
                reason: BlockReason::ManualTakeover(TakeoverPolicy::Pause),
                ..
            }
        ));
        assert!(control.paused.load(Ordering::Acquire));
        assert!(!control.stopped.load(Ordering::Acquire));
        assert!(matches!(
            committer.prepare(request()),
            Err(BlockReason::ResumeRequired)
        ));
    }

    struct FakeSendInputApi {
        inserted: u32,
        error_code: u32,
    }

    impl SendInputApi for FakeSendInputApi {
        fn send(
            &self,
            _inputs: &[windows::Win32::UI::Input::KeyboardAndMouse::INPUT],
        ) -> (u32, u32) {
            (self.inserted, self.error_code)
        }
    }

    struct StaticMouseEventSource(SessionInputMarker);

    impl MouseEventSource for StaticMouseEventSource {
        fn session_marker(&self) -> SessionInputMarker {
            self.0
        }

        fn snapshot(&self) -> MouseActivitySnapshot {
            MouseActivitySnapshot::default()
        }

        fn reset_movement_baseline(&self, _point: MousePoint) -> MouseActivitySnapshot {
            MouseActivitySnapshot::default()
        }
    }

    #[test]
    fn sink_derives_its_marker_from_the_bound_monitor() {
        let marker = marker(707);
        let monitor = Arc::new(ManualInputMonitor::with_event_source(Arc::new(
            StaticMouseEventSource(marker),
        )));

        let sink = WindowsInputSink::with_monitor(monitor);

        assert_eq!(sink.marker, marker);
    }

    #[test]
    fn movement_and_click_inputs_carry_the_session_marker() {
        let marker = SessionInputMarker::generate_with(|bytes| {
            bytes.copy_from_slice(&707_usize.to_ne_bytes());
            true
        })
        .unwrap();

        for flags in [
            windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
            windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTDOWN,
        ] {
            let input = marked_mouse_input(flags, marker);
            assert_eq!(unsafe { input.Anonymous.mi.dwExtraInfo }, marker.get());
        }
    }

    #[test]
    fn send_input_adapter_distinguishes_full_zero_and_partial_results() {
        let inputs = [
            super::marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTDOWN,
                marker(11),
            ),
            super::marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTUP,
                marker(11),
            ),
        ];
        assert_eq!(
            dispatch_inputs(
                &FakeSendInputApi {
                    inserted: 2,
                    error_code: 0
                },
                &inputs
            ),
            Ok(())
        );
        assert_eq!(
            dispatch_inputs(
                &FakeSendInputApi {
                    inserted: 0,
                    error_code: 5
                },
                &inputs
            ),
            Err(SendInputFailure::ZeroInsertion {
                expected: 2,
                error_code: 5
            })
        );
        assert_eq!(
            dispatch_inputs(
                &FakeSendInputApi {
                    inserted: 1,
                    error_code: 87
                },
                &inputs
            ),
            Err(SendInputFailure::PartialInsertion {
                inserted: 1,
                expected: 2,
                error_code: 87
            })
        );
    }

    fn marker(value: usize) -> SessionInputMarker {
        SessionInputMarker::generate_with(|bytes| {
            bytes.copy_from_slice(&value.to_ne_bytes());
            true
        })
        .unwrap()
    }
}
