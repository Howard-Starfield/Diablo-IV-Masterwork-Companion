use std::{sync::Arc, thread, time::Duration};

use anyhow::Result;
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
    automation::{MouseButton, StopSource},
    config::MouseMovementProfile,
    macro_engine::{
        BlockReason, CommittedInputOutcome, InputDispatchFailure, InputDispatchOutcome,
        LiveActionInput, PreCommitInputBlock, SendInputFailure,
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
        Ok(self.observer.takeover_detected()?)
    }

    pub fn reset_baseline(&self) -> Result<()> {
        let baseline = cursor_position()?;
        self.observer.reset_baseline(MousePoint {
            x: baseline.x,
            y: baseline.y,
        })?;
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

    fn dispatch_live_action(
        &self,
        api: &dyn SendInputApi,
        destination: Point,
        button: MouseButton,
        movement: Option<&MouseMovementProfile>,
        stop: &dyn StopSource,
        commit: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
        validate_after_movement: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
    ) -> InputDispatchOutcome {
        let start = match cursor_position() {
            Ok(start) => start,
            Err(error) => {
                return InputDispatchOutcome::PreCommitBlocked(PreCommitInputBlock::InputFailure {
                    message: error.to_string(),
                });
            }
        };
        let screen = match virtual_screen_geometry() {
            Ok(screen) => screen,
            Err(message) => {
                return InputDispatchOutcome::PreCommitBlocked(PreCommitInputBlock::InputFailure {
                    message,
                });
            }
        };
        let (points, segment_delay) = movement_plan(start, destination, movement);
        let movement_inputs = points
            .into_iter()
            .map(|point| marked_move_input_for_screen(point, screen, self.marker))
            .collect::<Vec<_>>();
        let (down, up) = match button {
            MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        };
        let click_inputs = [
            marked_mouse_input(down, self.marker),
            marked_mouse_input(up, self.marker),
        ];
        let mut manual_takeover = || self.monitor.manual_takeover_detected();
        dispatch_planned_action(
            api,
            &movement_inputs,
            segment_delay,
            &click_inputs,
            stop,
            &mut manual_takeover,
            commit,
            validate_after_movement,
        )
    }
}

impl LiveActionInput for WindowsInputSink {
    fn reset_manual_baseline(&self) -> Result<()> {
        WindowsInputSink::reset_manual_baseline(self)
    }

    fn manual_takeover_detected(&self) -> Result<bool> {
        self.monitor.manual_takeover_detected()
    }

    fn dispatch_action(
        &self,
        point: Point,
        button: MouseButton,
        movement: Option<&MouseMovementProfile>,
        stop: &dyn StopSource,
        commit: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
        validate_after_movement: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
    ) -> InputDispatchOutcome {
        self.dispatch_live_action(
            &WindowsSendInputApi,
            point,
            button,
            movement,
            stop,
            commit,
            validate_after_movement,
        )
    }
}

fn cursor_position() -> Result<Point> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }?;
    Ok(Point::new(point.x, point.y))
}

#[derive(Debug, Clone, Copy)]
struct VirtualScreenGeometry {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

fn virtual_screen_geometry() -> std::result::Result<VirtualScreenGeometry, String> {
    let screen = VirtualScreenGeometry {
        x: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
        y: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
        width: unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) },
        height: unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) },
    };
    if screen.width <= 1 || screen.height <= 1 {
        Err("virtual screen geometry is not actionable".to_string())
    } else {
        Ok(screen)
    }
}

fn movement_plan(
    start: Point,
    destination: Point,
    movement: Option<&MouseMovementProfile>,
) -> (Vec<Point>, Duration) {
    let duration_ms = movement
        .filter(|profile| profile.is_usable())
        .map_or(0, |profile| profile.duration_ms.clamp(1, 2_000));
    let segments = if duration_ms == 0 {
        1
    } else {
        (duration_ms / 8).clamp(2, 90)
    };
    let delay = if duration_ms == 0 {
        Duration::ZERO
    } else {
        Duration::from_millis((duration_ms / segments).max(1))
    };
    let dx = i64::from(destination.x) - i64::from(start.x);
    let dy = i64::from(destination.y) - i64::from(start.y);
    let points = (1..=segments)
        .map(|segment| {
            let progress = segment as f64 / segments as f64;
            Point::new(
                (f64::from(start.x) + dx as f64 * progress).round() as i32,
                (f64::from(start.y) + dy as f64 * progress).round() as i32,
            )
        })
        .collect();
    (points, delay)
}

fn marked_move_input_for_screen(
    point: Point,
    screen: VirtualScreenGeometry,
    marker: SessionInputMarker,
) -> INPUT {
    let normalized_x = ((i64::from(point.x) - i64::from(screen.x)) * 65_535)
        .checked_div(i64::from(screen.width - 1))
        .and_then(|value| i32::try_from(value.clamp(0, 65_535)).ok())
        .unwrap_or(0);
    let normalized_y = ((i64::from(point.y) - i64::from(screen.y)) * 65_535)
        .checked_div(i64::from(screen.height - 1))
        .and_then(|value| i32::try_from(value.clamp(0, 65_535)).ok())
        .unwrap_or(0);
    let mut input = marked_mouse_input(
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        marker,
    );
    input.Anonymous.mi.dx = normalized_x;
    input.Anonymous.mi.dy = normalized_y;
    input
}

fn dispatch_planned_action(
    api: &dyn SendInputApi,
    movement_inputs: &[INPUT],
    segment_delay: Duration,
    click_inputs: &[INPUT],
    stop: &dyn StopSource,
    manual_takeover: &mut dyn FnMut() -> Result<bool>,
    commit: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
    validate_after_movement: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
) -> InputDispatchOutcome {
    let first_movement = &movement_inputs[..1];
    if stop.is_stopped() {
        return InputDispatchOutcome::PreCommitBlocked(PreCommitInputBlock::Stopped);
    }
    match manual_takeover() {
        Ok(true) => {
            return InputDispatchOutcome::PreCommitBlocked(PreCommitInputBlock::ManualTakeover);
        }
        Ok(false) => {}
        Err(error) => {
            return InputDispatchOutcome::PreCommitBlocked(PreCommitInputBlock::InputFailure {
                message: error.to_string(),
            });
        }
    }
    if stop.is_stopped() {
        return InputDispatchOutcome::PreCommitBlocked(PreCommitInputBlock::Stopped);
    }
    if let Err(reason) = commit() {
        return InputDispatchOutcome::PreCommitBlocked(PreCommitInputBlock::Commit { reason });
    }

    if let Err(failure) = dispatch_inputs(api, first_movement) {
        return uncertain(InputDispatchFailure::SendInput(failure));
    }
    for input in &movement_inputs[1..] {
        if !segment_delay.is_zero() {
            thread::sleep(segment_delay);
        }
        if stop.is_stopped() {
            return uncertain(InputDispatchFailure::Stopped);
        }
        match manual_takeover() {
            Ok(true) => return uncertain(InputDispatchFailure::ManualTakeover),
            Ok(false) => {}
            Err(error) => {
                return uncertain(InputDispatchFailure::InputFailure {
                    message: error.to_string(),
                });
            }
        }
        if let Err(failure) = dispatch_inputs(api, std::slice::from_ref(input)) {
            return uncertain(InputDispatchFailure::SendInput(failure));
        }
    }
    if stop.is_stopped() {
        return uncertain(InputDispatchFailure::Stopped);
    }
    match manual_takeover() {
        Ok(true) => return uncertain(InputDispatchFailure::ManualTakeover),
        Ok(false) => {}
        Err(error) => {
            return uncertain(InputDispatchFailure::InputFailure {
                message: error.to_string(),
            });
        }
    }
    if let Err(reason) = validate_after_movement() {
        return uncertain(InputDispatchFailure::Validation { reason });
    }
    if stop.is_stopped() {
        return uncertain(InputDispatchFailure::Stopped);
    }
    match manual_takeover() {
        Ok(true) => return uncertain(InputDispatchFailure::ManualTakeover),
        Ok(false) => {}
        Err(error) => {
            return uncertain(InputDispatchFailure::InputFailure {
                message: error.to_string(),
            });
        }
    }
    match dispatch_inputs(api, click_inputs) {
        Ok(()) => {
            if stop.is_stopped() {
                return uncertain(InputDispatchFailure::Stopped);
            }
            match manual_takeover() {
                Ok(true) => return uncertain(InputDispatchFailure::ManualTakeover),
                Ok(false) => {}
                Err(error) => {
                    return uncertain(InputDispatchFailure::InputFailure {
                        message: error.to_string(),
                    });
                }
            }
            if let Err(reason) = validate_after_movement() {
                return uncertain(InputDispatchFailure::Validation { reason });
            }
            InputDispatchOutcome::Committed(CommittedInputOutcome::Dispatched)
        }
        Err(failure) => uncertain(InputDispatchFailure::SendInput(failure)),
    }
}

fn uncertain(failure: InputDispatchFailure) -> InputDispatchOutcome {
    InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch { failure })
}

trait SendInputApi {
    fn dispatch(&self, inputs: &[INPUT]) -> std::result::Result<(), SendInputFailure>;
}

struct WindowsSendInputApi;

impl SendInputApi for WindowsSendInputApi {
    fn dispatch(&self, inputs: &[INPUT]) -> std::result::Result<(), SendInputFailure> {
        unsafe {
            SetLastError(WIN32_ERROR(0));
            let inserted = SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
            let error_code = GetLastError().0;
            classify_send_input_result(inserted, inputs.len(), error_code)
        }
    }
}

fn classify_send_input_result(
    inserted: u32,
    input_count: usize,
    error_code: u32,
) -> std::result::Result<(), SendInputFailure> {
    let expected = u32::try_from(input_count).unwrap_or(u32::MAX);
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

fn dispatch_inputs(
    api: &dyn SendInputApi,
    inputs: &[INPUT],
) -> std::result::Result<(), SendInputFailure> {
    api.dispatch(inputs)
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
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::Duration,
    };

    use anyhow::{Result, bail};

    use super::{
        ManualInputMonitor, SendInputApi, WindowsInputSink, classify_send_input_result,
        dispatch_inputs, dispatch_planned_action, marked_mouse_input,
    };
    use crate::engine::platform::windows_mouse_hook::{
        MouseActivitySnapshot, MouseEventSource, MouseEventSourceFailure, MouseEventSourceHealth,
        MousePoint, SessionInputMarker,
    };

    use crate::engine::{
        automation::{Clock, MouseButton, StopSource, TargetGuard, TargetSnapshot},
        macro_engine::{
            ActionAttemptId, ActionAuthorization, ActionCommitter, ActionCommitterCreateError,
            ActionOutcome, ActionPrepareRequest, ActionState, BlockReason, CommitContext,
            CommittedInputOutcome, InputDispatchFailure, InputDispatchOutcome, Limit,
            LiveActionInput, LiveActionSession, LiveControlSink, MovementOutcome, ObservationToken,
            PreCommitInputBlock, ResumeAuthorization, SendInputFailure, TakeoverPolicy,
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

    struct ScriptedClock(Mutex<VecDeque<u64>>);
    impl Clock for ScriptedClock {
        fn now_ms(&self) -> u64 {
            let mut times = self.0.lock().unwrap();
            if times.len() > 1 {
                times.pop_front().unwrap()
            } else {
                *times.front().expect("scripted clock is empty")
            }
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

    struct StoppingTarget {
        stop: Arc<Stop>,
        validation_count: AtomicU64,
        stop_on_validation: u64,
    }

    impl TargetGuard for StoppingTarget {
        fn snapshot(&self) -> Result<TargetSnapshot> {
            Ok(target())
        }

        fn validate(&self, _expected: &TargetSnapshot) -> Result<()> {
            let validation = self.validation_count.fetch_add(1, Ordering::AcqRel) + 1;
            if validation == self.stop_on_validation {
                self.stop.0.store(true, Ordering::Release);
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingInput {
        calls: Mutex<Vec<&'static str>>,
        baseline_resets: AtomicU64,
        takeover: AtomicBool,
        takeover_checks: AtomicU64,
        takeover_on_check: AtomicU64,
        fail_dispatch: AtomicBool,
        block_after_commit: AtomicBool,
        movement_failure: Mutex<Option<SendInputFailure>>,
        stop_during_move: Mutex<Option<Arc<Stop>>>,
        stop_during_dispatch: Mutex<Option<Arc<Stop>>>,
        movement_outcome: Mutex<Option<MovementOutcome>>,
    }

    impl LiveActionInput for RecordingInput {
        fn reset_manual_baseline(&self) -> Result<()> {
            self.baseline_resets.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        fn manual_takeover_detected(&self) -> Result<bool> {
            let check = self.takeover_checks.fetch_add(1, Ordering::AcqRel) + 1;
            Ok(self.takeover.load(Ordering::Acquire)
                || self.takeover_on_check.load(Ordering::Acquire) == check)
        }
        fn dispatch_action(
            &self,
            _point: Point,
            _button: MouseButton,
            _movement: Option<&crate::engine::config::MouseMovementProfile>,
            stop: &dyn StopSource,
            commit: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
            validate_after_movement: &mut dyn FnMut() -> std::result::Result<(), BlockReason>,
        ) -> InputDispatchOutcome {
            if stop.is_stopped() {
                return InputDispatchOutcome::PreCommitBlocked(PreCommitInputBlock::Stopped);
            }
            match self.manual_takeover_detected() {
                Ok(true) => {
                    return InputDispatchOutcome::PreCommitBlocked(
                        PreCommitInputBlock::ManualTakeover,
                    );
                }
                Ok(false) => {}
                Err(error) => {
                    return InputDispatchOutcome::PreCommitBlocked(
                        PreCommitInputBlock::InputFailure {
                            message: error.to_string(),
                        },
                    );
                }
            }
            if stop.is_stopped() {
                return InputDispatchOutcome::PreCommitBlocked(PreCommitInputBlock::Stopped);
            }
            if let Err(reason) = commit() {
                return InputDispatchOutcome::PreCommitBlocked(PreCommitInputBlock::Commit {
                    reason,
                });
            }
            self.calls.lock().unwrap().push("move");
            if let Some(stop) = self.stop_during_move.lock().unwrap().as_ref() {
                stop.0.store(true, Ordering::Release);
            }
            if let Some(failure) = self.movement_failure.lock().unwrap().take() {
                return InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                    failure: InputDispatchFailure::SendInput(failure),
                });
            }
            match self
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
                }) {
                MovementOutcome::Reached => {}
                MovementOutcome::Cancelled => {
                    return InputDispatchOutcome::Committed(
                        CommittedInputOutcome::UncertainDispatch {
                            failure: InputDispatchFailure::Stopped,
                        },
                    );
                }
                MovementOutcome::ManualTakeover => {
                    return InputDispatchOutcome::Committed(
                        CommittedInputOutcome::UncertainDispatch {
                            failure: InputDispatchFailure::ManualTakeover,
                        },
                    );
                }
            }
            if let Err(reason) = validate_after_movement() {
                return InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                    failure: InputDispatchFailure::Validation { reason },
                });
            }
            if self.block_after_commit.load(Ordering::Acquire) {
                return InputDispatchOutcome::PreCommitBlocked(PreCommitInputBlock::Stopped);
            }
            self.calls.lock().unwrap().push("dispatch");
            if let Some(stop) = self.stop_during_dispatch.lock().unwrap().as_ref() {
                stop.0.store(true, Ordering::Release);
                return InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                    failure: InputDispatchFailure::Stopped,
                });
            }
            if self.fail_dispatch.load(Ordering::Acquire) {
                return InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                    failure: InputDispatchFailure::SendInput(SendInputFailure::ZeroInsertion {
                        expected: 2,
                        error_code: 5,
                    }),
                });
            }
            InputDispatchOutcome::Committed(CommittedInputOutcome::Dispatched)
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
        .unwrap()
    }

    #[test]
    fn same_session_run_rejects_a_second_committer_and_keeps_one_replay_budget() {
        let input = Arc::new(RecordingInput::default());
        let session = LiveActionSession::new(
            Arc::new(ScriptedTarget(Mutex::new(vec![target()]))),
            input.clone(),
            Arc::new(RecordingControl::default()),
        );
        session.activate_for_test(target());
        let first = ActionCommitter::new(
            session.clone(),
            Arc::new(FixedClock(100)),
            "run-1",
            Limit::Finite(1),
            8,
        )
        .unwrap();
        let cloned_attempt = request();
        let replay = cloned_attempt.clone();

        assert!(matches!(
            ActionCommitter::new(
                session,
                Arc::new(FixedClock(100)),
                "run-1",
                Limit::Unlimited,
                128,
            ),
            Err(ActionCommitterCreateError::RunAlreadyRegistered { run_id }) if run_id == "run-1"
        ));

        let prepared = first.prepare(cloned_attempt).unwrap();
        assert!(matches!(
            first.commit(
                prepared,
                &Stop::default(),
                CommitContext::new("run-1", 4, Some(token())),
            ),
            ActionOutcome::Dispatched { .. }
        ));
        assert!(matches!(
            first.prepare(replay),
            Err(BlockReason::AttemptReplay)
        ));
        let mut over_budget = request();
        over_budget.set_minimum_click_interval_for_test(0);
        let prepared = first.prepare(over_budget).unwrap();
        assert!(matches!(
            first.commit(
                prepared,
                &Stop::default(),
                CommitContext::new("run-1", 4, Some(token())),
            ),
            ActionOutcome::Blocked {
                reason: BlockReason::ClickBudgetExceeded,
                ..
            }
        ));
        assert_eq!(first.committed_clicks(), 1);
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
    fn committer_registration_preserves_distinct_runs_and_sessions() {
        let session = LiveActionSession::new(
            Arc::new(ScriptedTarget(Mutex::new(vec![target()]))),
            Arc::new(RecordingInput::default()),
            Arc::new(RecordingControl::default()),
        );
        let run_one = ActionCommitter::new(
            session.clone(),
            Arc::new(FixedClock(100)),
            "run-1",
            Limit::Finite(1),
            8,
        );
        let run_two = ActionCommitter::new(
            session,
            Arc::new(FixedClock(100)),
            "run-2",
            Limit::Finite(1),
            8,
        );
        let other_session = LiveActionSession::new(
            Arc::new(ScriptedTarget(Mutex::new(vec![target()]))),
            Arc::new(RecordingInput::default()),
            Arc::new(RecordingControl::default()),
        );
        let same_run_elsewhere = ActionCommitter::new(
            other_session,
            Arc::new(FixedClock(100)),
            "run-1",
            Limit::Finite(1),
            8,
        );

        assert!(run_one.is_ok());
        assert!(run_two.is_ok());
        assert!(same_run_elsewhere.is_ok());
    }

    #[test]
    fn committer_registration_does_not_retain_a_dropped_session() {
        let session = LiveActionSession::new(
            Arc::new(ScriptedTarget(Mutex::new(vec![target()]))),
            Arc::new(RecordingInput::default()),
            Arc::new(RecordingControl::default()),
        );
        let weak_session = Arc::downgrade(&session);
        let committer = ActionCommitter::new(
            session.clone(),
            Arc::new(FixedClock(100)),
            "run-1",
            Limit::Finite(1),
            8,
        )
        .unwrap();
        drop(committer);

        assert!(matches!(
            ActionCommitter::new(
                session.clone(),
                Arc::new(FixedClock(100)),
                "run-1",
                Limit::Finite(1),
                8,
            ),
            Err(ActionCommitterCreateError::RunAlreadyRegistered { .. })
        ));
        drop(session);

        assert!(weak_session.upgrade().is_none());
    }

    #[test]
    fn zero_attempt_capacity_is_a_typed_constructor_error() {
        let session = LiveActionSession::new(
            Arc::new(ScriptedTarget(Mutex::new(vec![target()]))),
            Arc::new(RecordingInput::default()),
            Arc::new(RecordingControl::default()),
        );

        assert!(matches!(
            ActionCommitter::new(
                session,
                Arc::new(FixedClock(100)),
                "run-1",
                Limit::Finite(1),
                0,
            ),
            Err(ActionCommitterCreateError::ZeroAttemptCapacity)
        ));
    }

    #[test]
    fn live_macro_sink_cannot_bypass_committer_via_legacy_input_sink() {
        let live_source = include_str!("windows_input.rs");
        let legacy_impl = ["impl ", "InputSink", " for ", "WindowsInputSink"].concat();
        let enchant_source = include_str!("windows_impl.rs");
        let guarded_enchant_impl = ["impl ", "InputSink", " for ", "SendInputController"].concat();
        let main_source = include_str!("../../main.rs");
        let removed_distance_mismatch = ["fn ", "point_distance_squared"].concat();

        assert!(!live_source.contains(&legacy_impl));
        assert!(!live_source.contains(&removed_distance_mismatch));
        assert!(enchant_source.contains(&guarded_enchant_impl));
        assert!(main_source.contains("SendInputController"));
    }

    #[test]
    fn remaining_live_input_geometry_handles_extreme_i32_endpoints() {
        let start = Point::new(i32::MIN, i32::MIN);
        let destination = Point::new(i32::MAX, i32::MAX);

        let (points, delay) = super::movement_plan(start, destination, None);

        assert_eq!(points, vec![destination]);
        assert_eq!(delay, Duration::ZERO);
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
    fn stop_raised_during_expensive_target_preflight_is_caught_before_commit() {
        let stop = Arc::new(Stop::default());
        let input = Arc::new(RecordingInput::default());
        let session = LiveActionSession::new(
            Arc::new(StoppingTarget {
                stop: stop.clone(),
                validation_count: AtomicU64::new(0),
                stop_on_validation: 2,
            }),
            input.clone(),
            Arc::new(RecordingControl::default()),
        );
        session.activate_for_test(target());
        let committer = ActionCommitter::new(
            session,
            Arc::new(FixedClock(100)),
            "run-1",
            Limit::Finite(10),
            8,
        )
        .unwrap();
        let prepared = committer.prepare(request()).unwrap();

        let result = committer.commit(
            prepared,
            stop.as_ref(),
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
        assert_eq!(committer.committed_clicks(), 0);
    }

    #[test]
    fn sequenced_manual_event_immediately_before_boundary_blocks_without_input() {
        let input = Arc::new(RecordingInput::default());
        input.takeover_on_check.store(2, Ordering::Release);
        let committer = committer(vec![target(), target()], input.clone());
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
        assert_eq!(committer.committed_clicks(), 0);
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
    fn stop_between_movement_segments_is_uncertain_and_consumes_budget() {
        let stop = Arc::new(Stop::default());
        let input = Arc::new(RecordingInput::default());
        *input.stop_during_move.lock().unwrap() = Some(stop.clone());
        let committer = committer(vec![target(), target()], input.clone());
        let prepared = committer.prepare(request()).unwrap();

        let result = committer.commit(
            prepared,
            stop.as_ref(),
            CommitContext::new("run-1", 4, Some(token())),
        );

        assert!(matches!(result, ActionOutcome::UncertainDispatch { .. }));
        assert_eq!(*input.calls.lock().unwrap(), vec!["move"]);
        assert_eq!(committer.committed_clicks(), 1);
    }

    #[test]
    fn first_movement_zero_or_partial_is_uncertain_and_replay_protected() {
        for failure in [
            SendInputFailure::ZeroInsertion {
                expected: 1,
                error_code: 5,
            },
            // A real one-event Win32 movement cannot naturally insert a positive count below
            // expected. The adapter seam still injects this typed result to prove the state
            // machine cannot misclassify a first-call partial result as retryable.
            SendInputFailure::PartialInsertion {
                inserted: 1,
                expected: 2,
                error_code: 87,
            },
        ] {
            let input = Arc::new(RecordingInput::default());
            *input.movement_failure.lock().unwrap() = Some(failure);
            let committer = committer(vec![target()], input);
            let mut original = request();
            original.set_minimum_click_interval_for_test(0);
            let replay = original.clone();
            let prepared = committer.prepare(original).unwrap();

            let result = committer.commit(
                prepared,
                &Stop::default(),
                CommitContext::new("run-1", 4, Some(token())),
            );

            assert!(matches!(result, ActionOutcome::UncertainDispatch { .. }));
            assert_eq!(committer.committed_clicks(), 1);
            assert!(matches!(
                committer.prepare(replay),
                Err(BlockReason::AttemptReplay)
            ));
        }
    }

    #[test]
    fn postcommit_blocked_trait_outcome_is_not_misclassified_as_retryable() {
        let input = Arc::new(RecordingInput::default());
        input.block_after_commit.store(true, Ordering::Release);
        let committer = committer(vec![target(), target()], input);
        let prepared = committer.prepare(request()).unwrap();

        let result = committer.commit(
            prepared,
            &Stop::default(),
            CommitContext::new("run-1", 4, Some(token())),
        );

        assert!(matches!(result, ActionOutcome::UncertainDispatch { .. }));
        assert_eq!(committer.committed_clicks(), 1);
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
    fn target_change_after_first_movement_is_uncertain_and_consumes_budget() {
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

        assert!(matches!(result, ActionOutcome::UncertainDispatch { .. }));
        assert_eq!(*input.calls.lock().unwrap(), vec!["move"]);
        assert_eq!(committer.committed_clicks(), 1);
    }

    #[test]
    fn token_invalidation_after_first_movement_is_uncertain_and_consumes_budget() {
        let input = Arc::new(RecordingInput::default());
        let session = LiveActionSession::new(
            Arc::new(ScriptedTarget(Mutex::new(vec![
                target(),
                target(),
                target(),
            ]))),
            input.clone(),
            Arc::new(RecordingControl::default()),
        );
        session.activate_for_test(target());
        let committer = ActionCommitter::new(
            session,
            Arc::new(ScriptedClock(Mutex::new([100, 100, 100, 2_000].into()))),
            "run-1",
            Limit::Finite(10),
            8,
        )
        .unwrap();
        let prepared = committer.prepare(request()).unwrap();

        let result = committer.commit(
            prepared,
            &Stop::default(),
            CommitContext::new("run-1", 4, Some(token())),
        );

        assert!(matches!(result, ActionOutcome::UncertainDispatch { .. }));
        assert_eq!(*input.calls.lock().unwrap(), vec!["move"]);
        assert_eq!(committer.committed_clicks(), 1);
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
    fn takeover_after_first_movement_is_uncertain_and_uses_selected_policy() {
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

        assert!(matches!(result, ActionOutcome::UncertainDispatch { .. }));
        assert_eq!(*input.calls.lock().unwrap(), vec!["move"]);
        assert_eq!(committer.committed_clicks(), 1);
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
        )
        .unwrap();
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
        )
        .unwrap();
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

    struct ScriptedSendInputApi {
        outcomes: Mutex<VecDeque<std::result::Result<(), SendInputFailure>>>,
        call_lengths: Mutex<Vec<usize>>,
        stop_after_call: Option<(usize, Arc<Stop>)>,
        manual_after_call: Option<(usize, Arc<AtomicBool>)>,
    }

    impl ScriptedSendInputApi {
        fn new(outcomes: Vec<std::result::Result<(), SendInputFailure>>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into()),
                call_lengths: Mutex::new(Vec::new()),
                stop_after_call: None,
                manual_after_call: None,
            }
        }
    }

    impl SendInputApi for ScriptedSendInputApi {
        fn dispatch(
            &self,
            inputs: &[windows::Win32::UI::Input::KeyboardAndMouse::INPUT],
        ) -> std::result::Result<(), SendInputFailure> {
            let call = {
                let mut calls = self.call_lengths.lock().unwrap();
                calls.push(inputs.len());
                calls.len()
            };
            if let Some((stop_call, stop)) = &self.stop_after_call
                && call == *stop_call
            {
                stop.0.store(true, Ordering::Release);
            }
            if let Some((manual_call, manual)) = &self.manual_after_call
                && call == *manual_call
            {
                manual.store(true, Ordering::Release);
            }
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("missing scripted SendInput outcome")
        }
    }

    impl SendInputApi for FakeSendInputApi {
        fn dispatch(
            &self,
            inputs: &[windows::Win32::UI::Input::KeyboardAndMouse::INPUT],
        ) -> std::result::Result<(), SendInputFailure> {
            classify_send_input_result(self.inserted, inputs.len(), self.error_code)
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

        fn health(&self) -> MouseEventSourceHealth {
            MouseEventSourceHealth::Running
        }
    }

    struct FailingHealthMouseEventSource {
        marker: SessionInputMarker,
        checks: AtomicU64,
        fail_on_check: u64,
    }

    impl MouseEventSource for FailingHealthMouseEventSource {
        fn session_marker(&self) -> SessionInputMarker {
            self.marker
        }

        fn snapshot(&self) -> MouseActivitySnapshot {
            MouseActivitySnapshot::default()
        }

        fn reset_movement_baseline(&self, _point: MousePoint) -> MouseActivitySnapshot {
            MouseActivitySnapshot::default()
        }

        fn health(&self) -> MouseEventSourceHealth {
            let check = self.checks.fetch_add(1, Ordering::AcqRel) + 1;
            if check == self.fail_on_check {
                MouseEventSourceHealth::Failed(MouseEventSourceFailure::WorkerPanicked)
            } else {
                MouseEventSourceHealth::Running
            }
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

    #[test]
    fn planned_action_first_movement_zero_and_typed_partial_are_uncertain() {
        for failure in [
            SendInputFailure::ZeroInsertion {
                expected: 1,
                error_code: 5,
            },
            // One movement INPUT cannot naturally produce inserted < expected. Injecting the
            // typed adapter result proves the committed state machine still treats it as
            // uncertainty without batching movement or weakening between-segment checks.
            SendInputFailure::PartialInsertion {
                inserted: 1,
                expected: 2,
                error_code: 87,
            },
        ] {
            let api = ScriptedSendInputApi::new(vec![Err(failure.clone())]);
            let movement = [marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
                marker(41),
            )];
            let click = [
                marked_mouse_input(
                    windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTDOWN,
                    marker(41),
                ),
                marked_mouse_input(
                    windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTUP,
                    marker(41),
                ),
            ];
            let committed = AtomicBool::new(false);
            let mut manual = || Ok(false);
            let mut commit = || {
                committed.store(true, Ordering::Release);
                Ok(())
            };
            let mut validate = || panic!("validation must not run after failed first movement");

            let result = dispatch_planned_action(
                &api,
                &movement,
                Duration::ZERO,
                &click,
                &Stop::default(),
                &mut manual,
                &mut commit,
                &mut validate,
            );

            assert_eq!(
                result,
                InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                    failure: InputDispatchFailure::SendInput(failure),
                })
            );
            assert!(committed.load(Ordering::Acquire));
            assert_eq!(*api.call_lengths.lock().unwrap(), vec![1]);
        }
    }

    #[test]
    fn planned_action_stop_between_segments_is_committed_uncertain() {
        let stop = Arc::new(Stop::default());
        let mut api = ScriptedSendInputApi::new(vec![Ok(())]);
        api.stop_after_call = Some((1, stop.clone()));
        let movement = [
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
                marker(42),
            ),
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
                marker(42),
            ),
        ];
        let click = [
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTDOWN,
                marker(42),
            ),
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTUP,
                marker(42),
            ),
        ];
        let mut manual = || Ok(false);
        let mut commit = || Ok(());
        let mut validate = || panic!("validation must not run after stop between segments");

        let result = dispatch_planned_action(
            &api,
            &movement,
            Duration::ZERO,
            &click,
            stop.as_ref(),
            &mut manual,
            &mut commit,
            &mut validate,
        );

        assert_eq!(
            result,
            InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                failure: InputDispatchFailure::Stopped,
            })
        );
        assert_eq!(*api.call_lengths.lock().unwrap(), vec![1]);
    }

    #[test]
    fn planned_action_focus_loss_after_movement_is_uncertain_before_click() {
        let api = ScriptedSendInputApi::new(vec![Ok(())]);
        let movement = [marked_mouse_input(
            windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
            marker(43),
        )];
        let click = [
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTDOWN,
                marker(43),
            ),
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTUP,
                marker(43),
            ),
        ];
        let mut manual = || Ok(false);
        let mut commit = || Ok(());
        let mut validate = || Err(BlockReason::TargetChanged);

        let result = dispatch_planned_action(
            &api,
            &movement,
            Duration::ZERO,
            &click,
            &Stop::default(),
            &mut manual,
            &mut commit,
            &mut validate,
        );

        assert_eq!(
            result,
            InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                failure: InputDispatchFailure::Validation {
                    reason: BlockReason::TargetChanged,
                },
            })
        );
        assert_eq!(*api.call_lengths.lock().unwrap(), vec![1]);
    }

    #[test]
    fn planned_action_requires_full_movement_and_click_dispatch() {
        let movement = [marked_mouse_input(
            windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
            marker(44),
        )];
        let click = [
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTDOWN,
                marker(44),
            ),
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTUP,
                marker(44),
            ),
        ];
        for (click_outcome, expected) in [
            (
                Ok(()),
                InputDispatchOutcome::Committed(CommittedInputOutcome::Dispatched),
            ),
            (
                Err(SendInputFailure::PartialInsertion {
                    inserted: 1,
                    expected: 2,
                    error_code: 5,
                }),
                InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                    failure: InputDispatchFailure::SendInput(SendInputFailure::PartialInsertion {
                        inserted: 1,
                        expected: 2,
                        error_code: 5,
                    }),
                }),
            ),
        ] {
            let api = ScriptedSendInputApi::new(vec![Ok(()), click_outcome]);
            let mut manual = || Ok(false);
            let mut commit = || Ok(());
            let mut validate = || Ok(());

            let result = dispatch_planned_action(
                &api,
                &movement,
                Duration::ZERO,
                &click,
                &Stop::default(),
                &mut manual,
                &mut commit,
                &mut validate,
            );

            assert_eq!(result, expected);
            assert_eq!(*api.call_lengths.lock().unwrap(), vec![1, 2]);
        }
    }

    #[test]
    fn planned_action_stop_during_click_syscall_is_committed_uncertain() {
        let stop = Arc::new(Stop::default());
        let mut api = ScriptedSendInputApi::new(vec![Ok(()), Ok(())]);
        api.stop_after_call = Some((2, stop.clone()));
        let movement = [marked_mouse_input(
            windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
            marker(46),
        )];
        let click = [
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTDOWN,
                marker(46),
            ),
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTUP,
                marker(46),
            ),
        ];
        let mut manual = || Ok(false);
        let mut commit = || Ok(());
        let mut validate = || Ok(());

        let result = dispatch_planned_action(
            &api,
            &movement,
            Duration::ZERO,
            &click,
            stop.as_ref(),
            &mut manual,
            &mut commit,
            &mut validate,
        );

        assert_eq!(
            result,
            InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                failure: InputDispatchFailure::Stopped,
            })
        );
        assert_eq!(*api.call_lengths.lock().unwrap(), vec![1, 2]);
    }

    #[test]
    fn planned_action_manual_takeover_during_click_syscall_is_committed_uncertain() {
        let manual_takeover = Arc::new(AtomicBool::new(false));
        let mut api = ScriptedSendInputApi::new(vec![Ok(()), Ok(())]);
        api.manual_after_call = Some((2, manual_takeover.clone()));
        let movement = [marked_mouse_input(
            windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
            marker(47),
        )];
        let click = [
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTDOWN,
                marker(47),
            ),
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTUP,
                marker(47),
            ),
        ];
        let mut manual = || Ok(manual_takeover.load(Ordering::Acquire));
        let mut commit = || Ok(());
        let mut validate = || Ok(());

        let result = dispatch_planned_action(
            &api,
            &movement,
            Duration::ZERO,
            &click,
            &Stop::default(),
            &mut manual,
            &mut commit,
            &mut validate,
        );

        assert_eq!(
            result,
            InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                failure: InputDispatchFailure::ManualTakeover,
            })
        );
        assert_eq!(*api.call_lengths.lock().unwrap(), vec![1, 2]);
    }

    #[test]
    fn planned_action_validation_failure_after_click_syscall_is_committed_uncertain() {
        let api = ScriptedSendInputApi::new(vec![Ok(()), Ok(())]);
        let movement = [marked_mouse_input(
            windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
            marker(48),
        )];
        let click = [
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTDOWN,
                marker(48),
            ),
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTUP,
                marker(48),
            ),
        ];
        let validations = AtomicU64::new(0);
        let mut manual = || Ok(false);
        let mut commit = || Ok(());
        let mut validate = || {
            if validations.fetch_add(1, Ordering::AcqRel) == 0 {
                Ok(())
            } else {
                Err(BlockReason::StaleObservation)
            }
        };

        let result = dispatch_planned_action(
            &api,
            &movement,
            Duration::ZERO,
            &click,
            &Stop::default(),
            &mut manual,
            &mut commit,
            &mut validate,
        );

        assert_eq!(
            result,
            InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                failure: InputDispatchFailure::Validation {
                    reason: BlockReason::StaleObservation,
                },
            })
        );
        assert_eq!(*api.call_lengths.lock().unwrap(), vec![1, 2]);
    }

    #[test]
    fn planned_action_manual_takeover_between_segments_is_committed_uncertain() {
        let api = ScriptedSendInputApi::new(vec![Ok(())]);
        let movement = [
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
                marker(45),
            ),
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
                marker(45),
            ),
        ];
        let click = [
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTDOWN,
                marker(45),
            ),
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTUP,
                marker(45),
            ),
        ];
        let manual_checks = AtomicU64::new(0);
        let mut manual = || Ok(manual_checks.fetch_add(1, Ordering::AcqRel) > 0);
        let mut commit = || Ok(());
        let mut validate = || panic!("validation must not run after manual takeover");

        let result = dispatch_planned_action(
            &api,
            &movement,
            Duration::ZERO,
            &click,
            &Stop::default(),
            &mut manual,
            &mut commit,
            &mut validate,
        );

        assert_eq!(
            result,
            InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                failure: InputDispatchFailure::ManualTakeover,
            })
        );
        assert_eq!(*api.call_lengths.lock().unwrap(), vec![1]);
    }

    #[test]
    fn hook_health_failure_before_commit_blocks_without_input() {
        let monitor =
            ManualInputMonitor::with_event_source(Arc::new(FailingHealthMouseEventSource {
                marker: marker(51),
                checks: AtomicU64::new(0),
                fail_on_check: 1,
            }));
        let api = ScriptedSendInputApi::new(Vec::new());
        let movement = [marked_mouse_input(
            windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
            marker(51),
        )];
        let click = [
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTDOWN,
                marker(51),
            ),
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTUP,
                marker(51),
            ),
        ];
        let committed = AtomicBool::new(false);
        let mut health_check = || monitor.manual_takeover_detected();
        let mut commit = || {
            committed.store(true, Ordering::Release);
            Ok(())
        };
        let mut validate = || Ok(());

        let result = dispatch_planned_action(
            &api,
            &movement,
            Duration::ZERO,
            &click,
            &Stop::default(),
            &mut health_check,
            &mut commit,
            &mut validate,
        );

        assert!(matches!(
            result,
            InputDispatchOutcome::PreCommitBlocked(PreCommitInputBlock::InputFailure {
                message
            }) if message.contains("panicked")
        ));
        assert!(!committed.load(Ordering::Acquire));
        assert!(api.call_lengths.lock().unwrap().is_empty());
    }

    #[test]
    fn hook_health_failure_between_segments_is_committed_uncertain() {
        let monitor =
            ManualInputMonitor::with_event_source(Arc::new(FailingHealthMouseEventSource {
                marker: marker(52),
                checks: AtomicU64::new(0),
                fail_on_check: 2,
            }));
        let api = ScriptedSendInputApi::new(vec![Ok(())]);
        let movement = [
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
                marker(52),
            ),
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE,
                marker(52),
            ),
        ];
        let click = [
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTDOWN,
                marker(52),
            ),
            marked_mouse_input(
                windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_LEFTUP,
                marker(52),
            ),
        ];
        let mut health_check = || monitor.manual_takeover_detected();
        let mut commit = || Ok(());
        let mut validate = || panic!("validation must not run after hook health failure");

        let result = dispatch_planned_action(
            &api,
            &movement,
            Duration::ZERO,
            &click,
            &Stop::default(),
            &mut health_check,
            &mut commit,
            &mut validate,
        );

        assert!(matches!(
            result,
            InputDispatchOutcome::Committed(CommittedInputOutcome::UncertainDispatch {
                failure: InputDispatchFailure::InputFailure { message }
            }) if message.contains("panicked")
        ));
        assert_eq!(*api.call_lengths.lock().unwrap(), vec![1]);
    }

    fn marker(value: usize) -> SessionInputMarker {
        SessionInputMarker::generate_with(|bytes| {
            bytes.copy_from_slice(&value.to_ne_bytes());
            true
        })
        .unwrap()
    }
}
