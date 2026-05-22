#[cfg(windows)]
mod windows_impl;

#[cfg(windows)]
pub use windows_impl::{
    EscStopSignal, SendInputController, WindowsOcrReader, XcapRegionCapture,
    enable_per_monitor_dpi_awareness, record_mouse_movement_profile, select_screen_point,
    select_screen_rect,
};
