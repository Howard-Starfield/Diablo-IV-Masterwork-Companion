#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Cursor,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

mod engine;
mod macro_ui;
mod ui_state;
mod ui_theme;

use crate::engine::{
    config::{EnchantConfig, MouseMovementProfile, default_mouse_movement_profile},
    enchant_loop::{EnchantEvent, EnchantRunner, OcrReader, RegionCapture},
    macro_engine::{
        AssetRef, AssetStore, ClusterPolicy, ControllerRunRequest, DEFAULT_MAX_SCORE_CELLS,
        ImageMatchCandidate, ImageMatchConfig, ImageMatchResult, ImageMatcher, ImageRule,
        ImageRuleVerification, ImageRuleVerificationInput, LocalImageRuleReverification,
        LocalImageRuleVerificationInput, LocalNegativeImageSample, MacroController, MacroStore,
        NegativeCorpusSample, NegativeSampleEvaluationInputs, PendingImageImport,
        PreparedPackageImport, RegionDefinition, RunEvent, RunMode, SavedRevision, TargetProfile,
        authoring_test_text_rule, cluster_peaks,
    },
    matcher::{MatchResult, match_affix},
    platform::{
        CaptureRequestId, CapturedTargetBinding, CapturedTargetProfile, EscStopSignal,
        MacroCaptureKind, MacroCaptureRequest, MacroCaptureSelection, SendInputController,
        WindowsMacroRuntimeBundle, WindowsOcrReader, XcapRegionCapture,
        build_windows_macro_runtime, enable_per_monitor_dpi_awareness, preferred_window_placement,
        record_mouse_movement_profile, resolve_target_from_selection, select_macro_capture,
        select_screen_rect,
    },
    types::{PointRatio, Rect, RectRatio},
};
use crate::macro_ui::{
    AuthoringSessionId, EditorAuthoringKind, EditorAuthoringOutcome, EditorAuthoringRequest,
    EditorAuthoringResult, EditorDraft, ImagePackageReverificationStage, MacroIntent, MacroPage,
    MacroPageState, RunDefinitionSnapshot, SavedMacroIdentity, WizardAuthoringKind,
    WizardAuthoringOutcome, WizardAuthoringRequest, WizardAuthoringResult, WizardDetector,
};
use crate::ui_state::UiStateStore;
use eframe::{
    App, CreationContext,
    egui::{
        self, Align, Button, CentralPanel, Color32, Context, Frame, Grid, Layout, RichText, Sense,
        Slider, Stroke, TopBottomPanel, Ui, Vec2, ViewportCommand, Widget,
    },
};
use image::{DynamicImage, GrayImage, ImageFormat, Luma};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, SetLastError, WIN32_ERROR,
        },
        System::Threading::CreateMutexW,
        UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW},
    },
    core::{HSTRING, w},
};

const APP_WIDTH: f32 = 900.0;
const APP_HEIGHT: f32 = 1080.0;
const MIN_APP_WIDTH: f32 = 720.0;
const MIN_APP_HEIGHT: f32 = 680.0;
const CALIBRATION_BUTTON_WIDTH: f32 = 138.0;
const ACTION_BUTTON_HEIGHT: f32 = 38.0;
const SINGLE_INSTANCE_MUTEX_NAME: &str = "Local\\BoBoCompanion.SingleInstance.v1";

fn macro_run_request(intent: &MacroIntent) -> Option<ControllerRunRequest> {
    match intent {
        MacroIntent::DryRun => Some(ControllerRunRequest::once(RunMode::DryRun)),
        MacroIntent::RunOnce => Some(ControllerRunRequest::once(RunMode::ObservationOnly)),
        MacroIntent::Run => Some(ControllerRunRequest::continuous(RunMode::ObservationOnly)),
        MacroIntent::RunLive => Some(ControllerRunRequest::continuous(RunMode::Live)),
        _ => None,
    }
}

struct NamedMutexCreation<H> {
    handle: H,
    already_exists: bool,
}

trait NamedMutexBackend: Clone {
    type Handle;

    fn create(&self, name: &str) -> anyhow::Result<NamedMutexCreation<Self::Handle>>;
    fn close(&self, handle: Self::Handle);
}

#[derive(Debug, Clone, Copy)]
struct Win32NamedMutexBackend;

impl NamedMutexBackend for Win32NamedMutexBackend {
    type Handle = HANDLE;

    fn create(&self, name: &str) -> anyhow::Result<NamedMutexCreation<Self::Handle>> {
        let name = HSTRING::from(name);
        unsafe { SetLastError(WIN32_ERROR(0)) };
        let handle = unsafe { CreateMutexW(None, false, &name) }
            .map_err(|error| anyhow::anyhow!("failed to create single-instance mutex: {error}"))?;
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        Ok(NamedMutexCreation {
            handle,
            already_exists,
        })
    }

    fn close(&self, handle: Self::Handle) {
        let _ = unsafe { CloseHandle(handle) };
    }
}

struct SingleInstanceGuard<B: NamedMutexBackend> {
    backend: B,
    handle: Option<B::Handle>,
}

impl<B: NamedMutexBackend> SingleInstanceGuard<B> {
    fn acquire_with(backend: B, name: &str) -> anyhow::Result<Option<Self>> {
        let created = backend.create(name)?;
        if created.already_exists {
            backend.close(created.handle);
            return Ok(None);
        }
        Ok(Some(Self {
            backend,
            handle: Some(created.handle),
        }))
    }
}

impl<B: NamedMutexBackend> Drop for SingleInstanceGuard<B> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.backend.close(handle);
        }
    }
}

fn show_startup_notice(message: &str) {
    let message = HSTRING::from(message);
    unsafe {
        MessageBoxW(
            None,
            &message,
            w!("BoBo Companion"),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

fn main() -> eframe::Result<()> {
    enable_per_monitor_dpi_awareness();

    // Keep this guard alive for the entire UI process so staged macro revisions cannot be
    // concurrently recovered or replaced by another application instance.
    let _single_instance = match SingleInstanceGuard::acquire_with(
        Win32NamedMutexBackend,
        SINGLE_INSTANCE_MUTEX_NAME,
    ) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            show_startup_notice("BoBo Companion is already running.");
            return Ok(());
        }
        Err(error) => {
            show_startup_notice(&format!(
                "BoBo Companion could not establish its single-instance safety boundary:\n\n{error}"
            ));
            return Ok(());
        }
    };

    let (ui_state_store, ui_state_warning) = UiStateStore::open(ui_state_path());
    let placement = preferred_window_placement([APP_WIDTH, APP_HEIGHT]);
    let always_on_top = ui_state_store.state.always_on_top;
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("BoBo Companion")
        .with_inner_size(placement.inner_size)
        .with_min_inner_size([
            MIN_APP_WIDTH.min(placement.inner_size[0]),
            MIN_APP_HEIGHT.min(placement.inner_size[1]),
        ])
        .with_position(placement.outer_position)
        .with_window_level(if always_on_top {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        });
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
        Box::new(move |cc| Box::new(NativeApp::new(cc, ui_state_store, ui_state_warning))),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeConfig {
    targets_text: String,
    fuzzy_threshold: f64,
    #[serde(default)]
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
    MacroAuthoringFinished {
        request: WizardAuthoringRequest,
        outcome: NativeWizardAuthoringOutcome,
        target_binding: Option<CapturedTargetBinding>,
    },
    EditorAuthoringFinished {
        request: EditorAuthoringRequest,
        outcome: NativeEditorAuthoringOutcome,
        target_binding: Option<CapturedTargetBinding>,
    },
    ImagePackageReverificationFinished {
        request: ImagePackageReverificationRequest,
        outcome: NativeImagePackageReverificationOutcome,
    },
}

#[derive(Debug)]
enum NativeWizardAuthoringOutcome {
    Complete(WizardAuthoringOutcome),
    CapturedTemplate(PendingTemplateCapture),
}

#[derive(Debug)]
enum NativeEditorAuthoringOutcome {
    Complete(EditorAuthoringOutcome),
    CapturedTemplate(PendingTemplateCapture),
}

#[derive(Debug)]
struct PendingTemplateCapture {
    bytes: Vec<u8>,
    previous: Option<AssetRef>,
}

#[derive(Debug)]
struct PublishedTemplateCapture {
    asset: AssetRef,
    staged_successor: bool,
}

/// Native-only transaction state for image packages. Unlike editor authoring,
/// this never exposes a draft or asset store to the UI and cannot publish an
/// authoring asset. The persistence verifier owns the only asset installation.
#[derive(Debug)]
struct PendingImagePackageReverification {
    pending: PendingImageImport,
    binding: Option<CapturedTargetBinding>,
    active_rule_index: usize,
    evidence: Option<LocalImagePackageEvidence>,
    completions: Vec<LocalImageRuleReverification>,
    next_request_id: u64,
    guard: ImagePackageReverificationSessionGuard,
}

#[derive(Debug)]
struct LocalImagePackageEvidence {
    region: RegionDefinition,
    template_png: Option<Vec<u8>>,
    target_region_png: Option<Vec<u8>>,
    negative_pngs: Vec<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct ImagePackageReverificationRequest {
    token: ImagePackageReverificationToken,
    step: ImagePackageReverificationStep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImagePackageReverificationToken {
    generation: u64,
    request_id: u64,
    expected_stage: ImagePackageReverificationStage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImagePackageReverificationSessionGuard {
    generation: u64,
    stage: ImagePackageReverificationStage,
    in_flight_request_id: Option<u64>,
}

impl ImagePackageReverificationSessionGuard {
    fn accepts(self, token: &ImagePackageReverificationToken) -> bool {
        self.generation == token.generation
            && self.stage == token.expected_stage
            && self.in_flight_request_id == Some(token.request_id)
    }
}

#[derive(Debug, Clone)]
enum ImagePackageReverificationStep {
    CaptureTarget,
    CaptureRegion {
        binding: CapturedTargetBinding,
        region_id: String,
        region_revision: u64,
    },
    CaptureTemplate {
        binding: CapturedTargetBinding,
        region: RegionDefinition,
    },
    CaptureNegative {
        binding: CapturedTargetBinding,
        region: RegionDefinition,
    },
}

#[derive(Debug)]
enum NativeImagePackageReverificationOutcome {
    CapturedTarget(CapturedTargetBinding),
    CapturedRegion(RegionDefinition),
    CapturedTemplate {
        template_png: Vec<u8>,
        target_region_png: Vec<u8>,
    },
    CapturedNegative(Vec<u8>),
    Failed(String),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPage {
    Enchant,
    Macro,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BottomSurface {
    EnchantActions,
    MacroMonitor,
}

fn bottom_surface(page: AppPage) -> BottomSurface {
    match page {
        AppPage::Enchant => BottomSurface::EnchantActions,
        AppPage::Macro => BottomSurface::MacroMonitor,
    }
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
    page: AppPage,
    macro_state: MacroPageState,
    config: NativeConfig,
    config_path: PathBuf,
    ui_state_store: UiStateStore,
    ui_state_warning: Option<String>,
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
    macro_authoring_targets: HashMap<AuthoringSessionId, CapturedTargetBinding>,
    macro_store: Option<Arc<MacroStore>>,
    selected_saved_revision: Option<SavedRevision>,
    macro_controller: Option<MacroController>,
    active_macro_bundle: Option<WindowsMacroRuntimeBundle>,
    pending_image_package_reverification: Option<PendingImagePackageReverification>,
    next_image_package_reverification_generation: u64,
}

impl NativeApp {
    fn new(
        cc: &CreationContext<'_>,
        ui_state_store: UiStateStore,
        ui_state_warning: Option<String>,
    ) -> Self {
        ui_theme::apply(&cc.egui_ctx);
        let (tx, rx) = mpsc::channel();
        let config_path = config_path();
        let (config, migrated_config) = load_native_config(&config_path);
        let macro_store = open_macro_authoring_store();
        Self {
            page: AppPage::Enchant,
            macro_state: MacroPageState::default(),
            config,
            config_path,
            ui_state_store,
            ui_state_warning,
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
            macro_authoring_targets: HashMap::new(),
            macro_store,
            selected_saved_revision: None,
            macro_controller: None,
            active_macro_bundle: None,
            pending_image_package_reverification: None,
            next_image_package_reverification_generation: 1,
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

    fn save_ui_state_if_dirty(&mut self) {
        if let Err(error) = self.ui_state_store.save_if_dirty() {
            self.ui_state_warning = Some(format!("Could not save UI preferences: {error}"));
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

    fn refresh_macro_library(&mut self) {
        let Some(store) = self.macro_store.as_ref() else {
            return;
        };
        let rows = store
            .list_macros()
            .ok()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|summary| {
                store.load_current(&summary.id).ok().map(|saved| {
                    crate::macro_ui::project_definition(
                        &saved.definition,
                        Some(saved.definition.revision),
                        summary.enabled,
                        &[],
                        &Default::default(),
                        None,
                    )
                })
            })
            .collect();
        self.macro_state.library_rows = rows;
    }

    fn select_saved_macro(&mut self, macro_id: String) {
        if self.macro_controller.is_some() {
            self.macro_state.editor_feedback =
                Some("Finish the active run before selecting another saved macro.".into());
            return;
        }
        let Some(store) = self.macro_store.as_ref() else {
            self.macro_state.editor_feedback = Some("Macro store is unavailable.".into());
            return;
        };
        match store.load_current(&macro_id) {
            Ok(saved) => {
                let revision = saved.definition.revision;
                self.selected_saved_revision = Some(saved.clone());
                self.macro_state.load_saved_draft(
                    saved.definition,
                    SavedMacroIdentity {
                        macro_id,
                        revision,
                        definition_hash: saved.definition_hash,
                    },
                )
            }
            Err(error) => {
                self.macro_state.editor_feedback = Some(format!("Could not load macro: {error}"))
            }
        }
    }

    fn save_macro_draft(&mut self) {
        let Some(store) = self.macro_store.as_ref() else {
            self.macro_state.editor_feedback = Some("Macro store is unavailable.".into());
            return;
        };
        let Some(draft) = self.macro_state.draft.as_ref() else {
            return;
        };
        if !crate::macro_ui::editor_validation_problems(draft).is_empty() {
            self.macro_state.editor_feedback = Some("Validate the draft before saving.".into());
            return;
        }
        let mut candidate = draft.definition.clone();
        candidate.revision = self
            .macro_state
            .selected_saved
            .as_ref()
            .map(|saved| saved.revision.saturating_add(1))
            .unwrap_or(1);
        match store.save_validated(candidate) {
            Ok(saved) => {
                self.selected_saved_revision = Some(saved.clone());
                let identity = SavedMacroIdentity {
                    macro_id: saved.definition.id.clone(),
                    revision: saved.definition.revision,
                    definition_hash: saved.definition_hash,
                };
                self.macro_state
                    .load_saved_draft(saved.definition, identity);
                self.macro_state.editor_feedback = Some("Saved validated revision.".into());
                self.refresh_macro_library();
            }
            Err(error) => self.macro_state.editor_feedback = Some(format!("Save failed: {error}")),
        }
    }

    fn start_saved_macro(&mut self, request: ControllerRunRequest) {
        if self.macro_controller.is_some() {
            self.macro_state.editor_feedback = Some("A macro run is already active.".into());
            return;
        }
        let Some(store) = self.macro_store.as_ref() else {
            self.macro_state.editor_feedback = Some("Macro store is unavailable.".into());
            return;
        };
        let Some(selected) = self.macro_state.selected_saved.clone() else {
            self.macro_state.editor_feedback =
                Some("Select a saved revision before running.".into());
            return;
        };
        let Some(native_selected) = self.selected_saved_revision.as_ref() else {
            self.macro_state.editor_feedback =
                Some("Reload the saved revision before running.".into());
            return;
        };
        if native_selected.definition.id != selected.macro_id
            || native_selected.definition.revision != selected.revision
            || native_selected.definition_hash != selected.definition_hash
        {
            self.macro_state.editor_feedback =
                Some("Saved selection changed; reload it before running.".into());
            return;
        }
        let Ok(saved) = store.load_current(&selected.macro_id) else {
            self.macro_state.editor_feedback =
                Some("Selected saved revision is unavailable.".into());
            return;
        };
        if saved.definition.revision != selected.revision
            || saved.definition_hash != selected.definition_hash
        {
            self.macro_state.editor_feedback =
                Some("Saved revision changed; reload it before running.".into());
            return;
        }
        self.selected_saved_revision = Some(saved.clone());
        let binding = self
            .macro_state
            .active_draft_session()
            .and_then(|session| self.macro_authoring_targets.get(&session))
            .filter(|binding| binding_matches_target_profile(binding, &saved.definition.target));
        let Some(binding) = binding else {
            self.macro_state.editor_feedback =
                Some("Capture the exact saved target before running this revision.".into());
            return;
        };
        let bundle = match build_windows_macro_runtime(binding, &saved.definition.target) {
            Ok(bundle) => bundle,
            Err(error) => {
                self.macro_state.editor_feedback =
                    Some(format!("Live runtime setup failed: {error}"));
                return;
            }
        };
        let controller = MacroController::new(bundle.runtime.clone(), Arc::clone(store), 256);
        if let Err(error) = controller.start_saved(&selected.macro_id, request) {
            self.macro_state.editor_feedback = Some(format!("Run failed to start: {error}"));
            return;
        }
        self.macro_controller = Some(controller);
        self.active_macro_bundle = Some(bundle);
        self.macro_state.controller_lifecycle = None;
        self.macro_state.controller_semantic = None;
    }

    fn sync_macro_runtime(&mut self) {
        let Some(controller) = self.macro_controller.as_ref() else {
            return;
        };
        let saved = controller.active_revision();
        let snapshot = controller.drain_monitor_snapshot();
        if let (Some(saved), Some(RunEvent::RunStarted { run_id, .. })) =
            (saved.as_ref(), snapshot.lifecycle.run_started.as_ref())
        {
            self.macro_state.running_snapshot = Some(RunDefinitionSnapshot::from_saved(
                run_id.clone(),
                saved.clone(),
            ));
        }
        self.macro_state.controller_lifecycle = Some(snapshot.lifecycle);
        self.macro_state.controller_semantic = Some(snapshot.semantic);
        for event in snapshot.replay {
            if self.macro_state.runtime_events.len() >= 256 {
                self.macro_state.runtime_events.remove(0);
            }
            if let RunEvent::RunStarted { run_id, .. } = &event {
                if let Some(saved) = saved.clone() {
                    self.macro_state.running_snapshot =
                        Some(RunDefinitionSnapshot::from_saved(run_id.clone(), saved));
                }
            }
            self.macro_state.runtime_events.push(event);
        }
        if controller.status() == crate::engine::macro_engine::MacroControllerStatus::Idle {
            if let Err(error) = controller.wait_until_idle(Duration::from_secs(2)) {
                self.macro_state.editor_feedback =
                    Some(format!("Macro worker shutdown failed: {error}"));
                return;
            }
            self.macro_controller = None;
            self.active_macro_bundle = None;
            self.macro_state.running_snapshot = None;
            self.refresh_macro_library();
        }
    }

    fn begin_image_package_reverification(&mut self, pending: PendingImageImport) {
        if self.pending_image_package_reverification.is_some() {
            self.macro_state.editor_feedback = Some(
                "Finish or cancel the current local image-package re-verification first.".into(),
            );
            return;
        }
        let rule_ids = pending.image_rule_ids().to_vec();
        let generation = self.next_image_package_reverification_generation;
        self.next_image_package_reverification_generation = self
            .next_image_package_reverification_generation
            .checked_add(1)
            .expect("image package re-verification generation overflow");
        self.macro_state
            .begin_image_package_reverification(rule_ids);
        self.macro_state.editor_feedback = Some(
            "Portable image data was discarded. Capture fresh local evidence for every image rule."
                .into(),
        );
        self.pending_image_package_reverification = Some(PendingImagePackageReverification {
            pending,
            binding: None,
            active_rule_index: 0,
            evidence: None,
            completions: Vec::new(),
            next_request_id: 1,
            guard: ImagePackageReverificationSessionGuard {
                generation,
                stage: ImagePackageReverificationStage::CaptureTarget,
                in_flight_request_id: None,
            },
        });
    }

    fn discard_image_package_reverification(&mut self, message: impl Into<String>) {
        self.pending_image_package_reverification = None;
        self.macro_state.clear_image_package_reverification();
        self.macro_state.editor_feedback = Some(message.into());
    }

    fn begin_next_image_package_capture(&mut self) {
        let request = {
            let Some(session) = self.pending_image_package_reverification.as_mut() else {
                self.macro_state.editor_feedback =
                    Some("No local image-package import is awaiting re-verification.".into());
                return;
            };
            if session.guard.in_flight_request_id.is_some() {
                return;
            }
            let id = session.next_request_id;
            session.next_request_id = session.next_request_id.saturating_add(1);
            let rule_id = match session
                .pending
                .image_rule_ids()
                .get(session.active_rule_index)
            {
                Some(rule_id) => rule_id,
                None => {
                    self.discard_image_package_reverification(
                        "Image package re-verification lost its active rule; import was discarded.",
                    );
                    return;
                }
            };
            let step = match (&session.binding, &session.evidence) {
                (None, _) => ImagePackageReverificationStep::CaptureTarget,
                (Some(binding), None) => {
                    let source_region = session
                        .pending
                        .definition()
                        .image_rules
                        .iter()
                        .find(|rule| rule.id == *rule_id)
                        .and_then(|rule| {
                            session
                                .pending
                                .definition()
                                .regions
                                .iter()
                                .find(|region| region.id == rule.region_id)
                        });
                    let Some(source_region) = source_region else {
                        self.discard_image_package_reverification(
                            "Image package re-verification is missing a rule region; import was discarded.",
                        );
                        return;
                    };
                    ImagePackageReverificationStep::CaptureRegion {
                        binding: binding.clone(),
                        region_id: source_region.id.clone(),
                        // The portable geometry and revision are deliberately
                        // non-authoritative. The preserved id binds this fresh
                        // local capture to the pending rule; revision restarts
                        // from local evidence rather than the package value.
                        region_revision: 1,
                    }
                }
                (Some(binding), Some(evidence)) if evidence.template_png.is_none() => {
                    ImagePackageReverificationStep::CaptureTemplate {
                        binding: binding.clone(),
                        region: evidence.region.clone(),
                    }
                }
                (Some(binding), Some(evidence)) if evidence.negative_pngs.is_empty() => {
                    ImagePackageReverificationStep::CaptureNegative {
                        binding: binding.clone(),
                        region: evidence.region.clone(),
                    }
                }
                _ => {
                    self.discard_image_package_reverification(
                        "Image package re-verification evidence was inconsistent; import was discarded.",
                    );
                    return;
                }
            };
            let token = ImagePackageReverificationToken {
                generation: session.guard.generation,
                request_id: id,
                expected_stage: session.guard.stage,
            };
            session.guard.in_flight_request_id = Some(id);
            ImagePackageReverificationRequest { token, step }
        };
        let tx = self.tx.clone();
        let repaint = self.egui_ctx.clone();
        thread::spawn(move || {
            let outcome = run_image_package_reverification_request(&request);
            send_ui_event(
                &tx,
                &repaint,
                UiEvent::ImagePackageReverificationFinished { request, outcome },
            );
        });
    }

    fn apply_image_package_reverification_result(
        &mut self,
        request: ImagePackageReverificationRequest,
        outcome: NativeImagePackageReverificationOutcome,
    ) {
        let Some(mut session) = self.pending_image_package_reverification.take() else {
            return;
        };
        if !session.guard.accepts(&request.token) {
            self.pending_image_package_reverification = Some(session);
            self.macro_state.editor_feedback =
                Some("Discarded stale local image-package capture result.".into());
            return;
        }
        session.guard.in_flight_request_id = None;

        match outcome {
            NativeImagePackageReverificationOutcome::CapturedTarget(binding) => {
                session.binding = Some(binding);
                session.guard.stage = ImagePackageReverificationStage::CaptureRegion;
                self.macro_state.set_image_package_reverification_stage(
                    session.active_rule_index,
                    ImagePackageReverificationStage::CaptureRegion,
                );
                self.pending_image_package_reverification = Some(session);
            }
            NativeImagePackageReverificationOutcome::CapturedRegion(region) => {
                session.evidence = Some(LocalImagePackageEvidence {
                    region,
                    template_png: None,
                    target_region_png: None,
                    negative_pngs: Vec::new(),
                });
                session.guard.stage = ImagePackageReverificationStage::CaptureTemplate;
                self.macro_state.set_image_package_reverification_stage(
                    session.active_rule_index,
                    ImagePackageReverificationStage::CaptureTemplate,
                );
                self.pending_image_package_reverification = Some(session);
            }
            NativeImagePackageReverificationOutcome::CapturedTemplate {
                template_png,
                target_region_png,
            } => {
                let Some(evidence) = session.evidence.as_mut() else {
                    self.discard_image_package_reverification(
                        "Template capture arrived without local region evidence; import was discarded.",
                    );
                    return;
                };
                evidence.template_png = Some(template_png);
                evidence.target_region_png = Some(target_region_png);
                session.guard.stage = ImagePackageReverificationStage::CaptureNegative;
                self.macro_state.set_image_package_reverification_stage(
                    session.active_rule_index,
                    ImagePackageReverificationStage::CaptureNegative,
                );
                self.pending_image_package_reverification = Some(session);
            }
            NativeImagePackageReverificationOutcome::CapturedNegative(negative_png) => {
                let completion = complete_pending_image_rule_reverification(
                    self.macro_store.as_deref(),
                    &session,
                    negative_png,
                );
                let completion = match completion {
                    Ok(completion) => completion,
                    Err(error) => {
                        self.discard_image_package_reverification(format!(
                            "Local image re-verification failed; import was discarded: {error}"
                        ));
                        return;
                    }
                };
                session.completions.push(completion);
                session.active_rule_index += 1;
                session.evidence = None;
                if session.active_rule_index < session.pending.image_rule_ids().len() {
                    session.guard.stage = ImagePackageReverificationStage::CaptureRegion;
                    self.macro_state.set_image_package_reverification_stage(
                        session.active_rule_index,
                        ImagePackageReverificationStage::CaptureRegion,
                    );
                    self.pending_image_package_reverification = Some(session);
                    return;
                }
                let Some(store) = self.macro_store.as_ref() else {
                    self.discard_image_package_reverification(
                        "Macro store became unavailable; image import was discarded.",
                    );
                    return;
                };
                match store.commit_image_package_import(session.pending, session.completions) {
                    Ok(saved) => {
                        let identity = SavedMacroIdentity {
                            macro_id: saved.definition.id.clone(),
                            revision: saved.definition.revision,
                            definition_hash: saved.definition_hash.clone(),
                        };
                        self.selected_saved_revision = Some(saved.clone());
                        self.macro_state
                            .load_saved_draft(saved.definition, identity);
                        self.macro_state.clear_image_package_reverification();
                        self.macro_state.editor_feedback = Some(
                            "Image package imported after local verifier-owned re-verification."
                                .into(),
                        );
                        self.refresh_macro_library();
                    }
                    Err(error) => self.discard_image_package_reverification(format!(
                        "Image package commit failed; import was discarded: {error}"
                    )),
                }
            }
            NativeImagePackageReverificationOutcome::Failed(error) => self
                .discard_image_package_reverification(format!(
                    "Local image capture failed; import was discarded: {error}"
                )),
        }
    }

    fn dispatch_macro_intents(&mut self) {
        if self.macro_state.selected_saved.is_none() {
            self.selected_saved_revision = None;
        }
        while let Some(intent) = self.macro_state.take_intent() {
            if let Some(request) = macro_run_request(&intent) {
                self.start_saved_macro(request);
                continue;
            }
            match intent {
                MacroIntent::Select { macro_id } => self.select_saved_macro(macro_id),
                MacroIntent::Validate => self.macro_state.validate_draft(),
                MacroIntent::Save => self.save_macro_draft(),
                MacroIntent::DryRun
                | MacroIntent::RunOnce
                | MacroIntent::Run
                | MacroIntent::RunLive => unreachable!("run intents are handled above"),
                MacroIntent::Pause => {
                    if let Some(controller) = self.macro_controller.as_ref() {
                        controller.pause();
                    }
                }
                MacroIntent::Resume => {
                    if let Some(controller) = self.macro_controller.as_ref() {
                        if let Err(error) = controller.resume() {
                            self.macro_state.editor_feedback =
                                Some(format!("Resume failed: {error}"));
                        }
                    }
                }
                MacroIntent::Stop => {
                    if let Some(controller) = self.macro_controller.as_ref() {
                        controller.stop();
                    }
                }
                MacroIntent::SetEnabled { enabled } => {
                    if let (Some(store), Some(saved)) = (
                        self.macro_store.as_ref(),
                        self.macro_state.selected_saved.as_ref(),
                    ) {
                        if let Err(error) = store.set_macro_enabled(&saved.macro_id, enabled) {
                            self.macro_state.editor_feedback =
                                Some(format!("Enablement failed: {error}"));
                        } else {
                            self.refresh_macro_library();
                        }
                    }
                }
                MacroIntent::Delete => {
                    if let (Some(store), Some(saved)) = (
                        self.macro_store.as_ref(),
                        self.macro_state.selected_saved.clone(),
                    ) {
                        match store.delete_macro(&saved.macro_id) {
                            Ok(()) => {
                                self.macro_state.clear_selected_saved();
                                self.selected_saved_revision = None;
                                self.refresh_macro_library();
                            }
                            Err(error) => {
                                self.macro_state.editor_feedback =
                                    Some(format!("Delete failed: {error}"))
                            }
                        }
                    }
                }
                MacroIntent::Rename { name } => {
                    if let (Some(store), Some(saved)) = (
                        self.macro_store.as_ref(),
                        self.macro_state.selected_saved.clone(),
                    ) {
                        match store.rename_macro(&saved.macro_id, &saved.definition_hash, &name) {
                            Ok(saved) => {
                                self.selected_saved_revision = Some(saved.clone());
                                let revision = saved.definition.revision;
                                let macro_id = saved.definition.id.clone();
                                let definition_hash = saved.definition_hash;
                                self.macro_state.load_saved_draft(
                                    saved.definition,
                                    SavedMacroIdentity {
                                        macro_id,
                                        revision,
                                        definition_hash,
                                    },
                                );
                                self.refresh_macro_library();
                            }
                            Err(error) => {
                                self.macro_state.editor_feedback =
                                    Some(format!("Rename failed: {error}"))
                            }
                        }
                    }
                }
                MacroIntent::Duplicate { macro_id, name } => {
                    if let (Some(store), Some(saved)) = (
                        self.macro_store.as_ref(),
                        self.macro_state.selected_saved.as_ref(),
                    ) {
                        match store.duplicate_macro(&saved.macro_id, &macro_id, &name) {
                            Ok(saved) => {
                                self.selected_saved_revision = Some(saved.clone());
                                let revision = saved.definition.revision;
                                self.macro_state.load_saved_draft(
                                    saved.definition,
                                    SavedMacroIdentity {
                                        macro_id,
                                        revision,
                                        definition_hash: saved.definition_hash,
                                    },
                                );
                                self.refresh_macro_library();
                            }
                            Err(error) => {
                                self.macro_state.editor_feedback =
                                    Some(format!("Duplicate failed: {error}"))
                            }
                        }
                    }
                }
                MacroIntent::ShowHistory => {
                    if let Some(store) = self.macro_store.as_ref() {
                        match store.list_run_history() {
                            Ok(history) => {
                                self.macro_state.editor_feedback = Some(format!(
                                    "{} saved run history entr{}.",
                                    history.len(),
                                    if history.len() == 1 { "y" } else { "ies" }
                                ))
                            }
                            Err(error) => {
                                self.macro_state.editor_feedback =
                                    Some(format!("History unavailable: {error}"))
                            }
                        }
                    }
                }
                MacroIntent::DeleteHistory { run_id } => {
                    if let Some(store) = self.macro_store.as_ref() {
                        if let Err(error) = store.delete_run_history(&run_id) {
                            self.macro_state.editor_feedback =
                                Some(format!("History delete failed: {error}"));
                        }
                    }
                }
                MacroIntent::Export { package_root } => {
                    if let (Some(store), Some(saved)) = (
                        self.macro_store.as_ref(),
                        self.macro_state.selected_saved.as_ref(),
                    ) {
                        if let Err(error) = store
                            .export_current_package(&saved.macro_id, &PathBuf::from(package_root))
                        {
                            self.macro_state.editor_feedback =
                                Some(format!("Export failed: {error}"));
                        }
                    }
                }
                MacroIntent::ImportPackage { package_root } => {
                    if let Some(store) = self.macro_store.as_ref() {
                        match store.prepare_package_import(&PathBuf::from(package_root)) {
                            Ok(PreparedPackageImport::Text(prepared)) => {
                                match store.commit_text_package_import(prepared) {
                                    Ok(saved) => {
                                        self.selected_saved_revision = Some(saved.clone());
                                        let revision = saved.definition.revision;
                                        let macro_id = saved.definition.id.clone();
                                        let definition_hash = saved.definition_hash;
                                        self.macro_state.load_saved_draft(
                                            saved.definition,
                                            SavedMacroIdentity {
                                                macro_id,
                                                revision,
                                                definition_hash,
                                            },
                                        );
                                        self.refresh_macro_library();
                                    }
                                    Err(error) => {
                                        self.macro_state.editor_feedback =
                                            Some(format!("Text import failed: {error}"))
                                    }
                                }
                            }
                            Ok(PreparedPackageImport::Image(pending)) => {
                                self.begin_image_package_reverification(pending)
                            }
                            Err(error) => {
                                self.macro_state.editor_feedback =
                                    Some(format!("Import failed: {error}"))
                            }
                        }
                    }
                }
                MacroIntent::ContinueImagePackageReverification => {
                    self.begin_next_image_package_capture()
                }
                MacroIntent::CancelImagePackageReverification => self
                    .discard_image_package_reverification(
                        "Image package import cancelled; no data was imported.",
                    ),
                MacroIntent::CleanupOrphans => {
                    if let Some(store) = self.macro_store.as_ref() {
                        let active_assets = self
                            .macro_controller
                            .as_ref()
                            .and_then(MacroController::active_revision)
                            .map(|saved| {
                                saved
                                    .pinned_assets
                                    .into_iter()
                                    .map(|pinned| pinned.asset)
                                    .collect::<HashSet<_>>()
                            })
                            .unwrap_or_default();
                        match store.cleanup_orphan_assets(&active_assets) {
                            Ok(removed) => {
                                self.macro_state.editor_feedback =
                                    Some(format!("Removed {removed} orphan asset(s)."))
                            }
                            Err(error) => {
                                self.macro_state.editor_feedback =
                                    Some(format!("Asset cleanup failed: {error}"))
                            }
                        }
                    }
                }
            }
        }
    }

    fn begin_macro_authoring(&mut self, request: WizardAuthoringRequest) {
        let Some(wizard) = self.macro_state.wizard.clone() else {
            return;
        };
        let target_binding = (request.kind != WizardAuthoringKind::CaptureTarget)
            .then(|| self.macro_state.target_profile_for_session(request.session))
            .flatten()
            .and_then(|profile| {
                authoring_target_for_session(
                    &self.macro_authoring_targets,
                    request.session,
                    |binding| binding_matches_target_profile(binding, profile),
                )
                .cloned()
            });
        let assets = self
            .macro_store
            .as_ref()
            .map(|store| store.assets().clone());
        let tx = self.tx.clone();
        let repaint = self.egui_ctx.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            let (outcome, captured_target) =
                run_macro_authoring_request(&request, &wizard, target_binding, assets);
            send_ui_event(
                &tx,
                &repaint,
                UiEvent::MacroAuthoringFinished {
                    request,
                    outcome,
                    target_binding: captured_target,
                },
            );
        });
    }

    fn begin_editor_authoring(&mut self, request: EditorAuthoringRequest) {
        let Some(draft) = self.macro_state.draft.clone() else {
            return;
        };
        let target_binding = editor_request_requires_bound_target(&request.kind)
            .then(|| self.macro_state.target_profile_for_session(request.session))
            .flatten()
            .and_then(|profile| {
                authoring_target_for_session(
                    &self.macro_authoring_targets,
                    request.session,
                    |binding| binding_matches_target_profile(binding, profile),
                )
                .cloned()
            });
        let assets = self
            .macro_store
            .as_ref()
            .map(|store| store.assets().clone());
        let tx = self.tx.clone();
        let repaint = self.egui_ctx.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            let (outcome, captured_target) =
                run_editor_authoring_request(&request, &draft, target_binding, assets);
            send_ui_event(
                &tx,
                &repaint,
                UiEvent::EditorAuthoringFinished {
                    request,
                    outcome,
                    target_binding: captured_target,
                },
            );
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
        self.status_message = "Running. Press ESC or Stop to stop.".to_string();
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
                UiEvent::MacroAuthoringFinished {
                    request,
                    outcome,
                    target_binding,
                } => {
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                    let session = request.session;
                    let mut staged_successor = None;
                    let current = self
                        .macro_state
                        .wizard_request_envelope_is_current(&request, request.kind);
                    let outcome = match outcome {
                        NativeWizardAuthoringOutcome::Complete(outcome) if current => outcome,
                        NativeWizardAuthoringOutcome::Complete(_) => {
                            self.macro_state.discard_wizard_result_envelope(&request);
                            self.macro_state.editor_feedback =
                                Some("Discarded stale macro authoring result.".into());
                            continue;
                        }
                        NativeWizardAuthoringOutcome::CapturedTemplate(pending) => {
                            let publish_current =
                                self.macro_state.wizard_request_envelope_is_current(
                                    &request,
                                    WizardAuthoringKind::CaptureTemplate,
                                );
                            match publish_pending_template_if_current(
                                publish_current,
                                self.macro_store.as_ref().map(|store| store.assets()),
                                pending,
                            ) {
                                Ok(Some(published)) => {
                                    if published.staged_successor {
                                        staged_successor = Some(published.asset.clone());
                                    }
                                    WizardAuthoringOutcome::Template {
                                        asset: published.asset,
                                    }
                                }
                                Ok(None) => {
                                    self.macro_state.discard_wizard_result_envelope(&request);
                                    self.macro_state.editor_feedback = Some(
                                        "Discarded stale template capture without publishing it."
                                            .into(),
                                    );
                                    continue;
                                }
                                Err(error) => WizardAuthoringOutcome::Failed(error.to_string()),
                            }
                        }
                    };
                    let result = WizardAuthoringResult {
                        session,
                        id: request.id,
                        fingerprint: request.fingerprint,
                        outcome,
                    };
                    match self.macro_state.apply_wizard_result(result) {
                        Ok(()) => {
                            if request.kind == WizardAuthoringKind::CaptureTarget {
                                if let Some(binding) = target_binding {
                                    self.macro_authoring_targets.insert(session, binding);
                                }
                            }
                        }
                        Err(error) => {
                            if let Some(asset) = staged_successor {
                                if let Some(store) = self.macro_store.as_ref() {
                                    let _ = store.assets().discard_staged_png_revision(&asset);
                                }
                            }
                            self.macro_state.editor_feedback =
                                Some(format!("Discarded macro authoring result: {error:?}"));
                        }
                    }
                }
                UiEvent::EditorAuthoringFinished {
                    request,
                    outcome,
                    target_binding,
                } => {
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                    let expected_kind = request.kind.clone();
                    let mut staged_successor = None;
                    let current = self
                        .macro_state
                        .editor_request_envelope_is_current(&request, &expected_kind);
                    let outcome = match outcome {
                        NativeEditorAuthoringOutcome::Complete(outcome) if current => outcome,
                        NativeEditorAuthoringOutcome::Complete(_) => {
                            self.macro_state.discard_editor_result_envelope(&request);
                            self.macro_state.editor_feedback =
                                Some("Discarded stale editor authoring result.".into());
                            continue;
                        }
                        NativeEditorAuthoringOutcome::CapturedTemplate(pending) => {
                            let publish_current = matches!(
                                request.kind,
                                EditorAuthoringKind::RecaptureTemplate { .. }
                            ) && self
                                .macro_state
                                .editor_request_envelope_is_current(&request, &expected_kind);
                            match publish_pending_template_if_current(
                                publish_current,
                                self.macro_store.as_ref().map(|store| store.assets()),
                                pending,
                            ) {
                                Ok(Some(published)) => {
                                    if published.staged_successor {
                                        staged_successor = Some(published.asset.clone());
                                    }
                                    EditorAuthoringOutcome::Template {
                                        asset: published.asset,
                                    }
                                }
                                Ok(None) => {
                                    self.macro_state.discard_editor_result_envelope(&request);
                                    self.macro_state.editor_feedback = Some(
                                        "Discarded stale template recapture without publishing it."
                                            .into(),
                                    );
                                    continue;
                                }
                                Err(error) => EditorAuthoringOutcome::Failed(error.to_string()),
                            }
                        }
                    };
                    let result = EditorAuthoringResult {
                        session: request.session,
                        id: request.id,
                        fingerprint: request.fingerprint.clone(),
                        outcome,
                    };
                    match self.macro_state.apply_editor_authoring_result(result) {
                        Ok(()) => {
                            install_captured_editor_target(
                                &mut self.macro_authoring_targets,
                                &request,
                                target_binding,
                            );
                        }
                        Err(error) => {
                            if let Some(asset) = staged_successor {
                                if let Some(store) = self.macro_store.as_ref() {
                                    let _ = store.assets().discard_staged_png_revision(&asset);
                                }
                            }
                            self.macro_state.editor_feedback =
                                Some(format!("Discarded editor authoring result: {error:?}"));
                        }
                    }
                }
                UiEvent::ImagePackageReverificationFinished { request, outcome } => {
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                    self.apply_image_package_reverification_result(request, outcome);
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
                self.status_message = "Stopped by ESC or Stop.".to_string();
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
        self.save_ui_state_if_dirty();
        self.sync_macro_runtime();
        if self.page == AppPage::Macro {
            self.refresh_macro_library();
        }

        TopBottomPanel::top("title_bar")
            .exact_height(42.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("BoBo Companion")
                            .strong()
                            .size(ui_theme::text::SECTION_TITLE),
                    );
                    ui.separator();
                    if ui
                        .selectable_label(self.page == AppPage::Enchant, "Enchant")
                        .clicked()
                    {
                        self.page = AppPage::Enchant;
                    }
                    if ui
                        .selectable_label(self.page == AppPage::Macro, "Macro")
                        .clicked()
                    {
                        self.page = AppPage::Macro;
                    }
                    if let Some(warning) = &self.ui_state_warning {
                        ui.label(
                            RichText::new("Preferences warning")
                                .size(ui_theme::text::META)
                                .color(Color32::from_rgb(255, 184, 94)),
                        )
                        .on_hover_text(warning);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .toggle_value(
                                &mut self.ui_state_store.state.always_on_top,
                                "Always on top",
                            )
                            .changed()
                        {
                            let level = if self.ui_state_store.state.always_on_top {
                                egui::WindowLevel::AlwaysOnTop
                            } else {
                                egui::WindowLevel::Normal
                            };
                            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
                            self.ui_state_store.mark_dirty();
                        }
                        if self.page == AppPage::Enchant {
                            status_pill(ui, self.status, self.status.label());
                        } else {
                            let target = self
                                .macro_state
                                .draft
                                .as_ref()
                                .map(|draft| draft.target.title_contains.as_str())
                                .filter(|title| !title.is_empty())
                                .unwrap_or("No target selected");
                            let saved = self
                                .macro_state
                                .selected_saved
                                .as_ref()
                                .map(|saved| format!("Saved r{}", saved.revision))
                                .unwrap_or_else(|| "Draft only".into());
                            ui.label(
                                RichText::new(format!("Target: {target}"))
                                    .size(ui_theme::text::SUPPORTING)
                                    .color(Color32::from_rgb(194, 143, 94)),
                            );
                            ui.label(
                                RichText::new(saved)
                                    .size(ui_theme::text::SUPPORTING)
                                    .color(Color32::from_gray(185)),
                            );
                        }
                    });
                });
            });

        match bottom_surface(self.page) {
            BottomSurface::EnchantActions => {
                TopBottomPanel::bottom("action_bar")
                    .exact_height(112.0)
                    .show(ctx, |ui| {
                        self.bottom_bar(ui);
                    });
            }
            BottomSurface::MacroMonitor => {
                TopBottomPanel::bottom("macro_run_monitor")
                    .exact_height(MacroPage::MONITOR_HEIGHT)
                    .show(ctx, |ui| {
                        MacroPage::show_bottom(ui, &mut self.macro_state);
                    });
            }
        }

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
                        match self.page {
                            AppPage::Enchant => self.content(ui, ctx),
                            AppPage::Macro => {
                                self.macro_state.hydrate_canvas_layout(&self.ui_state_store);
                                MacroPage::show(ui, &mut self.macro_state);
                                self.macro_state
                                    .persist_canvas_layout(&mut self.ui_state_store);
                            }
                        }
                    });
            });
        if self.page == AppPage::Macro {
            self.dispatch_macro_intents();
            let active_sessions = self.macro_state.active_authoring_sessions();
            prune_authoring_targets(&mut self.macro_authoring_targets, &active_sessions);
            if let Some(request) = self.macro_state.take_wizard_request() {
                self.begin_macro_authoring(request);
            }
            if let Some(request) = self.macro_state.take_editor_authoring_request() {
                self.begin_editor_authoring(request);
            }
        }
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
                    ui.label(
                        RichText::new("Occultist Affix Reroll")
                            .size(ui_theme::text::PAGE_TITLE)
                            .strong(),
                    );
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
                    ui.label(
                        RichText::new("Live OCR Result")
                            .strong()
                            .size(ui_theme::text::SECTION_TITLE),
                    );
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
                                    .size(ui_theme::text::BODY)
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
                            .size(ui_theme::text::META)
                            .color(Color32::from_gray(145)),
                    );
                    if result.capture_rect.width > 0 && result.capture_rect.height > 0 {
                        ui.label(
                            RichText::new(format!(
                                "Captured: {}",
                                format_rect(Some(result.capture_rect))
                            ))
                            .size(ui_theme::text::META)
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
                            .size(ui_theme::text::SECTION_TITLE)
                            .strong()
                            .color(Color32::from_gray(220)),
                    );
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.config.targets_text)
                            .desired_width(ui.available_width())
                            .font(egui::TextStyle::Body),
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
                .size(ui_theme::text::SUPPORTING)
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
                                    Button::new(
                                        RichText::new("Start").strong().color(Color32::BLACK),
                                    )
                                    .fill(Color32::from_rgb(246, 111, 25)),
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
                                    Button::new(
                                        RichText::new("Stop").strong().color(Color32::WHITE),
                                    ),
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

fn panel(ui: &mut Ui, title: &str, add_contents: impl FnOnce(&mut Ui)) {
    Frame::none()
        .fill(Color32::from_rgb(17, 20, 23))
        .stroke(Stroke::new(1.0, Color32::from_rgb(39, 45, 52)))
        .rounding(8.0)
        .inner_margin(egui::Margin::same(14.0))
        .show(ui, |ui| {
            ui.set_min_height(204.0);
            ui.label(
                RichText::new(title)
                    .strong()
                    .size(ui_theme::text::SECTION_TITLE),
            );
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
        .size(ui_theme::text::BODY)
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
                    .size(ui_theme::text::META)
                    .color(Color32::from_gray(145)),
            )
            .wrap(false),
        );
        ui.add(
            egui::Label::new(
                RichText::new(value)
                    .size(ui_theme::text::SUPPORTING)
                    .strong()
                    .color(color),
            )
            .wrap(false),
        );
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
                        .size(ui_theme::text::META)
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

fn select_authoring_image_match(
    candidates: Vec<ImageMatchCandidate>,
    rule: &ImageRule,
) -> anyhow::Result<ImageMatchResult> {
    Ok(ImageMatchResult::select(
        cluster_peaks(candidates, ClusterPolicy::default())?,
        rule,
    ))
}

fn match_authoring_image(
    image: &crate::engine::types::ScreenImage,
    capture_rect: Rect,
    template: &image::GrayImage,
    mask: Option<&image::GrayImage>,
    rule: &ImageRule,
) -> anyhow::Result<(crate::engine::macro_engine::RawImageMatch, ImageMatchResult)> {
    let raw = ImageMatcher.match_screen_image_masked(
        image,
        capture_rect,
        template,
        mask,
        &ImageMatchConfig {
            threshold: rule.threshold,
            scales_percent: rule.scales_percent.clone(),
        },
    )?;
    let selected = select_authoring_image_match(raw.candidates.clone(), rule)?;
    Ok((raw, selected))
}

fn require_stable_authoring_client(before: Rect, after: Rect) -> anyhow::Result<()> {
    anyhow::ensure!(
        before == after,
        "target client moved while the authoring capture was in progress; retry the capture"
    );
    Ok(())
}

fn native_wizard_image_rule(wizard: &crate::macro_ui::WizardState) -> anyhow::Result<ImageRule> {
    let mut rule = wizard
        .image_rule_for_authoring()
        .ok_or_else(|| anyhow::anyhow!("wizard image rule is unavailable"))?;
    rule.verification = None;
    Ok(rule)
}

fn binding_matches_target_profile(binding: &CapturedTargetBinding, target: &TargetProfile) -> bool {
    captured_profile_matches_target(binding.profile(), target)
}

fn captured_profile_matches_target(
    captured: &CapturedTargetProfile,
    target: &TargetProfile,
) -> bool {
    captured
        .process_path
        .eq_ignore_ascii_case(&target.process_path)
        && captured
            .window_class
            .eq_ignore_ascii_case(&target.window_class)
        && captured.title.contains(&target.title_contains)
        && captured.client_rect.width == target.captured_client_width
        && captured.client_rect.height == target.captured_client_height
        && captured.dpi == target.captured_dpi
}

fn authoring_target_for_session<'a, T>(
    targets: &'a HashMap<AuthoringSessionId, T>,
    session: AuthoringSessionId,
    accepts: impl FnOnce(&T) -> bool,
) -> Option<&'a T> {
    targets.get(&session).filter(|target| accepts(target))
}

fn editor_request_requires_bound_target(kind: &EditorAuthoringKind) -> bool {
    !matches!(kind, EditorAuthoringKind::CaptureTarget)
}

fn install_captured_editor_target<T>(
    targets: &mut HashMap<AuthoringSessionId, T>,
    request: &EditorAuthoringRequest,
    captured: Option<T>,
) -> bool {
    let Some(captured) = captured.filter(|_| request.kind == EditorAuthoringKind::CaptureTarget)
    else {
        return false;
    };
    targets.insert(request.session, captured);
    true
}

fn prune_authoring_targets<T>(
    targets: &mut HashMap<AuthoringSessionId, T>,
    active_sessions: &[AuthoringSessionId],
) {
    targets.retain(|session, _| active_sessions.contains(session));
}

fn publish_pending_template_if_current(
    current: bool,
    store: Option<&AssetStore>,
    pending: PendingTemplateCapture,
) -> anyhow::Result<Option<PublishedTemplateCapture>> {
    if !current {
        return Ok(None);
    }
    let store = store.ok_or_else(|| anyhow::anyhow!("macro asset store is unavailable"))?;
    let (asset, staged_successor) = match pending.previous {
        Some(previous) => (
            store.replace_staged_png_revision(&previous, &pending.bytes)?,
            true,
        ),
        None => (store.put_png(&pending.bytes)?, false),
    };
    Ok(Some(PublishedTemplateCapture {
        asset,
        staged_successor,
    }))
}

fn run_image_package_reverification_request(
    request: &ImagePackageReverificationRequest,
) -> NativeImagePackageReverificationOutcome {
    let run = || -> anyhow::Result<NativeImagePackageReverificationOutcome> {
        match &request.step {
            ImagePackageReverificationStep::CaptureTarget => {
                let selection = select_screen_rect(40)?;
                Ok(NativeImagePackageReverificationOutcome::CapturedTarget(
                    resolve_target_from_selection(selection)?,
                ))
            }
            ImagePackageReverificationStep::CaptureRegion {
                binding,
                region_id,
                region_revision,
            } => {
                let target = binding.prepare_client_rect()?;
                let response = select_macro_capture(MacroCaptureRequest {
                    id: CaptureRequestId(request.token.request_id),
                    kind: MacroCaptureKind::ImageSearchRegion,
                    target_client: target,
                    min_size: 4,
                })?;
                require_stable_authoring_client(target, binding.validate_client_rect()?)?;
                let MacroCaptureSelection::Region(rect) = response.selection else {
                    anyhow::bail!("image package region capture returned the wrong result type")
                };
                Ok(NativeImagePackageReverificationOutcome::CapturedRegion(
                    RegionDefinition {
                        id: region_id.clone(),
                        revision: *region_revision,
                        rect,
                    },
                ))
            }
            ImagePackageReverificationStep::CaptureTemplate { binding, region } => {
                let target = binding.prepare_client_rect()?;
                let response = select_macro_capture(MacroCaptureRequest {
                    id: CaptureRequestId(request.token.request_id),
                    kind: MacroCaptureKind::TemplateCrop,
                    target_client: target,
                    min_size: 4,
                })?;
                let MacroCaptureSelection::TemplateCrop { screen_rect, .. } = response.selection
                else {
                    anyhow::bail!("image package template capture returned the wrong result type")
                };
                let template = binding.capture_screen_region(target, screen_rect)?;
                let target_region =
                    binding.capture_screen_region(target, target.rect_from_ratio(region.rect))?;
                require_stable_authoring_client(target, binding.validate_client_rect()?)?;
                Ok(NativeImagePackageReverificationOutcome::CapturedTemplate {
                    template_png: encode_local_capture_png(template)?,
                    target_region_png: encode_local_capture_png(target_region)?,
                })
            }
            ImagePackageReverificationStep::CaptureNegative { binding, region } => {
                let target = binding.prepare_client_rect()?;
                let negative =
                    binding.capture_screen_region(target, target.rect_from_ratio(region.rect))?;
                require_stable_authoring_client(target, binding.validate_client_rect()?)?;
                Ok(NativeImagePackageReverificationOutcome::CapturedNegative(
                    encode_local_capture_png(negative)?,
                ))
            }
        }
    };
    match run() {
        Ok(outcome) => outcome,
        Err(error) => NativeImagePackageReverificationOutcome::Failed(error.to_string()),
    }
}

fn encode_local_capture_png(image: crate::engine::types::ScreenImage) -> anyhow::Result<Vec<u8>> {
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(image.rgba).write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    Ok(png)
}

fn fresh_opaque_mask_png(template_png: &[u8]) -> anyhow::Result<Vec<u8>> {
    let template = ImageRuleVerification::decode_template_png(template_png)?;
    let mask = GrayImage::from_pixel(template.width(), template.height(), Luma([255]));
    let mut png = Vec::new();
    DynamicImage::ImageLuma8(mask).write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    Ok(png)
}

fn target_profile_from_binding(binding: &CapturedTargetBinding) -> TargetProfile {
    let profile = binding.profile();
    TargetProfile {
        process_path: profile.process_path.clone(),
        window_class: profile.window_class.clone(),
        title_contains: profile.title.clone(),
        captured_client_width: profile.client_rect.width,
        captured_client_height: profile.client_rect.height,
        captured_dpi: profile.dpi,
    }
}

fn complete_pending_image_rule_reverification(
    store: Option<&MacroStore>,
    session: &PendingImagePackageReverification,
    negative_png: Vec<u8>,
) -> anyhow::Result<LocalImageRuleReverification> {
    let store = store.ok_or_else(|| anyhow::anyhow!("macro store is unavailable"))?;
    let binding = session
        .binding
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("local target capture is missing"))?;
    let rule_id = session
        .pending
        .image_rule_ids()
        .get(session.active_rule_index)
        .ok_or_else(|| anyhow::anyhow!("active image rule is missing"))?;
    let evidence = session
        .evidence
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("local image evidence is missing"))?;
    let template_png = evidence
        .template_png
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("local template evidence is missing"))?;
    let target_region_png = evidence
        .target_region_png
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("local target-region evidence is missing"))?;
    let rule = session
        .pending
        .definition()
        .image_rules
        .iter()
        .find(|rule| rule.id == *rule_id)
        .ok_or_else(|| anyhow::anyhow!("pending image rule is missing"))?;
    let mut negative_pngs = evidence.negative_pngs.clone();
    negative_pngs.push(negative_png);
    let negative_samples = negative_pngs
        .iter()
        .enumerate()
        .map(|(index, png)| {
            LocalNegativeImageSample::from_local_capture(
                format!("local-package/{rule_id}/negative/{index}"),
                png,
            )
        })
        .collect::<Vec<_>>();
    let mask_png = rule
        .transparent_mask
        .as_ref()
        .map(|_| fresh_opaque_mask_png(template_png))
        .transpose()?;
    store.complete_local_image_reverification(
        &session.pending,
        LocalImageRuleVerificationInput::from_local_capture(
            rule_id,
            template_png,
            mask_png.as_deref(),
            target_profile_from_binding(binding),
            evidence.region.clone(),
            target_region_png,
            &negative_samples,
            DEFAULT_MAX_SCORE_CELLS,
        ),
    )
}

fn run_macro_authoring_request(
    request: &WizardAuthoringRequest,
    wizard: &crate::macro_ui::WizardState,
    target_binding: Option<CapturedTargetBinding>,
    assets: Option<AssetStore>,
) -> (NativeWizardAuthoringOutcome, Option<CapturedTargetBinding>) {
    let run =
        || -> anyhow::Result<(NativeWizardAuthoringOutcome, Option<CapturedTargetBinding>)> {
            if request.kind == WizardAuthoringKind::CaptureTarget {
                let selection = select_screen_rect(40)?;
                let binding = resolve_target_from_selection(selection)?;
                let target = binding.profile();
                return Ok((
                    NativeWizardAuthoringOutcome::Complete(
                        WizardAuthoringOutcome::TargetGeometry {
                            process_path: target.process_path.clone(),
                            window_class: target.window_class.clone(),
                            title: target.title.clone(),
                            width: target.client_rect.width,
                            height: target.client_rect.height,
                            dpi: target.dpi,
                        },
                    ),
                    Some(binding),
                ));
            }
            let binding = target_binding.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "capture the concrete target window first for this authoring session"
                )
            })?;
            let target = binding.prepare_client_rect()?;
            if matches!(
                request.kind,
                WizardAuthoringKind::TestDetector | WizardAuthoringKind::CaptureImageNegative
            ) {
                let capture_rect = target.rect_from_ratio(wizard.region);
                let started = Instant::now();
                let image = binding.capture_screen_region(target, capture_rect)?;
                require_stable_authoring_client(target, binding.validate_client_rect()?)?;
                return match &wizard.detector {
                    WizardDetector::Text => {
                        anyhow::ensure!(
                            request.kind == WizardAuthoringKind::TestDetector,
                            "negative samples apply only to image rules"
                        );
                        let rule = wizard
                            .text_rule_for_authoring()
                            .ok_or_else(|| anyhow::anyhow!("wizard OCR rule is unavailable"))?;
                        let tested = authoring_test_text_rule(&image, capture_rect, &rule)?;
                        let evidence = tested
                            .words
                            .iter()
                            .map(|word| word.text.as_str())
                            .collect::<Vec<_>>()
                            .join(" ");
                        Ok((
                            NativeWizardAuthoringOutcome::Complete(
                                WizardAuthoringOutcome::DetectorTest {
                                    passed: tested.text_match.matched,
                                    evidence,
                                    elapsed_ms: started.elapsed().as_millis() as u64,
                                    image_verification: None,
                                },
                            ),
                            None,
                        ))
                    }
                    WizardDetector::Image { template } => {
                        let store = assets
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("macro asset store is unavailable"))?;
                        let template_bytes = store.read(template)?;
                        let template_image =
                            ImageRuleVerification::decode_template_png(&template_bytes)?;
                        let rule = native_wizard_image_rule(wizard)?;
                        anyhow::ensure!(
                            rule.template == *template,
                            "wizard image template changed before the authoring request"
                        );
                        let (result, selected) = match_authoring_image(
                            &image,
                            capture_rect,
                            &template_image,
                            None,
                            &rule,
                        )?;
                        if request.kind == WizardAuthoringKind::CaptureImageNegative {
                            anyhow::ensure!(
                                result.candidates.is_empty(),
                                "current frame still matches the template; show a known-negative frame"
                            );
                            let mut png = Vec::new();
                            DynamicImage::ImageRgba8(image.rgba.clone())
                                .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
                            let sample = NegativeCorpusSample {
                                stable_id: format!("wizard/negative/{}", request.id.0),
                                content_sha256: format!("{:x}", Sha256::digest(&png)),
                                measured_score: result.best.score,
                                evaluation: NegativeSampleEvaluationInputs::for_rule(
                                    &rule,
                                    wizard.target.captured_dpi,
                                    1,
                                    (capture_rect.width, capture_rect.height),
                                ),
                            };
                            return Ok((
                                NativeWizardAuthoringOutcome::Complete(
                                    WizardAuthoringOutcome::ImageNegativeSample(sample),
                                ),
                                None,
                            ));
                        }
                        let verification = if selected.matched
                            && !wizard.image_negative_samples.is_empty()
                        {
                            Some(
                                ImageRuleVerification::verify(ImageRuleVerificationInput {
                                    rule: &rule,
                                    template: &template_image,
                                    mask: None,
                                    captured_dpi: wizard.target.captured_dpi,
                                    current_dpi: wizard.target.captured_dpi,
                                    region_revision: 1,
                                    search_dimensions: (capture_rect.width, capture_rect.height),
                                    negative_samples: &wizard.image_negative_samples,
                                    observed_clusters: &selected.clusters,
                                    maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
                                })?
                                .into_artifact(),
                            )
                        } else {
                            None
                        };
                        Ok((
                            NativeWizardAuthoringOutcome::Complete(
                                WizardAuthoringOutcome::DetectorTest {
                                    passed: selected.matched && verification.is_some(),
                                    evidence: format!(
                                        "best {:.3}; {} candidate(s), {} cluster(s); {}",
                                        result.best.score,
                                        result.candidates.len(),
                                        selected.clusters.len(),
                                        if verification.is_some() {
                                            "local verification passed"
                                        } else if !selected.matched {
                                            "configured image selection policy did not select a match"
                                        } else {
                                            "capture at least one explicit negative sample"
                                        }
                                    ),
                                    elapsed_ms: started.elapsed().as_millis() as u64,
                                    image_verification: verification,
                                },
                            ),
                            None,
                        ))
                    }
                };
            }

            let capture_kind = match request.kind {
                WizardAuthoringKind::CaptureTextRegion => MacroCaptureKind::TextRegion,
                WizardAuthoringKind::CaptureImageRegion => MacroCaptureKind::ImageSearchRegion,
                WizardAuthoringKind::CaptureTemplate => MacroCaptureKind::TemplateCrop,
                WizardAuthoringKind::CaptureClickPoint => MacroCaptureKind::ClickPoint,
                WizardAuthoringKind::CaptureClickRegion => MacroCaptureKind::ClickRegion,
                WizardAuthoringKind::CaptureTarget
                | WizardAuthoringKind::TestDetector
                | WizardAuthoringKind::CaptureImageNegative => unreachable!(),
            };
            let response = select_macro_capture(MacroCaptureRequest {
                id: CaptureRequestId(request.id.0),
                kind: capture_kind,
                target_client: target,
                min_size: if capture_kind == MacroCaptureKind::ClickPoint {
                    1
                } else {
                    4
                },
            })?;
            require_stable_authoring_client(target, binding.validate_client_rect()?)?;
            let outcome = match response.selection {
                MacroCaptureSelection::Region(rect) => {
                    NativeWizardAuthoringOutcome::Complete(WizardAuthoringOutcome::Region(rect))
                }
                MacroCaptureSelection::Point(point) => {
                    NativeWizardAuthoringOutcome::Complete(WizardAuthoringOutcome::Point(point))
                }
                MacroCaptureSelection::TemplateCrop {
                    region: _,
                    screen_rect,
                } => {
                    let captured = binding.capture_screen_region(target, screen_rect)?;
                    require_stable_authoring_client(target, binding.validate_client_rect()?)?;
                    let mut bytes = Vec::new();
                    DynamicImage::ImageRgba8(captured.rgba)
                        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)?;
                    let previous = match &wizard.detector {
                        WizardDetector::Image { template } if template.revision > 0 => {
                            Some(template.clone())
                        }
                        _ => None,
                    };
                    NativeWizardAuthoringOutcome::CapturedTemplate(PendingTemplateCapture {
                        bytes,
                        previous,
                    })
                }
            };
            Ok((outcome, None))
        };

    match run() {
        Ok(result) => result,
        Err(error) if error.to_string().to_ascii_lowercase().contains("cancel") => (
            NativeWizardAuthoringOutcome::Complete(WizardAuthoringOutcome::Cancelled),
            None,
        ),
        Err(error) => (
            NativeWizardAuthoringOutcome::Complete(WizardAuthoringOutcome::Failed(
                error.to_string(),
            )),
            None,
        ),
    }
}

fn run_editor_authoring_request(
    request: &EditorAuthoringRequest,
    draft: &EditorDraft,
    target_binding: Option<CapturedTargetBinding>,
    assets: Option<AssetStore>,
) -> (NativeEditorAuthoringOutcome, Option<CapturedTargetBinding>) {
    if request.kind == EditorAuthoringKind::CaptureTarget {
        let captured =
            || -> anyhow::Result<(NativeEditorAuthoringOutcome, CapturedTargetBinding)> {
                let selection = select_screen_rect(40)?;
                let binding = resolve_target_from_selection(selection)?;
                let target = binding.profile();
                let outcome = NativeEditorAuthoringOutcome::Complete(
                    EditorAuthoringOutcome::TargetGeometry {
                        process_path: target.process_path.clone(),
                        window_class: target.window_class.clone(),
                        title: target.title.clone(),
                        width: target.client_rect.width,
                        height: target.client_rect.height,
                        dpi: target.dpi,
                    },
                );
                Ok((outcome, binding))
            };
        return match captured() {
            Ok((outcome, binding)) => (outcome, Some(binding)),
            Err(error) if error.to_string().to_ascii_lowercase().contains("cancel") => (
                NativeEditorAuthoringOutcome::Complete(EditorAuthoringOutcome::Cancelled),
                None,
            ),
            Err(error) => (
                NativeEditorAuthoringOutcome::Complete(EditorAuthoringOutcome::Failed(
                    error.to_string(),
                )),
                None,
            ),
        };
    }
    (
        run_bound_editor_authoring_request(request, draft, target_binding, assets),
        None,
    )
}

fn run_bound_editor_authoring_request(
    request: &EditorAuthoringRequest,
    draft: &EditorDraft,
    target_binding: Option<CapturedTargetBinding>,
    assets: Option<AssetStore>,
) -> NativeEditorAuthoringOutcome {
    let run = || -> anyhow::Result<NativeEditorAuthoringOutcome> {
        let binding = target_binding.as_ref().ok_or_else(|| {
            anyhow::anyhow!("capture a concrete target for this authoring session first")
        })?;
        let target = binding.prepare_client_rect()?;
        match &request.kind {
            EditorAuthoringKind::CaptureTarget => unreachable!("handled before target lookup"),
            EditorAuthoringKind::RecaptureRegion { region_id } => {
                let kind = if draft
                    .image_rules
                    .iter()
                    .any(|rule| rule.region_id == *region_id)
                {
                    MacroCaptureKind::ImageSearchRegion
                } else {
                    MacroCaptureKind::TextRegion
                };
                let response = select_macro_capture(MacroCaptureRequest {
                    id: CaptureRequestId(request.id.0),
                    kind,
                    target_client: target,
                    min_size: 4,
                })?;
                require_stable_authoring_client(target, binding.validate_client_rect()?)?;
                let MacroCaptureSelection::Region(rect) = response.selection else {
                    anyhow::bail!("region capture returned the wrong result type")
                };
                Ok(NativeEditorAuthoringOutcome::Complete(
                    EditorAuthoringOutcome::Region(rect),
                ))
            }
            EditorAuthoringKind::RecaptureTemplate { rule_id } => {
                let rule = draft
                    .image_rules
                    .iter()
                    .find(|rule| rule.id == *rule_id)
                    .ok_or_else(|| anyhow::anyhow!("image rule is missing"))?;
                let response = select_macro_capture(MacroCaptureRequest {
                    id: CaptureRequestId(request.id.0),
                    kind: MacroCaptureKind::TemplateCrop,
                    target_client: target,
                    min_size: 4,
                })?;
                require_stable_authoring_client(target, binding.validate_client_rect()?)?;
                let MacroCaptureSelection::TemplateCrop { screen_rect, .. } = response.selection
                else {
                    anyhow::bail!("template capture returned the wrong result type")
                };
                let captured = binding.capture_screen_region(target, screen_rect)?;
                require_stable_authoring_client(target, binding.validate_client_rect()?)?;
                let mut bytes = Vec::new();
                DynamicImage::ImageRgba8(captured.rgba)
                    .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)?;
                Ok(NativeEditorAuthoringOutcome::CapturedTemplate(
                    PendingTemplateCapture {
                        bytes,
                        previous: Some(rule.template.clone()),
                    },
                ))
            }
            EditorAuthoringKind::TestOcr { block_id } => {
                let condition = find_editor_condition(&draft.blocks, block_id)
                    .ok_or_else(|| anyhow::anyhow!("selected OCR block no longer exists"))?;
                let crate::engine::macro_engine::Condition::Text { rule_id, .. } = condition else {
                    anyhow::bail!("selected block is not an OCR condition")
                };
                let rule = draft
                    .text_rules
                    .iter()
                    .find(|rule| &rule.id == rule_id)
                    .ok_or_else(|| anyhow::anyhow!("OCR rule is missing"))?;
                let region = draft
                    .regions
                    .iter()
                    .find(|region| region.id == rule.region_id)
                    .ok_or_else(|| anyhow::anyhow!("OCR region is missing"))?;
                let started = Instant::now();
                let capture_rect = target.rect_from_ratio(region.rect);
                let image = binding.capture_screen_region(target, capture_rect)?;
                require_stable_authoring_client(target, binding.validate_client_rect()?)?;
                let tested = authoring_test_text_rule(&image, capture_rect, rule)?;
                let evidence = tested
                    .words
                    .iter()
                    .map(|word| word.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                Ok(NativeEditorAuthoringOutcome::Complete(
                    EditorAuthoringOutcome::DetectorTest {
                        passed: tested.text_match.matched,
                        evidence,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        rule_id: None,
                        image_verification: None,
                    },
                ))
            }
            EditorAuthoringKind::TestImage { block_id }
            | EditorAuthoringKind::CaptureImageNegative { block_id } => {
                let condition = find_editor_condition(&draft.blocks, block_id)
                    .ok_or_else(|| anyhow::anyhow!("selected image block no longer exists"))?;
                let crate::engine::macro_engine::Condition::Image { rule_id, .. } = condition
                else {
                    anyhow::bail!("selected block is not an image condition")
                };
                let rule = draft
                    .image_rules
                    .iter()
                    .find(|rule| &rule.id == rule_id)
                    .ok_or_else(|| anyhow::anyhow!("image rule is missing"))?;
                let region = draft
                    .regions
                    .iter()
                    .find(|region| region.id == rule.region_id)
                    .ok_or_else(|| anyhow::anyhow!("image region is missing"))?;
                let store = assets
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("macro asset store is unavailable"))?;
                let template =
                    ImageRuleVerification::decode_template_png(&store.read(&rule.template)?)?;
                let mask = rule
                    .transparent_mask
                    .as_ref()
                    .map(|asset| {
                        store
                            .read(asset)
                            .and_then(|bytes| ImageRuleVerification::decode_mask_png(&bytes))
                    })
                    .transpose()?;
                let started = Instant::now();
                let capture_rect = target.rect_from_ratio(region.rect);
                let image = binding.capture_screen_region(target, capture_rect)?;
                require_stable_authoring_client(target, binding.validate_client_rect()?)?;
                let (result, selected) =
                    match_authoring_image(&image, capture_rect, &template, mask.as_ref(), rule)?;
                if matches!(
                    &request.kind,
                    EditorAuthoringKind::CaptureImageNegative { .. }
                ) {
                    anyhow::ensure!(
                        result.candidates.is_empty(),
                        "current frame still matches the template; show a known-negative frame"
                    );
                    let mut png = Vec::new();
                    DynamicImage::ImageRgba8(image.rgba.clone())
                        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
                    return Ok(NativeEditorAuthoringOutcome::Complete(
                        EditorAuthoringOutcome::ImageNegativeSample {
                            block_id: block_id.clone(),
                            sample: NegativeCorpusSample {
                                stable_id: format!("editor/{}/negative/{}", block_id, request.id.0),
                                content_sha256: format!("{:x}", Sha256::digest(&png)),
                                measured_score: result.best.score,
                                evaluation: NegativeSampleEvaluationInputs::for_rule(
                                    rule,
                                    draft.target.captured_dpi,
                                    region.revision,
                                    (capture_rect.width, capture_rect.height),
                                ),
                            },
                        },
                    ));
                }
                let verification = if selected.matched && !request.image_negative_samples.is_empty()
                {
                    Some(
                        ImageRuleVerification::verify(ImageRuleVerificationInput {
                            rule,
                            template: &template,
                            mask: mask.as_ref(),
                            captured_dpi: draft.target.captured_dpi,
                            current_dpi: draft.target.captured_dpi,
                            region_revision: region.revision,
                            search_dimensions: (capture_rect.width, capture_rect.height),
                            negative_samples: &request.image_negative_samples,
                            observed_clusters: &selected.clusters,
                            maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
                        })?
                        .into_artifact(),
                    )
                } else {
                    None
                };
                Ok(NativeEditorAuthoringOutcome::Complete(
                    EditorAuthoringOutcome::DetectorTest {
                        passed: selected.matched && verification.is_some(),
                        evidence: format!(
                            "best {:.3}; {} candidate(s), {} cluster(s); {}",
                            result.best.score,
                            result.candidates.len(),
                            selected.clusters.len(),
                            if verification.is_some() {
                                "local verification passed"
                            } else if !selected.matched {
                                "configured image selection policy did not select a match"
                            } else {
                                "capture a negative frame"
                            }
                        ),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        rule_id: Some(rule.id.clone()),
                        image_verification: verification,
                    },
                ))
            }
        }
    };
    match run() {
        Ok(outcome) => outcome,
        Err(error) if error.to_string().to_ascii_lowercase().contains("cancel") => {
            NativeEditorAuthoringOutcome::Complete(EditorAuthoringOutcome::Cancelled)
        }
        Err(error) => NativeEditorAuthoringOutcome::Complete(EditorAuthoringOutcome::Failed(
            error.to_string(),
        )),
    }
}

fn find_editor_condition<'a>(
    blocks: &'a [crate::engine::macro_engine::Block],
    id: &str,
) -> Option<&'a crate::engine::macro_engine::Condition> {
    use crate::engine::macro_engine::{BlockKind, ObserveMode, TimeoutOutcome};
    fn timeout_body(
        condition: &crate::engine::macro_engine::Condition,
    ) -> Option<&[crate::engine::macro_engine::Block]> {
        let mode = match condition {
            crate::engine::macro_engine::Condition::Text { mode, .. }
            | crate::engine::macro_engine::Condition::Image { mode, .. } => mode,
        };
        match mode {
            ObserveMode::WaitForTrue {
                timeout_outcome: TimeoutOutcome::RunBody { body },
                ..
            }
            | ObserveMode::WaitForFalse {
                timeout_outcome: TimeoutOutcome::RunBody { body },
                ..
            } => Some(body),
            _ => None,
        }
    }
    for block in blocks {
        let condition = match &block.kind {
            BlockKind::Observe { condition }
            | BlockKind::If { condition, .. }
            | BlockKind::RepeatUntil { condition, .. } => Some(condition),
            _ => None,
        };
        if block.id == id {
            return condition;
        }
        let mut children: Vec<&[crate::engine::macro_engine::Block]> = Vec::new();
        match &block.kind {
            BlockKind::If {
                then_body,
                else_body,
                ..
            } => {
                children.push(then_body);
                children.push(else_body);
            }
            BlockKind::RepeatN { body, .. }
            | BlockKind::RepeatUntil { body, .. }
            | BlockKind::Continuous { body } => children.push(body),
            BlockKind::WatchGroup { group } => {
                for lane in &group.lanes {
                    children.push(&lane.then_body);
                }
                if let TimeoutOutcome::RunBody { body } = &group.timeout_outcome {
                    children.push(body);
                }
            }
            _ => {}
        }
        if let Some(condition) = condition {
            if let Some(body) = timeout_body(condition) {
                children.push(body);
            }
        }
        for child in children {
            if let Some(found) = find_editor_condition(child, id) {
                return Some(found);
            }
        }
    }
    None
}

fn open_macro_authoring_store() -> Option<Arc<MacroStore>> {
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("BoBo Companion")
        .join("Macro Authoring");
    MacroStore::open(&root).ok().map(Arc::new)
}

fn config_path() -> PathBuf {
    exe_root_dir().join("enchant_config_native.json")
}

fn ui_state_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("BoBo Companion")
        .join("ui-state.json")
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
    let mut changed = migrated_config;
    if config.mouse_movement.is_none() {
        config.mouse_movement = Some(default_mouse_movement_profile());
        changed = true;
    }
    (config, changed)
}

fn save_native_config(path: &PathBuf, config: &NativeConfig) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(config)?)?;
    Ok(())
}

#[cfg(test)]
mod routing_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::engine::macro_engine::{Limit, MatchSelectionPolicy};

    fn authoring_image_rule() -> ImageRule {
        ImageRule {
            id: "image-rule".into(),
            revision: 1,
            region_id: "region".into(),
            template: crate::engine::macro_engine::AssetRef {
                id: "template".into(),
                revision: 1,
                content_hash: "0".repeat(64),
            },
            transparent_mask: None,
            threshold: 0.95,
            scales_percent: vec![100],
            stable_frames: 1,
            maximum_center_drift_px: 5,
            minimum_runner_up_margin: 0.0,
            verification: None,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 100,
            timeout_ms: Limit::Unlimited,
        }
    }

    #[test]
    fn each_page_owns_one_distinct_fixed_bottom_surface() {
        assert_eq!(
            bottom_surface(AppPage::Enchant),
            BottomSurface::EnchantActions
        );
        assert_eq!(bottom_surface(AppPage::Macro), BottomSurface::MacroMonitor);
        assert!(MacroPage::MONITOR_HEIGHT < APP_HEIGHT / 3.0);
    }

    #[derive(Debug, Clone)]
    struct FakeNamedMutexBackend {
        already_exists: bool,
        closes: Arc<AtomicUsize>,
    }

    impl NamedMutexBackend for FakeNamedMutexBackend {
        type Handle = usize;

        fn create(&self, _name: &str) -> anyhow::Result<NamedMutexCreation<Self::Handle>> {
            Ok(NamedMutexCreation {
                handle: 7,
                already_exists: self.already_exists,
            })
        }

        fn close(&self, _handle: Self::Handle) {
            self.closes.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn single_instance_mutex_closes_second_launch_handle_and_holds_primary_until_drop() {
        let primary_closes = Arc::new(AtomicUsize::new(0));
        let primary = SingleInstanceGuard::acquire_with(
            FakeNamedMutexBackend {
                already_exists: false,
                closes: Arc::clone(&primary_closes),
            },
            "test-primary",
        )
        .unwrap()
        .expect("first launch owns the named object");
        assert_eq!(primary_closes.load(Ordering::SeqCst), 0);
        drop(primary);
        assert_eq!(primary_closes.load(Ordering::SeqCst), 1);

        let duplicate_closes = Arc::new(AtomicUsize::new(0));
        assert!(
            SingleInstanceGuard::acquire_with(
                FakeNamedMutexBackend {
                    already_exists: true,
                    closes: Arc::clone(&duplicate_closes),
                },
                "test-duplicate",
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(duplicate_closes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn win32_named_mutex_blocks_duplicate_until_primary_guard_drops() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!(
            "Local\\BoBoCompanion.SingleInstance.Test.{}.{}",
            std::process::id(),
            nonce
        );

        let primary = SingleInstanceGuard::acquire_with(Win32NamedMutexBackend, &name)
            .unwrap()
            .expect("unique mutex name must be available");
        assert!(
            SingleInstanceGuard::acquire_with(Win32NamedMutexBackend, &name)
                .unwrap()
                .is_none()
        );
        drop(primary);
        assert!(
            SingleInstanceGuard::acquire_with(Win32NamedMutexBackend, &name)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn authoring_exactly_one_rejects_multiple_visual_clusters() {
        let candidates = vec![
            ImageMatchCandidate {
                rect: Rect::new(10, 10, 8, 8),
                score: 0.99,
                scale_percent: 100,
            },
            ImageMatchCandidate {
                rect: Rect::new(80, 10, 8, 8),
                score: 0.98,
                scale_percent: 100,
            },
        ];

        let selected = select_authoring_image_match(candidates, &authoring_image_rule()).unwrap();

        assert_eq!(selected.clusters.len(), 2);
        assert!(!selected.matched);
        assert!(selected.selected.is_none());
    }

    #[test]
    fn authoring_capture_bracket_rejects_a_window_move() {
        let before = Rect::new(10, 20, 800, 600);
        require_stable_authoring_client(before, before).unwrap();
        let error =
            require_stable_authoring_client(before, Rect::new(11, 20, 800, 600)).unwrap_err();
        assert!(error.to_string().contains("moved while"));
    }

    #[test]
    fn stale_template_events_publish_neither_initial_identity_nor_successor() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let assets = store.assets();

        assert!(
            publish_pending_template_if_current(
                false,
                Some(assets),
                PendingTemplateCapture {
                    bytes: b"initial".to_vec(),
                    previous: None,
                },
            )
            .unwrap()
            .is_none()
        );
        let initial = assets.put_png(b"initial").unwrap();
        assert!(initial.id.ends_with("-1"));

        assert!(
            publish_pending_template_if_current(
                false,
                Some(assets),
                PendingTemplateCapture {
                    bytes: b"stale successor".to_vec(),
                    previous: Some(initial.clone()),
                },
            )
            .unwrap()
            .is_none()
        );
        let successor = assets
            .put_next_png_revision(&initial, b"accepted successor")
            .unwrap();
        assert_eq!(successor.revision, 2);
    }

    #[test]
    fn accepted_successor_capture_is_staged_until_state_or_store_accepts_it() {
        let temp = tempfile::tempdir().unwrap();
        let store = MacroStore::open(temp.path()).unwrap();
        let initial = store.assets().put_png(b"initial").unwrap();

        let published = publish_pending_template_if_current(
            true,
            Some(store.assets()),
            PendingTemplateCapture {
                bytes: b"candidate successor".to_vec(),
                previous: Some(initial.clone()),
            },
        )
        .unwrap()
        .unwrap();

        assert!(published.staged_successor);
        assert_eq!(published.asset.id, initial.id);
        assert_eq!(published.asset.revision, 2);
        let replacement = publish_pending_template_if_current(
            true,
            Some(store.assets()),
            PendingTemplateCapture {
                bytes: b"replacement after undo".to_vec(),
                previous: Some(initial.clone()),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(replacement.asset.id, initial.id);
        assert_eq!(replacement.asset.revision, 2);
        assert_ne!(replacement.asset.content_hash, published.asset.content_hash);
        assert!(store.assets().read(&published.asset).is_err());
        store
            .assets()
            .discard_staged_png_revision(&replacement.asset)
            .unwrap();
        let retry = store
            .assets()
            .stage_next_png_revision(&initial, b"retry")
            .unwrap();
        assert_eq!(retry.revision, 2);
    }

    #[test]
    fn durable_binding_profile_rejects_wrong_window_size_dpi_or_identity() {
        let captured = CapturedTargetProfile {
            process_path: r#"C:\games\Diablo IV.exe"#.into(),
            window_class: "Diablo IV Main Window".into(),
            title: "Diablo IV".into(),
            client_rect: Rect::new(10, 20, 800, 600),
            dpi: 144,
        };
        let target = TargetProfile {
            process_path: r#"C:\games\Diablo IV.exe"#.into(),
            window_class: "Diablo IV Main Window".into(),
            title_contains: "Diablo IV".into(),
            captured_client_width: 800,
            captured_client_height: 600,
            captured_dpi: 144,
        };
        assert!(captured_profile_matches_target(&captured, &target));

        let mut wrong = target.clone();
        wrong.captured_dpi = 96;
        assert!(!captured_profile_matches_target(&captured, &wrong));
        wrong = target.clone();
        wrong.captured_client_width += 1;
        assert!(!captured_profile_matches_target(&captured, &wrong));
        wrong = target;
        wrong.process_path = r#"C:\games\Other.exe"#.into();
        assert!(!captured_profile_matches_target(&captured, &wrong));
    }

    #[test]
    fn macro_run_intents_map_to_the_controller_modes_without_live_input() {
        assert_eq!(
            macro_run_request(&MacroIntent::DryRun),
            Some(ControllerRunRequest::once(RunMode::DryRun))
        );
        assert_eq!(
            macro_run_request(&MacroIntent::RunOnce),
            Some(ControllerRunRequest::once(RunMode::ObservationOnly))
        );
        assert_eq!(
            macro_run_request(&MacroIntent::Run),
            Some(ControllerRunRequest::continuous(RunMode::ObservationOnly))
        );
        assert_eq!(
            macro_run_request(&MacroIntent::RunLive),
            Some(ControllerRunRequest::continuous(RunMode::Live))
        );
        assert_eq!(macro_run_request(&MacroIntent::Stop), None);
    }

    #[test]
    fn authoring_target_routing_never_falls_back_to_another_session() {
        let draft_a = AuthoringSessionId(10);
        let wizard_b = AuthoringSessionId(20);
        let mut targets = HashMap::from([(draft_a, "draft-a"), (wizard_b, "wizard-b")]);

        assert_eq!(
            authoring_target_for_session(&targets, draft_a, |_| true),
            Some(&"draft-a")
        );
        assert_eq!(
            authoring_target_for_session(&targets, wizard_b, |_| true),
            Some(&"wizard-b")
        );
        assert!(authoring_target_for_session(&targets, AuthoringSessionId(30), |_| true).is_none());
        assert!(authoring_target_for_session(&targets, draft_a, |_| false).is_none());

        prune_authoring_targets(&mut targets, &[draft_a]);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets.get(&draft_a), Some(&"draft-a"));
        assert!(!targets.contains_key(&wizard_b));
    }

    #[test]
    fn editor_target_capture_bootstraps_without_borrowing_another_session_binding() {
        assert!(!editor_request_requires_bound_target(
            &EditorAuthoringKind::CaptureTarget
        ));
        assert!(editor_request_requires_bound_target(
            &EditorAuthoringKind::TestOcr {
                block_id: "observe-1".into(),
            }
        ));

        let draft = AuthoringSessionId(10);
        let wizard = AuthoringSessionId(20);
        let mut targets = HashMap::from([(draft, "draft-old"), (wizard, "wizard")]);
        let request = EditorAuthoringRequest {
            session: draft,
            id: crate::macro_ui::EditorAuthoringRequestId(1),
            fingerprint: "starter".into(),
            kind: EditorAuthoringKind::CaptureTarget,
            image_negative_samples: vec![],
        };

        assert!(install_captured_editor_target(
            &mut targets,
            &request,
            Some("draft-retargeted")
        ));

        assert_eq!(targets.get(&draft), Some(&"draft-retargeted"));
        assert_eq!(targets.get(&wizard), Some(&"wizard"));
    }

    #[test]
    fn native_wizard_matching_uses_the_canonical_emitted_image_rule() {
        let mut wizard = crate::macro_ui::WizardState::default();
        wizard.detector = WizardDetector::Image {
            template: authoring_image_rule().template,
        };
        let mut canonical = wizard.image_rule_for_authoring().unwrap();
        canonical.verification = None;

        assert_eq!(native_wizard_image_rule(&wizard).unwrap(), canonical);
        assert_eq!(canonical.maximum_center_drift_px, 3);
    }

    #[test]
    fn editor_authoring_match_uses_the_pinned_transparent_mask() {
        use image::{GrayImage, Luma, Rgba, RgbaImage};

        let template = GrayImage::from_fn(4, 4, |x, y| Luma([(x * 40 + y * 11) as u8]));
        let mask = GrayImage::from_fn(4, 4, |x, _| Luma([if x == 0 { 0 } else { 255 }]));
        let rgba = RgbaImage::from_fn(4, 4, |x, y| {
            let value = if x == 0 {
                255_u8.saturating_sub(template.get_pixel(x, y)[0])
            } else {
                template.get_pixel(x, y)[0]
            };
            Rgba([value, value, value, 255])
        });
        let image = crate::engine::types::ScreenImage::new(rgba);

        let (_, selected) = match_authoring_image(
            &image,
            Rect::new(0, 0, 4, 4),
            &template,
            Some(&mask),
            &authoring_image_rule(),
        )
        .unwrap();

        assert!(selected.matched);
        assert_eq!(selected.clusters.len(), 1);
    }

    #[test]
    fn stale_image_package_result_from_cancelled_session_cannot_match_new_session() {
        let stale = ImagePackageReverificationToken {
            generation: 7,
            request_id: 1,
            expected_stage: ImagePackageReverificationStage::CaptureTarget,
        };
        let replacement = ImagePackageReverificationSessionGuard {
            generation: 8,
            stage: ImagePackageReverificationStage::CaptureTarget,
            in_flight_request_id: Some(1),
        };

        assert!(!replacement.accepts(&stale));
        assert!(
            !ImagePackageReverificationSessionGuard {
                generation: 8,
                stage: ImagePackageReverificationStage::CaptureRegion,
                in_flight_request_id: Some(1),
            }
            .accepts(&ImagePackageReverificationToken {
                generation: 8,
                request_id: 1,
                expected_stage: ImagePackageReverificationStage::CaptureTarget,
            })
        );
        assert!(
            !ImagePackageReverificationSessionGuard {
                generation: 8,
                stage: ImagePackageReverificationStage::CaptureTarget,
                in_flight_request_id: Some(2),
            }
            .accepts(&ImagePackageReverificationToken {
                generation: 8,
                request_id: 1,
                expected_stage: ImagePackageReverificationStage::CaptureTarget,
            })
        );
    }
}
