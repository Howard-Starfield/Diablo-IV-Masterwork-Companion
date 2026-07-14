mod canvas;
mod canvas_layout;
mod canvas_model;
mod editor;
mod history;
mod inspector;
mod library;
mod monitor;
#[cfg(test)]
mod test_support;
mod wizard;

pub use editor::*;
pub use library::{MacroLibraryRow, project_definition};
pub use monitor::RunDefinitionSnapshot;
pub use wizard::*;

use std::collections::VecDeque;

use eframe::egui::{self, Button, Color32, Frame, RichText, Stroke, Ui};

use crate::engine::macro_engine::{
    ControllerLifecycleProjection, ControllerSemanticProjection, RunEvent, RunMode, RunStatus,
    ValidationProblem,
};
use crate::ui_theme::text;

use crate::ui_state::{MacroCanvasLayout, UiStateStore};
use canvas_model::{CanvasProjection, CanvasSelection, insertion_target_for_port, project_canvas};
use history::HistoryError;

pub use canvas_layout::{
    CanvasLayoutEngine, CanvasLayoutError, CanvasViewport, LayoutEdit, auto_arrange, fit_view,
    graph_bounds, node_rect, reconcile_layout, visible_nodes,
};
pub use history::{EditDomain, LayoutHistory, UiEditHistory};
use monitor::{
    MonitorProjection, project_last_completion,
    project_last_completion_with_controller_projections, project_monitor,
    project_monitor_with_controller_projections,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EditorAuthoringRequestId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorAuthoringKind {
    CaptureTarget,
    TestOcr { block_id: String },
    TestImage { block_id: String },
    RecaptureRegion { region_id: String },
    RecaptureTemplate { rule_id: String },
    CaptureImageNegative { block_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditorAuthoringRequest {
    pub session: wizard::AuthoringSessionId,
    pub id: EditorAuthoringRequestId,
    pub fingerprint: String,
    pub kind: EditorAuthoringKind,
    pub image_negative_samples: Vec<crate::engine::macro_engine::NegativeCorpusSample>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditorAuthoringOutcome {
    TargetGeometry {
        process_path: String,
        window_class: String,
        title: String,
        width: u32,
        height: u32,
        dpi: u32,
    },
    Region(crate::engine::types::RectRatio),
    Template {
        asset: crate::engine::macro_engine::AssetRef,
    },
    DetectorTest {
        passed: bool,
        evidence: String,
        elapsed_ms: u64,
        rule_id: Option<String>,
        image_verification: Option<crate::engine::macro_engine::ImageRuleVerificationArtifact>,
    },
    ImageNegativeSample {
        block_id: String,
        sample: crate::engine::macro_engine::NegativeCorpusSample,
    },
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditorAuthoringResult {
    pub session: wizard::AuthoringSessionId,
    pub id: EditorAuthoringRequestId,
    pub fingerprint: String,
    pub outcome: EditorAuthoringOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorAuthoringError {
    UnexpectedResult,
    StaleDraft,
    OutcomeMismatch,
    EditRejected,
}

/// Immutable identity of the saved definition that the UI has selected.  This is deliberately
/// separate from the editable draft: controls may only ask the composition root to run this
/// identity, never an in-memory editor value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedMacroIdentity {
    pub macro_id: String,
    pub revision: u64,
    pub definition_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacroIntent {
    Select {
        macro_id: String,
    },
    Validate,
    Save,
    DryRun {
        saved: SavedMacroIdentity,
    },
    RunOnce {
        saved: SavedMacroIdentity,
    },
    Run {
        saved: SavedMacroIdentity,
    },
    RunLive {
        saved: SavedMacroIdentity,
    },
    Pause,
    Resume,
    Stop,
    Rename {
        saved: SavedMacroIdentity,
        name: String,
    },
    Duplicate {
        source: SavedMacroIdentity,
        macro_id: String,
        name: String,
    },
    SetEnabled {
        saved: SavedMacroIdentity,
        enabled: bool,
    },
    Delete {
        saved: SavedMacroIdentity,
    },
    ShowHistory,
    DeleteHistory {
        run_id: String,
    },
    Export {
        package_root: String,
    },
    ImportPackage {
        package_root: String,
    },
    ContinueImagePackageReverification,
    CancelImagePackageReverification,
    CleanupOrphans,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroRunIntent {
    DryRun,
    RunOnce,
    ContinuousObservation,
    ContinuousLive,
}

/// Public composition vocabulary. The native shell translates these bounded UI-only values into
/// the accepted controller and store calls; widgets never receive those owners directly.
pub type MacroUiIntent = MacroIntent;

impl MacroIntent {
    fn is_stop(&self) -> bool {
        matches!(self, Self::Stop)
    }
}

/// UI-only view of an image-package import. It deliberately carries no portable
/// target, region, asset, or verification data: the composition root owns the
/// pending package transaction and may advance it only with local capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePackageReverificationStage {
    CaptureTarget,
    CaptureRegion,
    CaptureTemplate,
    CaptureNegative,
}

impl ImagePackageReverificationStage {
    pub fn instruction(self) -> &'static str {
        match self {
            Self::CaptureTarget => "Capture the local target window.",
            Self::CaptureRegion => "Recapture this rule's local image search region.",
            Self::CaptureTemplate => "Crop a fresh local template for this rule.",
            Self::CaptureNegative => {
                "Show a known-negative local frame, then capture its image evidence."
            }
        }
    }

    pub fn action_label(self) -> &'static str {
        match self {
            Self::CaptureTarget => "Capture local target",
            Self::CaptureRegion => "Recapture local region",
            Self::CaptureTemplate => "Capture local template",
            Self::CaptureNegative => "Capture negative evidence",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePackageReverificationProgress {
    pub rule_ids: Vec<String>,
    pub active_rule_index: usize,
    pub stage: ImagePackageReverificationStage,
}

impl ImagePackageReverificationProgress {
    pub fn new(rule_ids: Vec<String>) -> Self {
        assert!(
            !rule_ids.is_empty(),
            "image package re-verification requires at least one rule"
        );
        Self {
            rule_ids,
            active_rule_index: 0,
            stage: ImagePackageReverificationStage::CaptureTarget,
        }
    }

    pub fn active_rule_id(&self) -> Option<&str> {
        self.rule_ids
            .get(self.active_rule_index)
            .map(String::as_str)
    }
}

/// Small UI-only queue.  Stop is retained under pressure so a busy UI can never make the normal
/// controller stop path unreachable; native ESC is intentionally not represented here.
#[derive(Debug)]
pub struct MacroIntentQueue {
    capacity: usize,
    values: VecDeque<MacroIntent>,
}

impl MacroIntentQueue {
    pub const DEFAULT_CAPACITY: usize = 64;

    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "macro intent capacity must be positive");
        Self {
            capacity,
            values: VecDeque::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, intent: MacroIntent) {
        if self.values.len() < self.capacity {
            self.values.push_back(intent);
            return;
        }
        if intent.is_stop() {
            if let Some(index) = self.values.iter().position(|queued| !queued.is_stop()) {
                self.values.remove(index);
                self.values.push_back(intent);
            }
        }
    }

    pub fn pop(&mut self) -> Option<MacroIntent> {
        self.values.pop_front()
    }

    pub fn drain(&mut self) -> std::collections::vec_deque::Drain<'_, MacroIntent> {
        self.values.drain(..)
    }

    pub fn pending(&self) -> usize {
        self.values.len()
    }

    pub fn len(&self) -> usize {
        self.pending()
    }
}

/// Canonical editor state. It intentionally owns no runtime command sender, capture service,
/// mouse controller, or platform input handle.
#[derive(Debug)]
pub struct MacroPageState {
    pub draft: Option<EditorDraft>,
    pub selected_saved: Option<SavedMacroIdentity>,
    pub saved_revision: Option<u64>,
    pub enabled: bool,
    pub running_snapshot: Option<RunDefinitionSnapshot>,
    pub runtime_events: Vec<RunEvent>,
    pub controller_lifecycle: Option<ControllerLifecycleProjection>,
    pub controller_semantic: Option<ControllerSemanticProjection>,
    pub library_rows: Vec<MacroLibraryRow>,
    pub library_search: String,
    pub library_rename: String,
    pub library_package_path: String,
    pub confirm_library_delete: bool,
    pub image_package_reverification: Option<ImagePackageReverificationProgress>,
    /// Rebuildable presentation state only; it is never part of the executable draft.
    pub canvas_layout: MacroCanvasLayout,
    pub ui_edit_history: UiEditHistory,
    canvas_layout_dirty: bool,
    canvas_layout_hydrated: bool,
    intents: MacroIntentQueue,
    pub selected_block_id: Option<String>,
    selected_canvas: Option<CanvasSelection>,
    pending_canvas_port: Option<canvas_model::OutputPort>,
    pub pending_inspector_intent: Option<inspector::InspectorIntent>,
    pub editor_feedback: Option<String>,
    pending_conversion: Option<PendingConversion>,
    pub wizard: Option<wizard::WizardState>,
    pub pending_wizard_action: Option<wizard::WizardUiAction>,
    pub active_wizard_request: Option<wizard::WizardAuthoringRequest>,
    wizard_request_dispatched: bool,
    next_wizard_request_id: u64,
    wizard_session: Option<wizard::AuthoringSessionId>,
    draft_session: Option<wizard::AuthoringSessionId>,
    next_authoring_session_id: u64,
    pub active_editor_authoring_request: Option<EditorAuthoringRequest>,
    editor_authoring_dispatched: bool,
    next_editor_authoring_request_id: u64,
    editor_image_negative_samples:
        std::collections::HashMap<String, Vec<crate::engine::macro_engine::NegativeCorpusSample>>,
}

impl Default for MacroPageState {
    fn default() -> Self {
        Self {
            draft: None,
            selected_saved: None,
            saved_revision: None,
            enabled: true,
            running_snapshot: None,
            runtime_events: Vec::new(),
            controller_lifecycle: None,
            controller_semantic: None,
            library_rows: Vec::new(),
            library_search: String::new(),
            library_rename: String::new(),
            library_package_path: String::new(),
            confirm_library_delete: false,
            image_package_reverification: None,
            canvas_layout: MacroCanvasLayout::default(),
            ui_edit_history: UiEditHistory::default(),
            canvas_layout_dirty: false,
            canvas_layout_hydrated: false,
            intents: MacroIntentQueue::with_capacity(MacroIntentQueue::DEFAULT_CAPACITY),
            selected_block_id: None,
            selected_canvas: None,
            pending_canvas_port: None,
            pending_inspector_intent: None,
            editor_feedback: None,
            pending_conversion: None,
            wizard: None,
            pending_wizard_action: None,
            active_wizard_request: None,
            wizard_request_dispatched: false,
            next_wizard_request_id: 1,
            wizard_session: None,
            draft_session: None,
            next_authoring_session_id: 1,
            active_editor_authoring_request: None,
            editor_authoring_dispatched: false,
            next_editor_authoring_request_id: 1,
            editor_image_negative_samples: std::collections::HashMap::new(),
        }
    }
}

impl MacroPageState {
    pub fn set_selected_saved(&mut self, saved: SavedMacroIdentity) {
        self.saved_revision = Some(saved.revision);
        self.selected_saved = Some(saved);
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Applies a persisted enablement result only while the same saved macro remains selected.
    pub fn apply_enabled_if_selected(&mut self, saved: &SavedMacroIdentity, enabled: bool) {
        if self.selected_saved.as_ref() == Some(saved) {
            self.set_enabled(enabled);
        }
    }

    /// Request durable lifecycle state through NativeApp. The displayed value changes only when
    /// the store accepts the intent, so a persistence failure naturally leaves it unchanged.
    pub fn request_enabled(&mut self, enabled: bool) {
        if let Some(saved) = self.selected_saved.clone() {
            self.enqueue_intent(MacroIntent::SetEnabled { saved, enabled });
        }
    }

    pub fn request_run(&mut self, run: MacroRunIntent) {
        let Some(saved) = self.selected_saved.clone() else {
            return;
        };
        let intent = match run {
            MacroRunIntent::DryRun => MacroIntent::DryRun { saved },
            MacroRunIntent::RunOnce => MacroIntent::RunOnce { saved },
            MacroRunIntent::ContinuousObservation => MacroIntent::Run { saved },
            MacroRunIntent::ContinuousLive => MacroIntent::RunLive { saved },
        };
        self.enqueue_intent(intent);
    }

    pub fn clear_selected_saved(&mut self) {
        self.selected_saved = None;
        self.saved_revision = None;
    }

    pub fn selected_saved_macro_id(&self) -> Option<&str> {
        self.selected_saved
            .as_ref()
            .map(|saved| saved.macro_id.as_str())
    }

    pub fn enqueue_intent(&mut self, intent: MacroIntent) {
        self.intents.push(intent);
    }

    pub fn push_intent(&mut self, intent: MacroUiIntent) {
        self.enqueue_intent(intent);
    }

    pub fn pending_intent_count(&self) -> usize {
        self.intents.pending()
    }

    pub fn drain_intents(&mut self) -> std::collections::vec_deque::Drain<'_, MacroUiIntent> {
        self.intents.drain()
    }

    pub fn begin_image_package_reverification(&mut self, rule_ids: Vec<String>) {
        self.image_package_reverification = Some(ImagePackageReverificationProgress::new(rule_ids));
    }

    pub fn set_image_package_reverification_stage(
        &mut self,
        active_rule_index: usize,
        stage: ImagePackageReverificationStage,
    ) {
        if let Some(progress) = self.image_package_reverification.as_mut() {
            progress.active_rule_index = active_rule_index;
            progress.stage = stage;
        }
    }

    pub fn clear_image_package_reverification(&mut self) {
        self.image_package_reverification = None;
    }

    pub fn take_intent(&mut self) -> Option<MacroIntent> {
        self.intents.pop()
    }

    pub fn load_saved_draft(
        &mut self,
        definition: crate::engine::macro_engine::MacroDefinition,
        saved: SavedMacroIdentity,
    ) {
        self.begin_draft_session();
        self.canvas_layout =
            reconcile_layout(&project_canvas(&definition), MacroCanvasLayout::default());
        self.ui_edit_history = UiEditHistory::default();
        self.canvas_layout_dirty = false;
        self.canvas_layout_hydrated = false;
        self.draft = Some(EditorDraft::new(definition));
        self.set_selected_saved(saved);
        self.selected_block_id = None;
        self.selected_canvas = None;
        self.editor_feedback = None;
    }

    /// Reconcile separately loaded presentation state with the currently selected canonical tree.
    pub fn load_canvas_layout(&mut self, saved: MacroCanvasLayout) {
        if let Some(draft) = &self.draft {
            self.canvas_layout = reconcile_layout(&project_canvas(draft), saved);
            self.ui_edit_history = UiEditHistory::default();
            self.canvas_layout_dirty = false;
            self.canvas_layout_hydrated = true;
        }
    }

    pub fn hydrate_canvas_layout(&mut self, store: &UiStateStore) {
        if self.canvas_layout_hydrated {
            return;
        }
        let saved = self
            .selected_saved_macro_id()
            .and_then(|id| store.state.macro_layouts.get(id))
            .cloned()
            .unwrap_or_default();
        self.load_canvas_layout(saved);
    }

    pub fn persist_canvas_layout(&mut self, store: &mut UiStateStore) {
        let Some(macro_id) = self.selected_saved_macro_id().map(str::to_owned) else {
            return;
        };
        if !self.canvas_layout_dirty {
            return;
        }
        *store.macro_layout_mut(&macro_id) = self.canvas_layout.clone();
        store.mark_dirty();
        self.canvas_layout_dirty = false;
    }

    pub fn move_canvas_node(
        &mut self,
        block_id: &str,
        position: [f32; 2],
    ) -> Result<(), CanvasLayoutError> {
        if !self
            .draft
            .as_ref()
            .is_some_and(|draft| project_canvas(draft).node(block_id).is_some())
        {
            return Err(CanvasLayoutError::MissingNode(block_id.into()));
        }
        let edit = CanvasLayoutEngine::move_node(&mut self.canvas_layout, block_id, position)?;
        if let LayoutEdit::NodePosition { before, after, .. } = &edit {
            if before == after {
                return Ok(());
            }
        }
        self.ui_edit_history.record_layout(edit);
        self.canvas_layout_dirty = true;
        Ok(())
    }

    pub fn auto_arrange_canvas(&mut self) {
        let Some(draft) = &self.draft else { return };
        let mut next = auto_arrange(&project_canvas(draft));
        let before = self.canvas_layout.clone();
        next.pan = before.pan;
        next.zoom = before.zoom;
        next.library_width = before.library_width;
        next.inspector_width = before.inspector_width;
        if before == next {
            return;
        }
        self.canvas_layout = next.clone();
        self.ui_edit_history.record_layout(LayoutEdit::Layout {
            before,
            after: next,
        });
        self.canvas_layout_dirty = true;
    }

    pub fn reset_canvas_zoom(&mut self) {
        self.canvas_layout.zoom = 1.0;
        self.canvas_layout_dirty = true;
    }

    fn record_canvas_layout_edit(&mut self, edit: LayoutEdit) {
        self.ui_edit_history.record_layout(edit);
        self.canvas_layout_dirty = true;
    }

    fn mark_canvas_layout_changed(&mut self) {
        self.canvas_layout_dirty = true;
    }

    pub fn fit_canvas_view(&mut self, canvas_size: [f32; 2]) {
        let Some(draft) = &self.draft else { return };
        let viewport = fit_view(
            canvas_size,
            graph_bounds(&project_canvas(draft), &self.canvas_layout),
        );
        viewport.write_to_layout(&mut self.canvas_layout);
        self.canvas_layout_dirty = true;
    }

    fn undo_ui_edit(&mut self) -> Result<EditDomain, HistoryError> {
        let result = {
            let draft = self.draft.as_mut().ok_or(HistoryError::NothingToUndo)?;
            self.ui_edit_history.undo(draft, &mut self.canvas_layout)?
        };
        if result == EditDomain::Definition {
            self.reconcile_canvas_layout();
        }
        self.canvas_layout_dirty |= result == EditDomain::Layout;
        Ok(result)
    }

    fn redo_ui_edit(&mut self) -> Result<EditDomain, HistoryError> {
        let result = {
            let draft = self.draft.as_mut().ok_or(HistoryError::NothingToRedo)?;
            self.ui_edit_history.redo(draft, &mut self.canvas_layout)?
        };
        if result == EditDomain::Definition {
            self.reconcile_canvas_layout();
        }
        self.canvas_layout_dirty |= result == EditDomain::Layout;
        Ok(result)
    }

    fn reconcile_canvas_layout(&mut self) {
        if let Some(draft) = &self.draft {
            self.canvas_layout =
                reconcile_layout(&project_canvas(draft), self.canvas_layout.clone());
            self.canvas_layout_dirty = true;
        }
    }

    pub fn validate_draft(&mut self) {
        let _ = dispatch_editor_command(self, EditorCommand::MarkValidated);
    }
    fn allocate_authoring_session(&mut self) -> wizard::AuthoringSessionId {
        let session = wizard::AuthoringSessionId(self.next_authoring_session_id);
        self.next_authoring_session_id = self.next_authoring_session_id.saturating_add(1);
        session
    }

    fn begin_wizard_session(&mut self) {
        self.wizard_session = Some(self.allocate_authoring_session());
        self.cancel_wizard_authoring();
        self.active_editor_authoring_request = None;
        self.editor_authoring_dispatched = false;
        self.pending_inspector_intent = None;
    }

    fn begin_draft_session(&mut self) {
        self.draft_session = Some(self.allocate_authoring_session());
        self.active_editor_authoring_request = None;
        self.editor_authoring_dispatched = false;
    }

    fn cancel_wizard_authoring(&mut self) {
        self.active_wizard_request = None;
        self.wizard_request_dispatched = false;
        self.pending_wizard_action = None;
    }

    pub fn active_wizard_session(&self) -> Option<wizard::AuthoringSessionId> {
        self.wizard.as_ref().and(self.wizard_session)
    }

    pub fn active_draft_session(&self) -> Option<wizard::AuthoringSessionId> {
        self.draft.as_ref().and(self.draft_session)
    }

    pub fn active_authoring_sessions(&self) -> Vec<wizard::AuthoringSessionId> {
        [self.active_wizard_session(), self.active_draft_session()]
            .into_iter()
            .flatten()
            .collect()
    }

    pub fn target_profile_for_session(
        &self,
        session: wizard::AuthoringSessionId,
    ) -> Option<&crate::engine::macro_engine::TargetProfile> {
        if self.active_wizard_session() == Some(session) {
            return self.wizard.as_ref().map(|wizard| &wizard.target);
        }
        (self.active_draft_session() == Some(session))
            .then(|| self.draft.as_ref().map(|draft| &draft.definition.target))
            .flatten()
    }

    pub fn wizard_request_envelope_is_current(
        &self,
        request: &wizard::WizardAuthoringRequest,
        expected_kind: wizard::WizardAuthoringKind,
    ) -> bool {
        self.active_wizard_session() == Some(request.session)
            && request.kind == expected_kind
            && self.active_wizard_request.as_ref() == Some(request)
            && self
                .wizard
                .as_ref()
                .is_some_and(|wizard| wizard.review_fingerprint() == request.fingerprint)
    }

    pub fn discard_wizard_result_envelope(
        &mut self,
        request: &wizard::WizardAuthoringRequest,
    ) -> bool {
        if self.active_wizard_request.as_ref() != Some(request) {
            return false;
        }
        self.cancel_wizard_authoring();
        true
    }

    pub fn editor_request_envelope_is_current(
        &self,
        request: &EditorAuthoringRequest,
        expected_kind: &EditorAuthoringKind,
    ) -> bool {
        self.active_draft_session() == Some(request.session)
            && &request.kind == expected_kind
            && self.active_editor_authoring_request.as_ref() == Some(request)
            && self
                .draft
                .as_ref()
                .is_some_and(|draft| editor_authoring_fingerprint(draft) == request.fingerprint)
    }

    pub fn discard_editor_result_envelope(&mut self, request: &EditorAuthoringRequest) -> bool {
        if self.active_editor_authoring_request.as_ref() != Some(request) {
            return false;
        }
        self.active_editor_authoring_request = None;
        self.editor_authoring_dispatched = false;
        self.pending_inspector_intent = None;
        true
    }

    fn editor_mutations_allowed(&self) -> bool {
        self.wizard.is_none()
            && self.active_wizard_request.is_none()
            && self.running_snapshot.is_none()
            && self.active_editor_authoring_request.is_none()
            && self
                .draft
                .as_ref()
                .is_some_and(|draft| matches!(draft.editability, DraftEditability::Editable))
    }

    pub fn take_wizard_request(&mut self) -> Option<wizard::WizardAuthoringRequest> {
        if self.wizard_request_dispatched {
            return None;
        }
        let request = self.active_wizard_request.clone()?;
        self.wizard_request_dispatched = true;
        Some(request)
    }

    pub fn apply_wizard_result(
        &mut self,
        result: wizard::WizardAuthoringResult,
    ) -> Result<(), wizard::WizardResultError> {
        use wizard::{WizardActionTarget, WizardAuthoringKind, WizardAuthoringOutcome};
        let Some(request) = self.active_wizard_request.as_ref() else {
            return Err(wizard::WizardResultError::UnexpectedResult);
        };
        if Some(request.session) != self.wizard_session
            || result.session != request.session
            || request.id != result.id
            || request.fingerprint != result.fingerprint
        {
            return Err(wizard::WizardResultError::UnexpectedResult);
        }
        let Some(wizard_state) = self.wizard.as_mut() else {
            return Err(wizard::WizardResultError::StaleWizard);
        };
        if wizard_state.review_fingerprint() != request.fingerprint {
            self.active_wizard_request = None;
            self.wizard_request_dispatched = false;
            self.editor_feedback = Some("Discarded stale wizard capture/test result.".into());
            return Err(wizard::WizardResultError::StaleWizard);
        }

        let applied = match (request.kind, result.outcome) {
            (
                WizardAuthoringKind::CaptureTarget,
                WizardAuthoringOutcome::TargetGeometry {
                    process_path,
                    window_class,
                    title,
                    width,
                    height,
                    dpi,
                },
            ) => {
                wizard_state.record_target_capture(crate::engine::macro_engine::TargetProfile {
                    process_path,
                    window_class,
                    title_contains: title,
                    captured_client_width: width,
                    captured_client_height: height,
                    captured_dpi: dpi,
                });
                true
            }
            (
                WizardAuthoringKind::CaptureTextRegion | WizardAuthoringKind::CaptureImageRegion,
                WizardAuthoringOutcome::Region(rect),
            ) => {
                wizard_state.record_region_capture(rect);
                true
            }
            (WizardAuthoringKind::CaptureClickPoint, WizardAuthoringOutcome::Point(point)) => {
                wizard_state.action_target = WizardActionTarget::SavedPoint {
                    id: "click-point".into(),
                    point,
                };
                wizard_state.record_action_capture();
                true
            }
            (WizardAuthoringKind::CaptureClickRegion, WizardAuthoringOutcome::Region(rect)) => {
                wizard_state.action_target = WizardActionTarget::SavedRegion {
                    id: "click-region".into(),
                    rect,
                };
                wizard_state.record_action_capture();
                true
            }
            (WizardAuthoringKind::CaptureTemplate, WizardAuthoringOutcome::Template { asset }) => {
                wizard_state.detector = wizard::WizardDetector::Image { template: asset };
                wizard_state.invalidate_detector_proof();
                true
            }
            (
                WizardAuthoringKind::TestDetector,
                WizardAuthoringOutcome::DetectorTest {
                    passed,
                    evidence,
                    elapsed_ms,
                    image_verification,
                },
            ) => {
                if matches!(wizard_state.detector, wizard::WizardDetector::Image { .. }) {
                    wizard_state.record_image_detector_test(
                        passed,
                        evidence,
                        elapsed_ms,
                        image_verification,
                    );
                } else {
                    wizard_state.record_detector_test(passed, evidence, elapsed_ms);
                }
                true
            }
            (
                WizardAuthoringKind::CaptureImageNegative,
                WizardAuthoringOutcome::ImageNegativeSample(sample),
            ) => {
                wizard_state.image_negative_samples.push(sample);
                wizard_state.detector_test = None;
                wizard_state.image_verification = None;
                wizard_state.invalidate_dry_run_review();
                self.editor_feedback =
                    Some("Captured a revision-bound image negative sample.".into());
                true
            }
            (_, WizardAuthoringOutcome::Failed(message)) => {
                self.editor_feedback = Some(format!("Wizard request failed: {message}"));
                true
            }
            (_, WizardAuthoringOutcome::Cancelled) => {
                self.editor_feedback = Some("Wizard capture cancelled.".into());
                true
            }
            _ => false,
        };
        if !applied {
            return Err(wizard::WizardResultError::OutcomeMismatch);
        }
        self.active_wizard_request = None;
        self.wizard_request_dispatched = false;
        self.pending_wizard_action = None;
        Ok(())
    }

    pub fn take_editor_authoring_request(&mut self) -> Option<EditorAuthoringRequest> {
        if self.editor_authoring_dispatched {
            return None;
        }
        let request = self.active_editor_authoring_request.clone()?;
        self.editor_authoring_dispatched = true;
        Some(request)
    }

    pub fn apply_editor_authoring_result(
        &mut self,
        result: EditorAuthoringResult,
    ) -> Result<(), EditorAuthoringError> {
        let Some(request) = self.active_editor_authoring_request.as_ref() else {
            return Err(EditorAuthoringError::UnexpectedResult);
        };
        if Some(request.session) != self.draft_session
            || result.session != request.session
            || request.id != result.id
            || request.fingerprint != result.fingerprint
        {
            return Err(EditorAuthoringError::UnexpectedResult);
        }
        let Some(draft) = self.draft.as_ref() else {
            return Err(EditorAuthoringError::StaleDraft);
        };
        if editor_authoring_fingerprint(draft) != request.fingerprint {
            self.active_editor_authoring_request = None;
            self.editor_authoring_dispatched = false;
            return Err(EditorAuthoringError::StaleDraft);
        }
        let request_kind = request.kind.clone();
        let outcome = result.outcome;
        self.active_editor_authoring_request = None;
        self.editor_authoring_dispatched = false;
        self.pending_inspector_intent = None;
        match (request_kind, outcome) {
            (
                EditorAuthoringKind::CaptureTarget,
                EditorAuthoringOutcome::TargetGeometry {
                    process_path,
                    window_class,
                    title,
                    width,
                    height,
                    dpi,
                },
            ) => dispatch_editor_command(
                self,
                EditorCommand::ReplaceTarget {
                    target: crate::engine::macro_engine::TargetProfile {
                        process_path,
                        window_class,
                        title_contains: title,
                        captured_client_width: width,
                        captured_client_height: height,
                        captured_dpi: dpi,
                    },
                },
            )
            .map(|_| {
                self.editor_image_negative_samples.clear();
            })
            .map_err(|_| EditorAuthoringError::EditRejected),
            (
                EditorAuthoringKind::RecaptureRegion { region_id },
                EditorAuthoringOutcome::Region(rect),
            ) => dispatch_editor_command(
                self,
                EditorCommand::ApplyRecapture {
                    region_id,
                    rect,
                    image_template: None,
                },
            )
            .map(|_| {
                self.editor_image_negative_samples.clear();
            })
            .map_err(|_| EditorAuthoringError::EditRejected),
            (
                EditorAuthoringKind::RecaptureTemplate { rule_id },
                EditorAuthoringOutcome::Template { asset },
            ) => dispatch_editor_command(
                self,
                EditorCommand::ApplyTemplateRecapture {
                    rule_id,
                    template: asset,
                },
            )
            .map(|_| {
                self.editor_image_negative_samples.clear();
            })
            .map_err(|_| EditorAuthoringError::EditRejected),
            (
                request_kind @ (EditorAuthoringKind::TestOcr { .. }
                | EditorAuthoringKind::TestImage { .. }),
                EditorAuthoringOutcome::DetectorTest {
                    passed,
                    evidence,
                    elapsed_ms,
                    rule_id,
                    image_verification,
                },
            ) => {
                if image_verification.is_some() && !passed {
                    return Err(EditorAuthoringError::OutcomeMismatch);
                }
                let (block_id, effective_passed) = match request_kind {
                    EditorAuthoringKind::TestOcr { block_id } => {
                        if rule_id.is_some() || image_verification.is_some() {
                            return Err(EditorAuthoringError::OutcomeMismatch);
                        }
                        (block_id, passed)
                    }
                    EditorAuthoringKind::TestImage { block_id } => {
                        let expected_rule = match condition_for_block(
                            &self.draft.as_ref().unwrap().definition,
                            &block_id,
                        ) {
                            Some(crate::engine::macro_engine::Condition::Image {
                                rule_id, ..
                            }) => rule_id.clone(),
                            _ => return Err(EditorAuthoringError::OutcomeMismatch),
                        };
                        if passed {
                            match (rule_id, image_verification) {
                                (Some(rule_id), Some(verification)) => {
                                    if expected_rule != rule_id {
                                        return Err(EditorAuthoringError::OutcomeMismatch);
                                    }
                                    dispatch_editor_command(
                                        self,
                                        EditorCommand::ApplyImageVerification {
                                            rule_id,
                                            verification,
                                        },
                                    )
                                    .map_err(|_| EditorAuthoringError::EditRejected)?;
                                    (block_id, true)
                                }
                                (None, None) => {
                                    // A positive image test without a revision-bound verification
                                    // artifact is ambiguous and cannot make the draft run-ready.
                                    dispatch_editor_command(
                                        self,
                                        EditorCommand::ClearImageVerification {
                                            rule_id: expected_rule,
                                        },
                                    )
                                    .map_err(|_| EditorAuthoringError::EditRejected)?;
                                    (block_id, false)
                                }
                                _ => return Err(EditorAuthoringError::OutcomeMismatch),
                            }
                        } else {
                            dispatch_editor_command(
                                self,
                                EditorCommand::ClearImageVerification {
                                    rule_id: expected_rule,
                                },
                            )
                            .map_err(|_| EditorAuthoringError::EditRejected)?;
                            (block_id, false)
                        }
                    }
                    _ => unreachable!(),
                };
                let draft = self.draft.as_mut().unwrap();
                if effective_passed {
                    draft.clear_detector_test_failure(&block_id);
                } else {
                    let fingerprint = detector_fingerprint_for_block(&draft.definition, &block_id)
                        .ok_or(EditorAuthoringError::OutcomeMismatch)?;
                    draft.record_detector_test_failure(&block_id, fingerprint, evidence.clone());
                }
                self.editor_feedback = Some(format!(
                    "Detector test {} in {elapsed_ms} ms: {evidence}",
                    if effective_passed { "passed" } else { "failed" }
                ));
                Ok(())
            }
            (
                EditorAuthoringKind::CaptureImageNegative { block_id },
                EditorAuthoringOutcome::ImageNegativeSample {
                    block_id: outcome_block,
                    sample,
                },
            ) if block_id == outcome_block => {
                self.editor_image_negative_samples
                    .entry(block_id)
                    .or_default()
                    .push(sample);
                self.editor_feedback = Some("Captured an image negative sample.".into());
                Ok(())
            }
            (kind, EditorAuthoringOutcome::Failed(message)) => {
                match kind {
                    EditorAuthoringKind::TestOcr { block_id } => {
                        let fingerprint = detector_fingerprint_for_block(
                            &self.draft.as_ref().unwrap().definition,
                            &block_id,
                        )
                        .ok_or(EditorAuthoringError::OutcomeMismatch)?;
                        self.draft.as_mut().unwrap().record_detector_test_failure(
                            block_id,
                            fingerprint,
                            message.clone(),
                        );
                    }
                    EditorAuthoringKind::TestImage { block_id } => {
                        let rule_id = match condition_for_block(
                            &self.draft.as_ref().unwrap().definition,
                            &block_id,
                        ) {
                            Some(crate::engine::macro_engine::Condition::Image {
                                rule_id, ..
                            }) => Some(rule_id.clone()),
                            _ => None,
                        };
                        if let Some(rule_id) = rule_id {
                            let _ = dispatch_editor_command(
                                self,
                                EditorCommand::ClearImageVerification { rule_id },
                            );
                        }
                        let fingerprint = detector_fingerprint_for_block(
                            &self.draft.as_ref().unwrap().definition,
                            &block_id,
                        )
                        .ok_or(EditorAuthoringError::OutcomeMismatch)?;
                        self.draft.as_mut().unwrap().record_detector_test_failure(
                            block_id,
                            fingerprint,
                            message.clone(),
                        );
                    }
                    _ => {}
                }
                self.editor_feedback = Some(format!("Authoring request failed: {message}"));
                Ok(())
            }
            (_, EditorAuthoringOutcome::Cancelled) => {
                self.editor_feedback = Some("Authoring capture cancelled.".into());
                Ok(())
            }
            _ => Err(EditorAuthoringError::OutcomeMismatch),
        }
    }
}

fn editor_authoring_fingerprint(draft: &EditorDraft) -> String {
    format!(
        "{}|{:?}|{:?}|{:?}",
        draft.definition.revision,
        draft.definition.regions,
        draft.definition.text_rules,
        draft.definition.image_rules
    )
}

fn begin_editor_authoring(state: &mut MacroPageState, kind: EditorAuthoringKind) {
    if state.wizard.is_some() {
        state.editor_feedback = Some("Close the guided wizard before detector authoring.".into());
        return;
    }
    if state.active_editor_authoring_request.is_some() {
        state.editor_feedback = Some("Wait for the current detector request to finish.".into());
        return;
    }
    if state.draft_session.is_none() && state.draft.is_some() {
        state.begin_draft_session();
    }
    let Some(draft) = state.draft.as_ref() else {
        state.editor_feedback = Some("No draft is available for detector authoring.".into());
        return;
    };
    let id = EditorAuthoringRequestId(state.next_editor_authoring_request_id);
    state.next_editor_authoring_request_id =
        state.next_editor_authoring_request_id.saturating_add(1);
    let image_negative_samples = match &kind {
        EditorAuthoringKind::TestImage { block_id }
        | EditorAuthoringKind::CaptureImageNegative { block_id } => state
            .editor_image_negative_samples
            .get(block_id)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    state.active_editor_authoring_request = Some(EditorAuthoringRequest {
        session: state
            .draft_session
            .expect("draft session exists with draft"),
        id,
        fingerprint: editor_authoring_fingerprint(draft),
        kind,
        image_negative_samples,
    });
    state.editor_authoring_dispatched = false;
}

#[derive(Debug, Clone, PartialEq)]
struct PendingConversion {
    block_id: String,
    preview: ConversionPreview,
    required_values: Vec<(String, String)>,
    command: EditorCommand,
    structural_children: bool,
}

/// The page never drops the canvas at narrow widths: supporting panes become explicit drawers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneMode {
    ThreePane,
    ThreePaneCompact,
    CanvasWithDrawers,
}

pub fn pane_mode(width: f32) -> PaneMode {
    if width >= 1100.0 {
        PaneMode::ThreePane
    } else if width >= 720.0 {
        PaneMode::ThreePaneCompact
    } else {
        PaneMode::CanvasWithDrawers
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunControlAvailability {
    pub can_validate: bool,
    pub can_dry_run: bool,
    pub can_run_once: bool,
    pub can_run_continuous: bool,
    pub can_run_live: bool,
    pub can_pause: bool,
    pub can_resume: bool,
    pub can_stop: bool,
    pub primary_label: &'static str,
    pub primary_detail: &'static str,
    pub disabled_reason: Option<String>,
}

pub fn run_control_availability(state: &MacroPageState) -> RunControlAvailability {
    let monitor = monitor_from_runtime_state(state, state.selected_saved_macro_id());
    let can_start =
        state.selected_saved.is_some() && state.enabled && monitor.status == RunStatus::Idle;
    let disabled_reason = (!can_start).then(|| {
        if state.selected_saved.is_none() {
            "Save and select a validated macro before running.".to_string()
        } else if !state.enabled {
            "Enable this saved macro before running it.".to_string()
        } else {
            "A macro run is already active or has not returned to idle.".to_string()
        }
    });
    let (primary_label, primary_detail) = match monitor.mode {
        Some(RunMode::DryRun) => ("Dry Run", "Observe only"),
        Some(RunMode::Live) => ("Run", "Live"),
        Some(RunMode::ObservationOnly) => ("Run Once", "Observe only"),
        None => ("Run", "Live"),
    };

    RunControlAvailability {
        can_validate: state.draft.is_some() && state.editor_mutations_allowed(),
        can_dry_run: can_start,
        can_run_once: can_start,
        can_run_continuous: can_start,
        can_run_live: can_start,
        can_pause: monitor.status == RunStatus::Running,
        can_resume: monitor.status == RunStatus::Paused,
        can_stop: matches!(
            monitor.status,
            RunStatus::Running | RunStatus::Paused | RunStatus::Stopping
        ),
        primary_label,
        primary_detail,
        disabled_reason,
    }
}

#[derive(Debug, Default)]
pub struct MacroPage;

impl MacroPage {
    pub const BOTTOM_MIN_HEIGHT: f32 = 184.0;
    pub const BOTTOM_DEFAULT_HEIGHT: f32 = 276.0;
    pub const BOTTOM_MAX_HEIGHT: f32 = 520.0;

    pub fn show(ui: &mut Ui, state: &mut MacroPageState) {
        if let Some(draft) = &mut state.draft {
            draft.editability = if state.running_snapshot.is_some() {
                DraftEditability::Running {
                    revision: draft.definition.revision,
                }
            } else {
                DraftEditability::Editable
            };
        }
        let selected_macro_id = state.selected_saved_macro_id();
        let monitor = monitor_from_runtime_state(state, selected_macro_id);
        let last_completion = last_completion_from_runtime_state(state, selected_macro_id);
        let problems = state
            .draft
            .as_ref()
            .map(editor_validation_problems)
            .unwrap_or_default();
        let library_rows =
            project_library_rows(state, &monitor, &problems, last_completion.as_ref());
        let canvas = state
            .draft
            .as_ref()
            .map(|definition| project_canvas(definition))
            .unwrap_or(CanvasProjection {
                nodes: Vec::new(),
                groups: Vec::new(),
                edges: Vec::new(),
            });

        ui.set_width(ui.available_width());
        ui.vertical(|ui| {
            image_package_reverification(ui, state);
            let wizard_pending = state.active_wizard_request.is_some();
            let wizard_action = state
                .wizard
                .as_mut()
                .and_then(|wizard_state| wizard::show(ui, wizard_state, wizard_pending));
            if let Some(action) = wizard_action {
                apply_wizard_ui_action(state, action);
            }
            if state.wizard.is_some() {
                ui.add_space(8.0);
            }
            if let Some(target) = status_strip(ui, state, &monitor, &problems) {
                select_canvas(state, CanvasSelection::Block(target));
            }
            ui.add_space(8.0);
            if let Some(target) = workspace(ui, state, &library_rows, &canvas, &monitor, &problems)
            {
                select_canvas(state, target);
            }
        });
    }

    /// Bottom composition deliberately owns presentation only. NativeApp drains the bounded
    /// intent queue after rendering and remains the sole controller/store owner.
    pub fn show_bottom(ui: &mut Ui, state: &mut MacroPageState) {
        let selected_macro_id = state.selected_saved_macro_id();
        let monitor = monitor_from_runtime_state(state, selected_macro_id);
        let controls = run_control_availability(state);
        egui::ScrollArea::vertical()
            .id_source("macro-bottom-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if ui.available_width() < 720.0 {
                    run_controls(ui, state, &controls);
                    ui.add_space(6.0);
                    monitor::show(ui, &monitor);
                } else {
                    ui.columns(2, |columns| {
                        run_controls(&mut columns[0], state, &controls);
                        monitor::show(&mut columns[1], &monitor);
                    });
                }
            });
    }
}

fn image_package_reverification(ui: &mut Ui, state: &mut MacroPageState) {
    let Some(progress) = state.image_package_reverification.clone() else {
        return;
    };
    let rule = progress.active_rule_id().unwrap_or("local image rule");
    Frame::none()
        .fill(Color32::from_rgb(35, 27, 19))
        .stroke(Stroke::new(1.0, Color32::from_rgb(174, 91, 43)))
        .rounding(6.0)
        .inner_margin(egui::Margin::same(10.0))
        .show(ui, |ui| {
            ui.label(
                RichText::new("LOCAL IMAGE PACKAGE RE-VERIFICATION")
                    .strong()
                    .size(text::SUPPORTING)
                    .color(Color32::from_rgb(225, 174, 108)),
            );
            ui.label(
                RichText::new(format!(
                    "Rule {}/{}: {rule}",
                    progress.active_rule_index + 1,
                    progress.rule_ids.len()
                ))
                .monospace()
                .size(text::META)
                .color(Color32::from_gray(180)),
            );
            ui.label(
                RichText::new(progress.stage.instruction())
                    .size(text::SUPPORTING)
                    .color(Color32::from_gray(210)),
            );
            ui.horizontal(|ui| {
                if ui.button(progress.stage.action_label()).clicked() {
                    state.enqueue_intent(MacroIntent::ContinueImagePackageReverification);
                }
                if ui.button("Cancel import").clicked() {
                    state.enqueue_intent(MacroIntent::CancelImagePackageReverification);
                }
            });
        });
    ui.add_space(8.0);
}

fn monitor_from_runtime_state(
    state: &MacroPageState,
    selected_macro_id: Option<&str>,
) -> MonitorProjection {
    match (&state.controller_lifecycle, &state.controller_semantic) {
        (Some(lifecycle), Some(semantic)) => project_monitor_with_controller_projections(
            selected_macro_id,
            state.running_snapshot.as_ref(),
            &state.runtime_events,
            lifecycle,
            semantic,
        ),
        _ => project_monitor(
            selected_macro_id,
            state.running_snapshot.as_ref(),
            &state.runtime_events,
        ),
    }
}

fn last_completion_from_runtime_state(
    state: &MacroPageState,
    selected_macro_id: Option<&str>,
) -> Option<monitor::StopOutcome> {
    let macro_id = selected_macro_id?;
    match (&state.controller_lifecycle, &state.controller_semantic) {
        (Some(lifecycle), Some(semantic)) => project_last_completion_with_controller_projections(
            &state.runtime_events,
            macro_id,
            lifecycle,
            semantic,
        ),
        _ => project_last_completion(&state.runtime_events, macro_id),
    }
}

fn project_library_rows(
    state: &MacroPageState,
    monitor: &MonitorProjection,
    problems: &[ValidationProblem],
    last_completion: Option<&monitor::StopOutcome>,
) -> Vec<MacroLibraryRow> {
    let mut rows = state.library_rows.clone();
    let (Some(definition), Some(saved)) = (state.draft.as_ref(), state.selected_saved.as_ref())
    else {
        return rows;
    };
    if definition.id != saved.macro_id || state.saved_revision != Some(saved.revision) {
        return rows;
    }
    let projected = project_definition(
        definition,
        state.saved_revision,
        state.enabled,
        problems,
        monitor,
        last_completion,
    );
    if let Some(existing) = rows.iter_mut().find(|row| row.id == projected.id) {
        *existing = projected;
    } else {
        rows.push(projected);
    }
    rows
}

fn select_canvas(state: &mut MacroPageState, selection: CanvasSelection) {
    state.selected_block_id = match &selection {
        CanvasSelection::Block(id) => Some(id.clone()),
        CanvasSelection::Lane { lane_id, .. } => Some(lane_id.clone()),
        CanvasSelection::TimeoutBody { .. } => None,
    };
    state.selected_canvas = Some(selection);
}

fn apply_wizard_ui_action(state: &mut MacroPageState, action: wizard::WizardUiAction) {
    if state.active_wizard_request.is_some() && !matches!(action, wizard::WizardUiAction::Finish(_))
    {
        state.editor_feedback = Some("Wait for the current wizard request to finish.".into());
        return;
    }
    match action {
        wizard::WizardUiAction::Finish(output) => {
            let selected = output
                .definition
                .blocks
                .first()
                .map(|block| block.id.clone());
            let generated_session = state
                .wizard_session
                .take()
                .unwrap_or_else(|| state.allocate_authoring_session());
            state.draft = Some(EditorDraft::new(output.definition));
            state.draft_session = Some(generated_session);
            state.clear_selected_saved();
            state.selected_block_id = selected.clone();
            state.selected_canvas = selected.map(CanvasSelection::Block);
            state.wizard = None;
            state.cancel_wizard_authoring();
            state.editor_feedback = Some(
                "Wizard created an unsaved canonical draft. Every step remains editable.".into(),
            );
        }
        request => {
            if state.wizard_session.is_none() && state.wizard.is_some() {
                state.begin_wizard_session();
            }
            let kind = match request {
                wizard::WizardUiAction::CaptureTarget => wizard::WizardAuthoringKind::CaptureTarget,
                wizard::WizardUiAction::CaptureRegion(wizard::WizardDetectorKind::Text) => {
                    wizard::WizardAuthoringKind::CaptureTextRegion
                }
                wizard::WizardUiAction::CaptureRegion(wizard::WizardDetectorKind::Image) => {
                    wizard::WizardAuthoringKind::CaptureImageRegion
                }
                wizard::WizardUiAction::CaptureTemplate => {
                    wizard::WizardAuthoringKind::CaptureTemplate
                }
                wizard::WizardUiAction::CaptureClickPoint => {
                    wizard::WizardAuthoringKind::CaptureClickPoint
                }
                wizard::WizardUiAction::CaptureClickRegion => {
                    wizard::WizardAuthoringKind::CaptureClickRegion
                }
                wizard::WizardUiAction::TestDetector => wizard::WizardAuthoringKind::TestDetector,
                wizard::WizardUiAction::CaptureImageNegative => {
                    wizard::WizardAuthoringKind::CaptureImageNegative
                }
                wizard::WizardUiAction::Finish(_) => unreachable!(),
            };
            let Some(wizard_state) = state.wizard.as_ref() else {
                return;
            };
            let id = wizard::WizardRequestId(state.next_wizard_request_id);
            state.next_wizard_request_id = state.next_wizard_request_id.saturating_add(1);
            state.active_wizard_request = Some(wizard::WizardAuthoringRequest {
                session: state
                    .wizard_session
                    .expect("wizard session exists with wizard"),
                id,
                fingerprint: wizard_state.review_fingerprint(),
                kind,
            });
            state.wizard_request_dispatched = false;
            state.pending_wizard_action = Some(request);
        }
    }
}

fn current_canvas_selection(state: &MacroPageState) -> Option<CanvasSelection> {
    if let Some(id) = &state.selected_block_id {
        if let Some((group_id, _, _, _)) = state
            .draft
            .as_ref()
            .and_then(|draft| locate_watch_lane(draft, id))
        {
            return Some(CanvasSelection::Lane {
                group_id,
                lane_id: id.clone(),
            });
        }
        return Some(CanvasSelection::Block(id.clone()));
    }
    state.selected_canvas.clone()
}

fn status_strip(
    ui: &mut Ui,
    state: &mut MacroPageState,
    monitor: &MonitorProjection,
    problems: &[ValidationProblem],
) -> Option<String> {
    let mut navigate = None;
    Frame::none()
        .fill(Color32::from_rgb(17, 20, 22))
        .stroke(Stroke::new(1.0, Color32::from_rgb(48, 52, 55)))
        .rounding(6.0)
        .inner_margin(egui::Margin::same(11.0))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                status_fact(
                    ui,
                    "TARGET",
                    state
                        .draft
                        .as_ref()
                        .map(|definition| definition.target.title_contains.as_str())
                        .filter(|title| !title.is_empty())
                        .unwrap_or("No target selected"),
                );
                status_fact(ui, "WINDOW", "Not connected");
                status_fact(ui, "FOREGROUND", "Unknown");
                status_fact(
                    ui,
                    "DISPLAY",
                    state
                        .draft
                        .as_ref()
                        .map(|definition| {
                            format!(
                                "{}x{} | {} DPI",
                                definition.target.captured_client_width,
                                definition.target.captured_client_height,
                                definition.target.captured_dpi
                            )
                        })
                        .as_deref()
                        .unwrap_or("--"),
                );
                status_fact(ui, "GEOMETRY", "Snapshot only");
                status_fact(ui, "REVISION", revision_summary(state, monitor).as_str());
                status_fact(ui, "VALIDATION", validation_summary(state, problems));
            });
            ui.add_space(9.0);
            ui.horizontal_wrapped(|ui| {
                if state.draft.is_none()
                    && ui
                        .add_enabled(
                            state.wizard.is_none() && state.active_wizard_request.is_none(),
                            Button::new("Create starter draft"),
                        )
                        .clicked()
                {
                    state.begin_draft_session();
                    state.draft = Some(EditorDraft::new(starter_macro_definition()));
                    state.selected_block_id = Some("observe-1".into());
                    state.selected_canvas = Some(CanvasSelection::Block("observe-1".into()));
                    state.editor_feedback =
                        Some("Created an unsaved starter draft for editor authoring.".into());
                }
                let wizard_pending = state.active_wizard_request.is_some();
                if state.wizard.is_none()
                    && ui
                        .add_enabled(
                            !wizard_pending && state.active_editor_authoring_request.is_none(),
                            Button::new("Guided wizard"),
                        )
                        .clicked()
                {
                    state.begin_wizard_session();
                    state.wizard = Some(wizard::WizardState::default());
                } else if state.wizard.is_some()
                    && ui
                        .add_enabled(!wizard_pending, Button::new("Close wizard"))
                        .clicked()
                {
                    state.wizard = None;
                    state.wizard_session = None;
                    state.cancel_wizard_authoring();
                }
                let can_edit = state.editor_mutations_allowed();
                if state.draft.is_some() {
                    let target_label = if state
                        .draft
                        .as_ref()
                        .is_some_and(|draft| draft.target.process_path.is_empty())
                    {
                        "Capture Target"
                    } else {
                        "Retarget"
                    };
                    if ui
                        .add_enabled(can_edit, Button::new(target_label))
                        .clicked()
                    {
                        begin_editor_authoring(state, EditorAuthoringKind::CaptureTarget);
                    }
                }
                let can_save = can_edit
                    && state.draft.is_some()
                    && problems.is_empty()
                    && state
                        .draft
                        .as_ref()
                        .is_some_and(|draft| draft.status == DraftStatus::Ready);
                if ui.add_enabled(can_save, Button::new("Save")).clicked() {
                    state.enqueue_intent(MacroIntent::Save);
                }
                if let Some(target) = inspector::problem_navigation_target(problems, 0) {
                    if ui.small_button("Open first problem").clicked() {
                        navigate = Some(target);
                    }
                }
            });
        });
    navigate
}

fn run_controls(ui: &mut Ui, state: &mut MacroPageState, controls: &RunControlAvailability) {
    section(ui, "RUN CONTROLS", |ui| {
        ui.label(
            RichText::new(
                "Validate before saving. Dry Run performs observation only; Live may inject input.",
            )
            .size(text::SUPPORTING)
            .color(Color32::from_gray(176)),
        );
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if disabled_action(
                ui,
                controls.can_validate,
                Button::new("Validate"),
                controls,
                "Create or select a draft before validating.",
                false,
            ) {
                state.enqueue_intent(MacroIntent::Validate);
            }
            if disabled_action(
                ui,
                controls.can_dry_run,
                Button::new("Dry Run\nObserve only"),
                controls,
                "Save and select a validated macro before running.",
                true,
            ) {
                state.request_run(MacroRunIntent::DryRun);
            }
            if disabled_action(
                ui,
                controls.can_run_once,
                Button::new("Run Once"),
                controls,
                "Save and select a validated macro before running.",
                true,
            ) {
                state.request_run(MacroRunIntent::RunOnce);
            }
            if disabled_action(
                ui,
                controls.can_run_continuous,
                Button::new("Run\nObserve only"),
                controls,
                "Save and select a validated macro before running.",
                true,
            ) {
                state.request_run(MacroRunIntent::ContinuousObservation);
            }
            if disabled_action(
                ui,
                controls.can_run_live,
                Button::new(
                    RichText::new("Run\nLive")
                        .strong()
                        .color(Color32::from_rgb(242, 174, 109)),
                ),
                controls,
                "Save and select a validated macro before running.",
                true,
            ) {
                state.request_run(MacroRunIntent::ContinuousLive);
            }
            if disabled_action(
                ui,
                controls.can_pause,
                Button::new("Pause"),
                controls,
                "Pause is available only while a macro is running.",
                false,
            ) {
                state.enqueue_intent(MacroIntent::Pause);
            }
            if disabled_action(
                ui,
                controls.can_resume,
                Button::new("Resume"),
                controls,
                "Resume is available only while a macro is paused.",
                false,
            ) {
                state.enqueue_intent(MacroIntent::Resume);
            }
            let stop = Button::new(
                RichText::new("Stop")
                    .strong()
                    .color(Color32::from_rgb(245, 125, 112)),
            );
            if disabled_action(
                ui,
                controls.can_stop,
                stop,
                controls,
                "Stop is available only while a macro is active.",
                false,
            ) {
                state.enqueue_intent(MacroIntent::Stop);
            }
        });
    });
}

fn disabled_action(
    ui: &mut Ui,
    enabled: bool,
    button: Button,
    controls: &RunControlAvailability,
    fallback_reason: &str,
    use_run_reason: bool,
) -> bool {
    let response = ui.add_enabled(enabled, button);
    if !enabled {
        let reason = if use_run_reason {
            controls
                .disabled_reason
                .as_deref()
                .unwrap_or(fallback_reason)
        } else {
            fallback_reason
        };
        response.clone().on_hover_text(reason);
    }
    response.clicked()
}

fn validation_summary(state: &MacroPageState, problems: &[ValidationProblem]) -> &'static str {
    match state.draft.as_ref() {
        None => "No definition",
        Some(_) if !problems.is_empty() => "Needs revalidation",
        Some(draft) if draft.status == DraftStatus::NeedsValidation => "Needs revalidation",
        Some(_) => "Valid",
    }
}

fn starter_macro_definition() -> crate::engine::macro_engine::MacroDefinition {
    use crate::engine::macro_engine::*;
    use crate::engine::types::RectRatio;
    MacroDefinition {
        schema_version: MACRO_SCHEMA_VERSION,
        id: "starter-macro".into(),
        name: "Starter Macro".into(),
        revision: 1,
        target: TargetProfile {
            process_path: String::new(),
            window_class: String::new(),
            title_contains: "Diablo IV".into(),
            captured_client_width: 1280,
            captured_client_height: 720,
            captured_dpi: 96,
        },
        regions: vec![RegionDefinition {
            id: "starter-region".into(),
            revision: 1,
            rect: RectRatio {
                x: 0.25,
                y: 0.25,
                width: 0.5,
                height: 0.2,
            },
        }],
        points: vec![],
        text_rules: vec![TextRule {
            id: "starter-text".into(),
            revision: 1,
            region_id: "starter-region".into(),
            language: "en-US".into(),
            preprocess: PreprocessProfile::Grayscale,
            expected: "Edit expected text".into(),
            match_mode: TextMatchMode::Contains,
            threshold: 0.9,
            case_sensitive: false,
            allow_cross_line: false,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 250,
            timeout_ms: Limit::Unlimited,
            stable_frames: 2,
        }],
        image_rules: vec![],
        blocks: vec![Block {
            id: "observe-1".into(),
            enabled: true,
            kind: BlockKind::Observe {
                condition: Condition::Text {
                    source_block_id: "observe-1".into(),
                    rule_id: "starter-text".into(),
                    mode: ObserveMode::CheckNow,
                },
            },
        }],
        safety: SafetyPolicy {
            max_runtime_ms: Limit::Unlimited,
            max_clicks: Limit::Unlimited,
            max_observation_retries: Limit::Unlimited,
            max_observations_per_second: 20,
            minimum_click_interval_ms: 100,
            focus_loss: FocusLossPolicy::Stop,
        },
    }
}

fn status_fact(ui: &mut Ui, label: &str, value: &str) {
    ui.vertical(|ui| {
        ui.label(
            RichText::new(label)
                .monospace()
                .size(text::META)
                .strong()
                .color(Color32::from_rgb(164, 127, 88)),
        );
        ui.label(
            RichText::new(value)
                .size(text::SUPPORTING)
                .color(Color32::from_gray(202)),
        );
    });
    ui.add_space(10.0);
}

fn revision_summary(state: &MacroPageState, monitor: &MonitorProjection) -> String {
    let draft = state
        .draft
        .as_ref()
        .map(|definition| definition.revision.to_string())
        .unwrap_or_else(|| "--".to_string());
    let saved = state
        .saved_revision
        .map(|revision| revision.to_string())
        .unwrap_or_else(|| "--".to_string());
    let running = monitor
        .running_revision
        .map(|revision| revision.to_string())
        .unwrap_or_else(|| "--".to_string());
    format!("draft {draft} | saved {saved} | running {running}")
}

fn workspace(
    ui: &mut Ui,
    state: &mut MacroPageState,
    library_rows: &[MacroLibraryRow],
    canvas: &CanvasProjection,
    monitor: &MonitorProjection,
    problems: &[ValidationProblem],
) -> Option<CanvasSelection> {
    let mut selection = None;
    let selected_owned = state.selected_block_id.clone();
    let selected = selected_owned.as_deref();
    let selected_canvas = current_canvas_selection(state);
    let projection = state
        .draft
        .as_ref()
        .and_then(|definition| {
            selected.map(|id| inspector::project_inspector(definition, id, problems))
        })
        .unwrap_or(inspector::InspectorProjection::Empty);
    let editable = state.editor_mutations_allowed();
    let filtered_rows = library_rows
        .iter()
        .filter(|row| {
            let query = state.library_search.trim().to_ascii_lowercase();
            query.is_empty()
                || row.name.to_ascii_lowercase().contains(&query)
                || row.status.label().to_ascii_lowercase().contains(&query)
        })
        .cloned()
        .collect::<Vec<_>>();
    match pane_mode(ui.available_width()) {
        PaneMode::ThreePane | PaneMode::ThreePaneCompact => {
            let compact = pane_mode(ui.available_width()) == PaneMode::ThreePaneCompact;
            let side_width = if compact { 175.0 } else { 250.0 };
            let side_max = if compact { 230.0 } else { 340.0 };
            egui::SidePanel::left("macro-library-pane")
                .resizable(true)
                .default_width(side_width)
                .min_width(150.0)
                .max_width(side_max)
                .show_inside(ui, |ui| {
                    section(ui, "LIBRARY", |ui| {
                        library_pane(ui, state, &filtered_rows);
                    });
                });
            egui::SidePanel::right("macro-inspector-pane")
                .resizable(true)
                .default_width(side_width)
                .min_width(170.0)
                .max_width(side_max)
                .show_inside(ui, |ui| {
                    section(ui, "BLOCK INSPECTOR", |ui| {
                        if let Some(intent) = inspector::show(ui, &projection, editable) {
                            handle_inspector_intent(state, intent);
                        }
                    });
                });
            egui::CentralPanel::default()
                .frame(Frame::none())
                .show_inside(ui, |ui| {
                    section(ui, "EVENT CANVAS", |ui| {
                        editor_toolbar(ui, state);
                        selection = show_interactive_canvas(
                            ui,
                            state,
                            canvas,
                            monitor.active_block.as_deref(),
                            selected_canvas.as_ref(),
                        );
                    });
                });
        }
        PaneMode::CanvasWithDrawers => {
            section(ui, "EVENT CANVAS", |ui| {
                editor_toolbar(ui, state);
                selection = show_interactive_canvas(
                    ui,
                    state,
                    canvas,
                    monitor.active_block.as_deref(),
                    selected_canvas.as_ref(),
                );
            });
            ui.add_space(8.0);
            egui::CollapsingHeader::new("Library drawer")
                .default_open(false)
                .show(ui, |ui| library_pane(ui, state, &filtered_rows));
            egui::CollapsingHeader::new("Inspector drawer")
                .default_open(false)
                .show(ui, |ui| {
                    if let Some(intent) = inspector::show(ui, &projection, editable) {
                        handle_inspector_intent(state, intent);
                    }
                });
        }
    }
    selection
}

fn library_pane(ui: &mut Ui, state: &mut MacroPageState, rows: &[MacroLibraryRow]) {
    ui.horizontal(|ui| {
        ui.label("Search");
        ui.text_edit_singleline(&mut state.library_search);
    });
    if ui
        .add_enabled(
            state.wizard.is_none() && state.active_wizard_request.is_none(),
            Button::new("New Macro"),
        )
        .clicked()
    {
        create_starter_draft(state);
    }
    if let Some(intent) = library::show(ui, rows, state.selected_saved_macro_id()) {
        state.enqueue_intent(intent);
    }
    ui.add_space(4.0);
    egui::CollapsingHeader::new("Manage selected macro")
        .default_open(false)
        .show(ui, |ui| {
            ui.label(RichText::new("Secondary actions").size(text::SUPPORTING));
            if state.selected_saved.is_some() {
                let mut enabled = state.enabled;
                if ui
                    .checkbox(&mut enabled, "Enabled")
                    .on_hover_text("Disabled macros remain saved but cannot be started.")
                    .changed()
                {
                    state.request_enabled(enabled);
                }
            }
            ui.text_edit_singleline(&mut state.library_rename)
                .on_hover_text("New name for Rename or Duplicate.");
            ui.horizontal_wrapped(|ui| {
                let has_selected = state.selected_saved.is_some();
                if ui
                    .add_enabled(
                        has_selected && !state.library_rename.trim().is_empty(),
                        Button::new("Rename"),
                    )
                    .clicked()
                {
                    let Some(saved) = state.selected_saved.clone() else {
                        return;
                    };
                    state.enqueue_intent(MacroIntent::Rename {
                        saved,
                        name: state.library_rename.trim().to_owned(),
                    });
                }
                if let Some(source) = state.selected_saved.as_ref().cloned() {
                    if ui
                        .add_enabled(
                            !state.library_rename.trim().is_empty(),
                            Button::new("Duplicate"),
                        )
                        .clicked()
                    {
                        state.enqueue_intent(MacroIntent::Duplicate {
                            macro_id: source.macro_id.clone(),
                            source,
                            name: state.library_rename.trim().to_owned(),
                        });
                    }
                }
            });
            ui.text_edit_singleline(&mut state.library_package_path)
                .on_hover_text("Package folder for Import or Export.");
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        !state.library_package_path.trim().is_empty(),
                        Button::new("Import"),
                    )
                    .clicked()
                {
                    state.enqueue_intent(MacroIntent::ImportPackage {
                        package_root: state.library_package_path.trim().to_owned(),
                    });
                }
                if state.selected_saved.is_some() {
                    if ui
                        .add_enabled(
                            !state.library_package_path.trim().is_empty(),
                            Button::new("Export"),
                        )
                        .clicked()
                    {
                        state.enqueue_intent(MacroIntent::Export {
                            package_root: state.library_package_path.trim().to_owned(),
                        });
                    }
                    ui.checkbox(&mut state.confirm_library_delete, "Confirm delete");
                    if ui
                        .add_enabled(state.confirm_library_delete, Button::new("Delete selected"))
                        .clicked()
                    {
                        if let Some(saved) = state.selected_saved.clone() {
                            state.enqueue_intent(MacroIntent::Delete { saved });
                        }
                        state.confirm_library_delete = false;
                    }
                }
            });
        });
}

fn create_starter_draft(state: &mut MacroPageState) {
    state.begin_draft_session();
    state.draft = Some(EditorDraft::new(starter_macro_definition()));
    state.selected_block_id = Some("observe-1".into());
    state.selected_canvas = Some(CanvasSelection::Block("observe-1".into()));
    state.editor_feedback = Some("Created an unsaved starter draft for editor authoring.".into());
}

fn show_interactive_canvas(
    ui: &mut Ui,
    state: &mut MacroPageState,
    canvas: &CanvasProjection,
    active_block: Option<&str>,
    current_selection: Option<&CanvasSelection>,
) -> Option<CanvasSelection> {
    if canvas.nodes.is_empty() {
        ui.label("Create or select a macro to inspect its canonical blocks.");
        return None;
    }
    let shortcuts = !ui.ctx().wants_keyboard_input()
        && ui.input(|input| {
            (input.modifiers.command && input.key_pressed(egui::Key::Z))
                || (input.modifiers.command && input.key_pressed(egui::Key::Y))
                || input.key_pressed(egui::Key::F)
        });
    if shortcuts {
        let input = ui.input(|input| {
            (
                input.modifiers.command && input.key_pressed(egui::Key::Z),
                input.modifiers.command && input.key_pressed(egui::Key::Y),
                input.key_pressed(egui::Key::F),
            )
        });
        if input.0 {
            let _ = dispatch_editor_command(state, EditorCommand::Undo);
        }
        if input.1 {
            let _ = dispatch_editor_command(state, EditorCommand::Redo);
        }
        if input.2 {
            state.fit_canvas_view([ui.available_width().max(1.0), canvas::CANVAS_HEIGHT]);
        }
    }
    let editable = state.editor_mutations_allowed();
    let response = canvas::show(
        ui,
        canvas,
        &mut state.canvas_layout,
        current_selection,
        active_block,
        state.draft.as_ref(),
        editable,
    );
    apply_canvas_response(state, response)
}

fn apply_canvas_response(
    state: &mut MacroPageState,
    response: canvas::CanvasResponse,
) -> Option<CanvasSelection> {
    if response.layout_changed {
        state.mark_canvas_layout_changed();
    }
    if let Some(edit) = response.layout_edit {
        state.record_canvas_layout_edit(edit);
    }
    if let Some(command) = response.editor_command {
        let _ = dispatch_editor_command(state, command);
    }
    match response.action {
        Some(canvas::CanvasAction::RejectedConnection(message)) => {
            state.editor_feedback = Some(format!("Connection rejected: {message}"));
        }
        Some(canvas::CanvasAction::OpenAddStep { source, .. }) => {
            state.pending_canvas_port = Some(source);
            state.editor_feedback =
                Some("Choose a block below to add it at the connector drop location.".into());
        }
        _ => {}
    }
    response.selection
}

fn dispatch_editor_command(
    state: &mut MacroPageState,
    command: EditorCommand,
) -> Result<EditOutcome, EditorError> {
    if !state.editor_mutations_allowed() {
        state.editor_feedback =
            Some("Edit rejected: RunInProgress (finish pending authoring first).".into());
        return Err(EditorError::RunInProgress);
    }
    if command == EditorCommand::Undo {
        let result =
            state
                .undo_ui_edit()
                .map(|_| EditOutcome::Changed)
                .map_err(|error| match error {
                    HistoryError::Definition(error) => error,
                    HistoryError::NothingToUndo => EditorError::NothingToUndo,
                    HistoryError::NothingToRedo => EditorError::NothingToUndo,
                    HistoryError::Layout => EditorError::NothingToUndo,
                });
        state.editor_feedback = Some(match &result {
            Ok(_) => "Undid latest editor change.".into(),
            Err(error) => format!("Edit rejected: {error:?}"),
        });
        return result;
    }
    if command == EditorCommand::Redo {
        let result =
            state
                .redo_ui_edit()
                .map(|_| EditOutcome::Changed)
                .map_err(|error| match error {
                    HistoryError::Definition(error) => error,
                    HistoryError::NothingToUndo => EditorError::NothingToRedo,
                    HistoryError::NothingToRedo => EditorError::NothingToRedo,
                    HistoryError::Layout => EditorError::NothingToRedo,
                });
        state.editor_feedback = Some(match &result {
            Ok(_) => "Redid latest editor change.".into(),
            Err(error) => format!("Edit rejected: {error:?}"),
        });
        return result;
    }
    let clears_image_samples = matches!(&command, EditorCommand::ReplaceImageRule { .. });
    let result = state
        .draft
        .as_mut()
        .ok_or_else(|| EditorError::MissingBlock("no draft".into()))
        .and_then(|draft| apply_editor_command(draft, command));
    if clears_image_samples && matches!(result, Ok(EditOutcome::Changed)) {
        state.editor_image_negative_samples.clear();
    }
    if matches!(result, Ok(EditOutcome::Changed)) {
        let editor_undo_len = state
            .draft
            .as_ref()
            .map(EditorDraft::undo_len)
            .unwrap_or_default();
        state.ui_edit_history.record_definition(editor_undo_len);
        state.reconcile_canvas_layout();
    }
    state.editor_feedback = Some(match &result {
        Ok(EditOutcome::Changed) => "Draft updated; validation required.".into(),
        Ok(EditOutcome::Validated) => "Draft validated.".into(),
        Ok(EditOutcome::NoChange) => "No change.".into(),
        Err(error) => format!("Edit rejected: {error:?}"),
    });
    result
}

fn handle_inspector_intent(state: &mut MacroPageState, intent: inspector::InspectorIntent) {
    if state.wizard.is_some() {
        state.editor_feedback = Some("Close the guided wizard before editing this draft.".into());
        return;
    }
    match &intent {
        inspector::InspectorIntent::TestOcr { block_id } => {
            begin_editor_authoring(
                state,
                EditorAuthoringKind::TestOcr {
                    block_id: block_id.clone(),
                },
            );
            state.pending_inspector_intent = Some(intent);
            return;
        }
        inspector::InspectorIntent::TestImage { block_id } => {
            begin_editor_authoring(
                state,
                EditorAuthoringKind::TestImage {
                    block_id: block_id.clone(),
                },
            );
            state.pending_inspector_intent = Some(intent);
            return;
        }
        inspector::InspectorIntent::RecaptureRegion { region_id } => {
            begin_editor_authoring(
                state,
                EditorAuthoringKind::RecaptureRegion {
                    region_id: region_id.clone(),
                },
            );
            state.pending_inspector_intent = Some(intent);
            return;
        }
        inspector::InspectorIntent::RecaptureTemplate { rule_id } => {
            begin_editor_authoring(
                state,
                EditorAuthoringKind::RecaptureTemplate {
                    rule_id: rule_id.clone(),
                },
            );
            state.pending_inspector_intent = Some(intent);
            return;
        }
        inspector::InspectorIntent::CaptureImageNegative { block_id } => {
            begin_editor_authoring(
                state,
                EditorAuthoringKind::CaptureImageNegative {
                    block_id: block_id.clone(),
                },
            );
            state.pending_inspector_intent = Some(intent);
            return;
        }
        _ => {}
    }
    match inspector_editor_command(state.draft.as_ref(), &intent) {
        Ok(Some(command)) => {
            let _ = dispatch_editor_command(state, command);
        }
        Ok(None) => state.pending_inspector_intent = Some(intent),
        Err(message) => state.editor_feedback = Some(message),
    }
}

fn inspector_editor_command(
    draft: Option<&EditorDraft>,
    intent: &inspector::InspectorIntent,
) -> Result<Option<EditorCommand>, String> {
    use inspector::InspectorIntent;
    let path = |block_id: &str| {
        draft
            .and_then(|draft| locate_block_path(draft, block_id))
            .ok_or_else(|| format!("Selected block '{block_id}' is no longer available."))
    };
    let command = match intent {
        InspectorIntent::TestOcr { .. }
        | InspectorIntent::TestImage { .. }
        | InspectorIntent::RecaptureRegion { .. }
        | InspectorIntent::RecaptureTemplate { .. }
        | InspectorIntent::CaptureImageNegative { .. } => return Ok(None),
        InspectorIntent::InvalidEdit { message } => return Err(message.clone()),
        InspectorIntent::ReplaceTextRule { rule } => {
            EditorCommand::ReplaceTextRule { rule: rule.clone() }
        }
        InspectorIntent::ReplaceImageRule { rule } => {
            EditorCommand::ReplaceImageRule { rule: rule.clone() }
        }
        InspectorIntent::SetConditionMode { block_id, mode } => EditorCommand::SetConditionMode {
            path: path(block_id)?,
            mode: mode.clone(),
        },
        InspectorIntent::SetRepeatUntilMax { block_id, max } => EditorCommand::SetRepeatUntilMax {
            path: path(block_id)?,
            max_iterations: max.clone(),
        },
        InspectorIntent::SetWaitDuration {
            block_id,
            duration_ms,
        } => EditorCommand::SetWaitDuration {
            path: path(block_id)?,
            duration_ms: *duration_ms,
        },
        InspectorIntent::SetRepeatCount { block_id, count } => EditorCommand::SetRepeatCount {
            path: path(block_id)?,
            count: *count,
        },
        InspectorIntent::SetWatchSettings {
            block_id,
            timeout_ms,
            cooldown_ms,
        } => EditorCommand::SetWatchSettings {
            path: path(block_id)?,
            timeout_ms: timeout_ms.clone(),
            cooldown_ms: *cooldown_ms,
        },
    };
    Ok(Some(command))
}

fn editor_toolbar(ui: &mut Ui, state: &mut MacroPageState) {
    let editable = state.editor_mutations_allowed();
    let selected = state.selected_block_id.clone();
    let selected_canvas = current_canvas_selection(state);
    let pending_canvas_port = state.pending_canvas_port.clone();
    if let Some(feedback) = &state.editor_feedback {
        ui.label(
            RichText::new(feedback)
                .size(text::SUPPORTING)
                .color(Color32::from_rgb(196, 154, 106)),
        );
    }
    if let Some(intent) = state.pending_inspector_intent.clone() {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!("Pending observation intent: {intent:?}"))
                    .size(text::SUPPORTING)
                    .color(Color32::from_gray(150)),
            );
            if ui.small_button("Clear").clicked() {
                state.pending_inspector_intent = None;
            }
        });
    }
    ui.horizontal_wrapped(|ui| {
        if ui.add_enabled(editable, Button::new("Undo")).clicked() {
            let _ = dispatch_editor_command(state, EditorCommand::Undo);
        }
        if ui.add_enabled(editable, Button::new("Redo")).clicked() {
            let _ = dispatch_editor_command(state, EditorCommand::Redo);
        }
        if ui
            .add_enabled(editable, Button::new("Auto arrange"))
            .clicked()
        {
            state.auto_arrange_canvas();
        }
        if ui.button("Fit view").clicked() {
            state.fit_canvas_view([ui.available_width().max(1.0), 600.0]);
        }
        if ui.button("Reset zoom").clicked() {
            state.reset_canvas_zoom();
        }
        if ui.add_enabled(editable, Button::new("+ Note")).clicked() {
            if let Some(draft) = &state.draft {
                let block = crate::engine::macro_engine::Block {
                    id: next_unique_id(draft, "note"),
                    enabled: true,
                    kind: crate::engine::macro_engine::BlockKind::Comment {
                        text: "New step".into(),
                    },
                };
                let target = if let Some(port) = pending_canvas_port.as_ref() {
                    match insertion_target_for_port(&draft.definition, port) {
                        Ok(target) => target,
                        Err(error) => {
                            state.editor_feedback =
                                Some(format!("Connection rejected: {}", error.message()));
                            return;
                        }
                    }
                } else {
                    InsertionTarget {
                        container: ContainerPath::Root,
                        index: draft.blocks.len(),
                    }
                };
                if dispatch_editor_command(state, EditorCommand::InsertBlock { target, block })
                    .is_ok()
                {
                    state.pending_canvas_port = None;
                }
            }
        }
    });
    ui.horizontal_wrapped(|ui| {
        for (label, kind) in [
            ("+ Observe", PaletteKind::Observe),
            ("+ Action", PaletteKind::Action),
            ("+ IF", PaletteKind::If),
            ("+ Repeat", PaletteKind::Repeat),
            ("+ Watch", PaletteKind::Watch),
            ("+ Wait", PaletteKind::Wait),
            ("+ Stop", PaletteKind::Stop),
        ] {
            if ui.add_enabled(editable, Button::new(label)).clicked() {
                let command = state
                    .draft
                    .as_ref()
                    .ok_or_else(|| "No draft is open.".to_string())
                    .and_then(|draft| {
                        let command =
                            palette_command_for_selection(draft, selected_canvas.as_ref(), kind)?;
                        if let Some(port) = pending_canvas_port.as_ref() {
                            retarget_insert_command(draft, port, command)
                        } else {
                            Ok(command)
                        }
                    });
                match command {
                    Ok(command) => {
                        if dispatch_editor_command(state, command).is_ok() {
                            state.pending_canvas_port = None;
                        }
                    }
                    Err(message) => state.editor_feedback = Some(message),
                }
            }
        }
    });
    let Some(selected) = selected else { return };
    if let Some((group_id, index, len, enabled)) = state
        .draft
        .as_ref()
        .and_then(|draft| locate_watch_lane(draft, &selected))
    {
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(editable && index > 0, Button::new("Lane up"))
                .clicked()
            {
                let _ = dispatch_editor_command(
                    state,
                    EditorCommand::MoveLane {
                        group_id: group_id.clone(),
                        lane_id: selected.clone(),
                        to_index: index - 1,
                    },
                );
            }
            if ui
                .add_enabled(editable && index + 1 < len, Button::new("Lane down"))
                .clicked()
            {
                let _ = dispatch_editor_command(
                    state,
                    EditorCommand::MoveLane {
                        group_id: group_id.clone(),
                        lane_id: selected.clone(),
                        to_index: index + 1,
                    },
                );
            }
            if ui
                .add_enabled(
                    editable,
                    Button::new(if enabled {
                        "Disable lane"
                    } else {
                        "Enable lane"
                    }),
                )
                .clicked()
            {
                let _ = dispatch_editor_command(
                    state,
                    EditorCommand::SetLaneEnabled {
                        group_id,
                        lane_id: selected.clone(),
                        enabled: !enabled,
                    },
                );
            }
        });
        return;
    }
    let Some(path) = state
        .draft
        .as_ref()
        .and_then(|draft| locate_block_path(draft, &selected))
    else {
        return;
    };
    let Some((index, len)) = state
        .draft
        .as_ref()
        .and_then(|draft| sibling_position(draft, &path))
    else {
        return;
    };
    let block = state
        .draft
        .as_ref()
        .and_then(|draft| block_at_path(draft, &path))
        .cloned();
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(editable && index > 0, Button::new("Up"))
            .clicked()
        {
            let _ = dispatch_editor_command(
                state,
                EditorCommand::ReorderSibling {
                    path: path.clone(),
                    to_index: index - 1,
                },
            );
        }
        if ui
            .add_enabled(editable && index + 1 < len, Button::new("Down"))
            .clicked()
        {
            let _ = dispatch_editor_command(
                state,
                EditorCommand::ReorderSibling {
                    path: path.clone(),
                    to_index: index + 1,
                },
            );
        }
        if let Some(block) = &block {
            if ui
                .add_enabled(
                    editable,
                    Button::new(if block.enabled { "Disable" } else { "Enable" }),
                )
                .clicked()
            {
                let _ = dispatch_editor_command(
                    state,
                    EditorCommand::SetBlockEnabled {
                        path: path.clone(),
                        enabled: !block.enabled,
                    },
                );
            }
        }
        if ui.add_enabled(editable, Button::new("Duplicate")).clicked() {
            let _ = dispatch_editor_command(
                state,
                EditorCommand::DuplicateBlock {
                    source: path.clone(),
                    target: InsertionTarget {
                        container: path.container.clone(),
                        index: index + 1,
                    },
                },
            );
        }
        if !matches!(path.container, ContainerPath::Root)
            && ui
                .add_enabled(editable, Button::new("Move to root"))
                .clicked()
        {
            let root = state
                .draft
                .as_ref()
                .map(|draft| draft.blocks.len())
                .unwrap_or(0);
            let _ = dispatch_editor_command(
                state,
                EditorCommand::MoveBlock {
                    source: path.clone(),
                    target: InsertionTarget {
                        container: ContainerPath::Root,
                        index: root,
                    },
                },
            );
        }
    });
    if let Some(block) = block {
        ui.horizontal_wrapped(|ui| match block.kind {
            crate::engine::macro_engine::BlockKind::RepeatN { .. }
            | crate::engine::macro_engine::BlockKind::RepeatUntil { .. }
            | crate::engine::macro_engine::BlockKind::Continuous { .. } => {
                if ui
                    .add_enabled(editable, Button::new("Delete loop + contents"))
                    .clicked()
                {
                    let _ = dispatch_editor_command(
                        state,
                        EditorCommand::RemoveBlock {
                            path: path.clone(),
                            loop_choice: Some(LoopDeletionChoice::DeleteWithContents),
                        },
                    );
                }
                if ui
                    .add_enabled(editable, Button::new("Keep contents"))
                    .clicked()
                {
                    let _ = dispatch_editor_command(
                        state,
                        EditorCommand::RemoveBlock {
                            path: path.clone(),
                            loop_choice: Some(LoopDeletionChoice::KeepContents),
                        },
                    );
                }
            }
            _ => {
                if ui.add_enabled(editable, Button::new("Delete")).clicked() {
                    let _ = dispatch_editor_command(
                        state,
                        EditorCommand::RemoveBlock {
                            path: path.clone(),
                            loop_choice: None,
                        },
                    );
                }
            }
        });
        if let ContainerPath::IfThen { ref if_id } | ContainerPath::IfElse { ref if_id } =
            path.container
        {
            let branch = if matches!(path.container, ContainerPath::IfThen { .. }) {
                IfBranch::Then
            } else {
                IfBranch::Else
            };
            if ui
                .add_enabled(editable, Button::new("Transfer THEN / ELSE"))
                .clicked()
            {
                let _ = dispatch_editor_command(
                    state,
                    EditorCommand::TransferIfBranch {
                        if_id: if_id.clone(),
                        branch,
                        block_id: path.block_id.clone(),
                        to_index: 0,
                    },
                );
            }
        }
        if state
            .pending_conversion
            .as_ref()
            .is_some_and(|pending| pending.block_id == block.id)
        {
            let mut pending = state.pending_conversion.take().expect("checked pending");
            conversion_preview_ui(ui, &mut pending);
            let valid = state
                .draft
                .as_ref()
                .is_some_and(|draft| pending_conversion_valid(draft, &pending));
            let mut apply = false;
            let mut cancel = false;
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(editable && valid, Button::new("Apply conversion"))
                    .clicked()
                {
                    apply = true;
                }
                if ui.small_button("Cancel conversion").clicked() {
                    cancel = true;
                }
            });
            if !valid {
                ui.label(
                    RichText::new("Complete valid required values before applying.")
                        .color(Color32::from_rgb(224, 112, 75)),
                );
            }
            state.pending_conversion = Some(pending);
            if apply {
                let _ = confirm_pending_conversion(state);
            } else if cancel {
                cancel_pending_conversion(state);
            }
        } else {
            let choices = state
                .draft
                .as_ref()
                .map(|draft| conversion_choices(draft, &block, &path))
                .unwrap_or_default();
            ui.horizontal_wrapped(|ui| {
                for (label, preview) in choices {
                    if ui.add_enabled(editable, Button::new(label)).clicked() {
                        state.pending_conversion = Some(preview);
                    }
                }
            });
            let replacements = state
                .draft
                .as_ref()
                .map(|draft| replacement_choices(draft, &block, &path))
                .unwrap_or_default();
            ui.horizontal_wrapped(|ui| {
                for (label, preview) in replacements {
                    if ui.add_enabled(editable, Button::new(label)).clicked() {
                        state.pending_conversion = Some(preview);
                    }
                }
            });
        }
    }
}

fn retarget_insert_command(
    draft: &EditorDraft,
    port: &canvas_model::OutputPort,
    command: EditorCommand,
) -> Result<EditorCommand, String> {
    let EditorCommand::InsertBlock { block, .. } = command else {
        return Err("Canvas Add step can only insert a new canonical block.".into());
    };
    let target = insertion_target_for_port(&draft.definition, port)
        .map_err(|error| format!("Connection rejected: {}", error.message()))?;
    Ok(EditorCommand::InsertBlock { target, block })
}

fn conversion_preview_ui(ui: &mut Ui, pending: &mut PendingConversion) {
    let (preserved, required, removed, reason) = match &pending.preview {
        ConversionPreview::Compatible {
            preserved_fields,
            required_fields,
            removed_fields,
        } => (
            preserved_fields.join(", "),
            required_fields.join(", "),
            removed_fields.join(", "),
            None,
        ),
        ConversionPreview::ReplaceRequired { reason, .. } => {
            (String::new(), String::new(), String::new(), Some(*reason))
        }
    };
    if let Some(reason) = reason {
        ui.label(RichText::new(reason).color(Color32::from_rgb(224, 112, 75)));
    }
    if !preserved.is_empty() {
        ui.label(format!("Preserved: {preserved}"));
    }
    if !required.is_empty() {
        ui.label(format!("Required: {required}"));
    }
    if !removed.is_empty() {
        ui.label(format!("Removed (undo only): {removed}"));
    }
    for (field, value) in &pending.required_values {
        ui.label(format!("{field}: {value}"));
    }
    match &mut pending.command {
        EditorCommand::ConvertBlock {
            target: ConversionTarget::RepeatN { count },
            ..
        } => {
            ui.horizontal(|ui| {
                ui.label("Count");
                ui.add(egui::DragValue::new(count).clamp_range(0..=u32::MAX));
            });
            pending.required_values = vec![("count".into(), count.to_string())];
        }
        EditorCommand::ConvertBlock {
            target:
                ConversionTarget::RepeatUntil {
                    max_iterations: crate::engine::macro_engine::Limit::Finite(maximum),
                    ..
                },
            ..
        } => {
            ui.horizontal(|ui| {
                ui.label("Max iterations");
                ui.add(egui::DragValue::new(maximum).clamp_range(0..=u64::MAX));
            });
            pending.required_values = vec![("max_iterations".into(), maximum.to_string())];
        }
        EditorCommand::ReplaceBlock { replacement, .. } => match &mut replacement.kind {
            crate::engine::macro_engine::BlockKind::RepeatN { count, .. } => {
                ui.horizontal(|ui| {
                    ui.label("Count");
                    ui.add(egui::DragValue::new(count).clamp_range(0..=u32::MAX));
                });
                pending.required_values = vec![("count".into(), count.to_string())];
            }
            crate::engine::macro_engine::BlockKind::Wait { duration_ms } => {
                ui.horizontal(|ui| {
                    ui.label("Duration ms");
                    ui.add(egui::DragValue::new(duration_ms).clamp_range(0..=u64::MAX));
                });
                pending.required_values = vec![("duration_ms".into(), duration_ms.to_string())];
            }
            crate::engine::macro_engine::BlockKind::Comment { text } => {
                ui.horizontal(|ui| {
                    ui.label("Text");
                    ui.text_edit_singleline(text);
                });
                pending.required_values = vec![("text".into(), text.clone())];
            }
            _ => {}
        },
        _ => {}
    }
    if pending.structural_children {
        let disposition = match &pending.command {
            EditorCommand::ReplaceBlock {
                children: ChildDisposition::KeepOwnedContents,
                ..
            } => "Keep",
            EditorCommand::ReplaceBlock {
                children: ChildDisposition::DeleteOwnedContents,
                ..
            } => "Delete",
            _ => "Not applicable",
        };
        ui.label(format!("Selected child disposition: {disposition}"));
        ui.horizontal_wrapped(|ui| {
            ui.label("Owned children");
            if ui.button("Keep").clicked() {
                if let EditorCommand::ReplaceBlock { children, .. } = &mut pending.command {
                    *children = ChildDisposition::KeepOwnedContents;
                }
            }
            if ui.button("Delete").clicked() {
                if let EditorCommand::ReplaceBlock { children, .. } = &mut pending.command {
                    *children = ChildDisposition::DeleteOwnedContents;
                }
            }
        });
    }
}

fn pending_conversion_valid(draft: &EditorDraft, pending: &PendingConversion) -> bool {
    use crate::engine::macro_engine::{BlockKind, Limit};
    let required_values_valid = match &pending.command {
        EditorCommand::ConvertBlock { target, .. } => match target {
            ConversionTarget::ClickPoint { point_id, .. } => {
                draft.points.iter().any(|point| point.id == *point_id)
            }
            ConversionTarget::ClickRegion { region_id, .. } => {
                draft.regions.iter().any(|region| region.id == *region_id)
            }
            ConversionTarget::RepeatN { count } => *count > 0,
            ConversionTarget::RepeatUntil {
                condition,
                max_iterations,
            } => {
                let max_valid = !matches!(max_iterations, Limit::Finite(0));
                let source_id = match condition {
                    crate::engine::macro_engine::Condition::Text {
                        source_block_id, ..
                    }
                    | crate::engine::macro_engine::Condition::Image {
                        source_block_id, ..
                    } => source_block_id,
                };
                max_valid
                    && locate_block_path(draft, source_id)
                        .and_then(|path| block_at_path(draft, &path))
                        .is_some_and(|block| matches!(block.kind, BlockKind::Observe { .. }))
            }
            _ => true,
        },
        EditorCommand::ReplaceBlock { replacement, .. } => match &replacement.kind {
            BlockKind::Wait { duration_ms } => *duration_ms > 0,
            BlockKind::Comment { text } => !text.trim().is_empty(),
            BlockKind::RepeatN { count, .. } => *count > 0,
            BlockKind::Observe {
                condition: crate::engine::macro_engine::Condition::Text { rule_id, .. },
            } => draft.text_rules.iter().any(|rule| rule.id == *rule_id),
            BlockKind::Observe {
                condition: crate::engine::macro_engine::Condition::Image { rule_id, .. },
            } => draft.image_rules.iter().any(|rule| rule.id == *rule_id),
            BlockKind::Action {
                action:
                    crate::engine::macro_engine::Action::ClickTextMatch {
                        source_block_id, ..
                    },
            } => locate_block_path(draft, source_block_id)
                .and_then(|path| block_at_path(draft, &path))
                .is_some_and(|block| {
                    matches!(
                        block.kind,
                        BlockKind::Observe {
                            condition: crate::engine::macro_engine::Condition::Text { .. }
                        }
                    )
                }),
            BlockKind::Action {
                action:
                    crate::engine::macro_engine::Action::ClickImageMatch {
                        source_block_id, ..
                    },
            } => locate_block_path(draft, source_block_id)
                .and_then(|path| block_at_path(draft, &path))
                .is_some_and(|block| {
                    matches!(
                        block.kind,
                        BlockKind::Observe {
                            condition: crate::engine::macro_engine::Condition::Image { .. }
                        }
                    )
                }),
            _ => true,
        },
        _ => true,
    };
    if !required_values_valid {
        return false;
    }
    let mut candidate = draft.clone();
    apply_editor_command(&mut candidate, pending.command.clone()).is_ok()
}

fn confirm_pending_conversion(state: &mut MacroPageState) -> Result<EditOutcome, EditorError> {
    let pending = state
        .pending_conversion
        .take()
        .ok_or_else(|| EditorError::MissingBlock("no conversion preview".into()))?;
    if !state
        .draft
        .as_ref()
        .is_some_and(|draft| pending_conversion_valid(draft, &pending))
    {
        state.editor_feedback = Some("Conversion has invalid required values.".into());
        state.pending_conversion = Some(pending);
        return Err(EditorError::IncompatibleConversion);
    }
    dispatch_editor_command(state, pending.command)
}

fn cancel_pending_conversion(state: &mut MacroPageState) {
    state.pending_conversion = None;
    state.editor_feedback = Some("Conversion canceled; draft unchanged.".into());
}

fn compatible_conversion_preview(
    block: &crate::engine::macro_engine::Block,
    path: BlockPath,
    target: ConversionTarget,
) -> PendingConversion {
    let family = conversion_target_family(&target);
    PendingConversion {
        block_id: block.id.clone(),
        preview: preview_conversion(block, family),
        required_values: conversion_required_values(&target),
        command: EditorCommand::ConvertBlock { path, target },
        structural_children: false,
    }
}

fn conversion_choices(
    draft: &EditorDraft,
    block: &crate::engine::macro_engine::Block,
    path: &BlockPath,
) -> Vec<(String, PendingConversion)> {
    use crate::engine::macro_engine::{
        Action, BlockKind, Condition, Limit, MouseButton, ObserveMode, TimeoutOutcome,
    };
    let wait_true = |current: &ObserveMode| {
        if matches!(current, ObserveMode::WaitForTrue { .. }) {
            current.clone()
        } else {
            editor::transition_observe_mode(
                current,
                ObserveMode::WaitForTrue {
                    timeout_ms: Limit::Unlimited,
                    timeout_outcome: TimeoutOutcome::Continue,
                },
            )
        }
    };
    let wait_false = |current: &ObserveMode| {
        if matches!(current, ObserveMode::WaitForFalse { .. }) {
            current.clone()
        } else {
            editor::transition_observe_mode(
                current,
                ObserveMode::WaitForFalse {
                    timeout_ms: Limit::Unlimited,
                    timeout_outcome: TimeoutOutcome::Continue,
                },
            )
        }
    };
    let mut targets = Vec::new();
    match &block.kind {
        BlockKind::Observe {
            condition: Condition::Text { mode, .. },
        } => targets.extend([
            (
                "Text: Check now".into(),
                ConversionTarget::TextObservation {
                    mode: ObserveMode::CheckNow,
                },
            ),
            (
                "Text: Wait true".into(),
                ConversionTarget::TextObservation {
                    mode: wait_true(mode),
                },
            ),
            (
                "Text: Wait false".into(),
                ConversionTarget::TextObservation {
                    mode: wait_false(mode),
                },
            ),
        ]),
        BlockKind::Observe {
            condition: Condition::Image { mode, .. },
        } => targets.extend([
            (
                "Image: Check now".into(),
                ConversionTarget::ImageObservation {
                    mode: ObserveMode::CheckNow,
                },
            ),
            (
                "Image: Wait true".into(),
                ConversionTarget::ImageObservation {
                    mode: wait_true(mode),
                },
            ),
            (
                "Image: Wait false".into(),
                ConversionTarget::ImageObservation {
                    mode: wait_false(mode),
                },
            ),
        ]),
        BlockKind::Action {
            action: Action::ClickTextMatch { .. },
        } => targets.extend([
            (
                "Text click: Left".into(),
                ConversionTarget::ClickTextMatch {
                    button: MouseButton::Left,
                },
            ),
            (
                "Text click: Right".into(),
                ConversionTarget::ClickTextMatch {
                    button: MouseButton::Right,
                },
            ),
        ]),
        BlockKind::Action {
            action: Action::ClickImageMatch { .. },
        } => targets.extend([
            (
                "Image click: Left".into(),
                ConversionTarget::ClickImageMatch {
                    button: MouseButton::Left,
                },
            ),
            (
                "Image click: Right".into(),
                ConversionTarget::ClickImageMatch {
                    button: MouseButton::Right,
                },
            ),
        ]),
        BlockKind::Action {
            action: Action::ClickPoint { .. } | Action::ClickRegion { .. },
        } => {
            for button in [MouseButton::Left, MouseButton::Right] {
                targets.extend(draft.points.iter().map(|point| {
                    (
                        format!("Point: {} ({button:?})", point.id),
                        ConversionTarget::ClickPoint {
                            point_id: point.id.clone(),
                            button,
                        },
                    )
                }));
                targets.extend(draft.regions.iter().map(|region| {
                    (
                        format!("Region: {} ({button:?})", region.id),
                        ConversionTarget::ClickRegion {
                            region_id: region.id.clone(),
                            button,
                        },
                    )
                }));
            }
        }
        BlockKind::RepeatN { .. } => {
            for (source_id, condition) in observation_sources(draft) {
                targets.push((
                    format!("Repeat Until: {source_id}"),
                    ConversionTarget::RepeatUntil {
                        condition: condition_with_source(condition, source_id),
                        max_iterations: Limit::Finite(100),
                    },
                ));
            }
        }
        BlockKind::RepeatUntil { .. } => {
            targets.push(("Repeat N".into(), ConversionTarget::RepeatN { count: 2 }))
        }
        _ => {}
    }
    targets
        .into_iter()
        .filter(|(_, target)| conversion_target_changes(block, target))
        .map(|(label, target)| {
            (
                label,
                compatible_conversion_preview(block, path.clone(), target),
            )
        })
        .collect()
}

fn conversion_target_changes(
    block: &crate::engine::macro_engine::Block,
    target: &ConversionTarget,
) -> bool {
    use crate::engine::macro_engine::{Action, BlockKind, Condition};
    match (&block.kind, target) {
        (
            BlockKind::Observe {
                condition: Condition::Text { mode, .. },
            },
            ConversionTarget::TextObservation { mode: target },
        )
        | (
            BlockKind::Observe {
                condition: Condition::Image { mode, .. },
            },
            ConversionTarget::ImageObservation { mode: target },
        ) => mode != target,
        (
            BlockKind::Action {
                action: Action::ClickTextMatch { button, .. },
            },
            ConversionTarget::ClickTextMatch { button: target },
        )
        | (
            BlockKind::Action {
                action: Action::ClickImageMatch { button, .. },
            },
            ConversionTarget::ClickImageMatch { button: target },
        ) => button != target,
        (
            BlockKind::Action {
                action: Action::ClickPoint { point_id, button },
            },
            ConversionTarget::ClickPoint {
                point_id: target_id,
                button: target_button,
            },
        ) => point_id != target_id || button != target_button,
        (
            BlockKind::Action {
                action: Action::ClickRegion { region_id, button },
            },
            ConversionTarget::ClickRegion {
                region_id: target_id,
                button: target_button,
            },
        ) => region_id != target_id || button != target_button,
        _ => true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReplacementKind {
    DetectorText(String),
    DetectorImage(String),
    ActionText(String),
    ActionImage(String),
    Loop,
    Wait,
    Note,
    Stop,
}

fn replace_block_preview(
    block: &crate::engine::macro_engine::Block,
    path: BlockPath,
    replacement_kind: ReplacementKind,
) -> PendingConversion {
    use crate::engine::macro_engine::{
        Action, Block, BlockKind, Condition, MouseButton, ObserveMode,
    };
    let (kind, required_values) = match replacement_kind {
        ReplacementKind::DetectorText(rule_id) => (
            BlockKind::Observe {
                condition: Condition::Text {
                    source_block_id: block.id.clone(),
                    rule_id: rule_id.clone(),
                    mode: ObserveMode::CheckNow,
                },
            },
            vec![("text_rule".into(), rule_id)],
        ),
        ReplacementKind::DetectorImage(rule_id) => (
            BlockKind::Observe {
                condition: Condition::Image {
                    source_block_id: block.id.clone(),
                    rule_id: rule_id.clone(),
                    mode: ObserveMode::CheckNow,
                },
            },
            vec![("image_rule".into(), rule_id)],
        ),
        ReplacementKind::ActionText(source_id) => (
            BlockKind::Action {
                action: Action::ClickTextMatch {
                    source_block_id: source_id.clone(),
                    button: MouseButton::Left,
                },
            },
            vec![("text_source".into(), source_id)],
        ),
        ReplacementKind::ActionImage(source_id) => (
            BlockKind::Action {
                action: Action::ClickImageMatch {
                    source_block_id: source_id.clone(),
                    button: MouseButton::Left,
                },
            },
            vec![("image_source".into(), source_id)],
        ),
        ReplacementKind::Loop => (
            BlockKind::RepeatN {
                count: 2,
                body: vec![],
            },
            vec![("count".into(), "2".into())],
        ),
        ReplacementKind::Wait => (
            BlockKind::Wait { duration_ms: 250 },
            vec![("duration_ms".into(), "250".into())],
        ),
        ReplacementKind::Note => (
            BlockKind::Comment {
                text: "Replaced step".into(),
            },
            vec![("text".into(), "Replaced step".into())],
        ),
        ReplacementKind::Stop => (BlockKind::StopSuccess, vec![]),
    };
    PendingConversion {
        block_id: block.id.clone(),
        preview: ConversionPreview::ReplaceRequired {
            from: block_family_for_ui(block),
            to: BlockFamily::Other,
            reason: "Replacing an unrelated type requires confirmation; removed settings remain available only through undo.",
        },
        required_values,
        command: EditorCommand::ReplaceBlock {
            path,
            replacement: Block {
                id: block.id.clone(),
                enabled: block.enabled,
                kind,
            },
            children: ChildDisposition::KeepOwnedContents,
        },
        structural_children: has_replaceable_children(block),
    }
}

fn has_replaceable_children(block: &crate::engine::macro_engine::Block) -> bool {
    use crate::engine::macro_engine::{BlockKind, Condition, ObserveMode, TimeoutOutcome};
    match &block.kind {
        BlockKind::Observe { condition } => {
            let mode = match condition {
                Condition::Text { mode, .. } | Condition::Image { mode, .. } => mode,
            };
            matches!(
                mode,
                ObserveMode::WaitForTrue {
                    timeout_outcome: TimeoutOutcome::RunBody { .. },
                    ..
                } | ObserveMode::WaitForFalse {
                    timeout_outcome: TimeoutOutcome::RunBody { .. },
                    ..
                }
            )
        }
        BlockKind::If { .. }
        | BlockKind::RepeatN { .. }
        | BlockKind::RepeatUntil { .. }
        | BlockKind::Continuous { .. }
        | BlockKind::WatchGroup { .. } => true,
        _ => false,
    }
}

fn replacement_choices(
    draft: &EditorDraft,
    block: &crate::engine::macro_engine::Block,
    path: &BlockPath,
) -> Vec<(String, PendingConversion)> {
    let mut kinds = Vec::new();
    kinds.extend(draft.text_rules.iter().map(|rule| {
        (
            format!("Detector: Text {}", rule.id),
            ReplacementKind::DetectorText(rule.id.clone()),
        )
    }));
    kinds.extend(draft.image_rules.iter().map(|rule| {
        (
            format!("Detector: Image {}", rule.id),
            ReplacementKind::DetectorImage(rule.id.clone()),
        )
    }));
    kinds.extend(
        matched_observation_sources(draft)
            .into_iter()
            .map(|(id, condition)| match condition {
                crate::engine::macro_engine::Condition::Text { .. } => (
                    format!("Action: Text source {id}"),
                    ReplacementKind::ActionText(id),
                ),
                crate::engine::macro_engine::Condition::Image { .. } => (
                    format!("Action: Image source {id}"),
                    ReplacementKind::ActionImage(id),
                ),
            }),
    );
    kinds.extend(
        [
            ("Replace as Loop", ReplacementKind::Loop),
            ("Replace as Wait", ReplacementKind::Wait),
            ("Replace as Note", ReplacementKind::Note),
            ("Replace as Stop", ReplacementKind::Stop),
        ]
        .into_iter()
        .map(|(label, kind)| (label.into(), kind)),
    );
    kinds
        .into_iter()
        .map(|(label, kind)| (label, replace_block_preview(block, path.clone(), kind)))
        .collect()
}

fn conversion_target_family(target: &ConversionTarget) -> BlockFamily {
    match target {
        ConversionTarget::TextObservation { .. } => BlockFamily::TextObservation,
        ConversionTarget::ImageObservation { .. } => BlockFamily::ImageObservation,
        ConversionTarget::ClickTextMatch { .. } => BlockFamily::TextMatchedClick,
        ConversionTarget::ClickImageMatch { .. } => BlockFamily::ImageMatchedClick,
        ConversionTarget::ClickPoint { .. } | ConversionTarget::ClickRegion { .. } => {
            BlockFamily::SavedLocationClick
        }
        ConversionTarget::RepeatN { .. } | ConversionTarget::RepeatUntil { .. } => {
            BlockFamily::Loop
        }
    }
}

fn conversion_required_values(target: &ConversionTarget) -> Vec<(String, String)> {
    match target {
        ConversionTarget::ClickPoint { point_id, .. } => {
            vec![("point_id".into(), point_id.clone())]
        }
        ConversionTarget::ClickRegion { region_id, .. } => {
            vec![("region_id".into(), region_id.clone())]
        }
        ConversionTarget::RepeatN { count } => vec![("count".into(), count.to_string())],
        ConversionTarget::RepeatUntil { max_iterations, .. } => {
            vec![("max_iterations".into(), format!("{max_iterations:?}"))]
        }
        _ => vec![],
    }
}

fn block_family_for_ui(block: &crate::engine::macro_engine::Block) -> BlockFamily {
    use crate::engine::macro_engine::{Action, BlockKind, Condition};
    match &block.kind {
        BlockKind::Observe {
            condition: Condition::Text { .. },
        } => BlockFamily::TextObservation,
        BlockKind::Observe {
            condition: Condition::Image { .. },
        } => BlockFamily::ImageObservation,
        BlockKind::Action {
            action: Action::ClickTextMatch { .. },
        } => BlockFamily::TextMatchedClick,
        BlockKind::Action {
            action: Action::ClickImageMatch { .. },
        } => BlockFamily::ImageMatchedClick,
        BlockKind::Action {
            action: Action::ClickPoint { .. } | Action::ClickRegion { .. },
        } => BlockFamily::SavedLocationClick,
        BlockKind::RepeatN { .. } | BlockKind::RepeatUntil { .. } => BlockFamily::Loop,
        _ => BlockFamily::Other,
    }
}

fn next_unique_id(draft: &EditorDraft, prefix: &str) -> String {
    for ordinal in 1_u64.. {
        let id = format!("{prefix}-{ordinal}");
        if locate_block_path(draft, &id).is_none() && locate_watch_lane(draft, &id).is_none() {
            return id;
        }
    }
    unreachable!()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaletteKind {
    Observe,
    Action,
    If,
    Repeat,
    Watch,
    Wait,
    Stop,
}

#[cfg(test)]
fn palette_command(
    draft: &EditorDraft,
    selected_id: Option<&str>,
    kind: PaletteKind,
) -> Result<EditorCommand, String> {
    let selection = selected_id.map(|id| CanvasSelection::Block(id.to_string()));
    palette_command_for_selection(draft, selection.as_ref(), kind)
}

fn palette_command_for_selection(
    draft: &EditorDraft,
    selected: Option<&CanvasSelection>,
    kind: PaletteKind,
) -> Result<EditorCommand, String> {
    use crate::engine::macro_engine::{
        Action, Block, BlockKind, Condition, Limit, MouseButton, ObserveMode, PassiveCondition,
        TimeoutOutcome, WatchGroup, WatchLane,
    };

    let target = insertion_target(draft, selected);
    let selected_id = selected.and_then(|selection| match selection {
        CanvasSelection::Block(id) => Some(id.as_str()),
        CanvasSelection::Lane { lane_id, .. } => Some(lane_id.as_str()),
        CanvasSelection::TimeoutBody { .. } => None,
    });
    let id = next_unique_id(
        draft,
        match kind {
            PaletteKind::Observe => "observe",
            PaletteKind::Action => "action",
            PaletteKind::If => "if",
            PaletteKind::Repeat => "repeat",
            PaletteKind::Watch => "watch",
            PaletteKind::Wait => "wait",
            PaletteKind::Stop => "stop",
        },
    );
    let source = observation_source(draft, selected_id);
    let matched_source = matched_observation_source(draft, selected_id);
    let kind = match kind {
        PaletteKind::Observe => {
            if let Some(rule) = draft.text_rules.first() {
                BlockKind::Observe {
                    condition: Condition::Text {
                        source_block_id: id.clone(),
                        rule_id: rule.id.clone(),
                        mode: ObserveMode::CheckNow,
                    },
                }
            } else if let Some(rule) = draft.image_rules.first() {
                BlockKind::Observe {
                    condition: Condition::Image {
                        source_block_id: id.clone(),
                        rule_id: rule.id.clone(),
                        mode: ObserveMode::CheckNow,
                    },
                }
            } else {
                return Err("Add a text or image rule before inserting an observation.".into());
            }
        }
        PaletteKind::Action => match matched_source {
            Some((source_id, Condition::Text { .. })) => BlockKind::Action {
                action: Action::ClickTextMatch {
                    source_block_id: source_id,
                    button: MouseButton::Left,
                },
            },
            Some((source_id, Condition::Image { .. })) => BlockKind::Action {
                action: Action::ClickImageMatch {
                    source_block_id: source_id,
                    button: MouseButton::Left,
                },
            },
            None => return Err("Add or select an observation before inserting an action.".into()),
        },
        PaletteKind::If => {
            let Some((source_id, condition)) = source else {
                return Err("Add or select an observation before inserting an IF.".into());
            };
            BlockKind::If {
                condition: condition_with_source(condition, source_id),
                then_body: vec![],
                else_body: vec![],
            }
        }
        PaletteKind::Repeat => BlockKind::RepeatN {
            count: 2,
            body: vec![],
        },
        PaletteKind::Watch => {
            let Some((source_id, condition)) = source else {
                return Err("Add or select an observation before inserting a Watch Group.".into());
            };
            let lane_id = next_unique_id(draft, "lane");
            let passive = match condition {
                Condition::Text { rule_id, .. } => PassiveCondition::Text {
                    source_block_id: source_id,
                    rule_id,
                },
                Condition::Image { rule_id, .. } => PassiveCondition::Image {
                    source_block_id: source_id,
                    rule_id,
                },
            };
            BlockKind::WatchGroup {
                group: WatchGroup {
                    lanes: vec![WatchLane {
                        id: lane_id,
                        enabled: true,
                        condition: passive,
                        then_body: vec![],
                    }],
                    timeout_ms: Limit::Unlimited,
                    timeout_outcome: TimeoutOutcome::Continue,
                    cooldown_ms: 250,
                },
            }
        }
        PaletteKind::Wait => BlockKind::Wait { duration_ms: 250 },
        PaletteKind::Stop => BlockKind::StopSuccess,
    };
    Ok(EditorCommand::InsertBlock {
        target,
        block: Block {
            id,
            enabled: true,
            kind,
        },
    })
}

fn insertion_target(draft: &EditorDraft, selected: Option<&CanvasSelection>) -> InsertionTarget {
    let Some(selected) = selected else {
        return InsertionTarget {
            container: ContainerPath::Root,
            index: draft.blocks.len(),
        };
    };
    if let CanvasSelection::TimeoutBody { owner_id } = selected {
        let container = ContainerPath::TimeoutBody {
            owner_id: owner_id.clone(),
        };
        if let Some(index) = container_len(draft, &container) {
            return InsertionTarget { container, index };
        }
    }
    if let CanvasSelection::Lane { group_id, lane_id } = selected {
        let container = ContainerPath::WatchLaneBody {
            watch_id: group_id.clone(),
            lane_id: lane_id.clone(),
        };
        let index = container_len(draft, &container).unwrap_or(0);
        return InsertionTarget { container, index };
    }
    let CanvasSelection::Block(selected_id) = selected else {
        return InsertionTarget {
            container: ContainerPath::Root,
            index: draft.blocks.len(),
        };
    };
    let Some(path) = locate_block_path(draft, selected_id) else {
        return InsertionTarget {
            container: ContainerPath::Root,
            index: draft.blocks.len(),
        };
    };
    let child_container =
        block_at_path(draft, &path).and_then(|block| match &block.kind {
            crate::engine::macro_engine::BlockKind::If { .. } => Some(ContainerPath::IfThen {
                if_id: block.id.clone(),
            }),
            crate::engine::macro_engine::BlockKind::RepeatN { .. }
            | crate::engine::macro_engine::BlockKind::RepeatUntil { .. }
            | crate::engine::macro_engine::BlockKind::Continuous { .. } => {
                Some(ContainerPath::LoopBody {
                    loop_id: block.id.clone(),
                })
            }
            crate::engine::macro_engine::BlockKind::WatchGroup { group } => group
                .lanes
                .first()
                .map(|lane| ContainerPath::WatchLaneBody {
                    watch_id: block.id.clone(),
                    lane_id: lane.id.clone(),
                }),
            _ => None,
        });
    if let Some(container) = child_container {
        let index = container_len(draft, &container).unwrap_or(0);
        InsertionTarget { container, index }
    } else {
        let index = sibling_position(draft, &path)
            .map(|(index, _)| index + 1)
            .unwrap_or(0);
        InsertionTarget {
            container: path.container,
            index,
        }
    }
}

fn observation_source(
    draft: &EditorDraft,
    selected_id: Option<&str>,
) -> Option<(String, crate::engine::macro_engine::Condition)> {
    use crate::engine::macro_engine::BlockKind;
    let selected = selected_id
        .and_then(|id| locate_block_path(draft, id))
        .and_then(|path| block_at_path(draft, &path));
    if let Some(block) = selected {
        if let BlockKind::Observe { condition } = &block.kind {
            return Some((block.id.clone(), condition.clone()));
        }
    }
    observation_sources(draft).into_iter().next()
}

fn observation_sources(
    draft: &EditorDraft,
) -> Vec<(String, crate::engine::macro_engine::Condition)> {
    use crate::engine::macro_engine::BlockKind;
    let mut out = Vec::new();
    for_each_canonical_block(&draft.blocks, &mut |block| {
        if let BlockKind::Observe { condition } = &block.kind {
            out.push((block.id.clone(), condition.clone()));
        }
    });
    out
}

fn matched_observation_source(
    draft: &EditorDraft,
    selected_id: Option<&str>,
) -> Option<(String, crate::engine::macro_engine::Condition)> {
    let sources = matched_observation_sources(draft);
    selected_id
        .and_then(|selected| sources.iter().find(|(id, _)| id == selected).cloned())
        .or_else(|| sources.into_iter().next())
}

fn matched_observation_sources(
    draft: &EditorDraft,
) -> Vec<(String, crate::engine::macro_engine::Condition)> {
    observation_sources(draft)
        .into_iter()
        .filter(|(_, condition)| observation_has_match_geometry(draft, condition))
        .collect()
}

fn observation_has_match_geometry(
    draft: &EditorDraft,
    condition: &crate::engine::macro_engine::Condition,
) -> bool {
    use crate::engine::macro_engine::{Condition, TextMatchMode};
    match condition {
        Condition::Image { .. } => true,
        Condition::Text { rule_id, .. } => draft
            .text_rules
            .iter()
            .find(|rule| rule.id == *rule_id)
            .is_some_and(|rule| rule.match_mode != TextMatchMode::Absent),
    }
}

fn for_each_canonical_block<'a>(
    blocks: &'a [crate::engine::macro_engine::Block],
    visit: &mut impl FnMut(&'a crate::engine::macro_engine::Block),
) {
    for block in blocks {
        visit(block);
        for children in canonical_child_slices(block) {
            for_each_canonical_block(children, visit);
        }
    }
}

fn canonical_child_slices(
    block: &crate::engine::macro_engine::Block,
) -> Vec<&[crate::engine::macro_engine::Block]> {
    use crate::engine::macro_engine::{BlockKind, TimeoutOutcome};
    let mut children: Vec<&[crate::engine::macro_engine::Block]> = match &block.kind {
        BlockKind::If {
            then_body,
            else_body,
            ..
        } => vec![then_body.as_slice(), else_body.as_slice()],
        BlockKind::RepeatN { body, .. }
        | BlockKind::RepeatUntil { body, .. }
        | BlockKind::Continuous { body } => vec![body.as_slice()],
        BlockKind::WatchGroup { group } => group
            .lanes
            .iter()
            .map(|lane| lane.then_body.as_slice())
            .collect(),
        _ => vec![],
    };
    match &block.kind {
        BlockKind::Observe { condition }
        | BlockKind::If { condition, .. }
        | BlockKind::RepeatUntil { condition, .. } => {
            if let Some(body) = condition_timeout_slice(condition) {
                children.push(body);
            }
        }
        BlockKind::WatchGroup { group } => {
            if let TimeoutOutcome::RunBody { body } = &group.timeout_outcome {
                children.push(body);
            }
        }
        _ => {}
    }
    children
}

fn condition_timeout_slice(
    condition: &crate::engine::macro_engine::Condition,
) -> Option<&[crate::engine::macro_engine::Block]> {
    use crate::engine::macro_engine::{Condition, ObserveMode, TimeoutOutcome};
    let mode = match condition {
        Condition::Text { mode, .. } | Condition::Image { mode, .. } => mode,
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

fn condition_with_source(
    condition: crate::engine::macro_engine::Condition,
    source_block_id: String,
) -> crate::engine::macro_engine::Condition {
    use crate::engine::macro_engine::{Condition, ObserveMode};
    match condition {
        Condition::Text { rule_id, .. } => Condition::Text {
            source_block_id,
            rule_id,
            mode: ObserveMode::CheckNow,
        },
        Condition::Image { rule_id, .. } => Condition::Image {
            source_block_id,
            rule_id,
            mode: ObserveMode::CheckNow,
        },
    }
}

fn section(ui: &mut Ui, label: &str, contents: impl FnOnce(&mut Ui)) {
    Frame::none()
        .fill(Color32::from_rgb(17, 19, 21))
        .stroke(Stroke::new(1.0, Color32::from_rgb(43, 47, 50)))
        .rounding(6.0)
        .inner_margin(egui::Margin::same(10.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(label)
                    .monospace()
                    .size(text::SECTION_TITLE)
                    .strong()
                    .color(Color32::from_rgb(186, 143, 96)),
            );
            ui.add_space(7.0);
            contents(ui);
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::macro_engine::*;

    fn fixture() -> EditorDraft {
        EditorDraft::new(MacroDefinition {
            schema_version: MACRO_SCHEMA_VERSION,
            id: "macro".into(),
            name: "Macro".into(),
            revision: 1,
            target: TargetProfile {
                process_path: "game.exe".into(),
                window_class: "d4".into(),
                title_contains: "Diablo".into(),
                captured_client_width: 1280,
                captured_client_height: 720,
                captured_dpi: 96,
            },
            regions: vec![],
            points: vec![],
            text_rules: vec![TextRule {
                id: "rule".into(),
                revision: 1,
                region_id: "scan".into(),
                language: "en-US".into(),
                preprocess: PreprocessProfile::Grayscale,
                expected: "Salvage".into(),
                match_mode: TextMatchMode::Contains,
                threshold: 0.9,
                case_sensitive: false,
                allow_cross_line: false,
                match_policy: MatchSelectionPolicy::ExactlyOne,
                poll_interval_ms: 250,
                timeout_ms: Limit::Unlimited,
                stable_frames: 2,
            }],
            image_rules: vec![],
            blocks: vec![Block {
                id: "observe-1".into(),
                enabled: true,
                kind: BlockKind::Observe {
                    condition: Condition::Text {
                        source_block_id: "observe-1".into(),
                        rule_id: "rule".into(),
                        mode: ObserveMode::CheckNow,
                    },
                },
            }],
            safety: SafetyPolicy {
                max_runtime_ms: Limit::Unlimited,
                max_clicks: Limit::Unlimited,
                max_observation_retries: Limit::Unlimited,
                max_observations_per_second: 20,
                minimum_click_interval_ms: 100,
                focus_loss: FocusLossPolicy::Stop,
            },
        })
    }

    fn saved_identity() -> SavedMacroIdentity {
        SavedMacroIdentity {
            macro_id: "macro".into(),
            revision: 1,
            definition_hash: "saved-hash".into(),
        }
    }

    #[test]
    fn empty_macro_page_has_no_runtime_or_input_authority() {
        let state = MacroPageState::default();
        assert!(state.draft.is_none());
        assert!(state.runtime_events.is_empty());
        assert!(state.enabled);
    }

    #[test]
    fn nine_hundred_pixel_window_keeps_three_compact_panes() {
        assert_eq!(pane_mode(900.0), PaneMode::ThreePaneCompact);
    }

    #[test]
    fn narrow_window_keeps_canvas_and_uses_drawers() {
        assert_eq!(pane_mode(719.0), PaneMode::CanvasWithDrawers);
    }

    #[test]
    fn dry_run_is_never_reported_as_live_run() {
        let mut state = MacroPageState::default();
        state.set_selected_saved(SavedMacroIdentity {
            macro_id: "macro".into(),
            revision: 1,
            definition_hash: "hash".into(),
        });
        state.runtime_events.push(RunEvent::RunStarted {
            sequence: 1,
            elapsed_ms: 0,
            run_id: "dry-run".into(),
            macro_id: "macro".into(),
            revision: 1,
            definition_hash: "hash".into(),
            mode: RunMode::DryRun,
        });

        let controls = run_control_availability(&state);

        assert_eq!(controls.primary_label, "Dry Run");
        assert_eq!(controls.primary_detail, "Observe only");
    }

    #[test]
    fn ready_saved_macro_keeps_the_continuous_observation_run_available() {
        let mut state = MacroPageState::default();
        state.set_selected_saved(SavedMacroIdentity {
            macro_id: "macro".into(),
            revision: 1,
            definition_hash: "hash".into(),
        });

        assert!(run_control_availability(&state).can_run_continuous);
    }

    #[test]
    fn bottom_panel_has_a_resizable_height_range_for_compact_layouts() {
        assert!(MacroPage::BOTTOM_MIN_HEIGHT < MacroPage::BOTTOM_DEFAULT_HEIGHT);
        assert!(MacroPage::BOTTOM_DEFAULT_HEIGHT < MacroPage::BOTTOM_MAX_HEIGHT);
    }

    #[test]
    fn selected_macro_enablement_is_ui_state_not_a_default_assumption() {
        let mut state = MacroPageState::default();
        state.set_enabled(false);
        assert!(!state.enabled);
    }

    #[test]
    fn disabled_saved_macro_cannot_start_until_reenabled() {
        let mut state = MacroPageState::default();
        state.set_selected_saved(SavedMacroIdentity {
            macro_id: "macro".into(),
            revision: 1,
            definition_hash: "hash".into(),
        });
        state.set_enabled(false);

        let controls = run_control_availability(&state);
        assert!(!controls.can_run_continuous);
        assert_eq!(
            controls.disabled_reason.as_deref(),
            Some("Enable this saved macro before running it.")
        );
    }

    #[test]
    fn selected_library_row_uses_the_live_validation_projection() {
        let mut state = MacroPageState {
            draft: Some(fixture()),
            library_rows: vec![MacroLibraryRow {
                id: "macro".into(),
                name: "Macro".into(),
                revision: 1,
                status: library::MacroLibraryStatus::Ready,
                target: "Diablo".into(),
                dpi: 96,
                last_validation: "Valid".into(),
                last_run: "No completed run".into(),
            }],
            ..MacroPageState::default()
        };
        state.set_selected_saved(SavedMacroIdentity {
            macro_id: "macro".into(),
            revision: 1,
            definition_hash: "hash".into(),
        });
        let problems = vec![ValidationProblem {
            code: "test.invalid".into(),
            message: "Needs validation".into(),
            block_id: None,
        }];

        let rows = project_library_rows(&state, &MonitorProjection::default(), &problems, None);

        assert_eq!(
            rows[0].status,
            library::MacroLibraryStatus::NeedsRevalidation
        );
        assert_eq!(rows[0].last_validation, "1 issues");
    }

    #[test]
    fn unbound_draft_is_not_appended_to_the_saved_library() {
        let state = MacroPageState {
            draft: Some(fixture()),
            library_rows: vec![],
            ..MacroPageState::default()
        };

        let rows = project_library_rows(&state, &MonitorProjection::default(), &[], None);

        assert!(rows.is_empty());
    }

    #[test]
    fn enablement_request_leaves_displayed_state_unchanged_until_native_persistence_succeeds() {
        let mut state = MacroPageState::default();
        let saved = SavedMacroIdentity {
            macro_id: "macro".into(),
            revision: 1,
            definition_hash: "hash".into(),
        };
        state.set_selected_saved(saved.clone());
        state.set_enabled(true);

        state.request_enabled(false);

        assert!(state.enabled);
        assert_eq!(
            state.take_intent(),
            Some(MacroIntent::SetEnabled {
                saved,
                enabled: false
            })
        );
    }

    #[test]
    fn enablement_completion_does_not_update_a_newly_selected_macro() {
        let first = SavedMacroIdentity {
            macro_id: "first".into(),
            revision: 1,
            definition_hash: "first-hash".into(),
        };
        let second = SavedMacroIdentity {
            macro_id: "second".into(),
            revision: 1,
            definition_hash: "second-hash".into(),
        };
        let mut state = MacroPageState::default();
        state.set_selected_saved(second);
        state.set_enabled(true);

        state.apply_enabled_if_selected(&first, false);

        assert!(state.enabled);
    }

    #[test]
    fn run_request_keeps_the_saved_identity_captured_at_click_time() {
        let first = SavedMacroIdentity {
            macro_id: "first".into(),
            revision: 1,
            definition_hash: "first-hash".into(),
        };
        let second = SavedMacroIdentity {
            macro_id: "second".into(),
            revision: 2,
            definition_hash: "second-hash".into(),
        };
        let mut state = MacroPageState::default();
        state.set_selected_saved(first.clone());
        state.request_run(MacroRunIntent::ContinuousObservation);
        state.set_selected_saved(second);

        assert_eq!(state.take_intent(), Some(MacroIntent::Run { saved: first }));
    }

    #[test]
    fn structural_edit_reconciles_canvas_layout_without_recording_layout_history() {
        let mut state = MacroPageState {
            draft: Some(fixture()),
            ..MacroPageState::default()
        };
        state
            .canvas_layout
            .node_positions
            .insert("stale".into(), [1.0, 2.0]);
        let block = Block {
            id: "added".into(),
            enabled: true,
            kind: BlockKind::Comment {
                text: "Added".into(),
            },
        };

        assert_eq!(
            dispatch_editor_command(
                &mut state,
                EditorCommand::InsertBlock {
                    target: InsertionTarget {
                        container: ContainerPath::Root,
                        index: 1,
                    },
                    block,
                },
            ),
            Ok(EditOutcome::Changed)
        );

        assert!(!state.canvas_layout.node_positions.contains_key("stale"));
        assert!(state.canvas_layout.node_positions.contains_key("added"));
        assert_eq!(state.ui_edit_history.undo_len(), 1);
    }

    #[test]
    fn saved_identity_is_stable_when_the_editable_draft_changes() {
        let mut state = MacroPageState {
            draft: Some(fixture()),
            ..MacroPageState::default()
        };
        state.set_selected_saved(saved_identity());

        state.draft.as_mut().unwrap().definition.name = "Unsaved edit".into();
        state.request_run(MacroRunIntent::ContinuousObservation);

        assert_eq!(
            state.selected_saved.as_ref().unwrap().definition_hash,
            "saved-hash"
        );
        assert_eq!(
            state.take_intent(),
            Some(MacroIntent::Run {
                saved: saved_identity()
            })
        );
        assert_eq!(state.selected_saved_macro_id(), Some("macro"));
    }

    #[test]
    fn bounded_intents_evict_noncritical_work_but_retain_stop() {
        let mut intents = MacroIntentQueue::with_capacity(2);
        intents.push(MacroIntent::Select {
            macro_id: "one".into(),
        });
        intents.push(MacroIntent::Run {
            saved: saved_identity(),
        });
        intents.push(MacroIntent::Stop);

        assert_eq!(intents.len(), 2);
        assert_eq!(
            intents.pop(),
            Some(MacroIntent::Run {
                saved: saved_identity()
            })
        );
        assert_eq!(intents.pop(), Some(MacroIntent::Stop));
    }

    #[test]
    fn intent_queue_exposes_its_64_item_default_capacity_and_drains_in_order() {
        let mut intents = MacroIntentQueue::with_capacity(MacroIntentQueue::DEFAULT_CAPACITY);
        for index in 0..MacroIntentQueue::DEFAULT_CAPACITY {
            intents.push(MacroIntent::Select {
                macro_id: format!("macro-{index}"),
            });
        }
        intents.push(MacroIntent::Run {
            saved: saved_identity(),
        });

        assert_eq!(MacroIntentQueue::DEFAULT_CAPACITY, 64);
        assert_eq!(intents.pending(), 64);
        assert_eq!(
            intents.drain().collect::<Vec<_>>(),
            (0..64)
                .map(|index| MacroIntent::Select {
                    macro_id: format!("macro-{index}"),
                })
                .collect::<Vec<_>>()
        );
        assert_eq!(intents.pending(), 0);
    }

    #[test]
    fn revision_summary_distinguishes_draft_saved_and_running() {
        let mut monitor = MonitorProjection::default();
        monitor.running_revision = Some(3);
        let state = MacroPageState {
            saved_revision: Some(2),
            ..MacroPageState::default()
        };

        assert_eq!(
            revision_summary(&state, &monitor),
            "draft -- | saved 2 | running 3"
        );
    }

    #[test]
    fn palette_covers_core_types_and_inserts_nested_containers() {
        let mut draft = fixture();
        assert_eq!(next_unique_id(&draft, "observe"), "observe-2");
        for kind in [
            PaletteKind::Observe,
            PaletteKind::Action,
            PaletteKind::If,
            PaletteKind::Repeat,
            PaletteKind::Watch,
            PaletteKind::Wait,
            PaletteKind::Stop,
        ] {
            assert!(palette_command(&draft, Some("observe-1"), kind).is_ok());
        }

        let command = palette_command(&draft, Some("observe-1"), PaletteKind::If).unwrap();
        let EditorCommand::InsertBlock { block, .. } = &command else {
            panic!()
        };
        let if_id = block.id.clone();
        apply_editor_command(&mut draft, command).unwrap();
        let command = palette_command(&draft, Some(&if_id), PaletteKind::Repeat).unwrap();
        let EditorCommand::InsertBlock { target, block } = &command else {
            panic!()
        };
        assert_eq!(
            target.container,
            ContainerPath::IfThen {
                if_id: if_id.clone()
            }
        );
        let repeat_id = block.id.clone();
        apply_editor_command(&mut draft, command).unwrap();
        let EditorCommand::InsertBlock { target, .. } =
            palette_command(&draft, Some(&repeat_id), PaletteKind::Wait).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            target.container,
            ContainerPath::LoopBody { loop_id: repeat_id }
        );

        let BlockKind::Observe { condition } = &mut draft.blocks[0].kind else {
            panic!()
        };
        let Condition::Text { mode, .. } = condition else {
            panic!()
        };
        *mode = ObserveMode::WaitForTrue {
            timeout_ms: Limit::Finite(100),
            timeout_outcome: TimeoutOutcome::RunBody { body: vec![] },
        };
        let timeout = CanvasSelection::TimeoutBody {
            owner_id: "observe-1".into(),
        };
        let EditorCommand::InsertBlock { target, .. } =
            palette_command_for_selection(&draft, Some(&timeout), PaletteKind::Wait).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            target.container,
            ContainerPath::TimeoutBody {
                owner_id: "observe-1".into()
            }
        );
    }

    #[test]
    fn real_block_id_with_timeout_suffix_is_not_treated_as_a_timeout_marker() {
        let mut draft = fixture();
        let BlockKind::Observe { condition } = &mut draft.blocks[0].kind else {
            panic!()
        };
        let Condition::Text { mode, .. } = condition else {
            panic!()
        };
        *mode = ObserveMode::WaitForTrue {
            timeout_ms: Limit::Finite(100),
            timeout_outcome: TimeoutOutcome::RunBody { body: vec![] },
        };
        draft.blocks.push(Block {
            id: "observe-1-timeout".into(),
            enabled: true,
            kind: BlockKind::Comment {
                text: "real imported block".into(),
            },
        });

        let EditorCommand::InsertBlock { target, .. } =
            palette_command(&draft, Some("observe-1-timeout"), PaletteKind::Wait).unwrap()
        else {
            panic!()
        };
        assert_eq!(target.container, ContainerPath::Root);
        assert_eq!(target.index, draft.blocks.len());
    }

    #[test]
    fn inspector_intents_share_transactional_dispatch_and_surface_run_lock() {
        let mut state = MacroPageState {
            draft: Some(fixture()),
            ..MacroPageState::default()
        };
        let mut rule = state.draft.as_ref().unwrap().text_rules[0].clone();
        rule.expected = "Retry".into();
        let command = inspector_editor_command(
            state.draft.as_ref(),
            &inspector::InspectorIntent::ReplaceTextRule { rule },
        )
        .unwrap()
        .unwrap();
        dispatch_editor_command(&mut state, command).unwrap();
        assert_eq!(
            state.draft.as_ref().unwrap().text_rules[0].expected,
            "Retry"
        );

        state.draft.as_mut().unwrap().editability = DraftEditability::Running { revision: 2 };
        let path = locate_block_path(state.draft.as_ref().unwrap(), "observe-1").unwrap();
        let result = dispatch_editor_command(
            &mut state,
            EditorCommand::SetConditionMode {
                path,
                mode: ObserveMode::CheckNow,
            },
        );
        assert_eq!(result, Err(EditorError::RunInProgress));
        assert!(
            state
                .editor_feedback
                .as_deref()
                .unwrap()
                .contains("RunInProgress")
        );
    }

    #[test]
    fn inspector_dispatch_persists_required_text_and_image_rule_fields() {
        let mut state = MacroPageState {
            draft: Some(fixture()),
            ..MacroPageState::default()
        };
        let mut text = state.draft.as_ref().unwrap().text_rules[0].clone();
        text.match_mode = TextMatchMode::Absent;
        text.case_sensitive = true;
        text.allow_cross_line = true;
        text.preprocess = PreprocessProfile::HighContrast;
        text.match_policy = MatchSelectionPolicy::Topmost;
        text.timeout_ms = Limit::Finite(7_500);
        text.stable_frames = 4;
        let command = inspector_editor_command(
            state.draft.as_ref(),
            &inspector::InspectorIntent::ReplaceTextRule { rule: text },
        )
        .unwrap()
        .unwrap();
        dispatch_editor_command(&mut state, command).unwrap();
        let text = &state.draft.as_ref().unwrap().text_rules[0];
        assert_eq!(text.match_mode, TextMatchMode::Absent);
        assert!(text.case_sensitive && text.allow_cross_line);
        assert_eq!(text.preprocess, PreprocessProfile::HighContrast);
        assert_eq!(text.match_policy, MatchSelectionPolicy::Topmost);
        assert_eq!(text.timeout_ms, Limit::Finite(7_500));
        assert_eq!(text.stable_frames, 4);

        let template = |id: &str| AssetRef {
            id: id.into(),
            revision: 1,
            content_hash: format!("hash-{id}"),
        };
        state.draft.as_mut().unwrap().image_rules.push(ImageRule {
            id: "image-rule".into(),
            revision: 1,
            region_id: "scan".into(),
            template: template("old"),
            transparent_mask: None,
            threshold: 0.9,
            scales_percent: vec![100],
            stable_frames: 2,
            maximum_center_drift_px: 5,
            minimum_runner_up_margin: 0.05,
            verification: None,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 250,
            timeout_ms: Limit::Unlimited,
        });
        let mut image = state.draft.as_ref().unwrap().image_rules[0].clone();
        image.template = template("new");
        image.match_policy = MatchSelectionPolicy::Bottommost;
        image.timeout_ms = Limit::Finite(9_000);
        let command = inspector_editor_command(
            state.draft.as_ref(),
            &inspector::InspectorIntent::ReplaceImageRule { rule: image },
        )
        .unwrap()
        .unwrap();
        dispatch_editor_command(&mut state, command).unwrap();
        let image = &state.draft.as_ref().unwrap().image_rules[0];
        assert_eq!(image.template.id, "new");
        assert_eq!(image.match_policy, MatchSelectionPolicy::Bottommost);
        assert_eq!(image.timeout_ms, Limit::Finite(9_000));
        assert_eq!(image.verification, None);
    }

    #[test]
    fn conversion_previews_expose_replace_confirmation_and_required_values() {
        let draft = fixture();
        let block = &draft.blocks[0];
        let path = locate_block_path(&draft, &block.id).unwrap();
        let compatible = conversion_choices(&draft, block, &path)
            .into_iter()
            .next()
            .unwrap()
            .1;
        assert!(matches!(
            compatible.preview,
            ConversionPreview::Compatible { .. }
        ));

        let replacement = replace_block_preview(block, path, ReplacementKind::Wait);
        assert!(matches!(
            replacement.preview,
            ConversionPreview::ReplaceRequired { .. }
        ));
        assert_eq!(
            replacement.required_values,
            vec![("duration_ms".into(), "250".into())]
        );
    }

    #[test]
    fn observe_timeout_body_requires_explicit_replacement_disposition() {
        let mut draft = fixture();
        let BlockKind::Observe { condition } = &mut draft.blocks[0].kind else {
            panic!()
        };
        let Condition::Text { mode, .. } = condition else {
            panic!()
        };
        *mode = ObserveMode::WaitForTrue {
            timeout_ms: Limit::Finite(100),
            timeout_outcome: TimeoutOutcome::RunBody {
                body: vec![Block {
                    id: "fallback".into(),
                    enabled: true,
                    kind: BlockKind::Comment {
                        text: "keep".into(),
                    },
                }],
            },
        };
        let block = &draft.blocks[0];
        let path = locate_block_path(&draft, &block.id).unwrap();
        let pending = replace_block_preview(block, path, ReplacementKind::Wait);
        assert!(pending.structural_children);
    }

    #[test]
    fn conversion_choices_cover_saved_targets_buttons_and_replacement_families() {
        use crate::engine::types::{PointRatio, RectRatio};
        let mut draft = fixture();
        draft.points.push(PointDefinition {
            id: "point".into(),
            revision: 1,
            point: PointRatio { x: 0.5, y: 0.5 },
        });
        draft.regions.push(RegionDefinition {
            id: "region".into(),
            revision: 1,
            rect: RectRatio {
                x: 0.1,
                y: 0.1,
                width: 0.2,
                height: 0.2,
            },
        });
        draft.image_rules.push(ImageRule {
            id: "image-rule".into(),
            revision: 1,
            region_id: "region".into(),
            template: AssetRef {
                id: "template".into(),
                revision: 1,
                content_hash: "hash".into(),
            },
            transparent_mask: None,
            threshold: 0.9,
            scales_percent: vec![100],
            stable_frames: 2,
            maximum_center_drift_px: 5,
            minimum_runner_up_margin: 0.05,
            verification: None,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 250,
            timeout_ms: Limit::Unlimited,
        });
        draft.blocks.push(Block {
            id: "observe-image".into(),
            enabled: true,
            kind: BlockKind::Observe {
                condition: Condition::Image {
                    source_block_id: "observe-image".into(),
                    rule_id: "image-rule".into(),
                    mode: ObserveMode::CheckNow,
                },
            },
        });
        draft.blocks.push(Block {
            id: "saved-click".into(),
            enabled: true,
            kind: BlockKind::Action {
                action: Action::ClickPoint {
                    point_id: "point".into(),
                    button: MouseButton::Left,
                },
            },
        });
        let block = draft.blocks.last().unwrap();
        let path = locate_block_path(&draft, &block.id).unwrap();
        let labels = conversion_choices(&draft, block, &path)
            .into_iter()
            .map(|(label, _)| label)
            .collect::<Vec<_>>();
        assert!(!labels.contains(&"Point: point (Left)".into()));
        assert!(labels.contains(&"Point: point (Right)".into()));
        assert!(labels.contains(&"Region: region (Left)".into()));
        assert!(labels.contains(&"Region: region (Right)".into()));

        let replacements = replacement_choices(&draft, block, &path)
            .into_iter()
            .map(|(label, _)| label)
            .collect::<Vec<_>>();
        assert!(replacements.contains(&"Detector: Text rule".into()));
        assert!(replacements.contains(&"Detector: Image image-rule".into()));
        assert!(replacements.contains(&"Action: Text source observe-1".into()));
        assert!(replacements.contains(&"Action: Image source observe-image".into()));
        assert!(replacements.contains(&"Replace as Loop".into()));
    }

    #[test]
    fn observation_conversion_choices_preserve_configured_wait_outcomes() {
        let mut draft = fixture();
        let BlockKind::Observe { condition } = &mut draft.blocks[0].kind else {
            panic!()
        };
        let Condition::Text { mode, .. } = condition else {
            panic!()
        };
        *mode = ObserveMode::WaitForTrue {
            timeout_ms: Limit::Finite(812),
            timeout_outcome: TimeoutOutcome::RunBody {
                body: vec![Block {
                    id: "fallback".into(),
                    enabled: true,
                    kind: BlockKind::Comment {
                        text: "keep".into(),
                    },
                }],
            },
        };
        let block = &draft.blocks[0];
        let path = locate_block_path(&draft, &block.id).unwrap();
        let (_, pending) = conversion_choices(&draft, block, &path)
            .into_iter()
            .find(|(label, _)| label == "Text: Wait false")
            .unwrap();
        assert!(matches!(
            pending.command,
            EditorCommand::ConvertBlock {
                target: ConversionTarget::TextObservation {
                    mode: ObserveMode::WaitForFalse {
                        timeout_ms: Limit::Finite(812),
                        timeout_outcome: TimeoutOutcome::RunBody { ref body },
                    },
                },
                ..
            } if body[0].id == "fallback"
        ));
    }

    #[test]
    fn conversion_confirm_cancel_and_invalid_requirements_are_transactional() {
        let mut state = MacroPageState {
            draft: Some(fixture()),
            ..MacroPageState::default()
        };
        let before = state.draft.as_ref().unwrap().definition.clone();
        let block = state.draft.as_ref().unwrap().blocks[0].clone();
        let path = locate_block_path(state.draft.as_ref().unwrap(), &block.id).unwrap();
        state.pending_conversion = Some(replace_block_preview(
            &block,
            path.clone(),
            ReplacementKind::Wait,
        ));
        cancel_pending_conversion(&mut state);
        assert_eq!(state.draft.as_ref().unwrap().definition, before);
        assert!(state.pending_conversion.is_none());

        state.pending_conversion = Some(replace_block_preview(
            &block,
            path.clone(),
            ReplacementKind::Wait,
        ));
        confirm_pending_conversion(&mut state).unwrap();
        assert!(matches!(
            state.draft.as_ref().unwrap().blocks[0].kind,
            BlockKind::Wait { duration_ms: 250 }
        ));

        let mut state = MacroPageState {
            draft: Some(fixture()),
            ..MacroPageState::default()
        };
        state.pending_conversion = Some(PendingConversion {
            block_id: "observe-1".into(),
            preview: ConversionPreview::Compatible {
                preserved_fields: vec![],
                required_fields: vec!["count"],
                removed_fields: vec![],
            },
            required_values: vec![("count".into(), "0".into())],
            command: EditorCommand::ConvertBlock {
                path,
                target: ConversionTarget::RepeatN { count: 0 },
            },
            structural_children: false,
        });
        assert_eq!(
            confirm_pending_conversion(&mut state),
            Err(EditorError::IncompatibleConversion)
        );
        assert!(state.pending_conversion.is_some());
        assert_eq!(state.draft.as_ref().unwrap().definition, before);
    }

    #[test]
    fn replacement_preview_validates_sources_against_the_post_replacement_candidate() {
        let draft = fixture();
        let block = &draft.blocks[0];
        let path = locate_block_path(&draft, &block.id).unwrap();
        let pending =
            replace_block_preview(block, path, ReplacementKind::ActionText(block.id.clone()));
        assert!(!pending_conversion_valid(&draft, &pending));
    }

    #[test]
    fn observation_source_discovery_traverses_every_canonical_owned_container() {
        let observe = |id: &str| Block {
            id: id.into(),
            enabled: true,
            kind: BlockKind::Observe {
                condition: Condition::Text {
                    source_block_id: id.into(),
                    rule_id: "rule".into(),
                    mode: ObserveMode::CheckNow,
                },
            },
        };
        let timeout_condition = |owner: &str, child: &str| Condition::Text {
            source_block_id: owner.into(),
            rule_id: "rule".into(),
            mode: ObserveMode::WaitForTrue {
                timeout_ms: Limit::Finite(100),
                timeout_outcome: TimeoutOutcome::RunBody {
                    body: vec![observe(child)],
                },
            },
        };
        let mut draft = fixture();
        draft.blocks = vec![
            Block {
                id: "observe-owner".into(),
                enabled: true,
                kind: BlockKind::Observe {
                    condition: timeout_condition("observe-owner", "observe-timeout-source"),
                },
            },
            Block {
                id: "if-owner".into(),
                enabled: true,
                kind: BlockKind::If {
                    condition: timeout_condition("if-owner", "if-timeout-source"),
                    then_body: vec![observe("if-then-source")],
                    else_body: vec![observe("if-else-source")],
                },
            },
            Block {
                id: "repeat-n".into(),
                enabled: true,
                kind: BlockKind::RepeatN {
                    count: 2,
                    body: vec![observe("repeat-n-source")],
                },
            },
            Block {
                id: "repeat-until".into(),
                enabled: true,
                kind: BlockKind::RepeatUntil {
                    condition: timeout_condition("repeat-until", "repeat-timeout-source"),
                    max_iterations: Limit::Finite(2),
                    body: vec![observe("repeat-body-source")],
                },
            },
            Block {
                id: "continuous".into(),
                enabled: true,
                kind: BlockKind::Continuous {
                    body: vec![observe("continuous-source")],
                },
            },
            Block {
                id: "watch".into(),
                enabled: true,
                kind: BlockKind::WatchGroup {
                    group: WatchGroup {
                        lanes: vec![WatchLane {
                            id: "lane".into(),
                            enabled: true,
                            condition: PassiveCondition::Text {
                                source_block_id: "lane".into(),
                                rule_id: "rule".into(),
                            },
                            then_body: vec![observe("watch-lane-source")],
                        }],
                        timeout_ms: Limit::Finite(100),
                        timeout_outcome: TimeoutOutcome::RunBody {
                            body: vec![observe("watch-timeout-source")],
                        },
                        cooldown_ms: 0,
                    },
                },
            },
        ];

        let sources = observation_sources(&draft)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<std::collections::BTreeSet<_>>();
        for expected in [
            "observe-timeout-source",
            "if-then-source",
            "if-else-source",
            "if-timeout-source",
            "repeat-n-source",
            "repeat-body-source",
            "repeat-timeout-source",
            "continuous-source",
            "watch-lane-source",
            "watch-timeout-source",
        ] {
            assert!(sources.contains(expected), "missing {expected}");
        }

        let owner = &draft.blocks[0];
        let path = locate_block_path(&draft, &owner.id).unwrap();
        let pending = replacement_choices(&draft, owner, &path)
            .into_iter()
            .find(|(label, _)| label == "Action: Text source observe-timeout-source")
            .map(|(_, pending)| pending)
            .expect("timeout source replacement choice");
        assert!(pending_conversion_valid(&draft, &pending));
    }

    #[test]
    fn text_absent_observations_are_not_offered_as_matched_click_sources() {
        let mut draft = fixture();
        draft.text_rules[0].match_mode = TextMatchMode::Absent;

        assert!(palette_command(&draft, Some("observe-1"), PaletteKind::Action).is_err());
        let block = &draft.blocks[0];
        let path = locate_block_path(&draft, &block.id).unwrap();
        let labels = replacement_choices(&draft, block, &path)
            .into_iter()
            .map(|(label, _)| label)
            .collect::<Vec<_>>();
        assert!(!labels.contains(&"Action: Text source observe-1".into()));
    }

    #[test]
    fn starter_draft_opens_a_native_editor_surface() {
        let draft = EditorDraft::new(starter_macro_definition());
        assert_eq!(draft.blocks.len(), 1);
        assert!(locate_block_path(&draft, "observe-1").is_some());
        assert!(!draft.text_rules.is_empty());
    }

    #[test]
    fn guided_wizard_finish_opens_unsaved_canonical_editor_draft() {
        let mut wizard = wizard::WizardState::default();
        wizard.step = wizard::WizardStep::Finish;
        wizard.target_bound = true;
        wizard.target_generation = 1;
        wizard.region_capture_generation = Some(1);
        wizard.text_expected = "Ancestral".into();
        wizard.record_detector_test(true, "Ancestral", 9);
        wizard.mark_dry_run_reviewed();
        let output = wizard.finish().unwrap();
        let mut state = MacroPageState {
            wizard: Some(wizard),
            ..MacroPageState::default()
        };

        apply_wizard_ui_action(&mut state, wizard::WizardUiAction::Finish(output));

        assert!(state.wizard.is_none());
        assert_eq!(state.saved_revision, None);
        let draft = state.draft.as_ref().unwrap();
        assert_eq!(draft.name, "New Macro");
        assert!(locate_block_path(draft, "check-text").is_some());
    }

    #[test]
    fn wizard_rejects_stale_and_out_of_order_authoring_results() {
        let mut wizard = wizard::WizardState::default();
        wizard.target_bound = true;
        wizard.text_expected = "Ancestral".into();
        let mut state = MacroPageState {
            wizard: Some(wizard),
            ..MacroPageState::default()
        };
        apply_wizard_ui_action(
            &mut state,
            wizard::WizardUiAction::CaptureRegion(wizard::WizardDetectorKind::Text),
        );
        let current = state.take_wizard_request().unwrap();
        apply_wizard_ui_action(&mut state, wizard::WizardUiAction::CaptureClickPoint);
        assert!(state.take_wizard_request().is_none());

        let out_of_order = wizard::WizardAuthoringResult {
            session: current.session,
            id: wizard::WizardRequestId(current.id.0 + 1),
            fingerprint: current.fingerprint.clone(),
            outcome: wizard::WizardAuthoringOutcome::Region(crate::engine::types::RectRatio {
                x: 0.1,
                y: 0.1,
                width: 0.2,
                height: 0.2,
            }),
        };
        assert_eq!(
            state.apply_wizard_result(out_of_order),
            Err(wizard::WizardResultError::UnexpectedResult)
        );
        assert_eq!(state.active_wizard_request.as_ref().unwrap().id, current.id);

        state.wizard.as_mut().unwrap().text_expected = "Changed".into();
        let stale = wizard::WizardAuthoringResult {
            session: current.session,
            id: current.id,
            fingerprint: current.fingerprint,
            outcome: wizard::WizardAuthoringOutcome::Point(crate::engine::types::PointRatio {
                x: 0.4,
                y: 0.6,
            }),
        };
        assert_eq!(
            state.apply_wizard_result(stale),
            Err(wizard::WizardResultError::StaleWizard)
        );
    }

    #[test]
    fn wizard_session_rejects_results_after_close_reopen_and_finish_cancels_pending() {
        let mut state = MacroPageState::default();
        state.begin_wizard_session();
        state.wizard = Some(wizard::WizardState::default());
        apply_wizard_ui_action(&mut state, wizard::WizardUiAction::CaptureTarget);
        let old = state.take_wizard_request().unwrap();

        state.wizard = None;
        state.wizard_session = None;
        state.cancel_wizard_authoring();
        state.begin_wizard_session();
        state.wizard = Some(wizard::WizardState::default());
        assert_ne!(state.active_wizard_session(), Some(old.session));
        assert_eq!(
            state.apply_wizard_result(wizard::WizardAuthoringResult {
                session: old.session,
                id: old.id,
                fingerprint: old.fingerprint,
                outcome: wizard::WizardAuthoringOutcome::Cancelled,
            }),
            Err(wizard::WizardResultError::UnexpectedResult)
        );

        let mut completed = wizard::WizardState::default();
        completed.step = wizard::WizardStep::Finish;
        completed.target_bound = true;
        completed.target_generation = 1;
        completed.region_capture_generation = Some(1);
        completed.text_expected = "Ancestral".into();
        completed.record_detector_test(true, "matched", 1);
        completed.mark_dry_run_reviewed();
        let output = completed.finish().unwrap();
        state.wizard = Some(completed);
        apply_wizard_ui_action(&mut state, wizard::WizardUiAction::CaptureTarget);
        assert!(state.active_wizard_request.is_some());
        apply_wizard_ui_action(&mut state, wizard::WizardUiAction::Finish(output));
        assert!(state.active_wizard_request.is_none());
        assert!(state.wizard.is_none());
    }

    #[test]
    fn wizard_session_isolated_from_existing_draft_and_finish_transfers_ownership() {
        let original = EditorDraft::new(starter_macro_definition());
        let mut state = MacroPageState {
            draft: Some(original.clone()),
            ..MacroPageState::default()
        };
        state.begin_draft_session();
        let original_session = state.active_draft_session().unwrap();

        state.begin_wizard_session();
        state.wizard = Some(wizard::WizardState::default());
        let wizard_session = state.active_wizard_session().unwrap();
        assert_ne!(wizard_session, original_session);
        assert_eq!(state.active_draft_session(), Some(original_session));
        assert_eq!(state.draft.as_ref(), Some(&original));
        assert_eq!(
            dispatch_editor_command(&mut state, EditorCommand::MarkValidated),
            Err(EditorError::RunInProgress)
        );
        handle_inspector_intent(
            &mut state,
            inspector::InspectorIntent::TestOcr {
                block_id: "observe-1".into(),
            },
        );
        assert!(state.active_editor_authoring_request.is_none());

        state.wizard = None;
        state.wizard_session = None;
        state.cancel_wizard_authoring();
        assert_eq!(state.active_draft_session(), Some(original_session));
        assert_eq!(state.draft.as_ref(), Some(&original));

        state.begin_wizard_session();
        let mut completed = wizard::WizardState::default();
        completed.step = wizard::WizardStep::Finish;
        completed.target_bound = true;
        completed.target_generation = 1;
        completed.region_capture_generation = Some(1);
        completed.text_expected = "Ancestral".into();
        completed.record_detector_test(true, "matched", 1);
        completed.mark_dry_run_reviewed();
        let output = completed.finish().unwrap();
        state.wizard = Some(completed);
        let generated_session = state.active_wizard_session().unwrap();
        apply_wizard_ui_action(&mut state, wizard::WizardUiAction::Finish(output));
        assert_eq!(state.active_draft_session(), Some(generated_session));
        assert_ne!(state.active_draft_session(), Some(original_session));
    }

    #[test]
    fn request_envelopes_are_read_only_stale_safe_and_exact_discard_preserves_newer() {
        let mut state = MacroPageState::default();
        state.begin_wizard_session();
        state.wizard = Some(wizard::WizardState::default());
        apply_wizard_ui_action(&mut state, wizard::WizardUiAction::CaptureTarget);
        let old_wizard = state.take_wizard_request().unwrap();
        assert!(state.wizard_request_envelope_is_current(
            &old_wizard,
            wizard::WizardAuthoringKind::CaptureTarget
        ));
        state.wizard.as_mut().unwrap().text_expected = "edited".into();
        assert!(!state.wizard_request_envelope_is_current(
            &old_wizard,
            wizard::WizardAuthoringKind::CaptureTarget
        ));
        assert!(state.discard_wizard_result_envelope(&old_wizard));
        apply_wizard_ui_action(&mut state, wizard::WizardUiAction::CaptureTarget);
        let newer_wizard = state.active_wizard_request.clone().unwrap();
        assert!(!state.discard_wizard_result_envelope(&old_wizard));
        assert_eq!(state.active_wizard_request.as_ref(), Some(&newer_wizard));
        state.wizard = None;
        state.wizard_session = None;
        assert!(!state.wizard_request_envelope_is_current(
            &newer_wizard,
            wizard::WizardAuthoringKind::CaptureTarget
        ));

        let mut state = MacroPageState {
            draft: Some(EditorDraft::new(starter_macro_definition())),
            ..MacroPageState::default()
        };
        state.begin_draft_session();
        let kind = EditorAuthoringKind::TestOcr {
            block_id: "observe-1".into(),
        };
        begin_editor_authoring(&mut state, kind.clone());
        let old_editor = state.take_editor_authoring_request().unwrap();
        assert!(state.editor_request_envelope_is_current(&old_editor, &kind));
        apply_editor_command(
            state.draft.as_mut().unwrap(),
            EditorCommand::InsertBlock {
                target: InsertionTarget {
                    container: ContainerPath::Root,
                    index: 1,
                },
                block: Block {
                    id: "edited".into(),
                    enabled: true,
                    kind: BlockKind::Comment {
                        text: "stale request".into(),
                    },
                },
            },
        )
        .unwrap();
        assert!(!state.editor_request_envelope_is_current(&old_editor, &kind));
        assert!(state.discard_editor_result_envelope(&old_editor));
        begin_editor_authoring(&mut state, kind.clone());
        let newer_editor = state.active_editor_authoring_request.clone().unwrap();
        assert!(!state.discard_editor_result_envelope(&old_editor));
        assert_eq!(
            state.active_editor_authoring_request.as_ref(),
            Some(&newer_editor)
        );
    }

    #[test]
    fn accepted_target_capture_binds_durable_profile_and_invalidates_proofs() {
        let mut wizard = wizard::WizardState::default();
        wizard.text_expected = "Ancestral".into();
        wizard.target_bound = true;
        wizard.record_detector_test(true, "old", 1);
        wizard.mark_dry_run_reviewed();
        let mut state = MacroPageState {
            wizard: Some(wizard),
            ..MacroPageState::default()
        };
        apply_wizard_ui_action(&mut state, wizard::WizardUiAction::CaptureTarget);
        let request = state.take_wizard_request().unwrap();
        state
            .apply_wizard_result(wizard::WizardAuthoringResult {
                session: request.session,
                id: request.id,
                fingerprint: request.fingerprint,
                outcome: wizard::WizardAuthoringOutcome::TargetGeometry {
                    process_path: r#"C:\Games\Diablo IV.exe"#.into(),
                    window_class: "Diablo IV Main Window".into(),
                    title: "Diablo IV".into(),
                    width: 1920,
                    height: 1080,
                    dpi: 144,
                },
            })
            .unwrap();
        let wizard = state.wizard.as_ref().unwrap();
        assert_eq!(wizard.target.process_path, r#"C:\Games\Diablo IV.exe"#);
        assert_eq!(wizard.target.window_class, "Diablo IV Main Window");
        assert_eq!(wizard.target.captured_client_width, 1920);
        assert_eq!(wizard.target.captured_client_height, 1080);
        assert_eq!(wizard.target.captured_dpi, 144);
        assert!(wizard.detector_test.is_none());
        assert!(!wizard.dry_run_reviewed);
        assert_eq!(wizard.target_generation, 1);
        assert!(wizard.region_capture_generation.is_none());
        assert!(wizard.action_capture_generation.is_none());
    }

    #[test]
    fn starter_draft_can_capture_target_then_author_with_the_same_session() {
        let mut state = MacroPageState {
            draft: Some(EditorDraft::new(starter_macro_definition())),
            ..MacroPageState::default()
        };
        state.begin_draft_session();
        let session = state.active_draft_session().unwrap();

        begin_editor_authoring(&mut state, EditorAuthoringKind::CaptureTarget);
        let capture = state.take_editor_authoring_request().unwrap();
        assert_eq!(capture.session, session);
        state
            .apply_editor_authoring_result(EditorAuthoringResult {
                session,
                id: capture.id,
                fingerprint: capture.fingerprint,
                outcome: EditorAuthoringOutcome::TargetGeometry {
                    process_path: r#"C:\Games\Diablo IV.exe"#.into(),
                    window_class: "Diablo IV Main Window".into(),
                    title: "Diablo IV".into(),
                    width: 1920,
                    height: 1080,
                    dpi: 144,
                },
            })
            .unwrap();

        let draft = state.draft.as_ref().unwrap();
        assert_eq!(draft.target.process_path, r#"C:\Games\Diablo IV.exe"#);
        assert_eq!(draft.target.captured_client_width, 1920);
        assert_eq!(draft.target.captured_client_height, 1080);
        assert_eq!(draft.target.captured_dpi, 144);
        assert_eq!(draft.status, DraftStatus::NeedsValidation);

        begin_editor_authoring(
            &mut state,
            EditorAuthoringKind::TestOcr {
                block_id: "observe-1".into(),
            },
        );
        assert_eq!(
            state.take_editor_authoring_request().unwrap().session,
            session,
            "retargeting must preserve the draft session's native binding key"
        );
    }

    #[test]
    fn inspector_authoring_results_are_revision_bound_and_recapture_transactional() {
        let mut state = MacroPageState {
            draft: Some(fixture()),
            ..MacroPageState::default()
        };
        state
            .draft
            .as_mut()
            .unwrap()
            .regions
            .push(RegionDefinition {
                id: "scan".into(),
                revision: 1,
                rect: crate::engine::types::RectRatio {
                    x: 0.1,
                    y: 0.1,
                    width: 0.2,
                    height: 0.2,
                },
            });
        handle_inspector_intent(
            &mut state,
            inspector::InspectorIntent::RecaptureRegion {
                region_id: "scan".into(),
            },
        );
        let request = state.take_editor_authoring_request().unwrap();
        let result = EditorAuthoringResult {
            session: request.session,
            id: request.id,
            fingerprint: request.fingerprint.clone(),
            outcome: EditorAuthoringOutcome::Region(crate::engine::types::RectRatio {
                x: 0.2,
                y: 0.2,
                width: 0.3,
                height: 0.2,
            }),
        };
        state.apply_editor_authoring_result(result).unwrap();
        assert_eq!(state.draft.as_ref().unwrap().regions[0].revision, 2);

        handle_inspector_intent(
            &mut state,
            inspector::InspectorIntent::TestOcr {
                block_id: "observe-1".into(),
            },
        );
        let request = state.take_editor_authoring_request().unwrap();
        state.draft.as_mut().unwrap().definition.revision += 1;
        let result = EditorAuthoringResult {
            session: request.session,
            id: request.id,
            fingerprint: request.fingerprint,
            outcome: EditorAuthoringOutcome::DetectorTest {
                passed: true,
                evidence: "stale".into(),
                elapsed_ms: 1,
                rule_id: None,
                image_verification: None,
            },
        };
        assert_eq!(
            state.apply_editor_authoring_result(result),
            Err(EditorAuthoringError::StaleDraft)
        );
    }

    #[test]
    fn failed_ocr_test_tracks_detector_fingerprint_until_success_or_detector_edit() {
        let mut state = MacroPageState {
            draft: Some(EditorDraft::new(starter_macro_definition())),
            ..MacroPageState::default()
        };
        handle_inspector_intent(
            &mut state,
            inspector::InspectorIntent::TestOcr {
                block_id: "observe-1".into(),
            },
        );
        let failed = state.take_editor_authoring_request().unwrap();
        state
            .apply_editor_authoring_result(EditorAuthoringResult {
                session: failed.session,
                id: failed.id,
                fingerprint: failed.fingerprint,
                outcome: EditorAuthoringOutcome::DetectorTest {
                    passed: false,
                    evidence: "expected text not found".into(),
                    elapsed_ms: 5,
                    rule_id: None,
                    image_verification: None,
                },
            })
            .unwrap();
        assert!(
            editor_validation_problems(state.draft.as_ref().unwrap())
                .iter()
                .any(|problem| problem.code == "editor.detector_test_failed")
        );
        assert_eq!(
            apply_editor_command(state.draft.as_mut().unwrap(), EditorCommand::MarkValidated,),
            Err(EditorError::ValidationFailed)
        );

        apply_editor_command(
            state.draft.as_mut().unwrap(),
            EditorCommand::InsertBlock {
                target: InsertionTarget {
                    container: ContainerPath::Root,
                    index: 1,
                },
                block: Block {
                    id: "unrelated-comment".into(),
                    enabled: true,
                    kind: BlockKind::Comment {
                        text: "does not change OCR".into(),
                    },
                },
            },
        )
        .unwrap();
        assert!(
            editor_validation_problems(state.draft.as_ref().unwrap())
                .iter()
                .any(|problem| problem.code == "editor.detector_test_failed")
        );
        assert_eq!(
            apply_editor_command(state.draft.as_mut().unwrap(), EditorCommand::MarkValidated,),
            Err(EditorError::ValidationFailed)
        );

        handle_inspector_intent(
            &mut state,
            inspector::InspectorIntent::TestOcr {
                block_id: "observe-1".into(),
            },
        );
        let passed = state.take_editor_authoring_request().unwrap();
        state
            .apply_editor_authoring_result(EditorAuthoringResult {
                session: passed.session,
                id: passed.id,
                fingerprint: passed.fingerprint,
                outcome: EditorAuthoringOutcome::DetectorTest {
                    passed: true,
                    evidence: "matched".into(),
                    elapsed_ms: 4,
                    rule_id: None,
                    image_verification: None,
                },
            })
            .unwrap();
        assert!(
            !editor_validation_problems(state.draft.as_ref().unwrap())
                .iter()
                .any(|problem| problem.code == "editor.detector_test_failed")
        );
        assert!(
            apply_editor_command(state.draft.as_mut().unwrap(), EditorCommand::MarkValidated,)
                .is_ok()
        );

        handle_inspector_intent(
            &mut state,
            inspector::InspectorIntent::TestOcr {
                block_id: "observe-1".into(),
            },
        );
        let failed_again = state.take_editor_authoring_request().unwrap();
        state
            .apply_editor_authoring_result(EditorAuthoringResult {
                session: failed_again.session,
                id: failed_again.id,
                fingerprint: failed_again.fingerprint,
                outcome: EditorAuthoringOutcome::DetectorTest {
                    passed: false,
                    evidence: "expected text not found".into(),
                    elapsed_ms: 5,
                    rule_id: None,
                    image_verification: None,
                },
            })
            .unwrap();
        let mut edited_rule = state.draft.as_ref().unwrap().text_rules[0].clone();
        edited_rule.expected = "different detector input".into();
        apply_editor_command(
            state.draft.as_mut().unwrap(),
            EditorCommand::ReplaceTextRule { rule: edited_rule },
        )
        .unwrap();
        assert!(
            !editor_validation_problems(state.draft.as_ref().unwrap())
                .iter()
                .any(|problem| problem.code == "editor.detector_test_failed")
        );
        assert!(
            apply_editor_command(state.draft.as_mut().unwrap(), EditorCommand::MarkValidated,)
                .is_ok()
        );
    }

    #[test]
    fn editor_image_recapture_negative_test_restores_canonical_validity() {
        use image::{GrayImage, Luma};
        let template_ref = AssetRef {
            id: "template".into(),
            revision: 1,
            content_hash: "aa".repeat(32),
        };
        let mut definition = fixture().definition;
        definition.text_rules.clear();
        definition.regions.push(RegionDefinition {
            id: "image-region".into(),
            revision: 1,
            rect: crate::engine::types::RectRatio {
                x: 0.1,
                y: 0.1,
                width: 0.3,
                height: 0.2,
            },
        });
        definition.image_rules.push(ImageRule {
            id: "image-rule".into(),
            revision: 1,
            region_id: "image-region".into(),
            template: template_ref,
            transparent_mask: None,
            threshold: 0.9,
            scales_percent: vec![100],
            stable_frames: 2,
            maximum_center_drift_px: 5,
            minimum_runner_up_margin: 0.05,
            verification: None,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 100,
            timeout_ms: Limit::Unlimited,
        });
        definition.blocks[0].kind = BlockKind::Observe {
            condition: Condition::Image {
                source_block_id: "observe-1".into(),
                rule_id: "image-rule".into(),
                mode: ObserveMode::CheckNow,
            },
        };
        let mut state = MacroPageState {
            draft: Some(EditorDraft::new(definition)),
            ..MacroPageState::default()
        };

        handle_inspector_intent(
            &mut state,
            inspector::InspectorIntent::RecaptureRegion {
                region_id: "image-region".into(),
            },
        );
        let recapture = state.take_editor_authoring_request().unwrap();
        state
            .apply_editor_authoring_result(EditorAuthoringResult {
                session: recapture.session,
                id: recapture.id,
                fingerprint: recapture.fingerprint,
                outcome: EditorAuthoringOutcome::Region(crate::engine::types::RectRatio {
                    x: 0.2,
                    y: 0.2,
                    width: 0.3,
                    height: 0.2,
                }),
            })
            .unwrap();
        assert!(
            state.draft.as_ref().unwrap().image_rules[0]
                .verification
                .is_none()
        );

        let rule = state.draft.as_ref().unwrap().image_rules[0].clone();
        let region_revision = state.draft.as_ref().unwrap().regions[0].revision;
        let dimensions = (384, 144);
        let sample = NegativeCorpusSample {
            stable_id: "editor/observe-1/negative/1".into(),
            content_sha256: "11".repeat(32),
            measured_score: 0.1,
            evaluation: NegativeSampleEvaluationInputs::for_rule(
                &rule,
                state.draft.as_ref().unwrap().target.captured_dpi,
                region_revision,
                dimensions,
            ),
        };
        handle_inspector_intent(
            &mut state,
            inspector::InspectorIntent::CaptureImageNegative {
                block_id: "observe-1".into(),
            },
        );
        let negative = state.take_editor_authoring_request().unwrap();
        state
            .apply_editor_authoring_result(EditorAuthoringResult {
                session: negative.session,
                id: negative.id,
                fingerprint: negative.fingerprint,
                outcome: EditorAuthoringOutcome::ImageNegativeSample {
                    block_id: "observe-1".into(),
                    sample: sample.clone(),
                },
            })
            .unwrap();

        handle_inspector_intent(
            &mut state,
            inspector::InspectorIntent::TestImage {
                block_id: "observe-1".into(),
            },
        );
        let test = state.take_editor_authoring_request().unwrap();
        assert_eq!(test.image_negative_samples, vec![sample.clone()]);
        let template =
            GrayImage::from_fn(2, 2, |x, y| Luma([if (x + y) % 2 == 0 { 0 } else { 255 }]));
        let clusters = cluster_peaks(
            vec![ImageMatchCandidate {
                rect: crate::engine::types::Rect::new(10, 10, 2, 2),
                score: 0.95,
                scale_percent: 100,
            }],
            ClusterPolicy::default(),
        )
        .unwrap();
        let verification = ImageRuleVerification::verify(ImageRuleVerificationInput {
            rule: &rule,
            template: &template,
            mask: None,
            captured_dpi: state.draft.as_ref().unwrap().target.captured_dpi,
            current_dpi: state.draft.as_ref().unwrap().target.captured_dpi,
            region_revision,
            search_dimensions: dimensions,
            negative_samples: &[sample],
            observed_clusters: &clusters,
            maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
        })
        .unwrap()
        .into_artifact();
        assert_eq!(
            state.apply_editor_authoring_result(EditorAuthoringResult {
                session: test.session,
                id: test.id,
                fingerprint: test.fingerprint,
                outcome: EditorAuthoringOutcome::DetectorTest {
                    passed: true,
                    evidence: "mismatched rule".into(),
                    elapsed_ms: 4,
                    rule_id: Some("other-rule".into()),
                    image_verification: Some(verification.clone()),
                },
            }),
            Err(EditorAuthoringError::OutcomeMismatch)
        );
        assert!(
            state.draft.as_ref().unwrap().image_rules[0]
                .verification
                .is_none()
        );

        handle_inspector_intent(
            &mut state,
            inspector::InspectorIntent::TestImage {
                block_id: "observe-1".into(),
            },
        );
        let test = state.take_editor_authoring_request().unwrap();
        state
            .apply_editor_authoring_result(EditorAuthoringResult {
                session: test.session,
                id: test.id,
                fingerprint: test.fingerprint,
                outcome: EditorAuthoringOutcome::DetectorTest {
                    passed: true,
                    evidence: "matched and verified".into(),
                    elapsed_ms: 4,
                    rule_id: Some("image-rule".into()),
                    image_verification: Some(verification.clone()),
                },
            })
            .unwrap();

        assert!(
            state.draft.as_ref().unwrap().image_rules[0]
                .verification
                .is_some()
        );
        assert!(validate_macro(state.draft.as_ref().unwrap()).is_empty());

        handle_inspector_intent(
            &mut state,
            inspector::InspectorIntent::TestImage {
                block_id: "observe-1".into(),
            },
        );
        let failed = state.take_editor_authoring_request().unwrap();
        state
            .apply_editor_authoring_result(EditorAuthoringResult {
                session: failed.session,
                id: failed.id,
                fingerprint: failed.fingerprint,
                outcome: EditorAuthoringOutcome::DetectorTest {
                    passed: true,
                    evidence: "ambiguous candidates".into(),
                    elapsed_ms: 4,
                    rule_id: None,
                    image_verification: None,
                },
            })
            .unwrap();
        assert!(
            state.draft.as_ref().unwrap().image_rules[0]
                .verification
                .is_none()
        );
        assert!(
            editor_validation_problems(state.draft.as_ref().unwrap())
                .iter()
                .any(|problem| problem.code == "editor.detector_test_failed")
        );

        handle_inspector_intent(
            &mut state,
            inspector::InspectorIntent::TestImage {
                block_id: "observe-1".into(),
            },
        );
        let recovered = state.take_editor_authoring_request().unwrap();
        state
            .apply_editor_authoring_result(EditorAuthoringResult {
                session: recovered.session,
                id: recovered.id,
                fingerprint: recovered.fingerprint,
                outcome: EditorAuthoringOutcome::DetectorTest {
                    passed: true,
                    evidence: "matched and verified".into(),
                    elapsed_ms: 4,
                    rule_id: Some("image-rule".into()),
                    image_verification: Some(verification),
                },
            })
            .unwrap();
        assert!(
            state.draft.as_ref().unwrap().image_rules[0]
                .verification
                .is_some()
        );
        assert!(
            !editor_validation_problems(state.draft.as_ref().unwrap())
                .iter()
                .any(|problem| problem.code == "editor.detector_test_failed")
        );

        let mut edited = state.draft.as_ref().unwrap().image_rules[0].clone();
        edited.threshold = 0.92;
        dispatch_editor_command(&mut state, EditorCommand::ReplaceImageRule { rule: edited })
            .unwrap();
        handle_inspector_intent(
            &mut state,
            inspector::InspectorIntent::TestImage {
                block_id: "observe-1".into(),
            },
        );
        assert!(
            state
                .take_editor_authoring_request()
                .unwrap()
                .image_negative_samples
                .is_empty()
        );
    }

    #[test]
    fn validation_status_honors_editor_draft_staleness() {
        let mut state = MacroPageState {
            draft: Some(fixture()),
            ..MacroPageState::default()
        };
        assert_eq!(validation_summary(&state, &[]), "Valid");
        dispatch_editor_command(
            &mut state,
            EditorCommand::InsertBlock {
                target: InsertionTarget {
                    container: ContainerPath::Root,
                    index: 1,
                },
                block: Block {
                    id: "note".into(),
                    enabled: true,
                    kind: BlockKind::Comment {
                        text: "changed".into(),
                    },
                },
            },
        )
        .unwrap();
        assert_eq!(validation_summary(&state, &[]), "Needs revalidation");
        state.draft.as_mut().unwrap().status = DraftStatus::Ready;
        assert_eq!(validation_summary(&state, &[]), "Valid");
    }

    #[test]
    fn image_package_import_is_ui_progress_only_and_cancel_discards_it() {
        let mut state = MacroPageState::default();
        state.begin_image_package_reverification(vec!["image-a".into(), "image-b".into()]);

        let progress = state.image_package_reverification.as_ref().unwrap();
        assert_eq!(progress.active_rule_id(), Some("image-a"));
        assert_eq!(
            progress.stage,
            ImagePackageReverificationStage::CaptureTarget
        );

        state.clear_image_package_reverification();
        assert!(state.image_package_reverification.is_none());
    }
}
