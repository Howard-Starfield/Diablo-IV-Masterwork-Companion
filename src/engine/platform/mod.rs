#[cfg(windows)]
mod windows_impl;
#[cfg(windows)]
#[allow(dead_code)]
mod windows_input;
#[cfg(windows)]
#[allow(dead_code)]
mod windows_mouse_hook;
#[cfg(target_os = "windows")]
#[allow(dead_code)]
mod windows_ocr;
#[cfg(windows)]
#[allow(dead_code)]
mod windows_snapshot;
#[cfg(windows)]
#[allow(dead_code)]
mod windows_target;

#[cfg(windows)]
#[allow(unused_imports)]
pub use windows_impl::{
    CaptureRequestId, CapturedTargetBinding, CapturedTargetProfile, EscStopSignal,
    MacroCaptureKind, MacroCaptureRequest, MacroCaptureResponse, MacroCaptureSelection,
    SendInputController, WindowPlacement, WindowsOcrReader, XcapRegionCapture,
    clamp_window_placement, enable_per_monitor_dpi_awareness, preferred_window_placement,
    record_mouse_movement_profile, resolve_target_from_selection, select_macro_capture,
    select_screen_rect, xcap_window_target_guard,
};
#[cfg(windows)]
#[allow(unused_imports)]
pub(crate) use windows_impl::{WindowsMacroRuntimeBundle, build_windows_macro_runtime};
#[cfg(windows)]
#[allow(unused_imports)]
pub use windows_input::{ManualInputMonitor, WindowsInputSink};
#[cfg(target_os = "windows")]
#[allow(unused_imports)]
pub(crate) use windows_ocr::{OcrFrame, OcrPixelFormat, PositionedOcrWord, WindowsTextRecognizer};
#[cfg(windows)]
#[allow(unused_imports)]
pub use windows_target::{DurableTargetHints, WindowsTargetGuard};
