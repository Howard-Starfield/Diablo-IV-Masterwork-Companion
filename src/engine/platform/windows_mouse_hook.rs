use std::{
    io,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};
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

pub(crate) const RUNTIME_INPUT_MARKER: usize = 0x4D_41_43_52_4F;

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

pub trait MouseEventSource: Send + Sync {
    fn snapshot(&self) -> MouseActivitySnapshot;
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

    pub fn takeover_detected(&self) -> bool {
        self.source.snapshot().sequence != self.baseline.load(Ordering::Acquire)
    }

    pub fn reset_baseline(&self) {
        self.baseline
            .store(self.source.snapshot().sequence, Ordering::Release);
    }
}

#[derive(Debug, Default)]
struct MouseActivityLedger {
    snapshot: Mutex<MouseActivitySnapshot>,
}

impl MouseActivityLedger {
    fn observe(&self, raw: RawMouseEvent) {
        let Some(origin) = manual_origin(raw.flags, raw.extra_info) else {
            return;
        };
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sequence = snapshot.sequence.wrapping_add(1);
        snapshot.sequence = sequence;
        snapshot.last_origin = Some(origin);
        snapshot.last_event = Some(SequencedMouseEvent {
            sequence,
            kind: raw.kind,
            point: raw.point,
            origin,
        });
    }

    fn snapshot(&self) -> MouseActivitySnapshot {
        *self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn manual_origin(flags: u32, extra_info: usize) -> Option<MouseEventOrigin> {
    let injected = flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) != 0;
    if injected && extra_info == RUNTIME_INPUT_MARKER {
        None
    } else if injected {
        Some(MouseEventOrigin::ExternalInjected)
    } else {
        Some(MouseEventOrigin::Physical)
    }
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
    thread_id: u32,
    worker: Mutex<Option<JoinHandle<io::Result<()>>>>,
}

impl WindowsMouseHookEventSource {
    pub fn install() -> io::Result<Self> {
        let ledger = Arc::new(MouseActivityLedger::default());
        register_hook_ledger(&ledger)?;

        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let worker_ledger = ledger.clone();
        let worker = match thread::Builder::new()
            .name("macro-mouse-hook".to_string())
            .spawn(move || mouse_hook_thread(worker_ledger, ready_sender))
        {
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
    fn snapshot(&self) -> MouseActivitySnapshot {
        self.ledger.snapshot()
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

fn mouse_hook_thread(
    ledger: Arc<MouseActivityLedger>,
    ready: mpsc::SyncSender<io::Result<u32>>,
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
            break Ok(());
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
#[derive(Default)]
struct FakeMouseEventSource {
    ledger: MouseActivityLedger,
}

#[cfg(test)]
impl FakeMouseEventSource {
    fn emit(&self, event: RawMouseEvent) {
        self.ledger.observe(event);
    }
}

#[cfg(test)]
impl MouseEventSource for FakeMouseEventSource {
    fn snapshot(&self) -> MouseActivitySnapshot {
        self.ledger.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn physical_click_between_polls_is_retained_by_sequence() {
        let source = Arc::new(FakeMouseEventSource::default());
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw(MouseEventKind::LeftDown, 0, 0));
        source.emit(raw(MouseEventKind::LeftUp, 0, 0));

        assert!(observer.takeover_detected());
    }

    #[test]
    fn reset_discards_stale_activity_but_retains_new_activity() {
        let source = Arc::new(FakeMouseEventSource::default());
        source.emit(raw(MouseEventKind::Move, 0, 0));
        let observer = ManualMouseActivityObserver::new(source.clone());
        assert!(!observer.takeover_detected());

        source.emit(raw(MouseEventKind::RightDown, 0, 0));
        assert!(observer.takeover_detected());
        observer.reset_baseline();
        assert!(!observer.takeover_detected());

        source.emit(raw(MouseEventKind::RightUp, 0, 0));
        assert!(observer.takeover_detected());
    }

    #[test]
    fn owned_injected_movement_is_ignored() {
        let source = Arc::new(FakeMouseEventSource::default());
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw(
            MouseEventKind::Move,
            LLMHF_INJECTED,
            RUNTIME_INPUT_MARKER,
        ));

        assert!(!observer.takeover_detected());
        assert_eq!(source.snapshot().sequence, 0);
    }

    #[test]
    fn physical_click_during_owned_movement_is_not_suppressed() {
        let source = Arc::new(FakeMouseEventSource::default());
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw(
            MouseEventKind::Move,
            LLMHF_INJECTED,
            RUNTIME_INPUT_MARKER,
        ));
        source.emit(raw(MouseEventKind::LeftDown, 0, 0));
        source.emit(raw(MouseEventKind::LeftUp, 0, 0));

        assert!(observer.takeover_detected());
        assert_eq!(source.snapshot().sequence, 2);
        assert_eq!(
            source.snapshot().last_origin,
            Some(MouseEventOrigin::Physical)
        );
    }

    #[test]
    fn external_injected_events_count_as_manual_activity() {
        let source = Arc::new(FakeMouseEventSource::default());
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw(MouseEventKind::MiddleDown, LLMHF_INJECTED, 91));

        assert!(observer.takeover_detected());
        assert_eq!(
            source.snapshot().last_origin,
            Some(MouseEventOrigin::ExternalInjected)
        );
    }

    #[test]
    fn marker_without_injected_flag_counts_as_physical_activity() {
        let source = Arc::new(FakeMouseEventSource::default());
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw(MouseEventKind::Move, 0, RUNTIME_INPUT_MARKER));

        assert!(observer.takeover_detected());
        assert_eq!(
            source.snapshot().last_origin,
            Some(MouseEventOrigin::Physical)
        );
    }

    #[test]
    fn lower_integrity_injected_events_follow_injected_marker_rules() {
        let source = Arc::new(FakeMouseEventSource::default());
        let observer = ManualMouseActivityObserver::new(source.clone());

        source.emit(raw(MouseEventKind::Move, LLMHF_LOWER_IL_INJECTED, 71));
        assert!(observer.takeover_detected());
        observer.reset_baseline();

        source.emit(raw(
            MouseEventKind::Move,
            LLMHF_LOWER_IL_INJECTED,
            RUNTIME_INPUT_MARKER,
        ));
        assert!(!observer.takeover_detected());
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
        let source = WindowsMouseHookEventSource::install().unwrap();
        source.shutdown().unwrap();
    }

    fn raw(kind: MouseEventKind, flags: u32, extra_info: usize) -> RawMouseEvent {
        RawMouseEvent {
            kind,
            point: MousePoint { x: 10, y: 20 },
            flags,
            extra_info,
        }
    }
}
