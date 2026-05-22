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
                MOUSEEVENTF_LEFTUP, MOUSEINPUT, ReleaseCapture, SendInput, SetCapture, VK_ESCAPE,
                VK_LBUTTON,
            },
            WindowsAndMessaging::{
                CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW,
                DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetCursorPos, GetSystemMetrics,
                GetWindowLongPtrW, HTCLIENT, IDC_CROSS, LWA_ALPHA, LoadCursorW, MSG, PM_REMOVE,
                PeekMessageW, PostQuitMessage, RegisterClassW, SM_CXVIRTUALSCREEN,
                SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOW, SetCursorPos,
                SetForegroundWindow, SetLayeredWindowAttributes, SetWindowLongPtrW, ShowWindow,
                TranslateMessage, WM_DESTROY, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
                WM_MOUSEMOVE, WM_NCCREATE, WM_NCHITTEST, WM_PAINT, WNDCLASSW, WS_EX_LAYERED,
                WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
            },
        },
    },
    core::{HSTRING, PCWSTR, w},
};
use xcap::Monitor;

use super::super::{
    config::{MouseMovementModel, MouseMovementProfile, MouseMovementSample, MouseMovementStep},
    enchant_loop::{InputController, OcrReader, RegionCapture, StopSignal},
    types::{Point, Rect, ScreenImage},
};

pub fn enable_per_monitor_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

#[derive(Debug, Default, Clone)]
pub struct XcapRegionCapture;

impl RegionCapture for XcapRegionCapture {
    fn capture_region(&self, rect: Rect) -> Result<ScreenImage> {
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

impl InputController for SendInputController {
    fn click(&self, point: Point) -> Result<()> {
        click_at(point)
    }

    fn click_with_movement(
        &self,
        point: Point,
        movement: Option<&MouseMovementProfile>,
        stop: Option<&dyn StopSignal>,
    ) -> Result<()> {
        if stop.is_some_and(|stop| stop.should_stop()) {
            return Ok(());
        }
        if let Some(profile) = movement.filter(|profile| profile.is_usable()) {
            move_cursor_with_profile(point, profile, stop)?;
        }
        if stop.is_some_and(|stop| stop.should_stop()) {
            return Ok(());
        }
        click_at(point)
    }
}

fn click_at(point: Point) -> Result<()> {
    unsafe {
        SetCursorPos(point.x, point.y)?;
        let inputs = [
            mouse_input(MOUSEEVENTF_LEFTDOWN),
            mouse_input(MOUSEEVENTF_LEFTUP),
        ];
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

fn move_cursor_with_profile(
    target: Point,
    profile: &MouseMovementProfile,
    stop: Option<&dyn StopSignal>,
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
        if stop.is_some_and(|stop| stop.should_stop()) {
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
    stop: Option<&dyn StopSignal>,
) -> Result<()> {
    let recorded_id = fitts_index(profile.distance_px, model.target_width_px).max(0.1);
    let target_id = fitts_index(distance, model.target_width_px).max(0.1);
    let duration_ms = ((profile.duration_ms as f32 * target_id / recorded_id).round() as u64)
        .clamp(70, 1_800);
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
        if stop.is_some_and(|stop| stop.should_stop()) {
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
    stop: Option<&dyn StopSignal>,
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
        if stop.is_some_and(|stop| stop.should_stop()) {
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

fn sleep_until_or_stop(millis: u64, stop: Option<&dyn StopSignal>) -> bool {
    let mut remaining = millis;
    while remaining > 0 {
        if stop.is_some_and(|stop| stop.should_stop()) {
            return true;
        }
        let chunk = remaining.min(8);
        thread::sleep(Duration::from_millis(chunk));
        remaining -= chunk;
    }
    stop.is_some_and(|stop| stop.should_stop())
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

impl StopSignal for EscStopSignal {
    fn should_stop(&self) -> bool {
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
            WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW,
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
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);

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
        return Err(anyhow!("recorded mouse movement must move at least 8 pixels"));
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
