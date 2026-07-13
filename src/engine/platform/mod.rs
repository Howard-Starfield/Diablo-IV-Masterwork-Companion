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
    EscStopSignal, SendInputController, WindowsOcrReader, XcapRegionCapture,
    enable_per_monitor_dpi_awareness, record_mouse_movement_profile, select_screen_rect,
    xcap_window_target_guard,
};
#[cfg(windows)]
#[allow(unused_imports)]
pub use windows_input::{ManualInputMonitor, WindowsInputSink};
#[cfg(target_os = "windows")]
#[allow(unused_imports)]
pub(crate) use windows_ocr::{OcrFrame, OcrPixelFormat, PositionedOcrWord, WindowsTextRecognizer};
#[cfg(windows)]
#[allow(unused_imports)]
pub use windows_target::{DurableTargetHints, WindowsTargetGuard};
