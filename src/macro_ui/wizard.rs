use crate::engine::{
    macro_engine::{
        Action, AssetRef, Block, BlockKind, Condition, FocusLossPolicy, ImageRule,
        ImageRuleVerificationArtifact, Limit, MACRO_SCHEMA_VERSION, MacroDefinition,
        MatchSelectionPolicy, MouseButton, NegativeCorpusSample, ObserveMode, PreprocessProfile,
        SafetyPolicy, TargetProfile, TextMatchMode, TextRule, TimeoutOutcome, ValidationProblem,
        validate_macro,
    },
    types::RectRatio,
};
use eframe::egui::{self, DragValue, RichText, TextEdit, Ui};

pub const WIZARD_IMAGE_THRESHOLD: f32 = 0.95;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    Target,
    Region,
    Rule,
    DetectorTest,
    Action,
    Repetition,
    Failure,
    DryRun,
    Finish,
}

impl WizardStep {
    const ORDER: [Self; 9] = [
        Self::Target,
        Self::Region,
        Self::Rule,
        Self::DetectorTest,
        Self::Action,
        Self::Repetition,
        Self::Failure,
        Self::DryRun,
        Self::Finish,
    ];
}

#[derive(Debug, Clone, PartialEq)]
pub enum WizardDetector {
    Text,
    Image { template: AssetRef },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardDetectorKind {
    Text,
    Image,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WizardUiAction {
    CaptureTarget,
    CaptureRegion(WizardDetectorKind),
    CaptureTemplate,
    CaptureClickPoint,
    CaptureClickRegion,
    TestDetector,
    CaptureImageNegative,
    Finish(WizardOutput),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WizardRequestId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardAuthoringKind {
    CaptureTarget,
    CaptureTextRegion,
    CaptureImageRegion,
    CaptureTemplate,
    CaptureClickPoint,
    CaptureClickRegion,
    TestDetector,
    CaptureImageNegative,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WizardAuthoringRequest {
    pub id: WizardRequestId,
    pub fingerprint: String,
    pub kind: WizardAuthoringKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WizardAuthoringOutcome {
    TargetGeometry {
        process_path: String,
        window_class: String,
        title: String,
        width: u32,
        height: u32,
        dpi: u32,
    },
    Region(RectRatio),
    Point(crate::engine::types::PointRatio),
    Template {
        asset: AssetRef,
    },
    DetectorTest {
        passed: bool,
        evidence: String,
        elapsed_ms: u64,
        image_verification: Option<ImageRuleVerificationArtifact>,
    },
    ImageNegativeSample(NegativeCorpusSample),
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WizardAuthoringResult {
    pub id: WizardRequestId,
    pub fingerprint: String,
    pub outcome: WizardAuthoringOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardResultError {
    UnexpectedResult,
    StaleWizard,
    OutcomeMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WizardRepetition {
    RunOnce,
    Repeat(u32),
    Until { max_iterations: Limit<u64> },
    Continuous,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WizardActionTarget {
    MatchedResult,
    SavedPoint {
        id: String,
        point: crate::engine::types::PointRatio,
    },
    SavedRegion {
        id: String,
        rect: RectRatio,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WizardFailure {
    Stop { message: String },
    Continue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DetectorTestResult {
    pub passed: bool,
    pub evidence: String,
    pub elapsed_ms: u64,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WizardOutput {
    pub definition: MacroDefinition,
    pub validation_problems: Vec<ValidationProblem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WizardState {
    pub step: WizardStep,
    pub name: String,
    pub target: TargetProfile,
    pub target_bound: bool,
    pub region: RectRatio,
    pub detector: WizardDetector,
    pub text_expected: String,
    pub text_match_mode: TextMatchMode,
    pub observe_mode: ObserveMode,
    pub mouse_button: MouseButton,
    pub action_target: WizardActionTarget,
    pub repetition: WizardRepetition,
    pub failure: WizardFailure,
    pub detector_test: Option<DetectorTestResult>,
    pub image_verification: Option<ImageRuleVerificationArtifact>,
    pub image_negative_samples: Vec<NegativeCorpusSample>,
    pub dry_run_reviewed: bool,
    dry_run_fingerprint: Option<String>,
}

impl Default for WizardState {
    fn default() -> Self {
        Self {
            step: WizardStep::Target,
            name: "New Macro".into(),
            target: TargetProfile {
                process_path: String::new(),
                window_class: String::new(),
                title_contains: "Diablo IV".into(),
                captured_client_width: 1280,
                captured_client_height: 720,
                captured_dpi: 96,
            },
            target_bound: false,
            region: RectRatio {
                x: 0.25,
                y: 0.25,
                width: 0.5,
                height: 0.2,
            },
            detector: WizardDetector::Text,
            text_expected: String::new(),
            text_match_mode: TextMatchMode::Contains,
            observe_mode: ObserveMode::WaitForTrue {
                timeout_ms: Limit::Unlimited,
                timeout_outcome: TimeoutOutcome::StopError {
                    message: "Detector did not match".into(),
                },
            },
            mouse_button: MouseButton::Left,
            action_target: WizardActionTarget::MatchedResult,
            repetition: WizardRepetition::RunOnce,
            failure: WizardFailure::Stop {
                message: "Detector did not match".into(),
            },
            detector_test: None,
            image_verification: None,
            image_negative_samples: Vec::new(),
            dry_run_reviewed: false,
            dry_run_fingerprint: None,
        }
    }
}

impl WizardState {
    pub fn detector_kind(&self) -> WizardDetectorKind {
        match self.detector {
            WizardDetector::Text => WizardDetectorKind::Text,
            WizardDetector::Image { .. } => WizardDetectorKind::Image,
        }
    }

    pub fn next(&mut self) {
        let index = WizardStep::ORDER
            .iter()
            .position(|step| *step == self.step)
            .unwrap_or(0);
        self.step = WizardStep::ORDER[(index + 1).min(WizardStep::ORDER.len() - 1)];
    }

    pub fn back(&mut self) {
        let index = WizardStep::ORDER
            .iter()
            .position(|step| *step == self.step)
            .unwrap_or(0);
        self.step = WizardStep::ORDER[index.saturating_sub(1)];
    }

    pub fn finish(&self) -> Result<WizardOutput, String> {
        if self.step != WizardStep::Finish {
            return Err("Complete every wizard step before Finish".into());
        }
        if self.name.trim().is_empty() {
            return Err("Macro name is required".into());
        }
        if !self.target_bound {
            return Err("Capture and bind a concrete target before Finish".into());
        }
        if matches!(self.detector, WizardDetector::Text) && self.text_expected.trim().is_empty() {
            return Err("Expected OCR text is required".into());
        }
        if let WizardDetector::Image { template } = &self.detector {
            if template.revision == 0 || template.content_hash.is_empty() {
                return Err("Capture an image template before Finish".into());
            }
        }
        match self.detector_test.as_ref() {
            None => return Err("Detector test is required before Finish".into()),
            Some(result) if !result.passed => {
                return Err("Detector test failed; edit the rule or test again".into());
            }
            Some(result) if result.fingerprint != self.detector_fingerprint() => {
                return Err("Detector settings changed; run Detector Test again".into());
            }
            Some(_) => {}
        }
        if !self.dry_run_reviewed
            || self.dry_run_fingerprint.as_deref() != Some(self.review_fingerprint().as_str())
        {
            return Err("Dry Run review is required before Finish".into());
        }

        let region_id = "detect-region".to_string();
        let source_id = match self.detector {
            WizardDetector::Text => "wait-text",
            WizardDetector::Image { .. } => "wait-image",
        };
        let click_id = match self.detector {
            WizardDetector::Text => "click-text",
            WizardDetector::Image { .. } => "click-image",
        };
        let observe_mode = self.resolved_observe_mode();
        let condition = match self.detector {
            WizardDetector::Text => Condition::Text {
                source_block_id: source_id.into(),
                rule_id: "text-rule".into(),
                mode: observe_mode.clone(),
            },
            WizardDetector::Image { .. } => Condition::Image {
                source_block_id: source_id.into(),
                rule_id: "image-rule".into(),
                mode: observe_mode,
            },
        };
        let matched_action = |source: &str| match self.detector {
            WizardDetector::Text => Action::ClickTextMatch {
                source_block_id: source.into(),
                button: self.mouse_button,
            },
            WizardDetector::Image { .. } => Action::ClickImageMatch {
                source_block_id: source.into(),
                button: self.mouse_button,
            },
        };
        let action = match &self.action_target {
            WizardActionTarget::MatchedResult => matched_action(source_id),
            WizardActionTarget::SavedPoint { id, .. } => Action::ClickPoint {
                point_id: id.clone(),
                button: self.mouse_button,
            },
            WizardActionTarget::SavedRegion { id, .. } => Action::ClickRegion {
                region_id: id.clone(),
                button: self.mouse_button,
            },
        };
        let sequence = vec![
            Block {
                id: source_id.into(),
                enabled: true,
                kind: BlockKind::Observe { condition },
            },
            Block {
                id: click_id.into(),
                enabled: true,
                kind: BlockKind::Action {
                    action: action.clone(),
                },
            },
        ];
        let blocks = match &self.repetition {
            WizardRepetition::RunOnce => sequence,
            WizardRepetition::Repeat(count) => vec![Block {
                id: "repeat".into(),
                enabled: true,
                kind: BlockKind::RepeatN {
                    count: *count,
                    body: sequence,
                },
            }],
            WizardRepetition::Until { max_iterations } => {
                let repeat_source = "repeat-until";
                let repeat_condition = match self.detector {
                    WizardDetector::Text => Condition::Text {
                        source_block_id: repeat_source.into(),
                        rule_id: "text-rule".into(),
                        mode: ObserveMode::CheckNow,
                    },
                    WizardDetector::Image { .. } => Condition::Image {
                        source_block_id: repeat_source.into(),
                        rule_id: "image-rule".into(),
                        mode: ObserveMode::CheckNow,
                    },
                };
                let final_action = match &self.action_target {
                    WizardActionTarget::MatchedResult => matched_action(repeat_source),
                    _ => action,
                };
                vec![
                    Block {
                        id: repeat_source.into(),
                        enabled: true,
                        kind: BlockKind::RepeatUntil {
                            condition: repeat_condition,
                            max_iterations: max_iterations.clone(),
                            body: vec![Block {
                                id: "repeat-pacing".into(),
                                enabled: true,
                                kind: BlockKind::Wait { duration_ms: 100 },
                            }],
                        },
                    },
                    Block {
                        id: click_id.into(),
                        enabled: true,
                        kind: BlockKind::Action {
                            action: final_action,
                        },
                    },
                ]
            }
            WizardRepetition::Continuous => vec![Block {
                id: "continuous".into(),
                enabled: true,
                kind: BlockKind::Continuous { body: sequence },
            }],
        };
        let text_rules = self.text_rule_for_authoring().into_iter().collect();
        let image_rules = match &self.detector {
            WizardDetector::Text => vec![],
            WizardDetector::Image { template } => vec![ImageRule {
                id: "image-rule".into(),
                revision: 1,
                region_id: region_id.clone(),
                template: template.clone(),
                transparent_mask: None,
                threshold: WIZARD_IMAGE_THRESHOLD,
                scales_percent: vec![95, 100, 105],
                stable_frames: 2,
                maximum_center_drift_px: 5,
                minimum_runner_up_margin: 0.05,
                verification: self.image_verification.clone(),
                match_policy: MatchSelectionPolicy::ExactlyOne,
                poll_interval_ms: 100,
                timeout_ms: Limit::Unlimited,
            }],
        };
        let definition = MacroDefinition {
            schema_version: MACRO_SCHEMA_VERSION,
            id: "wizard-macro".into(),
            name: self.name.clone(),
            revision: 1,
            target: self.target.clone(),
            regions: vec![crate::engine::macro_engine::RegionDefinition {
                id: region_id,
                revision: 1,
                rect: self.region,
            }],
            points: match &self.action_target {
                WizardActionTarget::SavedPoint { id, point } => {
                    vec![crate::engine::macro_engine::PointDefinition {
                        id: id.clone(),
                        revision: 1,
                        point: *point,
                    }]
                }
                _ => vec![],
            },
            text_rules,
            image_rules,
            blocks,
            safety: SafetyPolicy {
                max_runtime_ms: Limit::Unlimited,
                max_clicks: Limit::Unlimited,
                max_observation_retries: Limit::Unlimited,
                max_observations_per_second: 20,
                minimum_click_interval_ms: 100,
                focus_loss: FocusLossPolicy::Stop,
            },
        };
        let mut definition = definition;
        if let WizardActionTarget::SavedRegion { id, rect } = &self.action_target {
            definition
                .regions
                .push(crate::engine::macro_engine::RegionDefinition {
                    id: id.clone(),
                    revision: 1,
                    rect: *rect,
                });
        }
        let validation_problems = validate_macro(&definition);
        if !validation_problems.is_empty() {
            return Err(format!(
                "Canonical validation failed: {}",
                validation_problems
                    .iter()
                    .map(|problem| problem.code.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(WizardOutput {
            definition,
            validation_problems,
        })
    }

    pub fn detector_fingerprint(&self) -> String {
        format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            self.target,
            self.region,
            self.detector,
            self.text_expected,
            self.text_match_mode,
            self.image_negative_samples
        )
    }

    pub fn text_rule_for_authoring(&self) -> Option<TextRule> {
        matches!(self.detector, WizardDetector::Text).then(|| TextRule {
            id: "text-rule".into(),
            revision: 1,
            region_id: "detect-region".into(),
            language: "en-US".into(),
            preprocess: PreprocessProfile::Grayscale,
            expected: self.text_expected.clone(),
            match_mode: self.text_match_mode,
            threshold: 0.9,
            case_sensitive: false,
            allow_cross_line: false,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 250,
            timeout_ms: Limit::Unlimited,
            stable_frames: 2,
        })
    }

    pub fn review_fingerprint(&self) -> String {
        format!(
            "{}|{:?}|{:?}|{:?}|{:?}",
            self.detector_fingerprint(),
            self.observe_mode,
            self.action_target,
            self.mouse_button,
            (&self.repetition, &self.failure)
        )
    }

    pub fn record_detector_test(
        &mut self,
        passed: bool,
        evidence: impl Into<String>,
        elapsed_ms: u64,
    ) {
        self.detector_test = Some(DetectorTestResult {
            passed,
            evidence: evidence.into(),
            elapsed_ms,
            fingerprint: self.detector_fingerprint(),
        });
        if matches!(self.detector, WizardDetector::Text) {
            self.image_verification = None;
        }
        self.dry_run_reviewed = false;
        self.dry_run_fingerprint = None;
    }

    pub fn record_image_detector_test(
        &mut self,
        passed: bool,
        evidence: impl Into<String>,
        elapsed_ms: u64,
        verification: Option<ImageRuleVerificationArtifact>,
    ) {
        self.image_verification = verification;
        self.record_detector_test(passed, evidence, elapsed_ms);
    }

    pub fn mark_dry_run_reviewed(&mut self) {
        self.dry_run_reviewed = true;
        self.dry_run_fingerprint = Some(self.review_fingerprint());
    }

    pub fn invalidate_detector_proof(&mut self) {
        self.detector_test = None;
        self.image_verification = None;
        self.image_negative_samples.clear();
        self.invalidate_dry_run_review();
    }

    pub fn invalidate_dry_run_review(&mut self) {
        self.dry_run_reviewed = false;
        self.dry_run_fingerprint = None;
    }

    fn reconcile_form_edits(&mut self, detector_before: &str, review_before: &str) {
        if self.detector_fingerprint() != detector_before {
            self.invalidate_detector_proof();
        } else if self.review_fingerprint() != review_before {
            self.invalidate_dry_run_review();
        }
    }

    fn resolved_observe_mode(&self) -> ObserveMode {
        let outcome = match &self.failure {
            WizardFailure::Stop { message } => TimeoutOutcome::StopError {
                message: message.clone(),
            },
            WizardFailure::Continue => TimeoutOutcome::Continue,
        };
        match &self.observe_mode {
            ObserveMode::CheckNow => ObserveMode::CheckNow,
            ObserveMode::WaitForTrue { timeout_ms, .. } => ObserveMode::WaitForTrue {
                timeout_ms: timeout_ms.clone(),
                timeout_outcome: outcome,
            },
            ObserveMode::WaitForFalse { timeout_ms, .. } => ObserveMode::WaitForFalse {
                timeout_ms: timeout_ms.clone(),
                timeout_outcome: outcome,
            },
        }
    }

    #[cfg(test)]
    fn completed_text_fixture() -> Self {
        Self {
            step: WizardStep::Finish,
            name: "Text Click".into(),
            text_expected: "Ancestral".into(),
            repetition: WizardRepetition::Continuous,
            detector_test: Some(DetectorTestResult {
                passed: true,
                evidence: "Ancestral".into(),
                elapsed_ms: 12,
                fingerprint: String::new(),
            }),
            dry_run_reviewed: true,
            target_bound: true,
            ..Self::default()
        }
        .with_current_proofs()
    }

    #[cfg(test)]
    fn with_current_proofs(mut self) -> Self {
        self.record_detector_test(true, "fixture", 12);
        self.mark_dry_run_reviewed();
        self
    }
}

pub fn show(ui: &mut Ui, state: &mut WizardState) -> Option<WizardUiAction> {
    let detector_before = state.detector_fingerprint();
    let review_before = state.review_fingerprint();
    let mut action = None;
    ui.group(|ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("GUIDED MACRO WIZARD").strong());
            ui.label(format!(
                "Step {} of 9: {:?}",
                step_number(state.step),
                state.step
            ));
        });
        ui.separator();
        match state.step {
            WizardStep::Target => {
                ui.label("Choose the window profile this macro is allowed to observe.");
                ui.add(TextEdit::singleline(&mut state.name).hint_text("Macro name"));
                if state.target_bound {
                    ui.label(format!("Process: {}", state.target.process_path));
                    ui.label(format!("Window class: {}", state.target.window_class));
                    ui.label(format!("Captured title: {}", state.target.title_contains));
                    ui.label(format!(
                        "Captured client: {} x {} at {} DPI",
                        state.target.captured_client_width,
                        state.target.captured_client_height,
                        state.target.captured_dpi
                    ));
                } else {
                    ui.label("No concrete target captured yet.");
                }
                if ui.button("Capture target window region").clicked() {
                    action = Some(WizardUiAction::CaptureTarget);
                }
            }
            WizardStep::Region => {
                ui.label("Hold, drag, and release around the area to detect.");
                ui.label(format!(
                    "Region: x {:.3}, y {:.3}, width {:.3}, height {:.3}",
                    state.region.x, state.region.y, state.region.width, state.region.height
                ));
                if ui.button("Capture detection region").clicked() {
                    action = Some(WizardUiAction::CaptureRegion(state.detector_kind()));
                }
                if state.detector_kind() == WizardDetectorKind::Image
                    && ui.button("Capture image template").clicked()
                {
                    action = Some(WizardUiAction::CaptureTemplate);
                }
            }
            WizardStep::Rule => {
                let mut kind = state.detector_kind();
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut kind, WizardDetectorKind::Text, "OCR text");
                    ui.selectable_value(&mut kind, WizardDetectorKind::Image, "Image match");
                });
                if kind != state.detector_kind() {
                    state.detector_test = None;
                    state.detector = match kind {
                        WizardDetectorKind::Text => WizardDetector::Text,
                        WizardDetectorKind::Image => WizardDetector::Image {
                            template: AssetRef {
                                id: "wizard-template".into(),
                                revision: 0,
                                content_hash: String::new(),
                            },
                        },
                    };
                }
                match &mut state.detector {
                    WizardDetector::Text => {
                        ui.add(
                            TextEdit::singleline(&mut state.text_expected)
                                .hint_text("Text OCR should look for"),
                        );
                        egui::ComboBox::from_label("Text match")
                            .selected_text(format!("{:?}", state.text_match_mode))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut state.text_match_mode,
                                    TextMatchMode::Exact,
                                    "Exact",
                                );
                                ui.selectable_value(
                                    &mut state.text_match_mode,
                                    TextMatchMode::Contains,
                                    "Contains",
                                );
                                ui.selectable_value(
                                    &mut state.text_match_mode,
                                    TextMatchMode::Fuzzy,
                                    "Fuzzy",
                                );
                            });
                    }
                    WizardDetector::Image { template } => {
                        ui.label(format!(
                            "Template: {} r{}{}",
                            template.id,
                            template.revision,
                            if template.content_hash.is_empty() {
                                " (capture required)"
                            } else {
                                ""
                            }
                        ));
                    }
                }
            }
            WizardStep::DetectorTest => {
                ui.label("Test observes only. It never moves or clicks the mouse.");
                if ui.button("Test detector").clicked() {
                    action = Some(WizardUiAction::TestDetector);
                }
                if state.detector_kind() == WizardDetectorKind::Image
                    && ui
                        .button("Capture current frame as negative sample")
                        .clicked()
                {
                    action = Some(WizardUiAction::CaptureImageNegative);
                }
                if state.detector_kind() == WizardDetectorKind::Image {
                    ui.label(format!(
                        "{} explicit negative sample(s)",
                        state.image_negative_samples.len()
                    ));
                }
                if let Some(result) = &state.detector_test {
                    ui.label(format!(
                        "{} in {} ms: {}",
                        if result.passed { "Passed" } else { "Failed" },
                        result.elapsed_ms,
                        result.evidence
                    ));
                }
            }
            WizardStep::Action => {
                ui.label("Click the fresh detector match when it qualifies.");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut state.mouse_button, MouseButton::Left, "Left click");
                    ui.selectable_value(&mut state.mouse_button, MouseButton::Right, "Right click");
                });
                let mut target_kind = match state.action_target {
                    WizardActionTarget::MatchedResult => 0,
                    WizardActionTarget::SavedPoint { .. } => 1,
                    WizardActionTarget::SavedRegion { .. } => 2,
                };
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut target_kind, 0, "Matched result");
                    ui.selectable_value(&mut target_kind, 1, "Saved point");
                    ui.selectable_value(&mut target_kind, 2, "Saved region");
                });
                match target_kind {
                    0 => state.action_target = WizardActionTarget::MatchedResult,
                    1 => {
                        if !matches!(state.action_target, WizardActionTarget::SavedPoint { .. }) {
                            state.action_target = WizardActionTarget::SavedPoint {
                                id: "click-point".into(),
                                point: crate::engine::types::PointRatio { x: 0.5, y: 0.5 },
                            };
                        }
                        if ui.button("Capture click point").clicked() {
                            action = Some(WizardUiAction::CaptureClickPoint);
                        }
                    }
                    _ => {
                        if !matches!(state.action_target, WizardActionTarget::SavedRegion { .. }) {
                            state.action_target = WizardActionTarget::SavedRegion {
                                id: "click-region".into(),
                                rect: RectRatio {
                                    x: 0.45,
                                    y: 0.45,
                                    width: 0.1,
                                    height: 0.1,
                                },
                            };
                        }
                        if ui.button("Capture click region").clicked() {
                            action = Some(WizardUiAction::CaptureClickRegion);
                        }
                    }
                }
                let mut wait = !matches!(state.observe_mode, ObserveMode::CheckNow);
                ui.checkbox(&mut wait, "Wait for match");
                if wait && matches!(state.observe_mode, ObserveMode::CheckNow) {
                    state.observe_mode = ObserveMode::WaitForTrue {
                        timeout_ms: Limit::Unlimited,
                        timeout_outcome: TimeoutOutcome::StopError {
                            message: "Detector did not match".into(),
                        },
                    };
                } else if !wait {
                    state.observe_mode = ObserveMode::CheckNow;
                }
            }
            WizardStep::Repetition => {
                let mut kind = match state.repetition {
                    WizardRepetition::RunOnce => 0,
                    WizardRepetition::Repeat(_) => 1,
                    WizardRepetition::Until { .. } => 2,
                    WizardRepetition::Continuous => 3,
                };
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut kind, 0, "Run once");
                    ui.selectable_value(&mut kind, 1, "Repeat");
                    ui.selectable_value(&mut kind, 2, "Until condition");
                    ui.selectable_value(&mut kind, 3, "Continuous");
                });
                state.repetition = match kind {
                    0 => WizardRepetition::RunOnce,
                    1 => {
                        let mut count = match state.repetition {
                            WizardRepetition::Repeat(count) => count,
                            _ => 2,
                        };
                        ui.add(DragValue::new(&mut count).clamp_range(1..=1_000_000));
                        WizardRepetition::Repeat(count)
                    }
                    2 => {
                        let mut unlimited = matches!(
                            state.repetition,
                            WizardRepetition::Until {
                                max_iterations: Limit::Unlimited
                            }
                        );
                        ui.checkbox(&mut unlimited, "Unlimited iterations");
                        let max_iterations = if unlimited {
                            Limit::Unlimited
                        } else {
                            let mut count = match state.repetition {
                                WizardRepetition::Until {
                                    max_iterations: Limit::Finite(count),
                                } => count,
                                _ => 100,
                            };
                            ui.add(DragValue::new(&mut count).clamp_range(1..=1_000_000));
                            Limit::Finite(count)
                        };
                        WizardRepetition::Until { max_iterations }
                    }
                    _ => WizardRepetition::Continuous,
                };
            }
            WizardStep::Failure => {
                let mut stop = matches!(state.failure, WizardFailure::Stop { .. });
                ui.checkbox(&mut stop, "Stop with error when a finite wait times out");
                state.failure = if stop {
                    let mut message = match &state.failure {
                        WizardFailure::Stop { message } => message.clone(),
                        WizardFailure::Continue => "Detector did not match".into(),
                    };
                    ui.add(TextEdit::singleline(&mut message).hint_text("Failure message"));
                    WizardFailure::Stop { message }
                } else {
                    WizardFailure::Continue
                };
            }
            WizardStep::DryRun => {
                ui.label("Review only: no live runtime starts and zero input is injected.");
                let response = ui.checkbox(
                    &mut state.dry_run_reviewed,
                    "I reviewed the detector, action, limits, and failure behavior",
                );
                if response.changed() {
                    if state.dry_run_reviewed {
                        state.mark_dry_run_reviewed();
                    } else {
                        state.dry_run_fingerprint = None;
                    }
                }
            }
            WizardStep::Finish => match state.finish() {
                Ok(output) => {
                    ui.label("Ready to create an unsaved editable draft.");
                    if ui.button("Finish wizard").clicked() {
                        action = Some(WizardUiAction::Finish(output));
                    }
                }
                Err(error) => {
                    ui.label(RichText::new(error).color(egui::Color32::LIGHT_RED));
                }
            },
        }
        ui.separator();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(state.step != WizardStep::Target, egui::Button::new("Back"))
                .clicked()
            {
                state.back();
            }
            if ui
                .add_enabled(state.step != WizardStep::Finish, egui::Button::new("Next"))
                .clicked()
            {
                state.next();
            }
        });
    });
    state.reconcile_form_edits(&detector_before, &review_before);
    action
}

fn step_number(step: WizardStep) -> usize {
    WizardStep::ORDER
        .iter()
        .position(|candidate| *candidate == step)
        .unwrap_or(0)
        + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::macro_engine::{Block, BlockKind, MouseButton, ObserveMode};

    fn image_verification(
        wizard: &WizardState,
    ) -> crate::engine::macro_engine::ImageRuleVerificationArtifact {
        use crate::engine::macro_engine::{
            DEFAULT_MAX_SCORE_CELLS, ImageRule, ImageRuleVerification, ImageRuleVerificationInput,
            MatchSelectionPolicy, NegativeCorpusSample, NegativeSampleEvaluationInputs,
        };
        use image::{GrayImage, Luma};
        let WizardDetector::Image { template } = &wizard.detector else {
            panic!("image wizard")
        };
        let rule = ImageRule {
            id: "image-rule".into(),
            revision: 1,
            region_id: "detect-region".into(),
            template: template.clone(),
            transparent_mask: None,
            threshold: WIZARD_IMAGE_THRESHOLD,
            scales_percent: vec![95, 100, 105],
            stable_frames: 2,
            maximum_center_drift_px: 5,
            minimum_runner_up_margin: 0.05,
            verification: None,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 100,
            timeout_ms: crate::engine::macro_engine::Limit::Unlimited,
        };
        let template_image =
            GrayImage::from_fn(2, 2, |x, y| Luma([if (x + y) % 2 == 0 { 0 } else { 255 }]));
        let dimensions = (640, 144);
        let evaluation = NegativeSampleEvaluationInputs::for_rule(
            &rule,
            wizard.target.captured_dpi,
            1,
            dimensions,
        );
        let samples = [NegativeCorpusSample {
            stable_id: "negative/sample".into(),
            content_sha256: "11".repeat(32),
            measured_score: 0.1,
            evaluation,
        }];
        ImageRuleVerification::verify(ImageRuleVerificationInput {
            rule: &rule,
            template: &template_image,
            mask: None,
            captured_dpi: wizard.target.captured_dpi,
            current_dpi: wizard.target.captured_dpi,
            region_revision: 1,
            search_dimensions: dimensions,
            negative_samples: &samples,
            observed_clusters: &[],
            maximum_score_cells: DEFAULT_MAX_SCORE_CELLS,
        })
        .unwrap()
        .into_artifact()
    }

    fn find_block<'a>(blocks: &'a [Block], id: &str) -> Option<&'a Block> {
        for block in blocks {
            if block.id == id {
                return Some(block);
            }
            let children: &[Block] = match &block.kind {
                BlockKind::RepeatN { body, .. }
                | BlockKind::RepeatUntil { body, .. }
                | BlockKind::Continuous { body } => body,
                _ => &[],
            };
            if let Some(found) = find_block(children, id) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn wizard_emits_canonical_editable_blocks() {
        let wizard = WizardState::completed_text_fixture();
        let authoring_rule = wizard.text_rule_for_authoring().unwrap();
        let output = wizard.finish().expect("completed wizard");

        assert!(matches!(
            output.definition.blocks[0].kind,
            BlockKind::Continuous { .. }
        ));
        assert!(find_block(&output.definition.blocks, "wait-text").is_some());
        assert!(find_block(&output.definition.blocks, "click-text").is_some());
        assert_eq!(output.definition.text_rules, vec![authoring_rule]);
        assert!(output.validation_problems.is_empty());
    }

    #[test]
    fn target_step_keeps_captured_identity_and_geometry_read_only() {
        let mut wizard = WizardState::completed_text_fixture();
        wizard.step = WizardStep::Target;
        wizard.target.process_path = r"C:\Games\Diablo IV.exe".into();
        wizard.target.window_class = "Diablo IV Main Window".into();
        wizard.target.title_contains = "Diablo IV".into();
        wizard.target.captured_client_width = 2560;
        wizard.target.captured_client_height = 1440;
        wizard.target.captured_dpi = 144;
        let captured = wizard.target.clone();

        let context = egui::Context::default();
        context.begin_frame(egui::RawInput::default());
        egui::CentralPanel::default().show(&context, |ui| {
            assert_eq!(show(ui, &mut wizard), None);
        });
        let _ = context.end_frame();

        assert!(wizard.target_bound);
        assert_eq!(wizard.target, captured);
    }

    #[test]
    fn form_edits_clear_only_the_proofs_they_make_stale() {
        let mut detector_edit = WizardState::completed_text_fixture();
        let negative_rule = ImageRule {
            id: "image-rule".into(),
            revision: 1,
            region_id: "detect-region".into(),
            template: AssetRef {
                id: "template".into(),
                revision: 1,
                content_hash: "hash".into(),
            },
            transparent_mask: None,
            threshold: WIZARD_IMAGE_THRESHOLD,
            scales_percent: vec![100],
            stable_frames: 2,
            maximum_center_drift_px: 5,
            minimum_runner_up_margin: 0.05,
            verification: None,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 100,
            timeout_ms: Limit::Unlimited,
        };
        detector_edit
            .image_negative_samples
            .push(NegativeCorpusSample {
                stable_id: "stale-negative".into(),
                content_sha256: "11".repeat(32),
                measured_score: 0.1,
                evaluation: crate::engine::macro_engine::NegativeSampleEvaluationInputs::for_rule(
                    &negative_rule,
                    96,
                    1,
                    (640, 144),
                ),
            });
        let detector_before = detector_edit.detector_fingerprint();
        let review_before = detector_edit.review_fingerprint();
        detector_edit.text_expected = "Greater Affix".into();
        detector_edit.reconcile_form_edits(&detector_before, &review_before);
        assert!(detector_edit.detector_test.is_none());
        assert!(detector_edit.image_negative_samples.is_empty());
        assert!(!detector_edit.dry_run_reviewed);

        let mut action_edit = WizardState::completed_text_fixture();
        let detector_before = action_edit.detector_fingerprint();
        let review_before = action_edit.review_fingerprint();
        action_edit.mouse_button = MouseButton::Right;
        action_edit.reconcile_form_edits(&detector_before, &review_before);
        assert!(action_edit.detector_test.is_some());
        assert!(!action_edit.dry_run_reviewed);
    }

    #[test]
    fn wizard_back_preserves_edits_and_wait_can_change_to_check() {
        let mut wizard = WizardState::completed_text_fixture();
        wizard.step = WizardStep::Action;
        wizard.back();
        assert_eq!(wizard.step, WizardStep::DetectorTest);
        assert_eq!(wizard.text_expected, "Ancestral");

        wizard.observe_mode = ObserveMode::CheckNow;
        wizard.step = WizardStep::Finish;
        wizard.mark_dry_run_reviewed();
        let output = wizard.finish().expect("editable canonical output");
        let observe = find_block(&output.definition.blocks, "wait-text").unwrap();
        assert!(matches!(
            observe.kind,
            BlockKind::Observe {
                condition: crate::engine::macro_engine::Condition::Text {
                    mode: ObserveMode::CheckNow,
                    ..
                }
            }
        ));
    }

    #[test]
    fn wizard_preserves_right_click_and_unlimited_repetition() {
        let mut wizard = WizardState::completed_text_fixture();
        wizard.mouse_button = MouseButton::Right;
        wizard.repetition = WizardRepetition::Continuous;
        wizard.mark_dry_run_reviewed();
        let output = wizard.finish().unwrap();
        let click = find_block(&output.definition.blocks, "click-text").unwrap();
        assert!(matches!(
            click.kind,
            BlockKind::Action {
                action: crate::engine::macro_engine::Action::ClickTextMatch {
                    button: MouseButton::Right,
                    ..
                }
            }
        ));
    }

    #[test]
    fn finish_requires_successful_detector_test_and_dry_run_review() {
        let mut wizard = WizardState::default();
        wizard.step = WizardStep::Finish;
        wizard.target_bound = true;
        wizard.text_expected = "Ancestral".into();
        assert!(wizard.finish().unwrap_err().contains("Detector test"));

        wizard.record_detector_test(true, "Ancestral", 10);
        assert!(wizard.finish().unwrap_err().contains("Dry Run"));

        wizard.mark_dry_run_reviewed();
        assert!(wizard.finish().is_ok());
    }

    #[test]
    fn image_wizard_emits_canonical_image_rule_and_click() {
        let mut wizard = WizardState::completed_text_fixture();
        wizard.detector = WizardDetector::Image {
            template: crate::engine::macro_engine::AssetRef {
                id: "template".into(),
                revision: 3,
                content_hash: "abc".into(),
            },
        };
        let verification = image_verification(&wizard);
        wizard.record_image_detector_test(true, "image matched", 8, Some(verification));
        wizard.mark_dry_run_reviewed();

        let output = wizard.finish().unwrap();

        assert!(output.definition.text_rules.is_empty());
        assert_eq!(output.definition.image_rules[0].template.revision, 3);
        assert!(find_block(&output.definition.blocks, "wait-image").is_some());
        assert!(find_block(&output.definition.blocks, "click-image").is_some());
    }

    #[test]
    fn wizard_emits_saved_point_and_region_click_targets() {
        let mut point = WizardState::completed_text_fixture();
        point.action_target = WizardActionTarget::SavedPoint {
            id: "click-point".into(),
            point: crate::engine::types::PointRatio { x: 0.4, y: 0.6 },
        };
        point.mark_dry_run_reviewed();
        let output = point.finish().unwrap();
        assert_eq!(output.definition.points[0].id, "click-point");
        assert!(matches!(
            find_block(&output.definition.blocks, "click-text")
                .unwrap()
                .kind,
            BlockKind::Action {
                action: crate::engine::macro_engine::Action::ClickPoint { .. }
            }
        ));

        let mut region = WizardState::completed_text_fixture();
        region.action_target = WizardActionTarget::SavedRegion {
            id: "click-region".into(),
            rect: crate::engine::types::RectRatio {
                x: 0.2,
                y: 0.3,
                width: 0.1,
                height: 0.1,
            },
        };
        region.mark_dry_run_reviewed();
        let output = region.finish().unwrap();
        assert!(
            output
                .definition
                .regions
                .iter()
                .any(|item| item.id == "click-region")
        );
        assert!(matches!(
            find_block(&output.definition.blocks, "click-text")
                .unwrap()
                .kind,
            BlockKind::Action {
                action: crate::engine::macro_engine::Action::ClickRegion { .. }
            }
        ));
    }

    #[test]
    fn repeat_until_uses_real_detector_condition_and_explicit_unlimited_max() {
        let mut wizard = WizardState::completed_text_fixture();
        wizard.repetition = WizardRepetition::Until {
            max_iterations: crate::engine::macro_engine::Limit::Unlimited,
        };
        wizard.mark_dry_run_reviewed();

        let output = wizard.finish().unwrap();

        assert!(matches!(
            output.definition.blocks[0].kind,
            BlockKind::RepeatUntil {
                condition: crate::engine::macro_engine::Condition::Text { .. },
                max_iterations: crate::engine::macro_engine::Limit::Unlimited,
                ..
            }
        ));
    }

    #[test]
    fn finish_rejects_unbound_target_and_any_canonical_validation_problem() {
        let mut wizard = WizardState::completed_text_fixture();
        wizard.target_bound = false;
        assert!(wizard.finish().unwrap_err().contains("target"));

        wizard.target_bound = true;
        wizard.text_match_mode = crate::engine::macro_engine::TextMatchMode::Absent;
        wizard.record_detector_test(true, "absent", 5);
        wizard.mark_dry_run_reviewed();
        assert!(wizard.finish().unwrap_err().contains("validation"));
    }

    #[test]
    fn target_region_or_template_change_clears_revision_bound_negative_samples() {
        use crate::engine::macro_engine::{
            ImageRule, MatchSelectionPolicy, NegativeCorpusSample, NegativeSampleEvaluationInputs,
        };
        let mut wizard = WizardState::completed_text_fixture();
        let template = AssetRef {
            id: "template".into(),
            revision: 1,
            content_hash: "abc".into(),
        };
        wizard.detector = WizardDetector::Image {
            template: template.clone(),
        };
        let rule = ImageRule {
            id: "image-rule".into(),
            revision: 1,
            region_id: "detect-region".into(),
            template,
            transparent_mask: None,
            threshold: WIZARD_IMAGE_THRESHOLD,
            scales_percent: vec![95, 100, 105],
            stable_frames: 2,
            maximum_center_drift_px: 5,
            minimum_runner_up_margin: 0.05,
            verification: None,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 100,
            timeout_ms: Limit::Unlimited,
        };
        wizard.image_negative_samples.push(NegativeCorpusSample {
            stable_id: "negative/a".into(),
            content_sha256: "11".repeat(32),
            measured_score: 0.1,
            evaluation: NegativeSampleEvaluationInputs::for_rule(&rule, 96, 1, (640, 144)),
        });

        wizard.invalidate_detector_proof();

        assert!(wizard.image_negative_samples.is_empty());
        assert!(wizard.detector_test.is_none());
        assert!(wizard.image_verification.is_none());
    }
}
