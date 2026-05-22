#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use eframe::{
    App, CreationContext,
    egui::{
        self, Align, Button, CentralPanel, Color32, Context, Frame, Grid, Layout, RichText, Sense,
        Slider, Stroke, TopBottomPanel, Ui, Vec2, ViewportCommand, Widget,
    },
};
use enchant_ocr_backend::{
    config::{EnchantConfig, MouseMovementProfile, default_mouse_movement_profile},
    enchant_loop::{EnchantEvent, EnchantRunner, OcrReader, RegionCapture},
    match_affix,
    matcher::MatchResult,
    platform::{
        EscStopSignal, SendInputController, WindowsOcrReader, XcapRegionCapture,
        enable_per_monitor_dpi_awareness, record_mouse_movement_profile, select_screen_rect,
    },
    types::{PointRatio, Rect, RectRatio},
};
use serde::{Deserialize, Serialize};

const APP_WIDTH: f32 = 600.0;
const APP_HEIGHT: f32 = 760.0;
const CALIBRATION_BUTTON_WIDTH: f32 = 138.0;
const ACTION_BUTTON_HEIGHT: f32 = 38.0;

fn main() -> eframe::Result<()> {
    enable_per_monitor_dpi_awareness();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("BoBo Companion")
        .with_inner_size([APP_WIDTH, APP_HEIGHT])
        .with_min_inner_size([APP_WIDTH, 680.0]);
    if let Some(icon) = load_window_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "BoBo Companion",
        options,
        Box::new(|cc| Box::new(NativeApp::new(cc))),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeConfig {
    targets_text: String,
    fuzzy_threshold: f64,
    max_attempts: u32,
    enchant_window: Option<Rect>,
    ocr_region: Option<RectRatio>,
    #[serde(default)]
    enchant_button_region: Option<RectRatio>,
    #[serde(default)]
    replace_button_region: Option<RectRatio>,
    #[serde(default)]
    close_button_region: Option<RectRatio>,
    enchant_button: Option<PointRatio>,
    replace_button: Option<PointRatio>,
    close_button: Option<PointRatio>,
    #[serde(default)]
    mouse_movement: Option<MouseMovementProfile>,
    wait_after_enchant_ms: u64,
    wait_after_replace_ms: u64,
    wait_after_close_ms: u64,
}

impl Default for NativeConfig {
    fn default() -> Self {
        let sample = EnchantConfig::sample();
        Self {
            targets_text: sample.targets.join(", "),
            fuzzy_threshold: sample.fuzzy_threshold,
            max_attempts: sample.max_attempts,
            enchant_window: None,
            ocr_region: None,
            enchant_button_region: None,
            replace_button_region: None,
            close_button_region: None,
            enchant_button: None,
            replace_button: None,
            close_button: None,
            mouse_movement: sample.mouse_movement,
            wait_after_enchant_ms: sample.wait_after_enchant_ms,
            wait_after_replace_ms: sample.wait_after_replace_ms,
            wait_after_close_ms: sample.wait_after_close_ms,
        }
    }
}

impl NativeConfig {
    fn targets(&self) -> Vec<String> {
        self.targets_text
            .split(',')
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }

    fn ready_config(&self) -> Option<EnchantConfig> {
        Some(EnchantConfig {
            targets: self.targets(),
            fuzzy_threshold: self.fuzzy_threshold,
            max_attempts: self.max_attempts,
            enchant_window: self.enchant_window?,
            ocr_region: self.ocr_region?,
            enchant_button: self.enchant_button_point()?,
            replace_button: self.replace_button_point()?,
            close_button: self.close_button_point()?,
            mouse_movement: self.mouse_movement.clone(),
            wait_after_enchant_ms: self.wait_after_enchant_ms,
            wait_after_replace_ms: self.wait_after_replace_ms,
            wait_after_close_ms: self.wait_after_close_ms,
        })
    }

    fn ocr_config(&self) -> Option<EnchantConfig> {
        let sample = EnchantConfig::sample();
        Some(EnchantConfig {
            targets: self.targets(),
            fuzzy_threshold: self.fuzzy_threshold,
            max_attempts: self.max_attempts,
            enchant_window: self.enchant_window?,
            ocr_region: self.ocr_region?,
            enchant_button: self.enchant_button_point().unwrap_or(sample.enchant_button),
            replace_button: self.replace_button_point().unwrap_or(sample.replace_button),
            close_button: self.close_button_point().unwrap_or(sample.close_button),
            mouse_movement: self.mouse_movement.clone(),
            wait_after_enchant_ms: self.wait_after_enchant_ms,
            wait_after_replace_ms: self.wait_after_replace_ms,
            wait_after_close_ms: self.wait_after_close_ms,
        })
    }

    fn enchant_button_point(&self) -> Option<PointRatio> {
        self.enchant_button_region
            .map(center_of_ratio)
            .or(self.enchant_button)
    }

    fn replace_button_point(&self) -> Option<PointRatio> {
        self.replace_button_region
            .map(center_of_ratio)
            .or(self.replace_button)
    }

    fn close_button_point(&self) -> Option<PointRatio> {
        self.close_button_region
            .map(center_of_ratio)
            .or(self.close_button)
    }

    fn has_enchant_button(&self) -> bool {
        self.enchant_button_region.is_some() || self.enchant_button.is_some()
    }

    fn has_replace_button(&self) -> bool {
        self.replace_button_region.is_some() || self.replace_button.is_some()
    }

    fn has_close_button(&self) -> bool {
        self.close_button_region.is_some() || self.close_button.is_some()
    }
}

#[derive(Debug, Clone, Copy)]
enum CaptureKind {
    EnchantWindow,
    AffixOcrRegion { window: Rect },
    EnchantButton { window: Rect },
    ReplaceButton { window: Rect },
    CloseButton { window: Rect },
}

#[derive(Debug)]
enum UiEvent {
    CaptureFinished(CaptureKind, anyhow::Result<CaptureValue>),
    MouseMovementRecorded(anyhow::Result<MouseMovementProfile>),
    OcrTestFinished(anyhow::Result<TestOcrResult>),
    StopRequested,
    BotEvent(EnchantEvent),
    BotFinished(anyhow::Result<()>),
}

#[derive(Debug)]
enum CaptureValue {
    Rect(Rect),
}

#[derive(Debug, Clone)]
struct TestOcrResult {
    result: MatchResult,
    ocr_time_ms: u64,
    capture_rect: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BotState {
    Ready,
    Calibrating,
    RecordingMovement,
    TestingOcr,
    Running,
    Matched,
    Stopped,
    NeedsCalibration,
    Error,
}

impl BotState {
    fn label(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Calibrating => "Calibrating",
            Self::RecordingMovement => "Recording Movement",
            Self::TestingOcr => "Testing OCR",
            Self::Running => "Running",
            Self::Matched => "Target found",
            Self::Stopped => "Stopped",
            Self::NeedsCalibration => "Needs calibration",
            Self::Error => "Error",
        }
    }
}

struct NativeApp {
    config: NativeConfig,
    config_path: PathBuf,
    egui_ctx: Context,
    tx: Sender<UiEvent>,
    rx: Receiver<UiEvent>,
    status: BotState,
    status_message: String,
    last_result: Option<TestOcrResult>,
    attempt: u32,
    stop_signal: Option<EscStopSignal>,
    stop_watcher_done: Option<Arc<AtomicBool>>,
    active_ocr_rect: Option<Rect>,
    dirty: bool,
}

impl NativeApp {
    fn new(cc: &CreationContext<'_>) -> Self {
        configure_style(&cc.egui_ctx);
        let (tx, rx) = mpsc::channel();
        let config_path = config_path();
        let (config, migrated_config) = load_native_config(&config_path);
        Self {
            config,
            config_path,
            egui_ctx: cc.egui_ctx.clone(),
            tx,
            rx,
            status: BotState::Ready,
            status_message: "Positions autosave and reload on next open.".to_string(),
            last_result: Some(TestOcrResult {
                result: match_affix(
                    "No OCR result yet",
                    &["Max Health".to_string()],
                    EnchantConfig::sample().fuzzy_threshold,
                ),
                ocr_time_ms: 0,
                capture_rect: Rect::new(0, 0, 0, 0),
            }),
            attempt: 0,
            stop_signal: None,
            stop_watcher_done: None,
            active_ocr_rect: None,
            dirty: migrated_config,
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn save_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        match save_native_config(&self.config_path, &self.config) {
            Ok(()) => {
                self.dirty = false;
            }
            Err(error) => {
                self.status = BotState::Error;
                self.status_message = format!("Failed to save config: {error}");
            }
        }
    }

    fn begin_capture(&mut self, _ctx: &Context, kind: CaptureKind) {
        if matches!(
            self.status,
            BotState::Running | BotState::Calibrating | BotState::RecordingMovement
        ) {
            return;
        }
        self.status = BotState::Calibrating;
        self.status_message = match kind {
            CaptureKind::EnchantWindow => {
                "Drag around the full Occultist enchant window.".to_string()
            }
            CaptureKind::AffixOcrRegion { .. } => {
                "Drag around the affix result text area.".to_string()
            }
            CaptureKind::EnchantButton { .. } => "Drag around the Enchant button.".to_string(),
            CaptureKind::ReplaceButton { .. } => {
                "Drag around the Replace Affix button.".to_string()
            }
            CaptureKind::CloseButton { .. } => "Drag around the Close button.".to_string(),
        };
        let tx = self.tx.clone();
        let repaint = self.egui_ctx.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            let result = match kind {
                CaptureKind::EnchantWindow
                | CaptureKind::AffixOcrRegion { .. }
                | CaptureKind::EnchantButton { .. }
                | CaptureKind::ReplaceButton { .. }
                | CaptureKind::CloseButton { .. } => select_screen_rect(10).map(CaptureValue::Rect),
            };
            send_ui_event(&tx, &repaint, UiEvent::CaptureFinished(kind, result));
        });
    }

    fn begin_mouse_movement_recording(&mut self) {
        if matches!(
            self.status,
            BotState::Running | BotState::Calibrating | BotState::RecordingMovement
        ) {
            return;
        }

        self.status = BotState::RecordingMovement;
        self.status_message =
            "Move the mouse naturally, then left-click to finish recording. Press ESC to cancel."
                .to_string();

        let tx = self.tx.clone();
        let repaint = self.egui_ctx.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            let result = record_mouse_movement_profile();
            send_ui_event(&tx, &repaint, UiEvent::MouseMovementRecorded(result));
        });
    }

    fn begin_ocr_test(&mut self) {
        let Some(config) = self.config.ocr_config() else {
            self.status = BotState::NeedsCalibration;
            self.status_message =
                "Set the window and affix OCR region before testing OCR.".to_string();
            return;
        };
        self.status = BotState::TestingOcr;
        self.status_message = "Reading the affix OCR region.".to_string();

        let tx = self.tx.clone();
        let repaint = self.egui_ctx.clone();
        thread::spawn(move || {
            let result = test_ocr(config);
            send_ui_event(&tx, &repaint, UiEvent::OcrTestFinished(result));
        });
    }

    fn start_bot(&mut self) {
        let Some(config) = self.config.ready_config() else {
            self.status = BotState::NeedsCalibration;
            self.status_message = "Finish all four calibration steps before starting.".to_string();
            return;
        };
        if self.config.targets().is_empty() {
            self.status = BotState::NeedsCalibration;
            self.status_message = "Add at least one target affix.".to_string();
            return;
        }
        if self.status == BotState::Running {
            return;
        }

        let stop = EscStopSignal::new();
        let stop_watcher_done = Arc::new(AtomicBool::new(false));
        self.stop_signal = Some(stop.clone());
        self.stop_watcher_done = Some(stop_watcher_done.clone());
        self.status = BotState::Running;
        self.status_message = "Running. Press ESC or Stop Bot to stop.".to_string();
        self.attempt = 0;

        let tx = self.tx.clone();
        let repaint = self.egui_ctx.clone();
        let stop_watcher = stop.clone();
        let stop_tx = tx.clone();
        let stop_repaint = repaint.clone();
        thread::spawn(move || {
            while !stop_watcher_done.load(Ordering::SeqCst) {
                if stop_watcher.is_stop_requested() {
                    stop_watcher.stop();
                    send_ui_event(&stop_tx, &stop_repaint, UiEvent::StopRequested);
                    break;
                }
                thread::sleep(Duration::from_millis(16));
            }
        });

        thread::spawn(move || {
            let runner = EnchantRunner::new(
                config,
                XcapRegionCapture,
                WindowsOcrReader::default(),
                SendInputController,
                stop,
            );
            let result = runner.run(|event| {
                send_ui_event(&tx, &repaint, UiEvent::BotEvent(event));
            });
            send_ui_event(&tx, &repaint, UiEvent::BotFinished(result.map(|_| ())));
        });
    }

    fn stop_bot(&mut self) {
        if let Some(stop) = &self.stop_signal {
            stop.stop();
        }
        self.status_message = "Stop requested.".to_string();
    }

    fn poll_events(&mut self, ctx: &Context) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                UiEvent::CaptureFinished(kind, result) => {
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                    self.handle_capture(kind, result);
                }
                UiEvent::MouseMovementRecorded(result) => {
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                    self.handle_mouse_movement_recorded(result);
                }
                UiEvent::OcrTestFinished(result) => self.handle_ocr_test(result),
                UiEvent::StopRequested => {
                    if self.status == BotState::Running {
                        self.status_message =
                            "Stop requested by ESC/global stop signal.".to_string();
                    }
                }
                UiEvent::BotEvent(event) => self.handle_bot_event(event),
                UiEvent::BotFinished(result) => {
                    if let Some(done) = self.stop_watcher_done.take() {
                        done.store(true, Ordering::SeqCst);
                    }
                    self.stop_signal = None;
                    if let Err(error) = result {
                        self.last_result = Some(live_status_result(
                            format!("Bot OCR/capture failed: {error}"),
                            self.active_ocr_rect.unwrap_or(Rect::new(0, 0, 0, 0)),
                        ));
                        self.status = BotState::Error;
                        self.status_message = format!("Bot stopped with error: {error}");
                    } else if self.status == BotState::Running {
                        self.status = BotState::Stopped;
                        self.status_message = "Bot stopped.".to_string();
                    }
                }
            }
            ctx.request_repaint();
        }
    }

    fn handle_capture(&mut self, kind: CaptureKind, result: anyhow::Result<CaptureValue>) {
        let Ok(value) = result else {
            self.status = BotState::Stopped;
            self.status_message = "Calibration cancelled.".to_string();
            return;
        };

        match (kind, value) {
            (CaptureKind::EnchantWindow, CaptureValue::Rect(rect)) => {
                self.config.enchant_window = Some(rect);
                self.status_message = "Window saved. Set the Enchant button next.".to_string();
            }
            (CaptureKind::AffixOcrRegion { window }, CaptureValue::Rect(rect)) => {
                self.config.ocr_region = Some(RectRatio::from_rect_relative(window, rect));
                self.status_message =
                    "Affix OCR region saved. Testing OCR automatically.".to_string();
                self.mark_dirty();
                self.begin_ocr_test();
                return;
            }
            (CaptureKind::EnchantButton { window }, CaptureValue::Rect(rect)) => {
                let ratio = RectRatio::from_rect_relative(window, rect);
                self.config.enchant_button_region = Some(ratio);
                self.config.enchant_button = Some(center_of_ratio(ratio));
                self.status_message = "Enchant button region saved.".to_string();
            }
            (CaptureKind::ReplaceButton { window }, CaptureValue::Rect(rect)) => {
                let ratio = RectRatio::from_rect_relative(window, rect);
                self.config.replace_button_region = Some(ratio);
                self.config.replace_button = Some(center_of_ratio(ratio));
                self.status_message = "Replace Affix button region saved.".to_string();
            }
            (CaptureKind::CloseButton { window }, CaptureValue::Rect(rect)) => {
                let ratio = RectRatio::from_rect_relative(window, rect);
                self.config.close_button_region = Some(ratio);
                self.config.close_button = Some(center_of_ratio(ratio));
                self.status_message = "Close button region saved.".to_string();
            }
        }

        self.status = if self.config.ready_config().is_some() {
            BotState::Ready
        } else {
            BotState::NeedsCalibration
        };
        self.mark_dirty();
    }

    fn handle_mouse_movement_recorded(&mut self, result: anyhow::Result<MouseMovementProfile>) {
        match result {
            Ok(profile) => {
                let samples = profile.samples.len();
                let duration_ms = profile.duration_ms;
                self.config.mouse_movement = Some(profile);
                self.status = if self.config.ready_config().is_some() {
                    BotState::Ready
                } else {
                    BotState::NeedsCalibration
                };
                self.status_message =
                    format!("Mouse movement saved: {samples} samples over {duration_ms} ms.");
                self.mark_dirty();
            }
            Err(error) => {
                self.status = BotState::Stopped;
                self.status_message = format!("Mouse movement recording cancelled: {error}");
            }
        }
    }

    fn handle_ocr_test(&mut self, result: anyhow::Result<TestOcrResult>) {
        match result {
            Ok(result) => {
                let result = self.retarget_ocr_result(result);
                self.status = if result.result.matched {
                    BotState::Matched
                } else {
                    BotState::Ready
                };
                self.status_message = if result.result.matched {
                    "Target affix detected in OCR region.".to_string()
                } else {
                    "OCR read completed with no target match.".to_string()
                };
                self.last_result = Some(result);
            }
            Err(error) => {
                let capture_rect = self
                    .config
                    .ocr_config()
                    .map(|config| config.enchant_window.rect_from_ratio(config.ocr_region))
                    .unwrap_or(Rect::new(0, 0, 0, 0));
                self.last_result = Some(live_status_result(
                    format!("OCR failed: {error}"),
                    capture_rect,
                ));
                self.status = BotState::Error;
                self.status_message = format!("OCR test failed: {error}");
            }
        }
    }

    fn handle_bot_event(&mut self, event: EnchantEvent) {
        match event {
            EnchantEvent::AttemptStarted { attempt } => {
                self.attempt = attempt;
                self.status = BotState::Running;
                self.status_message = format!("Attempt {attempt}: clicking Enchant.");
            }
            EnchantEvent::OcrReadStarted { rect } => {
                self.active_ocr_rect = Some(rect);
                self.last_result = Some(live_status_result("Scanning OCR region...", rect));
                self.status_message = format!("Attempt {}: scanning affix OCR.", self.attempt);
            }
            EnchantEvent::OcrReadFinished {
                result,
                ocr_time_ms,
            } => {
                let result = self.retarget_ocr_result(TestOcrResult {
                    result,
                    ocr_time_ms,
                    capture_rect: self.active_ocr_rect.unwrap_or(Rect::new(0, 0, 0, 0)),
                });
                let matched = result.result.matched;
                self.last_result = Some(result);
                self.status_message = if matched {
                    "Target matched. Leaving the result open for review.".to_string()
                } else {
                    "No match. Replacing and closing result.".to_string()
                };
            }
            EnchantEvent::TargetFound { .. } => {
                self.status = BotState::Matched;
                self.status_message = "Target found. Bot stopped before replace/close.".to_string();
            }
            EnchantEvent::MaxAttemptsReached { attempts } => {
                self.status = BotState::Stopped;
                self.status_message = format!("Stopped after {attempts} attempts.");
            }
            EnchantEvent::Stopped => {
                self.status = BotState::Stopped;
                self.status_message = "Stopped by ESC or Stop Bot.".to_string();
            }
            _ => {}
        }
    }

    fn retarget_ocr_result(&self, mut result: TestOcrResult) -> TestOcrResult {
        if result.result.normalized_text != "status message" {
            let raw_text = result.result.raw_text.clone();
            result.result = match_affix(
                &raw_text,
                &self.config.targets(),
                self.config.fuzzy_threshold,
            );
        }
        result
    }

    fn refresh_live_ocr_match(&mut self) {
        let Some(result) = self.last_result.take() else {
            return;
        };
        self.last_result = Some(self.retarget_ocr_result(result));
    }

    fn handle_live_match_setting_changed(&mut self, ctx: &Context) {
        self.refresh_live_ocr_match();
        self.mark_dirty();
        self.save_if_dirty();
        if self.status == BotState::Running {
            if let Some(stop) = &self.stop_signal {
                stop.stop();
            }
            self.status_message =
                "Target settings changed. Stop requested so the next run uses them.".to_string();
        }
        ctx.request_repaint();
    }
}

impl App for NativeApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.poll_events(ctx);
        self.save_if_dirty();

        TopBottomPanel::top("title_bar")
            .exact_height(42.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(8.0);
                    ui.label(RichText::new("BoBo Companion").strong().size(15.0));
                    ui.separator();
                    ui.label(
                        RichText::new("Occultist Affix Reroll")
                            .color(Color32::from_rgb(210, 214, 219)),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        status_pill(ui, self.status, self.status.label());
                    });
                });
            });

        TopBottomPanel::bottom("action_bar")
            .exact_height(112.0)
            .show(ctx, |ui| {
                self.bottom_bar(ui);
            });

        CentralPanel::default()
            .frame(
                Frame::none()
                    .fill(Color32::from_rgb(9, 11, 13))
                    .inner_margin(egui::Margin::same(12.0)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width((ui.available_width() - 14.0).max(0.0));
                        self.content(ui, ctx);
                    });
            });
    }
}

impl NativeApp {
    fn content(&mut self, ui: &mut Ui, ctx: &Context) {
        ui.set_width(ui.available_width());
        ui.vertical(|ui| {
            self.header(ui);
            ui.add_space(8.0);
            self.live_ocr(ui);
            ui.add_space(8.0);
            self.steps(ui, ctx);
            ui.add_space(8.0);
            if ui.available_width() >= 900.0 {
                ui.columns(2, |columns| {
                    self.setup_panel(&mut columns[0]);
                    self.status_panel(&mut columns[1]);
                });
            } else {
                self.setup_panel(ui);
                ui.add_space(8.0);
                self.status_panel(ui);
            }
        });
    }

    fn header(&self, ui: &mut Ui) {
        Frame::none()
            .fill(Color32::from_rgb(17, 20, 23))
            .stroke(Stroke::new(1.0, Color32::from_rgb(39, 45, 52)))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Occultist Affix Reroll").size(22.0).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new("Native Rust OCR").color(Color32::from_rgb(255, 145, 55)),
                        );
                    });
                });
                ui.label(
                    RichText::new("Live OCR enchant detection and automated reroll assistance")
                        .color(Color32::from_gray(150)),
                );
            });
    }

    fn live_ocr(&mut self, ui: &mut Ui) {
        let result = self.last_result.clone();
        let matched = result.as_ref().is_some_and(|r| r.result.matched);
        let accent = if matched {
            Color32::from_rgb(76, 202, 118)
        } else {
            Color32::from_rgb(239, 91, 76)
        };
        Frame::none()
            .fill(Color32::from_rgb(15, 17, 19))
            .stroke(Stroke::new(1.0, Color32::from_rgb(67, 45, 25)))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Live OCR Result").strong().size(15.0));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(if matched { "MATCH" } else { "NO MATCH" })
                                .color(accent)
                                .strong(),
                        );
                    });
                });
                ui.add_space(8.0);
                let raw = result
                    .as_ref()
                    .map(|r| r.result.raw_text.clone())
                    .unwrap_or_else(|| "No OCR result yet".to_string());
                let raw_display = if raw.trim().is_empty() {
                    "(no text detected in selected region)".to_string()
                } else {
                    raw
                };
                Frame::none()
                    .fill(Color32::from_rgb(11, 13, 15))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(42, 48, 54)))
                    .rounding(7.0)
                    .inner_margin(egui::Margin::symmetric(12.0, 10.0))
                    .show(ui, |ui| {
                        ui.set_min_height(46.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new(raw_display)
                                    .size(20.0)
                                    .strong()
                                    .color(Color32::from_rgb(255, 158, 58)),
                            );
                        });
                    });
                ui.add_space(8.0);
                Grid::new("ocr_metrics")
                    .num_columns(4)
                    .min_col_width(112.0)
                    .spacing([20.0, 4.0])
                    .show(ui, |ui| {
                        metric(ui, "Match", if matched { "Yes" } else { "No" }, accent);
                        metric(
                            ui,
                            "Closest",
                            result
                                .as_ref()
                                .and_then(|r| r.result.target.as_deref())
                                .unwrap_or("None"),
                            Color32::from_rgb(255, 158, 58),
                        );
                        metric(
                            ui,
                            "Score",
                            &format!(
                                "{:.0}%",
                                result.as_ref().map(|r| r.result.score).unwrap_or(0.0) * 100.0
                            ),
                            Color32::WHITE,
                        );
                        metric(
                            ui,
                            "OCR Time",
                            &format!("{} ms", result.as_ref().map(|r| r.ocr_time_ms).unwrap_or(0)),
                            Color32::WHITE,
                        );
                        ui.end_row();
                    });
                if let Some(result) = result.as_ref() {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!("Normalized: {}", result.result.normalized_text))
                            .size(12.0)
                            .color(Color32::from_gray(145)),
                    );
                    if result.capture_rect.width > 0 && result.capture_rect.height > 0 {
                        ui.label(
                            RichText::new(format!(
                                "Captured: {}",
                                format_rect(Some(result.capture_rect))
                            ))
                            .size(12.0)
                            .color(Color32::from_gray(145)),
                        );
                    }
                }
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Target Affix")
                            .size(15.0)
                            .strong()
                            .color(Color32::from_gray(220)),
                    );
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.config.targets_text)
                            .desired_width(ui.available_width())
                            .font(egui::TextStyle::Heading),
                    );
                    if response.changed() {
                        self.handle_live_match_setting_changed(ui.ctx());
                    }
                });
            });
    }

    fn steps(&mut self, ui: &mut Ui, ctx: &Context) {
        let window = self.config.enchant_window;
        let button_width = CALIBRATION_BUTTON_WIDTH;
        ui.horizontal(|ui| {
            if step_button(
                ui,
                button_width,
                "1",
                "Enchant Button",
                self.config.has_enchant_button(),
            )
            .clicked()
            {
                if let Some(window) = window {
                    self.begin_capture(ctx, CaptureKind::EnchantButton { window });
                } else {
                    self.status_message =
                        "First drag around the full Occultist window.".to_string();
                    self.begin_capture(ctx, CaptureKind::EnchantWindow);
                }
            }
            if step_button(
                ui,
                button_width,
                "2",
                "Affix OCR Region",
                self.config.ocr_region.is_some(),
            )
            .clicked()
            {
                if let Some(window) = window {
                    self.begin_capture(ctx, CaptureKind::AffixOcrRegion { window });
                } else {
                    self.status_message =
                        "First drag around the full Occultist window.".to_string();
                    self.begin_capture(ctx, CaptureKind::EnchantWindow);
                }
            }
            if step_button(
                ui,
                button_width,
                "3",
                "Replace Affix",
                self.config.has_replace_button(),
            )
            .clicked()
            {
                if let Some(window) = window {
                    self.begin_capture(ctx, CaptureKind::ReplaceButton { window });
                } else {
                    self.status_message =
                        "First drag around the full Occultist window.".to_string();
                    self.begin_capture(ctx, CaptureKind::EnchantWindow);
                }
            }
            if step_button(
                ui,
                button_width,
                "4",
                "Close Button",
                self.config.has_close_button(),
            )
            .clicked()
            {
                if let Some(window) = window {
                    self.begin_capture(ctx, CaptureKind::CloseButton { window });
                } else {
                    self.status_message =
                        "First drag around the full Occultist window.".to_string();
                    self.begin_capture(ctx, CaptureKind::EnchantWindow);
                }
            }
        });
    }

    fn setup_panel(&mut self, ui: &mut Ui) {
        panel(ui, "Enchant OCR Setup", |ui| {
            ui.set_width(ui.available_width());
            Grid::new("setup_grid")
                .num_columns(2)
                .spacing([18.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Max Attempts (0 = Infinite)");
                    if egui::DragValue::new(&mut self.config.max_attempts)
                        .clamp_range(0..=999)
                        .ui(ui)
                        .changed()
                    {
                        self.mark_dirty();
                    }
                    ui.end_row();
                    ui.label("Match Threshold");
                    if ui
                        .add(
                            Slider::new(&mut self.config.fuzzy_threshold, 0.0..=1.0)
                                .show_value(true),
                        )
                        .changed()
                    {
                        self.handle_live_match_setting_changed(ui.ctx());
                    }
                    ui.end_row();
                });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Calibrate Window").clicked() {
                    self.begin_capture(ui.ctx(), CaptureKind::EnchantWindow);
                }
                if ui.button("Test OCR").clicked() {
                    self.begin_ocr_test();
                }
                if ui.button("Record Mouse Movement").clicked() {
                    self.begin_mouse_movement_recording();
                }
            });
        });
    }

    fn status_panel(&self, ui: &mut Ui) {
        panel(ui, "Saved Calibration", |ui| {
            ui.set_width(ui.available_width());
            status_line(ui, "Window", format_rect(self.config.enchant_window));
            status_line(
                ui,
                "Affix OCR Region",
                format_rect_ratio(self.config.ocr_region),
            );
            status_line(
                ui,
                "Enchant Button Region",
                format_region_or_point(
                    self.config.enchant_button_region,
                    self.config.enchant_button,
                ),
            );
            status_line(
                ui,
                "Replace Button Region",
                format_region_or_point(
                    self.config.replace_button_region,
                    self.config.replace_button,
                ),
            );
            status_line(
                ui,
                "Close Button Region",
                format_region_or_point(self.config.close_button_region, self.config.close_button),
            );
            status_line(
                ui,
                "Mouse Movement",
                format_mouse_movement(self.config.mouse_movement.as_ref()),
            );
            ui.add_space(8.0);
            ui.label(RichText::new("Workflow").strong());
            ui.label(
                RichText::new(
                    "Enchant -> OCR scan -> stop on match -> Replace Affix -> Close -> repeat",
                )
                .size(12.0)
                .color(Color32::from_gray(150)),
            );
        });
    }

    fn bottom_bar(&mut self, ui: &mut Ui) {
        Frame::none()
            .fill(Color32::from_rgb(15, 18, 21))
            .stroke(Stroke::new(1.0, Color32::from_rgb(38, 44, 50)))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(10.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.vertical_centered(|ui| {
                    status_pill(ui, self.status, self.status.label());
                    ui.add(
                        egui::Label::new(
                            RichText::new(&self.status_message).color(Color32::from_gray(155)),
                        )
                        .wrap(true),
                    );
                    ui.add_space(6.0);
                    ui.horizontal_centered(|ui| {
                        let can_start = self.config.ready_config().is_some()
                            && self.status != BotState::Running;
                        let start = ui
                            .add_enabled_ui(can_start, |ui| {
                                ui.add_sized(
                                    [CALIBRATION_BUTTON_WIDTH, ACTION_BUTTON_HEIGHT],
                                    Button::new("Start Bot").fill(Color32::from_rgb(246, 111, 25)),
                                )
                            })
                            .inner;
                        if start.clicked() {
                            self.start_bot();
                        }
                        let stop = ui
                            .add_enabled_ui(self.status == BotState::Running, |ui| {
                                ui.add_sized(
                                    [CALIBRATION_BUTTON_WIDTH, ACTION_BUTTON_HEIGHT],
                                    Button::new("Stop Bot"),
                                )
                            })
                            .inner;
                        if stop.clicked() {
                            self.stop_bot();
                        }
                    });
                });
            });
    }
}

fn configure_style(ctx: &Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(9, 11, 13);
    visuals.window_fill = Color32::from_rgb(13, 16, 19);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(24, 28, 32);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(34, 39, 44);
    visuals.selection.bg_fill = Color32::from_rgb(244, 119, 32);
    ctx.set_visuals(visuals);
}

fn panel(ui: &mut Ui, title: &str, add_contents: impl FnOnce(&mut Ui)) {
    Frame::none()
        .fill(Color32::from_rgb(17, 20, 23))
        .stroke(Stroke::new(1.0, Color32::from_rgb(39, 45, 52)))
        .rounding(8.0)
        .inner_margin(egui::Margin::same(14.0))
        .show(ui, |ui| {
            ui.set_min_height(204.0);
            ui.label(RichText::new(title).strong().size(15.0));
            ui.add_space(10.0);
            add_contents(ui);
        });
}

fn step_button(ui: &mut Ui, width: f32, step: &str, label: &str, complete: bool) -> egui::Response {
    let fill = if complete {
        Color32::from_rgb(23, 48, 32)
    } else {
        Color32::from_rgb(27, 31, 35)
    };
    let stroke = if complete {
        Stroke::new(1.0, Color32::from_rgb(74, 159, 96))
    } else {
        Stroke::new(1.0, Color32::from_rgb(48, 55, 62))
    };
    let text = RichText::new(format!("{step}  {label}"))
        .size(13.0)
        .strong()
        .color(if complete {
            Color32::from_rgb(205, 255, 218)
        } else {
            Color32::WHITE
        });
    ui.add_sized([width, 38.0], Button::new(text).fill(fill).stroke(stroke))
}

fn metric(ui: &mut Ui, label: &str, value: &str, color: Color32) {
    ui.vertical(|ui| {
        ui.set_min_width(112.0);
        ui.add(
            egui::Label::new(
                RichText::new(label)
                    .size(12.0)
                    .color(Color32::from_gray(145)),
            )
            .wrap(false),
        );
        ui.add(egui::Label::new(RichText::new(value).size(14.0).strong().color(color)).wrap(false));
    });
}

fn status_pill(ui: &mut Ui, status: BotState, label: &str) {
    let color = match status {
        BotState::Running | BotState::Matched => Color32::from_rgb(76, 202, 118),
        BotState::Error | BotState::NeedsCalibration => Color32::from_rgb(239, 91, 76),
        BotState::Calibrating | BotState::RecordingMovement | BotState::TestingOcr => {
            Color32::from_rgb(255, 158, 58)
        }
        _ => Color32::from_rgb(130, 139, 148),
    };
    Frame::none()
        .fill(Color32::from_rgb(17, 20, 23))
        .stroke(Stroke::new(1.0, Color32::from_rgb(42, 48, 54)))
        .rounding(999.0)
        .inner_margin(egui::Margin::symmetric(10.0, 4.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.0, color);
                ui.label(
                    RichText::new(label)
                        .size(12.0)
                        .color(Color32::from_gray(210)),
                );
            });
        });
}

fn status_line(ui: &mut Ui, label: &str, value: String) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(Color32::from_gray(145)));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).color(Color32::from_rgb(96, 210, 124)));
        });
    });
}

fn format_rect(rect: Option<Rect>) -> String {
    rect.map(|r| format!("{}x{} at {}, {}", r.width, r.height, r.x, r.y))
        .unwrap_or_else(|| "Not set".to_string())
}

fn format_rect_ratio(rect: Option<RectRatio>) -> String {
    rect.map(|r| format!("{:.2}x{:.2} at {:.2}, {:.2}", r.width, r.height, r.x, r.y))
        .unwrap_or_else(|| "Not set".to_string())
}

fn format_region_or_point(region: Option<RectRatio>, point: Option<PointRatio>) -> String {
    if region.is_some() {
        return format_rect_ratio(region);
    }
    point
        .map(|p| format!("Point {:.2}, {:.2}", p.x, p.y))
        .unwrap_or_else(|| "Not set".to_string())
}

fn format_mouse_movement(profile: Option<&MouseMovementProfile>) -> String {
    profile
        .map(|profile| {
            if let Some(model) = profile.model {
                format!(
                    "Modeled, {} points, {} ms, {:.0} px",
                    model.point_count, profile.duration_ms, profile.distance_px
                )
            } else {
                format!(
                    "{} learned steps, {} ms, {:.0} px",
                    profile.movement_steps.len().max(profile.samples.len()),
                    profile.duration_ms,
                    profile.distance_px
                )
            }
        })
        .unwrap_or_else(|| "Direct cursor jump".to_string())
}

fn center_of_ratio(rect: RectRatio) -> PointRatio {
    PointRatio {
        x: rect.x + rect.width / 2.0,
        y: rect.y + rect.height / 2.0,
    }
}

fn live_status_result(message: impl Into<String>, capture_rect: Rect) -> TestOcrResult {
    TestOcrResult {
        result: MatchResult {
            matched: false,
            target: None,
            score: 0.0,
            raw_text: message.into(),
            normalized_text: "status message".to_string(),
        },
        ocr_time_ms: 0,
        capture_rect,
    }
}

fn send_ui_event(tx: &Sender<UiEvent>, ctx: &Context, event: UiEvent) {
    let _ = tx.send(event);
    ctx.request_repaint();
}

fn load_window_icon() -> Option<egui::IconData> {
    let mut candidates = vec![exe_root_dir().join("app_icon.png")];
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("app_icon.png"),
    );

    for path in candidates {
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        match eframe::icon_data::from_png_bytes(&bytes) {
            Ok(icon) => return Some(icon),
            Err(error) => {
                eprintln!("failed to load app icon: {error}");
            }
        }
    }
    None
}

fn test_ocr(config: EnchantConfig) -> anyhow::Result<TestOcrResult> {
    let started = Instant::now();
    let capture = XcapRegionCapture;
    let ocr = WindowsOcrReader::default();
    let rect = config.enchant_window.rect_from_ratio(config.ocr_region);
    let image = RegionCapture::capture_region(&capture, rect)?;
    let raw_text = OcrReader::read_text(&ocr, &image)?;
    let result = match_affix(&raw_text, &config.targets, config.fuzzy_threshold);
    Ok(TestOcrResult {
        result,
        ocr_time_ms: started.elapsed().as_millis() as u64,
        capture_rect: rect,
    })
}

fn config_path() -> PathBuf {
    exe_root_dir().join("enchant_config_native.json")
}

fn exe_root_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn legacy_config_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from).map(|base| {
        base.join("BoBo Companion")
            .join("enchant_config_native.json")
    })
}

fn load_native_config(path: &PathBuf) -> (NativeConfig, bool) {
    let (contents, migrated_config) = match fs::read_to_string(path) {
        Ok(contents) => (contents, false),
        Err(_) => {
            let Some(legacy_path) = legacy_config_path() else {
                return (NativeConfig::default(), true);
            };
            match fs::read_to_string(legacy_path) {
                Ok(contents) => (contents, true),
                Err(_) => return (NativeConfig::default(), true),
            }
        }
    };
    let mut config: NativeConfig = serde_json::from_str(&contents).unwrap_or_default();
    if config.mouse_movement.is_none() {
        config.mouse_movement = Some(default_mouse_movement_profile());
    }
    (config, migrated_config)
}

fn save_native_config(path: &PathBuf, config: &NativeConfig) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(config)?)?;
    Ok(())
}
