#[cfg(windows)]
mod windows_impl;
#[cfg(target_os = "windows")]
#[allow(dead_code)]
mod windows_ocr;

#[cfg(windows)]
pub use windows_impl::{
    EscStopSignal, SendInputController, WindowsOcrReader, XcapRegionCapture,
    enable_per_monitor_dpi_awareness, record_mouse_movement_profile, select_screen_rect,
};
#[cfg(target_os = "windows")]
#[allow(unused_imports)]
pub use windows_ocr::{OcrFrame, WindowsTextRecognizer};
