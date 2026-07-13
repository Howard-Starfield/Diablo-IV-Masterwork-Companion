use std::{
    future::IntoFuture,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use futures_lite::future;
use image::{DynamicImage, ImageFormat, Luma, RgbaImage, imageops};
use tempfile::NamedTempFile;
use windows::{
    Graphics::Imaging::BitmapDecoder,
    Media::Ocr::OcrEngine,
    Storage::{FileAccessMode, StorageFile},
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            BLACK_BRUSH, BeginPaint, CreatePen, DeleteObject, EndPaint, FillRect, GetStockObject,
            NULL_BRUSH, PAINTSTRUCT, PS_SOLID, Rectangle, SelectObject, SetBkMode, SetTextColor,
            TRANSPARENT, TextOutW,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
                SetThreadDpiAwarenessContext,
            },
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN,
                MOUSEEVENTF_LEFTUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEINPUT,
                ReleaseCapture, SendInput, SetCapture, VK_ESCAPE, VK_LBUTTON,
            },
            WindowsAndMessaging::{
                CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW,
                DestroyWindow, DispatchMessageW, GA_ROOT, GWLP_USERDATA, GetAncestor, GetCursorPos,
                GetSystemMetrics, GetWindowLongPtrW, HTCLIENT, IDC_CROSS, LWA_ALPHA, LoadCursorW,
                MSG, PM_REMOVE, PeekMessageW, PostQuitMessage, RegisterClassW, SM_CXVIRTUALSCREEN,
                SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOWNOACTIVATE,
                SetCursorPos, SetForegroundWindow, SetLayeredWindowAttributes, SetWindowLongPtrW,
                ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_DESTROY, WM_KEYDOWN,
                WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCHITTEST, WM_PAINT,
                WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
                WS_POPUP, WindowFromPoint,
            },
        },
    },
    core::{HSTRING, PCWSTR, w},
};
use xcap::{Monitor, Window};

use super::super::{
    automation::{
        AtomicCaptureSnapshot, AtomicFrameCapture, AtomicFrameSnapshotSource, CaptureSource,
        InputSink, MouseButton, StopSource, SystemClock, TargetGuard, TargetSnapshot,
    },
    config::{MouseMovementModel, MouseMovementProfile, MouseMovementSample, MouseMovementStep},
    enchant_loop::OcrReader,
    types::{Point, Rect, ScreenImage},
};
use super::windows_snapshot::{
    CanonicalWindowIdentity, Win32WindowsSnapshotSource, WindowsSnapshotSource,
    client_rect_in_screen, window_class, window_title,
};
use super::windows_target::{DurableTargetHints, WindowsTargetGuard};

/// Creates a run-local target guard from the same xcap window identity used by atomic capture.
/// The live HWND is never copied into the durable hints.
#[allow(dead_code)]
pub fn xcap_window_target_guard(window_id: u32, hints: DurableTargetHints) -> WindowsTargetGuard {
    WindowsTargetGuard::from_xcap_window_id(window_id, hints)
}

pub fn enable_per_monitor_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

#[derive(Debug, Default, Clone)]
pub struct XcapRegionCapture;

impl CaptureSource for XcapRegionCapture {
    fn capture(&self, rect: Rect) -> Result<ScreenImage> {
        let monitor = Monitor::from_point(rect.x, rect.y)
            .with_context(|| format!("failed to locate monitor for {}, {}", rect.x, rect.y))?;
        let monitor_x = monitor.x()?;
        let monitor_y = monitor.y()?;
        let local_x = (rect.x - monitor_x)
            .try_into()
            .context("OCR region is left of selected monitor")?;
        let local_y = (rect.y - monitor_y)
            .try_into()
            .context("OCR region is above selected monitor")?;
        let image = monitor
            .capture_region(local_x, local_y, rect.width, rect.height)
            .with_context(|| format!("failed to capture OCR region {:?}", rect))?;

        Ok(ScreenImage::new(image))
    }
}

pub trait WindowClientGeometrySource {
    fn client_rect(&self, window_id: u32) -> Result<Rect>;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Win32ClientGeometrySource;

impl WindowClientGeometrySource for Win32ClientGeometrySource {
    fn client_rect(&self, window_id: u32) -> Result<Rect> {
        client_rect_in_screen(CanonicalWindowIdentity::from_xcap_window_id(window_id))
            .with_context(|| format!("failed to query client rect for xcap window {window_id}"))
    }
}

#[derive(Debug, Clone)]
struct CapturedWindowImage {
    outer_rect: Rect,
    image: RgbaImage,
}

trait ConcreteWindowImageSource {
    fn capture_window(&self, window_id: u32) -> Result<CapturedWindowImage>;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct XcapConcreteWindowImageSource;

impl ConcreteWindowImageSource for XcapConcreteWindowImageSource {
    fn capture_window(&self, window_id: u32) -> Result<CapturedWindowImage> {
        let window = Window::all()
            .context("failed to enumerate concrete windows for capture")?
            .into_iter()
            .find(|window| window.id().ok() == Some(window_id))
            .ok_or_else(|| anyhow!("selected concrete window is no longer available"))?;
        anyhow::ensure!(
            !window.is_minimized().unwrap_or(true),
            "selected concrete window is minimized"
        );
        let outer_rect = Rect::new(
            window.x().context("failed to query concrete window x")?,
            window.y().context("failed to query concrete window y")?,
            window
                .width()
                .context("failed to query concrete window width")?,
            window
                .height()
                .context("failed to query concrete window height")?,
        );
        let image = window
            .capture_image()
            .context("failed to capture concrete window image")?;
        anyhow::ensure!(
            image.dimensions() == (outer_rect.width, outer_rect.height),
            "concrete window capture dimensions changed during capture"
        );
        Ok(CapturedWindowImage { outer_rect, image })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XcapWindowRegionCapture<G = Win32ClientGeometrySource, I = XcapConcreteWindowImageSource>
{
    window_id: u32,
    geometry: G,
    images: I,
}

impl XcapWindowRegionCapture<Win32ClientGeometrySource> {
    pub const fn new(window_id: u32) -> Self {
        Self {
            window_id,
            geometry: Win32ClientGeometrySource,
            images: XcapConcreteWindowImageSource,
        }
    }
}

impl<G> XcapWindowRegionCapture<G> {
    fn with_geometry(window_id: u32, geometry: G) -> Self {
        Self {
            window_id,
            geometry,
            images: XcapConcreteWindowImageSource,
        }
    }
}

impl<G, I> XcapWindowRegionCapture<G, I> {
    #[cfg(test)]
    fn with_sources(window_id: u32, geometry: G, images: I) -> Self {
        Self {
            window_id,
            geometry,
            images,
        }
    }
}

impl<G: WindowClientGeometrySource, I> XcapWindowRegionCapture<G, I> {
    fn screen_rect(&self, local: Rect) -> Result<Rect> {
        window_local_to_screen(self.geometry.client_rect(self.window_id)?, local)
    }
}

impl<G: WindowClientGeometrySource, I: ConcreteWindowImageSource> CaptureSource
    for XcapWindowRegionCapture<G, I>
{
    fn capture(&self, rect: Rect) -> Result<ScreenImage> {
        let requested = self.screen_rect(rect)?;
        let captured = self.images.capture_window(self.window_id)?;
        anyhow::ensure!(
            captured.image.dimensions() == (captured.outer_rect.width, captured.outer_rect.height),
            "concrete window capture dimensions do not match its outer frame"
        );
        let crop = screen_rect_relative_to(captured.outer_rect, requested)
            .context("requested client region is outside the concrete window image")?;
        Ok(ScreenImage::new(
            imageops::crop_imm(
                &captured.image,
                crop.x as u32,
                crop.y as u32,
                crop.width,
                crop.height,
            )
            .to_image(),
        ))
    }
}

#[derive(Debug, Clone)]
pub struct WindowsXcapWindowSnapshotSource<S = Win32WindowsSnapshotSource> {
    identity: CanonicalWindowIdentity,
    snapshots: S,
}

impl WindowsXcapWindowSnapshotSource<Win32WindowsSnapshotSource> {
    pub const fn new(window_id: u32) -> Self {
        Self {
            identity: CanonicalWindowIdentity::from_xcap_window_id(window_id),
            snapshots: Win32WindowsSnapshotSource,
        }
    }
}

impl<S> WindowsXcapWindowSnapshotSource<S> {
    #[cfg(test)]
    fn with_snapshot_source_for_test(identity: CanonicalWindowIdentity, snapshots: S) -> Self {
        Self {
            identity,
            snapshots,
        }
    }
}

impl<S: WindowsSnapshotSource> AtomicFrameSnapshotSource for WindowsXcapWindowSnapshotSource<S> {
    fn snapshot(&self, requested_region: Rect) -> Result<AtomicCaptureSnapshot> {
        let snapshot = self.snapshots.snapshot(self.identity)?;
        let client_rect = snapshot.client_rect;
        window_local_to_screen(client_rect, requested_region).with_context(|| {
            format!(
                "requested macro capture region is outside xcap window {}",
                self.identity.window_id()
            )
        })?;
        Ok(snapshot.atomic_capture_snapshot(requested_region))
    }
}

pub type XcapAtomicWindowCapture =
    AtomicFrameCapture<WindowsXcapWindowSnapshotSource, XcapWindowRegionCapture, SystemClock>;

/// Builds the production image-detection capture path for one concrete xcap window identity.
/// The returned source retains raw `capture` for OCR/Enchant and adds atomic `capture_frame`.
pub fn xcap_atomic_window_capture(window_id: u32) -> XcapAtomicWindowCapture {
    AtomicFrameCapture::new(
        WindowsXcapWindowSnapshotSource::new(window_id),
        XcapWindowRegionCapture::new(window_id),
        SystemClock::default(),
    )
}

fn rect_contains(container: Rect, nested: Rect) -> bool {
    let Some(container_right) = i64::from(container.x).checked_add(i64::from(container.width))
    else {
        return false;
    };
    let Some(container_bottom) = i64::from(container.y).checked_add(i64::from(container.height))
    else {
        return false;
    };
    let Some(nested_right) = i64::from(nested.x).checked_add(i64::from(nested.width)) else {
        return false;
    };
    let Some(nested_bottom) = i64::from(nested.y).checked_add(i64::from(nested.height)) else {
        return false;
    };
    i64::from(nested.x) >= i64::from(container.x)
        && i64::from(nested.y) >= i64::from(container.y)
        && nested_right <= container_right
        && nested_bottom <= container_bottom
}

fn window_local_to_screen(window: Rect, local: Rect) -> Result<Rect> {
    let local_bounds = Rect::new(0, 0, window.width, window.height);
    anyhow::ensure!(
        rect_contains(local_bounds, local),
        "capture region is outside the concrete xcap window"
    );
    let x = i64::from(window.x)
        .checked_add(i64::from(local.x))
        .and_then(|value| i32::try_from(value).ok())
        .context("window-local capture x coordinate overflowed")?;
    let y = i64::from(window.y)
        .checked_add(i64::from(local.y))
        .and_then(|value| i32::try_from(value).ok())
        .context("window-local capture y coordinate overflowed")?;
    Ok(Rect::new(x, y, local.width, local.height))
}

fn screen_rect_relative_to(container: Rect, screen: Rect) -> Result<Rect> {
    anyhow::ensure!(
        rect_contains(container, screen),
        "screen region is outside its container"
    );
    let x = i64::from(screen.x) - i64::from(container.x);
    let y = i64::from(screen.y) - i64::from(container.y);
    Ok(Rect::new(
        i32::try_from(x).context("relative capture x overflowed")?,
        i32::try_from(y).context("relative capture y overflowed")?,
        screen.width,
        screen.height,
    ))
}

#[derive(Debug, Default, Clone)]
pub struct WindowsOcrReader {
    pub save_debug_dir: Option<PathBuf>,
}

impl OcrReader for WindowsOcrReader {
    fn read_text(&self, image: &ScreenImage) -> Result<String> {
        let started = Instant::now();
        let processed = preprocess_for_ocr(&image.rgba);
        if let Some(dir) = &self.save_debug_dir {
            std::fs::create_dir_all(dir)?;
            let path = dir.join(format!(
                "ocr_processed_{}.png",
                started.elapsed().as_nanos()
            ));
            processed.save(&path)?;
        }

        let mut temp = NamedTempFile::new()?;
        processed.write_to(&mut temp, ImageFormat::Png)?;
        recognize_png_file(temp.path())
    }
}

fn preprocess_for_ocr(image: &RgbaImage) -> DynamicImage {
    let gray = DynamicImage::ImageRgba8(image.clone()).into_luma8();
    let scale = if image.width().max(image.height()) < 700 {
        3
    } else {
        2
    };
    let upscaled = imageops::resize(
        &gray,
        gray.width() * scale,
        gray.height() * scale,
        imageops::FilterType::CatmullRom,
    );

    let threshold = otsu_threshold(&upscaled);
    let mut out = upscaled;
    for pixel in out.pixels_mut() {
        let value = if pixel[0] > threshold { 255 } else { 0 };
        *pixel = Luma([value]);
    }

    if average_luma(&out) < 127.0 {
        for pixel in out.pixels_mut() {
            pixel[0] = 255 - pixel[0];
        }
    }

    DynamicImage::ImageLuma8(out)
}

fn average_luma(image: &image::GrayImage) -> f64 {
    let sum: u64 = image.pixels().map(|p| p[0] as u64).sum();
    sum as f64 / (image.width() as f64 * image.height() as f64)
}

fn otsu_threshold(image: &image::GrayImage) -> u8 {
    let mut hist = [0u32; 256];
    for pixel in image.pixels() {
        hist[pixel[0] as usize] += 1;
    }

    let total = (image.width() * image.height()) as f64;
    let sum: f64 = hist
        .iter()
        .enumerate()
        .map(|(idx, count)| idx as f64 * *count as f64)
        .sum();

    let mut sum_b = 0.0;
    let mut weight_b = 0.0;
    let mut max_variance = 0.0;
    let mut threshold = 0;

    for (idx, count) in hist.iter().enumerate() {
        weight_b += *count as f64;
        if weight_b == 0.0 {
            continue;
        }
        let weight_f = total - weight_b;
        if weight_f == 0.0 {
            break;
        }

        sum_b += idx as f64 * *count as f64;
        let mean_b = sum_b / weight_b;
        let mean_f = (sum - sum_b) / weight_f;
        let variance = weight_b * weight_f * (mean_b - mean_f).powi(2);
        if variance > max_variance {
            max_variance = variance;
            threshold = idx as u8;
        }
    }

    threshold
}

fn recognize_png_file(path: &std::path::Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("OCR temp path is not valid UTF-8"))?;
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()?;
    let file =
        future::block_on(StorageFile::GetFileFromPathAsync(&HSTRING::from(path))?.into_future())?;
    let stream = future::block_on(file.OpenAsync(FileAccessMode::Read)?.into_future())?;
    let decoder = future::block_on(BitmapDecoder::CreateAsync(&stream)?.into_future())?;
    let bitmap = future::block_on(decoder.GetSoftwareBitmapAsync()?.into_future())?;
    let result = future::block_on(engine.RecognizeAsync(&bitmap)?.into_future())?;
    Ok(result.Text()?.to_string_lossy())
}

#[derive(Debug, Default, Clone)]
pub struct SendInputController;

impl InputSink for SendInputController {
    fn move_and_click(
        &self,
        point: Point,
        button: MouseButton,
        movement: Option<&MouseMovementProfile>,
        stop: Option<&dyn StopSource>,
    ) -> Result<()> {
        if stop.is_some_and(|stop| stop.is_stopped()) {
            return Ok(());
        }
        if let Some(profile) = movement.filter(|profile| profile.is_usable()) {
            move_cursor_with_profile(point, profile, stop)?;
        }
        if stop.is_some_and(|stop| stop.is_stopped()) {
            return Ok(());
        }
        click_at(point, button)
    }
}

fn click_at(point: Point, button: MouseButton) -> Result<()> {
    unsafe {
        SetCursorPos(point.x, point.y)?;
        let (down, up) = mouse_button_flags(button);
        let inputs = [mouse_input(down), mouse_input(up)];
        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if sent != inputs.len() as u32 {
            return Err(anyhow!(
                "SendInput sent {sent}/{} mouse events",
                inputs.len()
            ));
        }
    }
    Ok(())
}

fn mouse_button_flags(
    button: MouseButton,
) -> (
    windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) {
    match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
    }
}

fn move_cursor_with_profile(
    target: Point,
    profile: &MouseMovementProfile,
    stop: Option<&dyn StopSource>,
) -> Result<()> {
    let start = cursor_pos()?;
    let dx = (target.x - start.x) as f32;
    let dy = (target.y - start.y) as f32;
    let distance = (dx * dx + dy * dy).sqrt();
    if distance < 1.0 {
        unsafe {
            SetCursorPos(target.x, target.y)?;
        }
        return Ok(());
    }

    let ux = dx / distance;
    let uy = dy / distance;
    let nx = -uy;
    let ny = ux;
    let duration_scale = (distance / profile.distance_px.max(1.0)).clamp(0.55, 1.8);
    let scaled_duration_ms =
        ((profile.duration_ms as f32 * duration_scale).round() as u64).clamp(60, 2_000);

    if let Some(model) = profile.model {
        move_cursor_with_motion_model(
            start,
            target,
            distance,
            (ux, uy),
            (nx, ny),
            profile,
            model,
            stop,
        )?;
        return Ok(());
    }

    if !profile.movement_steps.is_empty() {
        move_cursor_with_learned_steps(
            start,
            target,
            distance,
            (ux, uy),
            (nx, ny),
            profile,
            scaled_duration_ms,
            stop,
        )?;
        return Ok(());
    }

    let mut last_ms = 0;

    for sample in &profile.samples {
        let sample_time = sample.at_ms as f32 / profile.duration_ms.max(1) as f32;
        let at_ms = (sample_time * scaled_duration_ms as f32).round() as u64;
        if at_ms > last_ms {
            if sleep_until_or_stop(at_ms - last_ms, stop) {
                return Ok(());
            }
            last_ms = at_ms;
        }
        if stop.is_some_and(|stop| stop.is_stopped()) {
            return Ok(());
        }

        let along = sample.progress.clamp(0.0, 1.0) * distance;
        let side = sample.lateral.clamp(-0.75, 0.75) * distance;
        let x = start.x as f32 + ux * along + nx * side;
        let y = start.y as f32 + uy * along + ny * side;
        unsafe {
            SetCursorPos(x.round() as i32, y.round() as i32)?;
        }
    }

    unsafe {
        SetCursorPos(target.x, target.y)?;
    }
    Ok(())
}

fn move_cursor_with_motion_model(
    start: Point,
    target: Point,
    distance: f32,
    unit: (f32, f32),
    normal: (f32, f32),
    profile: &MouseMovementProfile,
    model: MouseMovementModel,
    stop: Option<&dyn StopSource>,
) -> Result<()> {
    let recorded_id = fitts_index(profile.distance_px, model.target_width_px).max(0.1);
    let target_id = fitts_index(distance, model.target_width_px).max(0.1);
    let duration_ms =
        ((profile.duration_ms as f32 * target_id / recorded_id).round() as u64).clamp(70, 1_800);
    let distance_scale = (distance / profile.distance_px.max(1.0)).sqrt();
    let point_count = ((model.point_count as f32 * distance_scale).round() as u32).clamp(10, 90);
    let curve = model.curve_lateral.clamp(-0.30, 0.30);
    let peak = model.curve_peak_progress.clamp(0.20, 0.80);
    let mut elapsed = 0;

    for index in 1..=point_count {
        let t = index as f32 / point_count as f32;
        let next_elapsed = (duration_ms as f32 * t).round() as u64;
        if next_elapsed > elapsed && sleep_until_or_stop(next_elapsed - elapsed, stop) {
            return Ok(());
        }
        elapsed = next_elapsed;
        if stop.is_some_and(|stop| stop.is_stopped()) {
            return Ok(());
        }

        let progress = minimum_jerk(t);
        let side = curved_lateral(progress, curve, peak) * distance;
        let along = progress * distance;
        let x = start.x as f32 + unit.0 * along + normal.0 * side;
        let y = start.y as f32 + unit.1 * along + normal.1 * side;
        unsafe {
            SetCursorPos(x.round() as i32, y.round() as i32)?;
        }
    }

    unsafe {
        SetCursorPos(target.x, target.y)?;
    }
    Ok(())
}

fn minimum_jerk(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    10.0 * t.powi(3) - 15.0 * t.powi(4) + 6.0 * t.powi(5)
}

fn curved_lateral(progress: f32, peak_lateral: f32, peak_progress: f32) -> f32 {
    if peak_lateral.abs() < 0.001 {
        return 0.0;
    }
    let p = progress.clamp(0.0, 1.0);
    let peak = peak_progress.clamp(0.20, 0.80);
    let shaped = if p <= peak {
        (p / peak * std::f32::consts::FRAC_PI_2).sin()
    } else {
        ((1.0 - p) / (1.0 - peak) * std::f32::consts::FRAC_PI_2).sin()
    };
    peak_lateral * shaped.max(0.0)
}

fn fitts_index(distance: f32, width: f32) -> f32 {
    (distance.max(1.0) / width.max(1.0) + 1.0).log2()
}

fn move_cursor_with_learned_steps(
    start: Point,
    target: Point,
    distance: f32,
    unit: (f32, f32),
    normal: (f32, f32),
    profile: &MouseMovementProfile,
    scaled_duration_ms: u64,
    stop: Option<&dyn StopSource>,
) -> Result<()> {
    let total_delay_ms: u64 = profile
        .movement_steps
        .iter()
        .map(|step| step.delay_ms)
        .sum::<u64>()
        .max(1);
    let time_scale = scaled_duration_ms as f32 / total_delay_ms as f32;
    let mut progress = 0.0;
    let mut lateral = 0.0;

    for step in &profile.movement_steps {
        let delay_ms = (step.delay_ms as f32 * time_scale).round() as u64;
        if sleep_until_or_stop(delay_ms, stop) {
            return Ok(());
        }
        if stop.is_some_and(|stop| stop.is_stopped()) {
            return Ok(());
        }

        progress = (progress + step.progress_delta).clamp(0.0, 1.0);
        lateral = (lateral + step.lateral_delta).clamp(-0.75, 0.75);
        let along = progress * distance;
        let side = lateral * distance;
        let x = start.x as f32 + unit.0 * along + normal.0 * side;
        let y = start.y as f32 + unit.1 * along + normal.1 * side;
        unsafe {
            SetCursorPos(x.round() as i32, y.round() as i32)?;
        }
    }

    unsafe {
        SetCursorPos(target.x, target.y)?;
    }
    Ok(())
}

fn sleep_until_or_stop(millis: u64, stop: Option<&dyn StopSource>) -> bool {
    let mut remaining = millis;
    while remaining > 0 {
        if stop.is_some_and(|stop| stop.is_stopped()) {
            return true;
        }
        let chunk = remaining.min(8);
        thread::sleep(Duration::from_millis(chunk));
        remaining -= chunk;
    }
    stop.is_some_and(|stop| stop.is_stopped())
}

fn mouse_input(flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[derive(Debug, Clone)]
pub struct EscStopSignal {
    external_stop: Arc<AtomicBool>,
}

impl EscStopSignal {
    pub fn new() -> Self {
        Self {
            external_stop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn stop(&self) {
        self.external_stop.store(true, Ordering::SeqCst);
    }

    pub fn is_stop_requested(&self) -> bool {
        self.external_stop.load(Ordering::SeqCst) || escape_pressed()
    }
}

impl Default for EscStopSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl StopSource for EscStopSignal {
    fn is_stopped(&self) -> bool {
        self.is_stop_requested()
    }
}

#[derive(Debug)]
struct OverlayState {
    min_size: u32,
    origin_x: i32,
    origin_y: i32,
    width: i32,
    height: i32,
    dragging: bool,
    start: POINT,
    current: POINT,
    result: Option<Result<Rect, String>>,
}

impl OverlayState {
    fn new(min_size: u32, origin_x: i32, origin_y: i32, width: i32, height: i32) -> Self {
        Self {
            min_size,
            origin_x,
            origin_y,
            width,
            height,
            dragging: false,
            start: POINT::default(),
            current: POINT::default(),
            result: None,
        }
    }

    fn selection_rect(&self) -> Option<RECT> {
        if !self.dragging {
            return None;
        }
        Some(RECT {
            left: self.start.x.min(self.current.x),
            top: self.start.y.min(self.current.y),
            right: self.start.x.max(self.current.x),
            bottom: self.start.y.max(self.current.y),
        })
    }

    fn finish_selection(&mut self) {
        let left = self.start.x.min(self.current.x);
        let top = self.start.y.min(self.current.y);
        let right = self.start.x.max(self.current.x);
        let bottom = self.start.y.max(self.current.y);
        let width = (right - left) as u32;
        let height = (bottom - top) as u32;

        self.result = if width < self.min_size || height < self.min_size {
            Some(Err(format!(
                "selected region is too small: {width}x{height}"
            )))
        } else {
            Some(Ok(Rect::new(
                self.origin_x + left,
                self.origin_y + top,
                width,
                height,
            )))
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CaptureRequestId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroCaptureKind {
    TextRegion,
    ImageSearchRegion,
    ClickRegion,
    ClickPoint,
    TemplateCrop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacroCaptureRequest {
    pub id: CaptureRequestId,
    pub kind: MacroCaptureKind,
    pub target_client: Rect,
    pub min_size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MacroCaptureSelection {
    Region(crate::engine::types::RectRatio),
    Point(crate::engine::types::PointRatio),
    TemplateCrop {
        region: crate::engine::types::RectRatio,
        screen_rect: Rect,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MacroCaptureResponse {
    pub id: CaptureRequestId,
    pub kind: MacroCaptureKind,
    pub selection: MacroCaptureSelection,
}

pub fn select_macro_capture(request: MacroCaptureRequest) -> Result<MacroCaptureResponse> {
    let rect = select_screen_rect_overlay(request.min_size)?;
    normalize_macro_capture(request, rect)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedTargetProfile {
    pub process_path: String,
    pub window_class: String,
    pub title: String,
    pub client_rect: Rect,
    pub dpi: u32,
}

#[derive(Debug, Clone)]
pub struct CapturedTargetBinding {
    profile: CapturedTargetProfile,
    expected: TargetSnapshot,
    guard: WindowsTargetGuard,
    activator: Arc<dyn TargetActivator>,
}

trait TargetActivator: std::fmt::Debug + Send + Sync {
    fn activate(&self, window_id: u64) -> Result<()>;
}

#[derive(Debug, Clone, Copy)]
struct Win32TargetActivator;

impl TargetActivator for Win32TargetActivator {
    fn activate(&self, window_id: u64) -> Result<()> {
        let identity = CanonicalWindowIdentity::from_window_id(window_id)?;
        anyhow::ensure!(
            unsafe { SetForegroundWindow(identity.hwnd()) }.as_bool(),
            "failed to bring the selected target window to the foreground"
        );
        Ok(())
    }
}

impl CapturedTargetBinding {
    pub fn profile(&self) -> &CapturedTargetProfile {
        &self.profile
    }

    /// Brings the selected target forward, then revalidates its concrete HWND,
    /// process instance and capture geometry. Only absolute movement is refreshed.
    pub fn prepare_client_rect(&self) -> Result<Rect> {
        self.activator.activate(self.expected.window_id)?;
        Ok(self
            .guard
            .refresh_authoring_snapshot(&self.expected)?
            .client_rect)
    }

    /// Post-capture validation is deliberately observation-only. In particular,
    /// it must not restore focus after an overlay or another application steals it.
    pub fn validate_client_rect(&self) -> Result<Rect> {
        Ok(self
            .guard
            .refresh_authoring_snapshot(&self.expected)?
            .client_rect)
    }

    /// Captures pixels from the concrete HWND image, never from the monitor compositor.
    /// The caller owns the prepare/capture/observation-only validation bracket.
    pub fn capture_screen_region(
        &self,
        client_rect: Rect,
        screen_rect: Rect,
    ) -> Result<ScreenImage> {
        let local = screen_rect_relative_to(client_rect, screen_rect)
            .context("authoring capture left the selected target client")?;
        XcapWindowRegionCapture::new(self.expected.window_id as u32).capture(local)
    }
}

pub fn resolve_target_from_selection(selection: Rect) -> Result<CapturedTargetBinding> {
    let center = POINT {
        x: selection.x + i32::try_from(selection.width / 2).unwrap_or(i32::MAX),
        y: selection.y + i32::try_from(selection.height / 2).unwrap_or(i32::MAX),
    };
    let child = unsafe { WindowFromPoint(center) };
    anyhow::ensure!(
        !child.is_invalid(),
        "no window exists under the selected target"
    );
    let root = unsafe { GetAncestor(child, GA_ROOT) };
    anyhow::ensure!(
        !root.is_invalid(),
        "failed to resolve the selected top-level window"
    );
    anyhow::ensure!(
        unsafe { SetForegroundWindow(root) }.as_bool(),
        "failed to bring the selected target window to the foreground"
    );
    let identity = CanonicalWindowIdentity::from_raw_hwnd(root.0 as isize)?;
    let snapshot = Win32WindowsSnapshotSource.snapshot(identity)?;
    let window_class = window_class(identity)?;
    let title = window_title(identity)?;
    let profile = CapturedTargetProfile {
        process_path: snapshot.process_path.clone(),
        window_class: window_class.clone(),
        title: title.clone(),
        client_rect: snapshot.client_rect,
        dpi: snapshot.dpi,
    };
    let guard = WindowsTargetGuard::from_window_id(
        identity.window_id(),
        DurableTargetHints {
            process_path: snapshot.process_path.clone(),
            window_class,
            title_contains: title,
        },
    )?;
    let expected = guard.snapshot()?;
    guard.refresh_authoring_snapshot(&expected)?;
    Ok(CapturedTargetBinding {
        profile,
        expected,
        guard,
        activator: Arc::new(Win32TargetActivator),
    })
}

fn normalize_macro_capture(
    request: MacroCaptureRequest,
    selected: Rect,
) -> Result<MacroCaptureResponse> {
    if request.target_client.width == 0 || request.target_client.height == 0 {
        return Err(anyhow!("target client geometry is empty"));
    }
    if selected.width < request.min_size || selected.height < request.min_size {
        return Err(anyhow!(
            "selected region is too small: {}x{}",
            selected.width,
            selected.height
        ));
    }
    let target_right = i64::from(request.target_client.x) + i64::from(request.target_client.width);
    let target_bottom =
        i64::from(request.target_client.y) + i64::from(request.target_client.height);
    let selected_right = i64::from(selected.x) + i64::from(selected.width);
    let selected_bottom = i64::from(selected.y) + i64::from(selected.height);
    if selected.x < request.target_client.x
        || selected.y < request.target_client.y
        || selected_right > target_right
        || selected_bottom > target_bottom
    {
        return Err(anyhow!("selection must stay inside the target client area"));
    }

    let region =
        crate::engine::types::RectRatio::from_rect_relative(request.target_client, selected);
    let selection = match request.kind {
        MacroCaptureKind::TextRegion
        | MacroCaptureKind::ImageSearchRegion
        | MacroCaptureKind::ClickRegion => MacroCaptureSelection::Region(region),
        MacroCaptureKind::ClickPoint => {
            let center_x = i64::from(selected.x) + i64::from(selected.width) / 2;
            let center_y = i64::from(selected.y) + i64::from(selected.height) / 2;
            MacroCaptureSelection::Point(crate::engine::types::PointRatio {
                x: (center_x - i64::from(request.target_client.x)) as f32
                    / request.target_client.width as f32,
                y: (center_y - i64::from(request.target_client.y)) as f32
                    / request.target_client.height as f32,
            })
        }
        MacroCaptureKind::TemplateCrop => MacroCaptureSelection::TemplateCrop {
            region,
            screen_rect: selected,
        },
    };
    Ok(MacroCaptureResponse {
        id: request.id,
        kind: request.kind,
        selection,
    })
}

/// Compatibility adapter used by the existing Enchant calibration flow.
pub fn select_screen_rect(min_size: u32) -> Result<Rect> {
    select_screen_rect_overlay(min_size)
}

fn select_screen_rect_overlay(min_size: u32) -> Result<Rect> {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let origin_x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let origin_y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if width <= 0 || height <= 0 {
            return Err(anyhow!("failed to determine virtual screen bounds"));
        }

        let hmodule = GetModuleHandleW(PCWSTR::null())?;
        let hinstance = HINSTANCE(hmodule.0);
        let class_name = w!("BoBoCompanionRegionOverlay");
        let cursor = LoadCursorW(None, IDC_CROSS).unwrap_or_default();
        let background = GetStockObject(BLACK_BRUSH);
        let wnd_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(region_overlay_proc),
            hInstance: hinstance,
            hCursor: cursor,
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(background.0),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wnd_class);

        let mut state = Box::new(OverlayState::new(
            min_size, origin_x, origin_y, width, height,
        ));
        let state_ptr = state.as_mut() as *mut OverlayState;
        let hwnd = CreateWindowExW(
            region_overlay_extended_style(),
            class_name,
            w!("Select Region"),
            WS_POPUP,
            origin_x,
            origin_y,
            width,
            height,
            None,
            None,
            Some(hinstance),
            Some(state_ptr.cast()),
        )
        .context("failed to create region overlay")?;

        SetLayeredWindowAttributes(hwnd, COLORREF(0), 86, LWA_ALPHA)
            .context("failed to configure translucent region overlay")?;
        let _ = ShowWindow(hwnd, region_overlay_show_command());

        let mut msg = MSG::default();
        while state.result.is_none() {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
                if state.result.is_some() {
                    break;
                }
            }
            if state.result.is_some() {
                break;
            }
            if escape_pressed() {
                state.result = Some(Err("screen selection cancelled".to_string()));
                let _ = DestroyWindow(hwnd);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        match state.result.take() {
            Some(Ok(rect)) => Ok(rect),
            Some(Err(error)) => Err(anyhow!(error)),
            None => Err(anyhow!("screen selection cancelled")),
        }
    }
}

fn region_overlay_extended_style() -> WINDOW_EX_STYLE {
    WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
}

fn region_overlay_show_command() -> windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD {
    SW_SHOWNOACTIVATE
}

unsafe extern "system" fn region_overlay_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let create = lparam.0 as *const CREATESTRUCTW;
            if !create.is_null() {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*create).lpCreateParams as isize);
                }
            }
            LRESULT(1)
        }
        WM_NCHITTEST => LRESULT(HTCLIENT as isize),
        WM_LBUTTONDOWN => {
            if let Some(state) = overlay_state(hwnd) {
                state.dragging = true;
                state.start = POINT {
                    x: lparam_x(lparam),
                    y: lparam_y(lparam),
                };
                state.current = state.start;
                unsafe {
                    SetCapture(hwnd);
                    let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, true);
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(state) = overlay_state(hwnd) {
                if state.dragging {
                    state.current = POINT {
                        x: lparam_x(lparam),
                        y: lparam_y(lparam),
                    };
                    unsafe {
                        let _ =
                            windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, true);
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(state) = overlay_state(hwnd) {
                if state.dragging {
                    state.current = POINT {
                        x: lparam_x(lparam),
                        y: lparam_y(lparam),
                    };
                    state.dragging = false;
                    state.finish_selection();
                    unsafe {
                        let _ = ReleaseCapture();
                        let _ = DestroyWindow(hwnd);
                    }
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wparam.0 as u16 == VK_ESCAPE.0 {
                if let Some(state) = overlay_state(hwnd) {
                    state.result = Some(Err("screen selection cancelled".to_string()));
                }
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_PAINT => {
            paint_overlay(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn overlay_state(hwnd: HWND) -> Option<&'static mut OverlayState> {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState };
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &mut *ptr })
    }
}

fn paint_overlay(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let Some(state) = overlay_state(hwnd) else {
            let _ = EndPaint(hwnd, &ps);
            return;
        };

        let bg_brush = windows::Win32::Graphics::Gdi::HBRUSH(GetStockObject(BLACK_BRUSH).0);
        let full = RECT {
            left: 0,
            top: 0,
            right: state.width,
            bottom: state.height,
        };
        FillRect(hdc, &full, bg_brush);

        let red = COLORREF(0x000000ff);
        let white = COLORREF(0x00ffffff);
        let pen = CreatePen(PS_SOLID, 2, red);
        let old_pen = SelectObject(hdc, pen.into());
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));

        if let Some(rect) = state.selection_rect() {
            let _ = Rectangle(hdc, rect.left, rect.top, rect.right, rect.bottom);
        }

        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, white);
        let title: Vec<u16> = "Drag to select the region".encode_utf16().collect();
        let help: Vec<u16> = "Press ESC to cancel".encode_utf16().collect();
        let _ = TextOutW(hdc, 24, 24, &title);
        let _ = TextOutW(hdc, 24, 46, &help);

        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(pen.into());
        let _ = EndPaint(hwnd, &ps);
    }
}

fn lparam_x(lparam: LPARAM) -> i32 {
    (lparam.0 as u32 & 0xffff) as i16 as i32
}

fn lparam_y(lparam: LPARAM) -> i32 {
    ((lparam.0 as u32 >> 16) & 0xffff) as i16 as i32
}

pub fn record_mouse_movement_profile() -> Result<MouseMovementProfile> {
    wait_until_left_button_released()?;

    let anchor = cursor_pos()?;
    let started = loop {
        if escape_pressed() {
            return Err(anyhow!("mouse movement recording cancelled"));
        }
        let point = cursor_pos()?;
        if point_distance(anchor, point) >= 6.0 {
            break Instant::now();
        }
        thread::sleep(Duration::from_millis(8));
    };
    let mut samples = vec![TimedPoint {
        at_ms: 0,
        point: cursor_pos()?,
    }];

    loop {
        if escape_pressed() {
            return Err(anyhow!("mouse movement recording cancelled"));
        }

        let point = cursor_pos()?;
        let at_ms = started.elapsed().as_millis() as u64;
        if samples.last().is_none_or(|sample| sample.point != point) {
            samples.push(TimedPoint { at_ms, point });
        }

        if left_button_pressed() {
            wait_until_left_button_released()?;
            let click_point = cursor_pos()?;
            let click_ms = started.elapsed().as_millis() as u64;
            if samples
                .last()
                .is_none_or(|sample| sample.point != click_point)
            {
                samples.push(TimedPoint {
                    at_ms: click_ms,
                    point: click_point,
                });
            }
            return analyze_mouse_movement(samples);
        }

        thread::sleep(Duration::from_millis(8));
    }
}

#[derive(Debug, Clone, Copy)]
struct TimedPoint {
    at_ms: u64,
    point: Point,
}

fn analyze_mouse_movement(samples: Vec<TimedPoint>) -> Result<MouseMovementProfile> {
    let samples = trim_mouse_recording(samples);
    if samples.len() < 2 {
        return Err(anyhow!("recorded mouse movement is too short"));
    }

    let start = samples.first().unwrap().point;
    let end = samples.last().unwrap().point;
    let dx = (end.x - start.x) as f32;
    let dy = (end.y - start.y) as f32;
    let distance = (dx * dx + dy * dy).sqrt();
    if distance < 8.0 {
        return Err(anyhow!(
            "recorded mouse movement must move at least 8 pixels"
        ));
    }

    let duration_ms = samples.last().unwrap().at_ms.max(1);
    let ux = dx / distance;
    let uy = dy / distance;
    let nx = -uy;
    let ny = ux;
    let mut analyzed = Vec::with_capacity(samples.len());
    let mut movement_steps = Vec::with_capacity(samples.len().saturating_sub(1));
    let mut previous_progress = 0.0;
    let mut previous_lateral = 0.0;
    let mut previous_time = 0;

    for sample in samples {
        let vx = (sample.point.x - start.x) as f32;
        let vy = (sample.point.y - start.y) as f32;
        let progress = ((vx * ux + vy * uy) / distance).clamp(0.0, 1.0);
        let lateral = ((vx * nx + vy * ny) / distance).clamp(-0.75, 0.75);
        let at_ms = sample.at_ms.min(duration_ms);
        if !analyzed.is_empty() {
            let progress_delta = progress - previous_progress;
            let lateral_delta = lateral - previous_lateral;
            let delay_ms = at_ms.saturating_sub(previous_time);
            if delay_ms > 0 || progress_delta.abs() > 0.0001 || lateral_delta.abs() > 0.0001 {
                movement_steps.push(MouseMovementStep {
                    delay_ms,
                    progress_delta,
                    lateral_delta,
                });
            }
        }
        analyzed.push(MouseMovementSample {
            at_ms,
            progress,
            lateral,
        });
        previous_progress = progress;
        previous_lateral = lateral;
        previous_time = at_ms;
    }

    if let Some(first) = analyzed.first_mut() {
        first.at_ms = 0;
        first.progress = 0.0;
    }
    if let Some(last) = analyzed.last_mut() {
        last.at_ms = duration_ms;
        last.progress = 1.0;
        last.lateral = 0.0;
    }
    normalize_movement_steps(&mut movement_steps);
    let model = learn_mouse_movement_model(&analyzed, distance, duration_ms);

    Ok(MouseMovementProfile {
        duration_ms,
        distance_px: distance,
        model: Some(model),
        movement_steps,
        samples: analyzed,
    })
}

fn trim_mouse_recording(samples: Vec<TimedPoint>) -> Vec<TimedPoint> {
    let deduped = dedupe_timed_points(samples);
    if deduped.len() <= 4 {
        return zero_start_times(deduped);
    }

    let start = deduped.first().unwrap().point;
    let end = deduped.last().unwrap().point;
    let dx = (end.x - start.x) as f32;
    let dy = (end.y - start.y) as f32;
    let distance = (dx * dx + dy * dy).sqrt();
    if distance < 12.0 {
        return zero_start_times(deduped);
    }

    let ux = dx / distance;
    let uy = dy / distance;
    let start_progress = 0.05;
    let end_progress = 0.96;

    let first = deduped
        .iter()
        .position(|sample| {
            let vx = (sample.point.x - start.x) as f32;
            let vy = (sample.point.y - start.y) as f32;
            (vx * ux + vy * uy) / distance >= start_progress
        })
        .unwrap_or(0)
        .saturating_sub(1);
    let last = deduped
        .iter()
        .rposition(|sample| {
            let vx = (sample.point.x - start.x) as f32;
            let vy = (sample.point.y - start.y) as f32;
            (vx * ux + vy * uy) / distance <= end_progress
        })
        .map(|index| (index + 1).min(deduped.len() - 1))
        .unwrap_or(deduped.len() - 1);

    if last <= first || last - first < 2 {
        return zero_start_times(deduped);
    }

    zero_start_times(deduped[first..=last].to_vec())
}

fn dedupe_timed_points(samples: Vec<TimedPoint>) -> Vec<TimedPoint> {
    let mut out = Vec::with_capacity(samples.len());
    for sample in samples {
        if out
            .last()
            .is_none_or(|previous: &TimedPoint| previous.point != sample.point)
        {
            out.push(sample);
        }
    }
    out
}

fn zero_start_times(mut samples: Vec<TimedPoint>) -> Vec<TimedPoint> {
    let Some(first) = samples.first().copied() else {
        return samples;
    };
    for sample in &mut samples {
        sample.at_ms = sample.at_ms.saturating_sub(first.at_ms);
    }
    samples
}

fn learn_mouse_movement_model(
    samples: &[MouseMovementSample],
    distance_px: f32,
    duration_ms: u64,
) -> MouseMovementModel {
    let point_count = samples.len().clamp(10, 90) as u32;
    let avg_step_ms = duration_ms / (samples.len().saturating_sub(1).max(1) as u64);
    let mut curve_lateral = 0.0_f32;
    let mut curve_peak_progress = 0.5_f32;
    for sample in samples {
        if sample.lateral.abs() > curve_lateral.abs() {
            curve_lateral = sample.lateral;
            curve_peak_progress = sample.progress;
        }
    }

    MouseMovementModel {
        point_count,
        avg_step_ms: avg_step_ms.max(4),
        curve_lateral: curve_lateral.clamp(-0.30, 0.30),
        curve_peak_progress: curve_peak_progress.clamp(0.20, 0.80),
        target_width_px: estimate_target_width(distance_px),
    }
}

fn estimate_target_width(distance_px: f32) -> f32 {
    (distance_px * 0.12).clamp(48.0, 140.0)
}

fn point_distance(a: Point, b: Point) -> f32 {
    let dx = (b.x - a.x) as f32;
    let dy = (b.y - a.y) as f32;
    (dx * dx + dy * dy).sqrt()
}

fn normalize_movement_steps(steps: &mut Vec<MouseMovementStep>) {
    if steps.is_empty() {
        return;
    }

    let total_progress: f32 = steps.iter().map(|step| step.progress_delta).sum();
    if total_progress.abs() > f32::EPSILON {
        for step in steps.iter_mut() {
            step.progress_delta /= total_progress;
        }
    }

    let total_lateral: f32 = steps.iter().map(|step| step.lateral_delta).sum();
    if total_lateral.abs() > f32::EPSILON {
        if let Some(last) = steps.last_mut() {
            last.lateral_delta -= total_lateral;
        }
    }
}

fn wait_until_left_button_released() -> Result<()> {
    while left_button_pressed() {
        if escape_pressed() {
            return Err(anyhow!("screen selection cancelled"));
        }
        thread::sleep(Duration::from_millis(16));
    }
    Ok(())
}

fn left_button_pressed() -> bool {
    unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 }
}

fn escape_pressed() -> bool {
    unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) < 0 }
}

fn cursor_pos() -> Result<Point> {
    let mut point = POINT::default();
    unsafe {
        GetCursorPos(&mut point)?;
    }
    Ok(Point::new(point.x, point.y))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use image::RgbaImage;

    use crate::engine::{
        automation::{AtomicFrameCapture, Clock, TargetGuard},
        types::ScreenImage,
    };

    use super::super::windows_snapshot::{
        CanonicalWindowIdentity, CanonicalWindowsSnapshot, DisplayProfileInputs,
        WindowsSnapshotSource,
    };
    use super::*;

    #[derive(Debug, Clone, Copy)]
    struct FakeClientGeometry {
        client_rect: Rect,
    }

    impl WindowClientGeometrySource for FakeClientGeometry {
        fn client_rect(&self, _window_id: u32) -> Result<Rect> {
            Ok(self.client_rect)
        }
    }

    #[derive(Debug, Clone)]
    struct FakeWindowsSnapshot(CanonicalWindowsSnapshot);

    impl WindowsSnapshotSource for FakeWindowsSnapshot {
        fn snapshot(&self, identity: CanonicalWindowIdentity) -> Result<CanonicalWindowsSnapshot> {
            anyhow::ensure!(
                identity == self.0.identity,
                "wrong canonical window identity"
            );
            Ok(self.0.clone())
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct FakeRawCapture;

    impl CaptureSource for FakeRawCapture {
        fn capture(&self, rect: Rect) -> Result<ScreenImage> {
            Ok(ScreenImage::new(RgbaImage::new(rect.width, rect.height)))
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct FixedClock;

    impl Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            123
        }
    }

    #[derive(Debug, Clone)]
    struct RecordingActivator(Arc<AtomicUsize>);

    impl TargetActivator for RecordingActivator {
        fn activate(&self, _window_id: u64) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn authoring_binding(
        expected_snapshot: CanonicalWindowsSnapshot,
        current_snapshot: CanonicalWindowsSnapshot,
        activations: Arc<AtomicUsize>,
    ) -> CapturedTargetBinding {
        let expected = expected_snapshot.target_snapshot();
        let profile = CapturedTargetProfile {
            process_path: expected_snapshot.process_path.clone(),
            window_class: String::new(),
            title: String::new(),
            client_rect: expected_snapshot.client_rect,
            dpi: expected_snapshot.dpi,
        };
        let guard = WindowsTargetGuard::with_snapshot_source_for_test(
            expected_snapshot.identity,
            DurableTargetHints {
                process_path: expected_snapshot.process_path.clone(),
                window_class: String::new(),
                title_contains: String::new(),
            },
            FakeWindowsSnapshot(current_snapshot),
        );
        CapturedTargetBinding {
            profile,
            expected,
            guard,
            activator: Arc::new(RecordingActivator(activations)),
        }
    }

    fn actionable_authoring_snapshot() -> CanonicalWindowsSnapshot {
        CanonicalWindowsSnapshot {
            identity: CanonicalWindowIdentity::from_xcap_window_id(42),
            process_id: 7,
            process_started_at_100ns: 100,
            process_path: r#"C:\games\Diablo IV.exe"#.to_string(),
            client_rect: Rect::new(100, 200, 800, 600),
            dpi: 144,
            display: DisplayProfileInputs {
                display_id: 11,
                monitor_rect: Rect::new(0, 0, 1_920, 1_080),
                work_rect: Rect::new(0, 0, 1_920, 1_040),
                flags: 1,
            },
            is_visible: true,
            is_minimized: false,
            is_foreground: true,
        }
    }

    #[test]
    fn authoring_post_validation_never_activates_and_rejects_lost_focus() {
        let activations = Arc::new(AtomicUsize::new(0));
        let expected = actionable_authoring_snapshot();
        let binding =
            authoring_binding(expected.clone(), expected.clone(), Arc::clone(&activations));

        assert_eq!(
            binding.validate_client_rect().unwrap(),
            Rect::new(100, 200, 800, 600)
        );
        assert_eq!(activations.load(Ordering::SeqCst), 0);
        binding.prepare_client_rect().unwrap();
        assert_eq!(activations.load(Ordering::SeqCst), 1);

        let mut unfocused = expected.clone();
        unfocused.is_foreground = false;
        let unfocused = authoring_binding(expected, unfocused, Arc::clone(&activations));
        assert!(unfocused.validate_client_rect().is_err());
        assert_eq!(
            activations.load(Ordering::SeqCst),
            1,
            "post validation must not restore stolen focus"
        );
    }

    #[test]
    fn nonactivating_overlay_lifecycle_preserves_a_valid_target() {
        let activations = Arc::new(AtomicUsize::new(0));
        let snapshot = actionable_authoring_snapshot();
        let binding = authoring_binding(snapshot.clone(), snapshot, Arc::clone(&activations));

        assert!(region_overlay_extended_style().contains(WS_EX_NOACTIVATE));
        assert_eq!(
            binding.prepare_client_rect().unwrap(),
            Rect::new(100, 200, 800, 600)
        );
        assert_eq!(
            binding.validate_client_rect().unwrap(),
            Rect::new(100, 200, 800, 600)
        );
        assert_eq!(
            activations.load(Ordering::SeqCst),
            1,
            "the observation-only post-overlay check must not reactivate the target"
        );
    }

    #[test]
    fn authoring_overlay_is_topmost_but_never_activates() {
        let style = region_overlay_extended_style();

        assert!(style.contains(WS_EX_TOPMOST));
        assert!(style.contains(WS_EX_NOACTIVATE));
        assert_eq!(region_overlay_show_command(), SW_SHOWNOACTIVATE);
    }

    #[derive(Debug, Clone)]
    struct FakeConcreteWindowImage {
        outer_rect: Rect,
        image: RgbaImage,
    }

    impl ConcreteWindowImageSource for FakeConcreteWindowImage {
        fn capture_window(&self, _window_id: u32) -> Result<CapturedWindowImage> {
            Ok(CapturedWindowImage {
                outer_rect: self.outer_rect,
                image: self.image.clone(),
            })
        }
    }

    #[test]
    fn concrete_window_capture_crops_the_window_frame_not_monitor_pixels() {
        let outer_rect = Rect::new(90, 180, 120, 100);
        let client = FakeClientGeometry {
            client_rect: Rect::new(100, 200, 100, 70),
        };
        let image = RgbaImage::from_fn(120, 100, |x, y| image::Rgba([x as u8, y as u8, 77, 255]));
        let capture = XcapWindowRegionCapture::with_sources(
            42,
            client,
            FakeConcreteWindowImage { outer_rect, image },
        );

        let captured = capture.capture(Rect::new(5, 6, 10, 8)).unwrap();

        assert_eq!(captured.rgba.dimensions(), (10, 8));
        assert_eq!(captured.rgba.get_pixel(0, 0).0, [15, 26, 77, 255]);
        assert_eq!(captured.rgba.get_pixel(9, 7).0, [24, 33, 77, 255]);
    }

    #[test]
    fn guard_and_atomic_capture_share_the_canonical_high_bit_window_snapshot() {
        let identity = CanonicalWindowIdentity::from_xcap_window_id(0x8000_0001);
        let source = FakeWindowsSnapshot(CanonicalWindowsSnapshot {
            identity,
            process_id: 7,
            process_started_at_100ns: 100,
            process_path: r#"C:\games\Diablo IV.exe"#.to_string(),
            client_rect: Rect::new(1_208, -269, 1_008, 729),
            dpi: 144,
            display: DisplayProfileInputs {
                display_id: 11,
                monitor_rect: Rect::new(0, -1_080, 1_920, 1_080),
                work_rect: Rect::new(0, -1_080, 1_920, 1_040),
                flags: 1,
            },
            is_visible: true,
            is_minimized: false,
            is_foreground: true,
        });
        let guard = WindowsTargetGuard::with_snapshot_source_for_test(
            identity,
            DurableTargetHints {
                process_path: String::new(),
                window_class: String::new(),
                title_contains: String::new(),
            },
            source.clone(),
        );
        let snapshots =
            WindowsXcapWindowSnapshotSource::with_snapshot_source_for_test(identity, source);
        let capture = AtomicFrameCapture::new(snapshots, FakeRawCapture, FixedClock);

        let target = guard.snapshot().unwrap();
        let frame = capture.capture_frame(Rect::new(25, 40, 100, 80)).unwrap();

        assert_eq!(identity.hwnd().0 as isize, i32::MIN as isize + 1);
        assert_eq!(frame.metadata.window_id, target.window_id);
        assert_eq!(frame.metadata.window_revision, target.window_revision);
        assert_eq!(frame.metadata.client_width, target.client_rect.width);
        assert_eq!(frame.metadata.client_height, target.client_rect.height);
        assert_eq!(frame.metadata.geometry_revision, target.geometry_revision);
        assert_eq!(
            frame.metadata.display_profile_revision,
            target.display_profile_revision
        );
        assert_eq!(frame.metadata.dpi, target.dpi);
    }

    #[test]
    fn right_click_selects_right_button_flags() {
        assert_eq!(
            mouse_button_flags(MouseButton::Right),
            (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)
        );
    }

    #[test]
    fn production_atomic_capture_constructor_is_available_for_macro_runtime_wiring() {
        let _capture = xcap_atomic_window_capture(42);
    }

    #[test]
    fn concrete_window_capture_translates_local_region_to_screen_coordinates() {
        let window = Rect::new(1_200, -300, 800, 600);

        assert_eq!(
            window_local_to_screen(window, Rect::new(25, 40, 100, 80)).unwrap(),
            Rect::new(1_225, -260, 100, 80)
        );
        assert!(window_local_to_screen(window, Rect::new(750, 40, 100, 80)).is_err());
    }

    #[test]
    fn framed_window_translation_uses_client_origin_and_bounds_not_xcap_outer_frame() {
        let xcap_outer_frame = Rect::new(1_200, -300, 1_024, 768);
        let client = FakeClientGeometry {
            // Eight-pixel side frame plus a 31-pixel title bar.
            client_rect: Rect::new(1_208, -269, 1_008, 729),
        };
        let local = Rect::new(25, 40, 100, 80);
        let capture = XcapWindowRegionCapture::with_geometry(42, client);

        let translated = capture.screen_rect(local).unwrap();

        assert_eq!(translated, Rect::new(1_233, -229, 100, 80));
        assert_ne!(
            translated,
            window_local_to_screen(xcap_outer_frame, local).unwrap()
        );
        assert!(capture.screen_rect(Rect::new(950, 700, 100, 80)).is_err());
    }

    #[test]
    fn xcap_window_id_reconstruction_sign_extends_windows_user_handles() {
        assert_eq!(
            CanonicalWindowIdentity::from_xcap_window_id(42).hwnd().0 as isize,
            42
        );
        assert_eq!(
            CanonicalWindowIdentity::from_xcap_window_id(0x8000_0001)
                .hwnd()
                .0 as isize,
            i32::MIN as isize + 1
        );
    }

    #[test]
    fn window_revision_changes_when_same_hwnd_and_pid_belong_to_restarted_process() {
        let identity = CanonicalWindowIdentity::from_xcap_window_id(42);
        let before = FakeWindowsSnapshot(CanonicalWindowsSnapshot {
            identity,
            process_id: 7,
            process_started_at_100ns: 100,
            process_path: String::new(),
            client_rect: Rect::new(0, 0, 800, 600),
            dpi: 96,
            display: DisplayProfileInputs {
                display_id: 11,
                monitor_rect: Rect::new(0, 0, 1_920, 1_080),
                work_rect: Rect::new(0, 0, 1_920, 1_040),
                flags: 1,
            },
            is_visible: true,
            is_minimized: false,
            is_foreground: true,
        })
        .0;
        let mut after = before.clone();
        after.process_started_at_100ns = 101;

        assert_ne!(before.window_revision(), after.window_revision());
    }

    #[test]
    fn macro_capture_normalizes_virtual_screen_rect_to_target_client() {
        let request = MacroCaptureRequest {
            id: CaptureRequestId(9),
            kind: MacroCaptureKind::TextRegion,
            target_client: Rect::new(-1_600, 120, 1_280, 720),
            min_size: 10,
        };

        let response = normalize_macro_capture(request, Rect::new(-1_320, 390, 420, 86)).unwrap();

        assert_eq!(response.id, CaptureRequestId(9));
        assert!(matches!(
            response.selection,
            MacroCaptureSelection::Region(rect)
                if rect == crate::engine::types::RectRatio::from_rect_relative(
                    Rect::new(-1_600, 120, 1_280, 720),
                    Rect::new(-1_320, 390, 420, 86)
                )
        ));
    }

    #[test]
    fn macro_capture_distinguishes_point_region_and_template() {
        let target = Rect::new(100, 200, 800, 600);
        let rect = Rect::new(300, 350, 200, 120);
        let point = normalize_macro_capture(
            MacroCaptureRequest {
                id: CaptureRequestId(1),
                kind: MacroCaptureKind::ClickPoint,
                target_client: target,
                min_size: 1,
            },
            rect,
        )
        .unwrap();
        assert!(matches!(
            point.selection,
            MacroCaptureSelection::Point(crate::engine::types::PointRatio { x, y })
                if (x - 0.375).abs() < f32::EPSILON && (y - 0.35).abs() < f32::EPSILON
        ));

        let template = normalize_macro_capture(
            MacroCaptureRequest {
                id: CaptureRequestId(2),
                kind: MacroCaptureKind::TemplateCrop,
                target_client: target,
                min_size: 4,
            },
            rect,
        )
        .unwrap();
        assert!(matches!(
            template.selection,
            MacroCaptureSelection::TemplateCrop { screen_rect, .. } if screen_rect == rect
        ));
    }

    #[test]
    fn macro_capture_rejects_too_small_and_outside_target() {
        let request = MacroCaptureRequest {
            id: CaptureRequestId(1),
            kind: MacroCaptureKind::ImageSearchRegion,
            target_client: Rect::new(100, 100, 500, 400),
            min_size: 10,
        };
        assert!(normalize_macro_capture(request, Rect::new(150, 150, 9, 20)).is_err());
        assert!(normalize_macro_capture(request, Rect::new(90, 150, 20, 20)).is_err());
    }
}
