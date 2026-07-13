use std::{
    io,
    num::NonZeroUsize,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};
use windows::Win32::Security::Cryptography::SystemPrng;
use windows::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, HC_ACTION, HHOOK, LLMHF_INJECTED, LLMHF_LOWER_IL_INJECTED,
        MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
        UnhookWindowsHookEx, WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
        WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN,
        WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionInputMarker(NonZeroUsize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SessionInputMarkerError {
    #[error("the operating system failed to generate a session input marker")]
    OsFailure,
    #[error("the operating system generated an invalid all-zero session input marker")]
    AllZero,
}

impl SessionInputMarker {
    pub fn generate() -> Result<Self, SessionInputMarkerError> {
        Self::generate_with(|bytes| unsafe { SystemPrng(bytes).as_bool() })
    }

    pub(crate) fn generate_with(
        mut fill: impl FnMut(&mut [u8]) -> bool,
    ) -> Result<Self, SessionInputMarkerError> {
        let mut bytes = [0; std::mem::size_of::<usize>()];
        if !fill(&mut bytes) {
            return Err(SessionInputMarkerError::OsFailure);
        }
        NonZeroUsize::new(usize::from_ne_bytes(bytes))
            .map(Self)
            .ok_or(SessionInputMarkerError::AllZero)
    }

    pub fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MousePoint {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    Move,
    LeftDown,
    LeftUp,
    RightDown,
    RightUp,
    MiddleDown,
    MiddleUp,
    XDown(u16),
    XUp(u16),
    VerticalWheel(i16),
    HorizontalWheel(i16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventOrigin {
    Physical,
    ExternalInjected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawMouseEvent {
    pub kind: MouseEventKind,
    pub point: MousePoint,
    pub flags: u32,
    pub extra_info: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequencedMouseEvent {
    pub sequence: u64,
    pub kind: MouseEventKind,
    pub point: MousePoint,
    pub origin: MouseEventOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseActivitySnapshot {
    pub sequence: u64,
    pub last_origin: Option<MouseEventOrigin>,
    pub last_event: Option<SequencedMouseEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MouseEventSourceFailure {
    #[error("mouse hook worker panicked")]
    WorkerPanicked,
    #[error("mouse hook worker exited unexpectedly")]
    UnexpectedExit,
    #[error("mouse hook worker failed: {message}")]
    WorkerError { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseEventSourceHealth {
    Starting,
    Running,
    ShutdownRequested,
    Stopped,
    Failed(MouseEventSourceFailure),
}

impl MouseEventSourceHealth {
    fn ensure_running(&self) -> Result<(), MouseEventSourceHealthError> {
        match self {
            Self::Running => Ok(()),
            Self::Starting => Err(MouseEventSourceHealthError::Starting),
            Self::ShutdownRequested => Err(MouseEventSourceHealthError::ShutdownRequested),
            Self::Stopped => Err(MouseEventSourceHealthError::Stopped),
            Self::Failed(failure) => Err(MouseEventSourceHealthError::Failed(failure.clone())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MouseEventSourceHealthError {
    #[error("mouse hook worker is still starting")]
    Starting,
    #[error("mouse hook worker is shutting down")]
    ShutdownRequested,
    #[error("mouse hook worker has stopped")]
    Stopped,
    #[error(transparent)]
    Failed(MouseEventSourceFailure),
}

pub trait MouseEventSource: Send + Sync {
    fn session_marker(&self) -> SessionInputMarker;
    fn snapshot(&self) -> MouseActivitySnapshot;
    fn reset_movement_baseline(&self, point: MousePoint) -> MouseActivitySnapshot;
    fn health(&self) -> MouseEventSourceHealth;
}

pub struct ManualMouseActivityObserver {
    source: Arc<dyn MouseEventSource>,
    baseline: AtomicU64,
}

impl ManualMouseActivityObserver {
    pub fn new(source: Arc<dyn MouseEventSource>) -> Self {
        let baseline = source.snapshot().sequence;
        Self {
            source,
            baseline: AtomicU64::new(baseline),
        }
    }

    pub fn takeover_detected(&self) -> Result<bool, MouseEventSourceHealthError> {
        self.source.health().ensure_running()?;
        Ok(self.source.snapshot().sequence != self.baseline.load(Ordering::Acquire))
    }

    pub fn reset_baseline(&self, point: MousePoint) -> Result<(), MouseEventSourceHealthError> {
        self.source.health().ensure_running()?;
        let snapshot = self.source.reset_movement_baseline(point);
        self.baseline.store(snapshot.sequence, Ordering::Release);
        Ok(())
    }
}

#[derive(Debug)]
struct MouseActivityLedger {
    marker: SessionInputMarker,
    movement_threshold_squared: i128,
    state: Mutex<MouseActivityState>,
}

#[derive(Debug)]
struct MouseActivityState {
    snapshot: MouseActivitySnapshot,
    movement_baseline: MousePoint,
}

impl MouseActivityLedger {
    fn new(marker: SessionInputMarker, movement_baseline: MousePoint, threshold_px: i32) -> Self {
        Self {
            marker,
            movement_threshold_squared: i128::from(threshold_px.max(1)).pow(2),
            state: Mutex::new(MouseActivityState {
                snapshot: MouseActivitySnapshot::default(),
                movement_baseline,
            }),
        }
    }

    fn observe(&self, raw: RawMouseEvent) {
        let injected = raw.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) != 0;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if injected && raw.extra_info == self.marker.get() {
            if raw.kind == MouseEventKind::Move {
                state.movement_baseline = raw.point;
            }
            return;
        }

        let origin = if injected {
            MouseEventOrigin::ExternalInjected
        } else {
            MouseEventOrigin::Physical
        };
        if raw.kind == MouseEventKind::Move {
            if point_distance_squared(raw.point, state.movement_baseline)
                < self.movement_threshold_squared
            {
                return;
            }
            state.movement_baseline = raw.point;
        }

        let sequence = state.snapshot.sequence.wrapping_add(1);
        state.snapshot.sequence = sequence;
        state.snapshot.last_origin = Some(origin);
        state.snapshot.last_event = Some(SequencedMouseEvent {
            sequence,
            kind: raw.kind,
            point: raw.point,
            origin,
        });
    }

    fn snapshot(&self) -> MouseActivitySnapshot {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot
    }

    fn reset_movement_baseline(&self, point: MousePoint) -> MouseActivitySnapshot {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.movement_baseline = point;
        state.snapshot
    }
}

fn point_distance_squared(left: MousePoint, right: MousePoint) -> i128 {
    let dx = i128::from(left.x) - i128::from(right.x);
    let dy = i128::from(left.y) - i128::from(right.y);
    dx * dx + dy * dy
}

fn mouse_event_kind_from_message(message: u32, mouse_data: u32) -> Option<MouseEventKind> {
    let high_word = (mouse_data >> 16) as u16;
    match message {
        WM_MOUSEMOVE => Some(MouseEventKind::Move),
        WM_LBUTTONDOWN => Some(MouseEventKind::LeftDown),
        WM_LBUTTONUP => Some(MouseEventKind::LeftUp),
        WM_RBUTTONDOWN => Some(MouseEventKind::RightDown),
        WM_RBUTTONUP => Some(MouseEventKind::RightUp),
        WM_MBUTTONDOWN => Some(MouseEventKind::MiddleDown),
        WM_MBUTTONUP => Some(MouseEventKind::MiddleUp),
        WM_XBUTTONDOWN => Some(MouseEventKind::XDown(high_word)),
        WM_XBUTTONUP => Some(MouseEventKind::XUp(high_word)),
        WM_MOUSEWHEEL => Some(MouseEventKind::VerticalWheel(high_word as i16)),
        WM_MOUSEHWHEEL => Some(MouseEventKind::HorizontalWheel(high_word as i16)),
        _ => None,
    }
}

static ACTIVE_MOUSE_HOOK: OnceLock<Mutex<Option<Weak<MouseActivityLedger>>>> = OnceLock::new();

pub struct WindowsMouseHookEventSource {
    ledger: Arc<MouseActivityLedger>,
    health: Arc<Mutex<MouseEventSourceHealth>>,
    shutdown_requested: Arc<AtomicBool>,
    thread_id: u32,
    worker: Mutex<Option<JoinHandle<io::Result<()>>>>,
}

impl WindowsMouseHookEventSource {
    pub fn install(
        marker: SessionInputMarker,
        movement_baseline: MousePoint,
        movement_threshold_px: i32,
    ) -> io::Result<Self> {
        let ledger = Arc::new(MouseActivityLedger::new(
            marker,
            movement_baseline,
            movement_threshold_px,
        ));
        register_hook_ledger(&ledger)?;
        let health = Arc::new(Mutex::new(MouseEventSourceHealth::Starting));
        let shutdown_requested = Arc::new(AtomicBool::new(false));

        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let worker_ledger = ledger.clone();
        let worker_health = health.clone();
        let worker_shutdown_requested = shutdown_requested.clone();
        let worker = match thread::Builder::new()
            .name("macro-mouse-hook".to_string())
            .spawn(move || {
                let thread_health = worker_health.clone();
                let thread_shutdown_requested = worker_shutdown_requested.clone();
                run_hook_worker(worker_health, worker_shutdown_requested, move || {
                    mouse_hook_thread(
                        worker_ledger,
                        ready_sender,
                        thread_health,
                        thread_shutdown_requested,
                    )
                })
            }) {
            Ok(worker) => worker,
            Err(error) => {
                unregister_hook_ledger(&ledger);
                return Err(error);
            }
        };

        let thread_id = match ready_receiver.recv() {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(error)) => {
                let _ = worker.join();
                unregister_hook_ledger(&ledger);
                return Err(error);
            }
            Err(_) => {
                let _ = worker.join();
                unregister_hook_ledger(&ledger);
                return Err(io::Error::other(
                    "mouse hook thread exited before reporting readiness",
                ));
            }
        };

        Ok(Self {
            ledger,
            health,
            shutdown_requested,
            thread_id,
            worker: Mutex::new(Some(worker)),
        })
    }

    pub fn shutdown(mut self) -> io::Result<()> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> io::Result<()> {
        let worker = self
            .worker
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(worker) = worker else {
            return Ok(());
        };

        self.shutdown_requested.store(true, Ordering::Release);
        set_source_health(&self.health, MouseEventSourceHealth::ShutdownRequested);

        let post_result =
            unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }
                .map_err(|error| io::Error::other(format!("failed to stop mouse hook: {error}")));
        let join_result = worker
            .join()
            .map_err(|_| io::Error::other("mouse hook thread panicked"))?;
        post_result?;
        join_result
    }
}

impl MouseEventSource for WindowsMouseHookEventSource {
    fn session_marker(&self) -> SessionInputMarker {
        self.ledger.marker
    }

    fn snapshot(&self) -> MouseActivitySnapshot {
        self.ledger.snapshot()
    }

    fn reset_movement_baseline(&self, point: MousePoint) -> MouseActivitySnapshot {
        self.ledger.reset_movement_baseline(point)
    }

    fn health(&self) -> MouseEventSourceHealth {
        self.health
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Drop for WindowsMouseHookEventSource {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn active_hook_slot() -> &'static Mutex<Option<Weak<MouseActivityLedger>>> {
    ACTIVE_MOUSE_HOOK.get_or_init(|| Mutex::new(None))
}

fn register_hook_ledger(ledger: &Arc<MouseActivityLedger>) -> io::Result<()> {
    let mut slot = active_hook_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.as_ref().and_then(Weak::upgrade).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a low-level mouse hook is already installed",
        ));
    }
    *slot = Some(Arc::downgrade(ledger));
    Ok(())
}

fn unregister_hook_ledger(ledger: &Arc<MouseActivityLedger>) {
    let mut slot = active_hook_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot
        .as_ref()
        .and_then(Weak::upgrade)
        .is_some_and(|active| Arc::ptr_eq(&active, ledger))
    {
        *slot = None;
    }
}

fn set_source_health(health: &Arc<Mutex<MouseEventSourceHealth>>, state: MouseEventSourceHealth) {
    *health
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = state;
}

fn run_hook_worker(
    health: Arc<Mutex<MouseEventSourceHealth>>,
    shutdown_requested: Arc<AtomicBool>,
    worker: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(worker)) {
        Ok(Ok(())) if shutdown_requested.load(Ordering::Acquire) => {
            set_source_health(&health, MouseEventSourceHealth::Stopped);
            Ok(())
        }
        Ok(Ok(())) => {
            let failure = MouseEventSourceFailure::UnexpectedExit;
            set_source_health(&health, MouseEventSourceHealth::Failed(failure.clone()));
            Err(io::Error::other(failure.to_string()))
        }
        Ok(Err(error)) => {
            let failure = if error.kind() == io::ErrorKind::UnexpectedEof {
                MouseEventSourceFailure::UnexpectedExit
            } else {
                MouseEventSourceFailure::WorkerError {
                    message: error.to_string(),
                }
            };
            set_source_health(&health, MouseEventSourceHealth::Failed(failure));
            Err(error)
        }
        Err(_) => {
            let failure = MouseEventSourceFailure::WorkerPanicked;
            set_source_health(&health, MouseEventSourceHealth::Failed(failure.clone()));
            Err(io::Error::other(failure.to_string()))
        }
    }
}

fn mouse_hook_thread(
    ledger: Arc<MouseActivityLedger>,
    ready: mpsc::SyncSender<io::Result<u32>>,
    health: Arc<Mutex<MouseEventSourceHealth>>,
    shutdown_requested: Arc<AtomicBool>,
) -> io::Result<()> {
    let thread_id = unsafe { GetCurrentThreadId() };
    let mut message = MSG::default();
    // Force creation of the thread message queue before exposing the thread ID to shutdown.
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
    let hook = match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(low_level_mouse_proc), None, 0) }
    {
        Ok(hook) => hook,
        Err(error) => {
            let error =
                io::Error::other(format!("failed to install low-level mouse hook: {error}"));
            let _ = ready.send(Err(io::Error::new(error.kind(), error.to_string())));
            unregister_hook_ledger(&ledger);
            return Err(error);
        }
    };
    let hook = HookGuard(Some(hook));
    set_source_health(&health, MouseEventSourceHealth::Running);
    if ready.send(Ok(thread_id)).is_err() {
        unregister_hook_ledger(&ledger);
        return Err(io::Error::other("mouse hook owner dropped during startup"));
    }

    let result = loop {
        let status = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        if status > 0 {
            continue;
        }
        if status == 0 {
            break if shutdown_requested.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "mouse hook message loop exited without a shutdown request",
                ))
            };
        }
        break Err(io::Error::last_os_error());
    };

    drop(hook);
    unregister_hook_ledger(&ledger);
    result
}

struct HookGuard(Option<HHOOK>);

impl Drop for HookGuard {
    fn drop(&mut self) {
        if let Some(hook) = self.0.take() {
            let _ = unsafe { UnhookWindowsHookEx(hook) };
        }
    }
}

unsafe extern "system" fn low_level_mouse_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code == HC_ACTION as i32 {
        let hook_data = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        if let Some(kind) = mouse_event_kind_from_message(wparam.0 as u32, hook_data.mouseData) {
            let ledger = active_hook_slot()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .and_then(Weak::upgrade);
            if let Some(ledger) = ledger {
                ledger.observe(RawMouseEvent {
                    kind,
                    point: MousePoint {
                        x: hook_data.pt.x,
                        y: hook_data.pt.y,
                    },
                    flags: hook_data.flags,
                    extra_info: hook_data.dwExtraInfo,
                });
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

#[cfg(test)]
struct FakeMouseEventSource {
    ledger: MouseActivityLedger,
    health: Mutex<MouseEventSourceHealth>,
}

#[cfg(test)]
impl FakeMouseEventSource {
    fn new(marker: SessionInputMarker, baseline: MousePoint, threshold_px: i32) -> Self {
        Self {
            ledger: MouseActivityLedger::new(marker, baseline, threshold_px),
            health: Mutex::new(MouseEventSourceHealth::Running),
        }
    }

    fn emit(&self, event: RawMouseEvent) {
        self.ledger.observe(event);
    }

    fn set_health(&self, health: MouseEventSourceHealth) {
        *self.health.lock().unwrap() = health;
    }
}

#[cfg(test)]
impl MouseEventSource for FakeMouseEventSource {
    fn session_marker(&self) -> SessionInputMarker {
        self.ledger.marker
    }

    fn snapshot(&self) -> MouseActivitySnapshot {
        self.ledger.snapshot()
    }

    fn reset_movement_baseline(&self, point: MousePoint) -> MouseActivitySnapshot {
        self.ledger.reset_movement_baseline(point)
    }

    fn health(&self) -> MouseEventSourceHealth {
        self.health.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicBool};

    use super::*;

    #[test]
    fn session_input_marker_generation_accepts_nonzero_generator_output() {
        let expected = usize::from_ne_bytes([0x5a; std::mem::size_of::<usize>()]);

        let marker = SessionInputMarker::generate_with(|bytes| {
            bytes.fill(0x5a);
            true
        })
        .unwrap();

        assert_eq!(marker.get(), expected);
    }

    #[test]
    fn session_input_marker_generation_reports_os_failure() {
        assert_eq!(
            SessionInputMarker::generate_with(|_| false),
            Err(SessionInputMarkerError::OsFailure)
        );
    }

    #[test]
    fn session_input_marker_generation_rejects_all_zero_output() {
        assert_eq!(
            SessionInputMarker::generate_with(|bytes| {
                bytes.fill(0);
                true
            }),
            Err(SessionInputMarkerError::AllZero)
        );
    }

    #[test]
    fn two_sessions_ignore_only_their_own_marker() {
        let marker_a = marker(101);
        let marker_b = marker(202);
        let source_a = Arc::new(FakeMouseEventSource::new(marker_a, point(0, 0), 4));
        let source_b = Arc::new(FakeMouseEventSource::new(marker_b, point(0, 0), 4));
        let observer_a = ManualMouseActivityObserver::new(source_a.clone());
        let observer_b = ManualMouseActivityObserver::new(source_b.clone());
        let event = raw_at(
            MouseEventKind::Move,
            point(10, 0),
            LLMHF_INJECTED,
            marker_a.get(),
        );

        source_a.emit(event);
        source_b.emit(event);

        assert!(!observer_a.takeover_detected().unwrap());
        assert!(observer_b.takeover_detected().unwrap());
        assert_eq!(
            source_b.snapshot().last_origin,
            Some(MouseEventOrigin::ExternalInjected)
        );
    }

    #[test]
    fn subthreshold_jitter_does_not_advance_manual_sequence() {
        let source = Arc::new(FakeMouseEventSource::new(marker(101), point(0, 0), 4));
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw_at(MouseEventKind::Move, point(1, 1), 0, 0));
        source.emit(raw_at(MouseEventKind::Move, point(2, 1), 0, 0));
        source.emit(raw_at(MouseEventKind::Move, point(3, 0), 0, 0));

        assert!(!observer.takeover_detected().unwrap());
        assert_eq!(source.snapshot().sequence, 0);
    }

    #[test]
    fn net_displacement_accumulates_from_baseline_until_threshold() {
        let source = Arc::new(FakeMouseEventSource::new(marker(101), point(0, 0), 4));
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw_at(MouseEventKind::Move, point(2, 0), 0, 0));
        source.emit(raw_at(MouseEventKind::Move, point(3, 0), 0, 0));
        assert!(!observer.takeover_detected().unwrap());
        source.emit(raw_at(MouseEventKind::Move, point(4, 0), 0, 0));

        assert!(observer.takeover_detected().unwrap());
        assert_eq!(source.snapshot().sequence, 1);
    }

    #[test]
    fn extreme_i32_endpoints_cannot_wrap_below_the_movement_threshold() {
        let source = Arc::new(FakeMouseEventSource::new(
            marker(101),
            point(i32::MIN, i32::MIN),
            4,
        ));
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw_at(
            MouseEventKind::Move,
            point(i32::MAX, i32::MAX),
            0,
            0,
        ));

        assert!(observer.takeover_detected().unwrap());
        assert_eq!(source.snapshot().sequence, 1);
    }

    #[test]
    fn owned_movement_updates_baseline_endpoint_without_takeover() {
        let owned = marker(101);
        let source = Arc::new(FakeMouseEventSource::new(owned, point(0, 0), 4));
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw_at(
            MouseEventKind::Move,
            point(100, 100),
            LLMHF_INJECTED,
            owned.get(),
        ));
        source.emit(raw_at(MouseEventKind::Move, point(103, 100), 0, 0));
        assert!(!observer.takeover_detected().unwrap());
        source.emit(raw_at(MouseEventKind::Move, point(104, 100), 0, 0));

        assert!(observer.takeover_detected().unwrap());
        assert_eq!(source.snapshot().sequence, 1);
    }

    #[test]
    fn button_activity_advances_sequence_immediately() {
        let source = Arc::new(FakeMouseEventSource::new(marker(101), point(0, 0), 4));
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw_at(MouseEventKind::LeftDown, point(0, 0), 0, 0));

        assert!(observer.takeover_detected().unwrap());
        assert_eq!(source.snapshot().sequence, 1);
    }

    #[test]
    fn reset_discards_stale_sequence_and_rebases_movement_endpoint() {
        let source = Arc::new(FakeMouseEventSource::new(marker(101), point(0, 0), 4));
        let observer = ManualMouseActivityObserver::new(source.clone());
        source.emit(raw_at(MouseEventKind::Move, point(4, 0), 0, 0));
        assert!(observer.takeover_detected().unwrap());

        observer.reset_baseline(point(100, 100)).unwrap();
        source.emit(raw_at(MouseEventKind::Move, point(103, 100), 0, 0));
        assert!(!observer.takeover_detected().unwrap());
        source.emit(raw_at(MouseEventKind::Move, point(104, 100), 0, 0));

        assert!(observer.takeover_detected().unwrap());
    }

    #[test]
    fn unhealthy_source_blocks_manual_baseline_reset() {
        let source = Arc::new(FakeMouseEventSource::new(marker(101), point(0, 0), 4));
        let observer = ManualMouseActivityObserver::new(source.clone());
        source.set_health(MouseEventSourceHealth::Failed(
            MouseEventSourceFailure::WorkerPanicked,
        ));

        assert_eq!(
            observer.reset_baseline(point(100, 100)),
            Err(MouseEventSourceHealthError::Failed(
                MouseEventSourceFailure::WorkerPanicked
            ))
        );
    }

    #[test]
    fn physical_click_between_polls_is_retained_by_sequence() {
        let source = Arc::new(FakeMouseEventSource::new(marker(101), point(0, 0), 4));
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw(MouseEventKind::LeftDown, 0, 0));
        source.emit(raw(MouseEventKind::LeftUp, 0, 0));

        assert!(observer.takeover_detected().unwrap());
    }

    #[test]
    fn reset_discards_stale_activity_but_retains_new_activity() {
        let source = Arc::new(FakeMouseEventSource::new(marker(101), point(0, 0), 4));
        source.emit(raw(MouseEventKind::Move, 0, 0));
        let observer = ManualMouseActivityObserver::new(source.clone());
        assert!(!observer.takeover_detected().unwrap());

        source.emit(raw(MouseEventKind::RightDown, 0, 0));
        assert!(observer.takeover_detected().unwrap());
        observer.reset_baseline(point(10, 20)).unwrap();
        assert!(!observer.takeover_detected().unwrap());

        source.emit(raw(MouseEventKind::RightUp, 0, 0));
        assert!(observer.takeover_detected().unwrap());
    }

    #[test]
    fn owned_injected_movement_is_ignored() {
        let source = Arc::new(FakeMouseEventSource::new(marker(101), point(0, 0), 4));
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw(MouseEventKind::Move, LLMHF_INJECTED, marker(101).get()));

        assert!(!observer.takeover_detected().unwrap());
        assert_eq!(source.snapshot().sequence, 0);
    }

    #[test]
    fn physical_click_during_owned_movement_is_not_suppressed() {
        let source = Arc::new(FakeMouseEventSource::new(marker(101), point(0, 0), 4));
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw(MouseEventKind::Move, LLMHF_INJECTED, marker(101).get()));
        source.emit(raw(MouseEventKind::LeftDown, 0, 0));
        source.emit(raw(MouseEventKind::LeftUp, 0, 0));

        assert!(observer.takeover_detected().unwrap());
        assert_eq!(source.snapshot().sequence, 2);
        assert_eq!(
            source.snapshot().last_origin,
            Some(MouseEventOrigin::Physical)
        );
    }

    #[test]
    fn external_injected_events_count_as_manual_activity() {
        let source = Arc::new(FakeMouseEventSource::new(marker(101), point(0, 0), 4));
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw(MouseEventKind::MiddleDown, LLMHF_INJECTED, 91));

        assert!(observer.takeover_detected().unwrap());
        assert_eq!(
            source.snapshot().last_origin,
            Some(MouseEventOrigin::ExternalInjected)
        );
    }

    #[test]
    fn marker_without_injected_flag_counts_as_physical_activity() {
        let source = Arc::new(FakeMouseEventSource::new(marker(101), point(0, 0), 4));
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw(MouseEventKind::Move, 0, marker(101).get()));

        assert!(observer.takeover_detected().unwrap());
        assert_eq!(
            source.snapshot().last_origin,
            Some(MouseEventOrigin::Physical)
        );
    }

    #[test]
    fn lower_integrity_injected_events_follow_injected_marker_rules() {
        let source = Arc::new(FakeMouseEventSource::new(marker(101), point(0, 0), 4));
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw(MouseEventKind::Move, LLMHF_LOWER_IL_INJECTED, 71));
        assert!(observer.takeover_detected().unwrap());
        observer.reset_baseline(point(10, 20)).unwrap();

        source.emit(raw(
            MouseEventKind::Move,
            LLMHF_LOWER_IL_INJECTED,
            marker(101).get(),
        ));
        assert!(!observer.takeover_detected().unwrap());
    }

    #[test]
    fn win32_messages_decode_to_typed_activity() {
        assert_eq!(
            mouse_event_kind_from_message(WM_MOUSEMOVE, 0),
            Some(MouseEventKind::Move)
        );
        assert_eq!(
            mouse_event_kind_from_message(WM_XBUTTONDOWN, u32::from(2_u16) << 16),
            Some(MouseEventKind::XDown(2))
        );
        assert_eq!(
            mouse_event_kind_from_message(WM_MOUSEWHEEL, u32::from((-120_i16) as u16) << 16),
            Some(MouseEventKind::VerticalWheel(-120))
        );
        assert_eq!(mouse_event_kind_from_message(0x1234, 0), None);
    }

    #[test]
    fn event_source_contract_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WindowsMouseHookEventSource>();
        assert_send_sync::<ManualMouseActivityObserver>();
    }

    #[test]
    fn windows_hook_reports_install_readiness_and_shuts_down() {
        let source = WindowsMouseHookEventSource::install(marker(101), point(0, 0), 4).unwrap();
        assert_eq!(source.health(), MouseEventSourceHealth::Running);
        source.shutdown().unwrap();
    }

    #[test]
    fn worker_guard_reports_unexpected_exit_and_panic() {
        for (worker, expected) in [
            (
                Box::new(|| Ok(())) as Box<dyn FnOnce() -> io::Result<()>>,
                MouseEventSourceFailure::UnexpectedExit,
            ),
            (
                Box::new(|| -> io::Result<()> { panic!("boom") })
                    as Box<dyn FnOnce() -> io::Result<()>>,
                MouseEventSourceFailure::WorkerPanicked,
            ),
        ] {
            let health = Arc::new(Mutex::new(MouseEventSourceHealth::Running));
            let shutdown_requested = Arc::new(AtomicBool::new(false));

            assert!(run_hook_worker(health.clone(), shutdown_requested, worker).is_err());
            assert_eq!(
                *health.lock().unwrap(),
                MouseEventSourceHealth::Failed(expected)
            );
        }
    }

    #[test]
    fn worker_guard_accepts_only_requested_shutdown_as_clean_exit() {
        let health = Arc::new(Mutex::new(MouseEventSourceHealth::ShutdownRequested));
        let shutdown_requested = Arc::new(AtomicBool::new(true));

        run_hook_worker(health.clone(), shutdown_requested, || Ok(())).unwrap();

        assert_eq!(*health.lock().unwrap(), MouseEventSourceHealth::Stopped);
    }

    fn raw(kind: MouseEventKind, flags: u32, extra_info: usize) -> RawMouseEvent {
        raw_at(kind, point(10, 20), flags, extra_info)
    }

    fn raw_at(
        kind: MouseEventKind,
        point: MousePoint,
        flags: u32,
        extra_info: usize,
    ) -> RawMouseEvent {
        RawMouseEvent {
            kind,
            point,
            flags,
            extra_info,
        }
    }

    fn point(x: i32, y: i32) -> MousePoint {
        MousePoint { x, y }
    }

    fn marker(value: usize) -> SessionInputMarker {
        SessionInputMarker::generate_with(|bytes| {
            bytes.copy_from_slice(&value.to_ne_bytes());
            true
        })
        .unwrap()
    }
}
