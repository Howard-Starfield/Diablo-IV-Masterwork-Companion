use eframe::egui::{Button, Checkbox, Color32, DragValue, RichText, Slider, TextEdit, Ui};

use crate::engine::macro_engine::{
    AssetRef, Block, BlockKind, Condition, ImageRule, Limit, MacroDefinition, MatchSelectionPolicy,
    ObserveMode, PassiveCondition, PreprocessProfile, TextMatchMode, TextRule, TimeoutOutcome,
    ValidationProblem,
};
use crate::ui_theme::text;

const EMPTY_INSPECTOR_PROMPT: &str = "Select a canonical canvas block.";

#[derive(Debug, Clone, PartialEq)]
pub enum InspectorIntent {
    TestOcr {
        block_id: String,
    },
    TestImage {
        block_id: String,
    },
    RecaptureRegion {
        region_id: String,
    },
    RecaptureTemplate {
        rule_id: String,
    },
    CaptureImageNegative {
        block_id: String,
    },
    ReplaceTextRule {
        rule: TextRule,
    },
    ReplaceImageRule {
        rule: ImageRule,
    },
    SetConditionMode {
        block_id: String,
        mode: ObserveMode,
    },
    SetRepeatUntilMax {
        block_id: String,
        max: Limit<u64>,
    },
    SetWaitDuration {
        block_id: String,
        duration_ms: u64,
    },
    SetRepeatCount {
        block_id: String,
        count: u32,
    },
    SetWatchSettings {
        block_id: String,
        timeout_ms: Limit<u64>,
        cooldown_ms: u64,
    },
    InvalidEdit {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextInspector {
    pub block_id: String,
    pub region_id: String,
    pub expected: String,
    pub mode: String,
    pub threshold: f64,
    pub normalization: String,
    pub profile: String,
    pub poll_interval_ms: u64,
    pub timeout: String,
    pub policy: String,
    pub actions: Vec<InspectorIntent>,
    pub problems: Vec<String>,
    pub lane_priority: Option<usize>,
    pub flow_fields: Vec<(String, String)>,
    pub rule: TextRule,
    pub observe_mode: ObserveMode,
    pub repeat_until_max: Option<Limit<u64>>,
    pub supports_observe_mode: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageInspector {
    pub block_id: String,
    pub region_id: String,
    pub template_id: String,
    pub scales_percent: Vec<u16>,
    pub threshold: f32,
    pub policy: String,
    pub stable_frames: u8,
    pub runner_up_margin: f32,
    pub poll_interval_ms: u64,
    pub timeout: String,
    pub actions: Vec<InspectorIntent>,
    pub problems: Vec<String>,
    pub lane_priority: Option<usize>,
    pub flow_fields: Vec<(String, String)>,
    pub rule: ImageRule,
    pub observe_mode: ObserveMode,
    pub repeat_until_max: Option<Limit<u64>>,
    pub available_templates: Vec<AssetRef>,
    pub supports_observe_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowInspector {
    pub block_id: String,
    pub kind: String,
    pub fields: Vec<(String, String)>,
    pub problems: Vec<String>,
    pub edit: Option<FlowEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowEdit {
    Wait {
        duration_ms: u64,
    },
    RepeatN {
        count: u32,
    },
    Watch {
        timeout_ms: Limit<u64>,
        cooldown_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum InspectorProjection {
    Text(TextInspector),
    Image(ImageInspector),
    Flow(FlowInspector),
    Empty,
}

pub fn problem_navigation_target(problems: &[ValidationProblem], index: usize) -> Option<String> {
    problems.get(index)?.block_id.clone()
}

pub fn project_inspector(
    definition: &MacroDefinition,
    selected_id: &str,
    problems: &[ValidationProblem],
) -> InspectorProjection {
    let selected_problems = || {
        problems
            .iter()
            .filter(|problem| problem.block_id.as_deref() == Some(selected_id))
            .map(|problem| problem.message.clone())
            .collect::<Vec<_>>()
    };
    if let Some(block) = find_block(&definition.blocks, selected_id) {
        return match &block.kind {
            BlockKind::Observe { condition }
            | BlockKind::If { condition, .. }
            | BlockKind::RepeatUntil { condition, .. } => match condition {
                Condition::Text { rule_id, mode, .. } => definition
                    .text_rules
                    .iter()
                    .find(|rule| rule.id == *rule_id)
                    .map(|rule| {
                        text_projection(
                            selected_id,
                            rule,
                            mode,
                            selected_problems(),
                            flow_fields(&block.kind),
                            repeat_until_max(&block.kind),
                            true,
                        )
                    })
                    .unwrap_or(InspectorProjection::Empty),
                Condition::Image { rule_id, mode, .. } => definition
                    .image_rules
                    .iter()
                    .find(|rule| rule.id == *rule_id)
                    .map(|rule| {
                        image_projection(
                            selected_id,
                            rule,
                            mode,
                            selected_problems(),
                            flow_fields(&block.kind),
                            repeat_until_max(&block.kind),
                            &definition.image_rules,
                            true,
                        )
                    })
                    .unwrap_or(InspectorProjection::Empty),
            },
            BlockKind::WatchGroup { group } => InspectorProjection::Flow(FlowInspector {
                block_id: selected_id.into(),
                kind: "WATCH GROUP".into(),
                fields: vec![
                    ("Timeout".into(), format_limit(&group.timeout_ms)),
                    ("Cooldown".into(), format!("{} ms", group.cooldown_ms)),
                    ("Priority lanes".into(), group.lanes.len().to_string()),
                ],
                problems: selected_problems(),
                edit: Some(FlowEdit::Watch {
                    timeout_ms: group.timeout_ms.clone(),
                    cooldown_ms: group.cooldown_ms,
                }),
            }),
            kind => flow_projection(selected_id, kind, selected_problems()),
        };
    }
    if let Some((priority, lane)) = find_lane(&definition.blocks, selected_id) {
        let problems = selected_problems();
        return match &lane.condition {
            PassiveCondition::Text { rule_id, .. } => definition
                .text_rules
                .iter()
                .find(|r| r.id == *rule_id)
                .map(|rule| {
                    text_projection(
                        selected_id,
                        rule,
                        &ObserveMode::CheckNow,
                        problems,
                        vec![],
                        None,
                        false,
                    )
                })
                .unwrap_or(InspectorProjection::Empty),
            PassiveCondition::Image { rule_id, .. } => definition
                .image_rules
                .iter()
                .find(|r| r.id == *rule_id)
                .map(|rule| {
                    image_projection(
                        selected_id,
                        rule,
                        &ObserveMode::CheckNow,
                        problems,
                        vec![],
                        None,
                        &definition.image_rules,
                        false,
                    )
                })
                .unwrap_or(InspectorProjection::Empty),
        }
        .with_priority(priority);
    }
    InspectorProjection::Empty
}

trait WithPriority {
    fn with_priority(self, priority: usize) -> Self;
}
impl WithPriority for InspectorProjection {
    fn with_priority(mut self, priority: usize) -> Self {
        match &mut self {
            Self::Text(p) => p.lane_priority = Some(priority),
            Self::Image(p) => p.lane_priority = Some(priority),
            _ => {}
        }
        self
    }
}

fn text_projection(
    id: &str,
    rule: &TextRule,
    mode: &ObserveMode,
    problems: Vec<String>,
    flow_fields: Vec<(String, String)>,
    repeat_until_max: Option<Limit<u64>>,
    supports_observe_mode: bool,
) -> InspectorProjection {
    InspectorProjection::Text(TextInspector {
        block_id: id.into(),
        region_id: rule.region_id.clone(),
        expected: rule.expected.clone(),
        mode: mode_label(mode).into(),
        threshold: rule.threshold,
        normalization: format!(
            "case {} | cross-line {}",
            on_off(rule.case_sensitive),
            on_off(rule.allow_cross_line)
        ),
        profile: format!("{:?}", rule.preprocess),
        poll_interval_ms: rule.poll_interval_ms,
        timeout: mode_timeout(mode).unwrap_or_else(|| format_limit(&rule.timeout_ms)),
        policy: format!("{:?}", rule.match_policy),
        actions: vec![
            InspectorIntent::TestOcr {
                block_id: id.into(),
            },
            InspectorIntent::RecaptureRegion {
                region_id: rule.region_id.clone(),
            },
        ],
        problems,
        lane_priority: None,
        flow_fields,
        rule: rule.clone(),
        observe_mode: mode.clone(),
        repeat_until_max,
        supports_observe_mode,
    })
}
fn image_projection(
    id: &str,
    rule: &ImageRule,
    mode: &ObserveMode,
    problems: Vec<String>,
    flow_fields: Vec<(String, String)>,
    repeat_until_max: Option<Limit<u64>>,
    image_rules: &[ImageRule],
    supports_observe_mode: bool,
) -> InspectorProjection {
    InspectorProjection::Image(ImageInspector {
        block_id: id.into(),
        region_id: rule.region_id.clone(),
        template_id: rule.template.id.clone(),
        scales_percent: rule.scales_percent.clone(),
        threshold: rule.threshold,
        policy: format!("{:?}", rule.match_policy),
        stable_frames: rule.stable_frames,
        runner_up_margin: rule.minimum_runner_up_margin,
        poll_interval_ms: rule.poll_interval_ms,
        timeout: mode_timeout(mode).unwrap_or_else(|| format_limit(&rule.timeout_ms)),
        actions: vec![
            InspectorIntent::TestImage {
                block_id: id.into(),
            },
            InspectorIntent::RecaptureRegion {
                region_id: rule.region_id.clone(),
            },
            InspectorIntent::RecaptureTemplate {
                rule_id: rule.id.clone(),
            },
            InspectorIntent::CaptureImageNegative {
                block_id: id.into(),
            },
        ],
        problems,
        lane_priority: None,
        flow_fields,
        rule: rule.clone(),
        observe_mode: mode.clone(),
        repeat_until_max,
        available_templates: image_rules
            .iter()
            .map(|rule| rule.template.clone())
            .collect(),
        supports_observe_mode,
    })
}
fn flow_projection(id: &str, kind: &BlockKind, problems: Vec<String>) -> InspectorProjection {
    let (label, fields, edit) = match kind {
        BlockKind::Wait { duration_ms } => (
            "WAIT",
            vec![("Duration".into(), format!("{duration_ms} ms"))],
            Some(FlowEdit::Wait {
                duration_ms: *duration_ms,
            }),
        ),
        BlockKind::RepeatN { count, .. } => (
            "REPEAT N",
            vec![("Limit".into(), count.to_string())],
            Some(FlowEdit::RepeatN { count: *count }),
        ),
        BlockKind::Continuous { .. } => (
            "CONTINUOUS",
            vec![("Limit".into(), "Unlimited".into())],
            None,
        ),
        BlockKind::If { .. } => ("IF", vec![], None),
        _ => ("BLOCK", vec![], None),
    };
    InspectorProjection::Flow(FlowInspector {
        block_id: id.into(),
        kind: label.into(),
        fields,
        problems,
        edit,
    })
}
fn flow_fields(kind: &BlockKind) -> Vec<(String, String)> {
    match kind {
        BlockKind::RepeatUntil { max_iterations, .. } => {
            vec![("Max iterations".into(), format_limit(max_iterations))]
        }
        BlockKind::If { .. } => vec![("Branches".into(), "THEN / ELSE".into())],
        _ => vec![],
    }
}
fn repeat_until_max(kind: &BlockKind) -> Option<Limit<u64>> {
    match kind {
        BlockKind::RepeatUntil { max_iterations, .. } => Some(max_iterations.clone()),
        _ => None,
    }
}
fn mode_label(mode: &ObserveMode) -> &'static str {
    match mode {
        ObserveMode::CheckNow => "Check now",
        ObserveMode::WaitForTrue { .. } => "Wait for match",
        ObserveMode::WaitForFalse { .. } => "Wait for disappearance",
    }
}
fn mode_timeout(mode: &ObserveMode) -> Option<String> {
    match mode {
        ObserveMode::CheckNow => None,
        ObserveMode::WaitForTrue { timeout_ms, .. }
        | ObserveMode::WaitForFalse { timeout_ms, .. } => Some(format_limit(timeout_ms)),
    }
}
fn format_limit<T: std::fmt::Display>(limit: &Limit<T>) -> String {
    match limit {
        Limit::Finite(v) => v.to_string(),
        Limit::Unlimited => "Unlimited".into(),
    }
}
fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn find_block<'a>(blocks: &'a [Block], id: &str) -> Option<&'a Block> {
    for block in blocks {
        if block.id == id {
            return Some(block);
        }
        for child in children(block) {
            if let Some(found) = find_block(child, id) {
                return Some(found);
            }
        }
    }
    None
}
fn find_lane<'a>(
    blocks: &'a [Block],
    id: &str,
) -> Option<(usize, &'a crate::engine::macro_engine::WatchLane)> {
    for block in blocks {
        if let BlockKind::WatchGroup { group } = &block.kind {
            if let Some((i, l)) = group.lanes.iter().enumerate().find(|(_, l)| l.id == id) {
                return Some((i + 1, l));
            }
        }
        for child in children(block) {
            if let Some(x) = find_lane(child, id) {
                return Some(x);
            }
        }
    }
    None
}
fn children(block: &Block) -> Vec<&[Block]> {
    let mut out = match &block.kind {
        BlockKind::If {
            then_body,
            else_body,
            ..
        } => vec![then_body.as_slice(), else_body.as_slice()],
        BlockKind::RepeatN { body, .. }
        | BlockKind::RepeatUntil { body, .. }
        | BlockKind::Continuous { body } => vec![body.as_slice()],
        BlockKind::WatchGroup { group } => {
            group.lanes.iter().map(|l| l.then_body.as_slice()).collect()
        }
        _ => vec![],
    };
    match &block.kind {
        BlockKind::Observe { condition }
        | BlockKind::If { condition, .. }
        | BlockKind::RepeatUntil { condition, .. } => {
            if let Some(body) = condition_timeout_body(condition) {
                out.push(body);
            }
        }
        BlockKind::WatchGroup { group } => {
            if let TimeoutOutcome::RunBody { body } = &group.timeout_outcome {
                out.push(body);
            }
        }
        _ => {}
    }
    out
}
fn condition_timeout_body(condition: &Condition) -> Option<&[Block]> {
    match condition {
        Condition::Text { mode, .. } | Condition::Image { mode, .. } => match mode {
            ObserveMode::WaitForTrue {
                timeout_outcome: TimeoutOutcome::RunBody { body },
                ..
            }
            | ObserveMode::WaitForFalse {
                timeout_outcome: TimeoutOutcome::RunBody { body },
                ..
            } => Some(body),
            _ => None,
        },
    }
}

pub fn show(
    ui: &mut Ui,
    projection: &InspectorProjection,
    editable: bool,
) -> Option<InspectorIntent> {
    let mut intent = None;
    match projection {
        InspectorProjection::Empty => {
            ui.label(
                RichText::new("No block selected")
                    .strong()
                    .color(Color32::from_gray(204)),
            );
            ui.label(EMPTY_INSPECTOR_PROMPT);
        }
        InspectorProjection::Text(p) => {
            heading(ui, "TEXT DETECTOR", &p.mode);
            if let Some(priority) = p.lane_priority {
                field(ui, "Lane priority", &priority.to_string());
            }
            for (key, value) in &p.flow_fields {
                field(ui, key, value);
            }
            let mut rule = p.rule.clone();
            let original_rule = rule.clone();
            editable_string(ui, "Region", &mut rule.region_id, editable);
            editable_string(ui, "Expected", &mut rule.expected, editable);
            if ui
                .add_enabled(
                    editable,
                    Button::new(format!("Text match: {:?}", rule.match_mode)),
                )
                .clicked()
            {
                rule.match_mode = next_text_match_mode(rule.match_mode);
            }
            editable_number(ui, "Threshold", &mut rule.threshold, 0.0..=1.0, editable);
            ui.add_enabled(
                editable,
                Checkbox::new(&mut rule.case_sensitive, "Case sensitive"),
            );
            ui.add_enabled(
                editable,
                Checkbox::new(&mut rule.allow_cross_line, "Allow cross-line"),
            );
            if ui
                .add_enabled(
                    editable,
                    Button::new(format!("Preprocess: {:?}", rule.preprocess)),
                )
                .clicked()
            {
                rule.preprocess = next_preprocess(rule.preprocess);
            }
            if ui
                .add_enabled(
                    editable,
                    Button::new(format!("Policy: {:?}", rule.match_policy)),
                )
                .clicked()
            {
                rule.match_policy = next_policy(rule.match_policy);
            }
            editable_u64(ui, "Polling ms", &mut rule.poll_interval_ms, editable);
            editable_u8(ui, "Stable frames", &mut rule.stable_frames, editable);
            limit_editor(ui, "Rule timeout ms", &mut rule.timeout_ms, editable);
            if rule != original_rule {
                intent = Some(InspectorIntent::ReplaceTextRule { rule });
            }
            if p.supports_observe_mode {
                mode_editor(ui, &p.block_id, &p.observe_mode, editable, &mut intent);
                mode_timeout_editor(ui, &p.block_id, &p.observe_mode, editable, &mut intent);
            }
            repeat_until_editor(
                ui,
                &p.block_id,
                p.repeat_until_max.as_ref(),
                editable,
                &mut intent,
            );
            actions(ui, &p.actions, editable, &mut intent);
            problems(ui, &p.problems);
        }
        InspectorProjection::Image(p) => {
            heading(ui, "IMAGE DETECTOR", &p.template_id);
            if let Some(priority) = p.lane_priority {
                field(ui, "Lane priority", &priority.to_string());
            }
            for (key, value) in &p.flow_fields {
                field(ui, key, value);
            }
            let mut rule = p.rule.clone();
            let original_rule = rule.clone();
            editable_string(ui, "Region", &mut rule.region_id, editable);
            template_editor(ui, &mut rule.template, &p.available_templates, editable);
            let scales_changed = image_scales_editor(ui, &mut rule.scales_percent, editable);
            editable_f32(ui, "Threshold", &mut rule.threshold, 0.0..=1.0, editable);
            editable_u8(ui, "Stable frames", &mut rule.stable_frames, editable);
            editable_f32(
                ui,
                "Runner-up",
                &mut rule.minimum_runner_up_margin,
                0.0..=1.0,
                editable,
            );
            editable_u64(ui, "Polling ms", &mut rule.poll_interval_ms, editable);
            limit_editor(ui, "Rule timeout ms", &mut rule.timeout_ms, editable);
            if ui
                .add_enabled(
                    editable,
                    Button::new(format!("Policy: {:?}", rule.match_policy)),
                )
                .clicked()
            {
                rule.match_policy = next_policy(rule.match_policy);
            }
            if rule != original_rule {
                intent = if scales_changed {
                    match validate_image_scales(&rule.scales_percent) {
                        Ok(()) => Some(InspectorIntent::ReplaceImageRule { rule }),
                        Err(message) => Some(InspectorIntent::InvalidEdit { message }),
                    }
                } else {
                    Some(InspectorIntent::ReplaceImageRule { rule })
                };
            }
            if p.supports_observe_mode {
                mode_editor(ui, &p.block_id, &p.observe_mode, editable, &mut intent);
                mode_timeout_editor(ui, &p.block_id, &p.observe_mode, editable, &mut intent);
            }
            repeat_until_editor(
                ui,
                &p.block_id,
                p.repeat_until_max.as_ref(),
                editable,
                &mut intent,
            );
            actions(ui, &p.actions, editable, &mut intent);
            problems(ui, &p.problems);
        }
        InspectorProjection::Flow(p) => {
            heading(ui, &p.kind, &p.block_id);
            for (k, v) in &p.fields {
                field(ui, k, v);
            }
            flow_editor(ui, p, editable, &mut intent);
            problems(ui, &p.problems);
        }
    }
    intent
}
fn heading(ui: &mut Ui, label: &str, summary: &str) {
    ui.label(
        RichText::new(label)
            .monospace()
            .strong()
            .color(Color32::from_rgb(224, 119, 53)),
    );
    ui.label(RichText::new(summary).color(Color32::from_gray(184)));
    ui.separator();
}
fn field(ui: &mut Ui, label: &str, value: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(label)
                .monospace()
                .size(text::META)
                .color(Color32::from_rgb(174, 142, 102)),
        );
        ui.label(value);
    });
}
fn editable_string(ui: &mut Ui, label: &str, value: &mut String, editable: bool) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        ui.add_enabled(editable, TextEdit::singleline(value));
    });
}
fn editable_number(
    ui: &mut Ui,
    label: &str,
    value: &mut f64,
    range: std::ops::RangeInclusive<f64>,
    editable: bool,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        ui.add_enabled(editable, Slider::new(value, range));
    });
}
fn editable_f32(
    ui: &mut Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    editable: bool,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        ui.add_enabled(editable, Slider::new(value, range));
    });
}
fn editable_u64(ui: &mut Ui, label: &str, value: &mut u64, editable: bool) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        ui.add_enabled(editable, DragValue::new(value).clamp_range(0..=u64::MAX));
    });
}
fn editable_u32(ui: &mut Ui, label: &str, value: &mut u32, editable: bool) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        ui.add_enabled(editable, DragValue::new(value).clamp_range(1..=u32::MAX));
    });
}
fn editable_u8(ui: &mut Ui, label: &str, value: &mut u8, editable: bool) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        ui.add_enabled(editable, DragValue::new(value).clamp_range(1..=u8::MAX));
    });
}

fn next_text_match_mode(mode: TextMatchMode) -> TextMatchMode {
    match mode {
        TextMatchMode::Exact => TextMatchMode::Contains,
        TextMatchMode::Contains => TextMatchMode::Fuzzy,
        TextMatchMode::Fuzzy => TextMatchMode::Absent,
        TextMatchMode::Absent => TextMatchMode::Exact,
    }
}

fn next_preprocess(profile: PreprocessProfile) -> PreprocessProfile {
    match profile {
        PreprocessProfile::Original => PreprocessProfile::Grayscale,
        PreprocessProfile::Grayscale => PreprocessProfile::HighContrast,
        PreprocessProfile::HighContrast => PreprocessProfile::SmallText,
        PreprocessProfile::SmallText => PreprocessProfile::Original,
    }
}

fn next_policy(policy: MatchSelectionPolicy) -> MatchSelectionPolicy {
    match policy {
        MatchSelectionPolicy::ExactlyOne => MatchSelectionPolicy::HighestScore,
        MatchSelectionPolicy::HighestScore => MatchSelectionPolicy::FirstReadingOrder,
        MatchSelectionPolicy::FirstReadingOrder => MatchSelectionPolicy::Topmost,
        MatchSelectionPolicy::Topmost => MatchSelectionPolicy::Bottommost,
        MatchSelectionPolicy::Bottommost => MatchSelectionPolicy::ExactlyOne,
    }
}

fn limit_editor(ui: &mut Ui, label: &str, limit: &mut Limit<u64>, editable: bool) {
    let mut value = match limit {
        Limit::Finite(value) => *value,
        Limit::Unlimited => 5_000,
    };
    let before = value;
    editable_u64(ui, label, &mut value, editable);
    if value != before {
        *limit = Limit::Finite(value);
    }
    if ui
        .add_enabled(
            editable,
            Button::new(if matches!(limit, Limit::Unlimited) {
                "Use finite timeout"
            } else {
                "Use unlimited timeout"
            }),
        )
        .clicked()
    {
        *limit = if matches!(limit, Limit::Unlimited) {
            Limit::Finite(value)
        } else {
            Limit::Unlimited
        };
    }
}

fn template_editor(ui: &mut Ui, template: &mut AssetRef, available: &[AssetRef], editable: bool) {
    ui.label(format!("Template: {}", template.id));
    ui.horizontal_wrapped(|ui| {
        for candidate in available {
            if candidate != template
                && ui
                    .add_enabled(editable, Button::new(format!("Use {}", candidate.id)))
                    .clicked()
            {
                *template = candidate.clone();
                break;
            }
        }
    });
}

fn image_scales_editor(ui: &mut Ui, scales: &mut Vec<u16>, editable: bool) -> bool {
    let mut changed = false;
    ui.label("Scales percent");
    ui.horizontal_wrapped(|ui| {
        for scale in scales.iter_mut() {
            changed |= ui
                .add_enabled(editable, DragValue::new(scale).clamp_range(1..=u16::MAX))
                .changed();
        }
        if ui.add_enabled(editable, Button::new("+ scale")).clicked() {
            let next = scales
                .last()
                .copied()
                .unwrap_or(95)
                .saturating_add(5)
                .max(1);
            scales.push(next);
            changed = true;
        }
        if ui
            .add_enabled(editable && scales.len() > 1, Button::new("- scale"))
            .clicked()
        {
            scales.pop();
            changed = true;
        }
    });
    changed
}

fn validate_image_scales(scales: &[u16]) -> Result<(), String> {
    use std::collections::BTreeSet;
    if scales.is_empty() {
        return Err("Image matching requires at least one scale.".into());
    }
    if scales.len() > crate::engine::macro_engine::DEFAULT_MAX_SCALES {
        return Err(format!(
            "Image matching supports at most {} scales.",
            crate::engine::macro_engine::DEFAULT_MAX_SCALES
        ));
    }
    let mut seen = BTreeSet::new();
    for scale in scales {
        if *scale == 0 {
            return Err("Image scales must be greater than zero percent.".into());
        }
        if !seen.insert(*scale) {
            return Err(format!("Image scale {scale}% is duplicated."));
        }
    }
    Ok(())
}

fn mode_editor(
    ui: &mut Ui,
    block_id: &str,
    current: &ObserveMode,
    editable: bool,
    intent: &mut Option<InspectorIntent>,
) {
    if ui
        .add_enabled(editable, Button::new("Cycle detector mode"))
        .clicked()
    {
        let mode = next_observe_mode(current);
        *intent = Some(InspectorIntent::SetConditionMode {
            block_id: block_id.into(),
            mode,
        });
    }
}

fn next_observe_mode(current: &ObserveMode) -> ObserveMode {
    match current {
        ObserveMode::CheckNow => ObserveMode::WaitForTrue {
            timeout_ms: Limit::Unlimited,
            timeout_outcome: TimeoutOutcome::Continue,
        },
        ObserveMode::WaitForTrue {
            timeout_ms,
            timeout_outcome,
        } => ObserveMode::WaitForFalse {
            timeout_ms: timeout_ms.clone(),
            timeout_outcome: timeout_outcome.clone(),
        },
        ObserveMode::WaitForFalse { .. } => ObserveMode::CheckNow,
    }
}

fn mode_timeout_editor(
    ui: &mut Ui,
    block_id: &str,
    current: &ObserveMode,
    editable: bool,
    intent: &mut Option<InspectorIntent>,
) {
    let mut timeout = match current {
        ObserveMode::WaitForTrue { timeout_ms, .. }
        | ObserveMode::WaitForFalse { timeout_ms, .. } => timeout_ms.clone(),
        ObserveMode::CheckNow => return,
    };
    let before = timeout.clone();
    limit_editor(ui, "Observe wait timeout ms", &mut timeout, editable);
    if timeout != before {
        *intent = Some(InspectorIntent::SetConditionMode {
            block_id: block_id.into(),
            mode: observe_mode_with_timeout(current, timeout).expect("wait mode"),
        });
    }
}

fn observe_mode_with_timeout(current: &ObserveMode, timeout_ms: Limit<u64>) -> Option<ObserveMode> {
    match current {
        ObserveMode::WaitForTrue {
            timeout_outcome, ..
        } => Some(ObserveMode::WaitForTrue {
            timeout_ms,
            timeout_outcome: timeout_outcome.clone(),
        }),
        ObserveMode::WaitForFalse {
            timeout_outcome, ..
        } => Some(ObserveMode::WaitForFalse {
            timeout_ms,
            timeout_outcome: timeout_outcome.clone(),
        }),
        ObserveMode::CheckNow => None,
    }
}

fn repeat_until_editor(
    ui: &mut Ui,
    block_id: &str,
    current: Option<&Limit<u64>>,
    editable: bool,
    intent: &mut Option<InspectorIntent>,
) {
    let Some(current) = current else { return };
    let mut maximum = match current {
        Limit::Finite(value) => *value,
        Limit::Unlimited => 100,
    };
    let before = maximum;
    editable_u64(ui, "Max iterations", &mut maximum, editable);
    if maximum != before {
        *intent = Some(InspectorIntent::SetRepeatUntilMax {
            block_id: block_id.into(),
            max: Limit::Finite(maximum),
        });
    }
    if ui
        .add_enabled(
            editable,
            Button::new(if matches!(current, Limit::Unlimited) {
                "Use finite max"
            } else {
                "Use unlimited max"
            }),
        )
        .clicked()
    {
        *intent = Some(InspectorIntent::SetRepeatUntilMax {
            block_id: block_id.into(),
            max: if matches!(current, Limit::Unlimited) {
                Limit::Finite(maximum)
            } else {
                Limit::Unlimited
            },
        });
    }
}

fn flow_editor(
    ui: &mut Ui,
    projection: &FlowInspector,
    editable: bool,
    intent: &mut Option<InspectorIntent>,
) {
    match &projection.edit {
        Some(FlowEdit::Wait { duration_ms }) => {
            let mut value = *duration_ms;
            editable_u64(ui, "Duration ms", &mut value, editable);
            if value != *duration_ms {
                *intent = Some(InspectorIntent::SetWaitDuration {
                    block_id: projection.block_id.clone(),
                    duration_ms: value,
                });
            }
        }
        Some(FlowEdit::RepeatN { count }) => {
            let mut value = *count;
            editable_u32(ui, "Repeat count", &mut value, editable);
            if value != *count {
                *intent = Some(InspectorIntent::SetRepeatCount {
                    block_id: projection.block_id.clone(),
                    count: value,
                });
            }
        }
        Some(FlowEdit::Watch {
            timeout_ms,
            cooldown_ms,
        }) => {
            let mut timeout = match timeout_ms {
                Limit::Finite(value) => *value,
                Limit::Unlimited => 5_000,
            };
            let mut cooldown = *cooldown_ms;
            editable_u64(ui, "Watch timeout ms", &mut timeout, editable);
            editable_u64(ui, "Cooldown ms", &mut cooldown, editable);
            if timeout
                != match timeout_ms {
                    Limit::Finite(value) => *value,
                    Limit::Unlimited => 5_000,
                }
                || cooldown != *cooldown_ms
            {
                *intent = Some(InspectorIntent::SetWatchSettings {
                    block_id: projection.block_id.clone(),
                    timeout_ms: Limit::Finite(timeout),
                    cooldown_ms: cooldown,
                });
            }
            if ui
                .add_enabled(
                    editable,
                    Button::new(if matches!(timeout_ms, Limit::Unlimited) {
                        "Use finite timeout"
                    } else {
                        "Use unlimited timeout"
                    }),
                )
                .clicked()
            {
                *intent = Some(InspectorIntent::SetWatchSettings {
                    block_id: projection.block_id.clone(),
                    timeout_ms: if matches!(timeout_ms, Limit::Unlimited) {
                        Limit::Finite(timeout)
                    } else {
                        Limit::Unlimited
                    },
                    cooldown_ms: cooldown,
                });
            }
        }
        None => {}
    }
}
fn actions(
    ui: &mut Ui,
    actions: &[InspectorIntent],
    editable: bool,
    out: &mut Option<InspectorIntent>,
) {
    ui.horizontal_wrapped(|ui| {
        for action in actions {
            let Some(label) = (match action {
                InspectorIntent::TestOcr { .. } => Some("Test OCR"),
                InspectorIntent::TestImage { .. } => Some("Test Image"),
                InspectorIntent::RecaptureRegion { .. } => Some("Recapture"),
                InspectorIntent::RecaptureTemplate { .. } => Some("Recapture template"),
                InspectorIntent::CaptureImageNegative { .. } => Some("Add negative frame"),
                _ => None,
            }) else {
                continue;
            };
            if ui.add_enabled(editable, Button::new(label)).clicked() {
                *out = Some(action.clone());
            }
        }
    });
    ui.label(
        RichText::new("Tests observe only; they never inject input.")
            .size(text::SUPPORTING)
            .color(Color32::from_gray(112)),
    );
}
fn problems(ui: &mut Ui, items: &[String]) {
    for item in items {
        ui.label(RichText::new(item).color(Color32::from_rgb(224, 112, 75)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::macro_engine::*;

    fn definition() -> MacroDefinition {
        MacroDefinition {
            schema_version: MACRO_SCHEMA_VERSION,
            id: "m".into(),
            name: "m".into(),
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
            image_rules: vec![],
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
                timeout_ms: Limit::Finite(2_000),
                stable_frames: 2,
            }],
            blocks: vec![Block {
                id: "observe".into(),
                enabled: true,
                kind: BlockKind::Observe {
                    condition: Condition::Text {
                        source_block_id: "observe".into(),
                        rule_id: "rule".into(),
                        mode: ObserveMode::WaitForTrue {
                            timeout_ms: Limit::Finite(2_000),
                            timeout_outcome: TimeoutOutcome::Continue,
                        },
                    },
                },
            }],
            safety: SafetyPolicy {
                max_runtime_ms: Limit::Finite(10_000),
                max_clicks: Limit::Finite(2),
                max_observation_retries: Limit::Finite(2),
                max_observations_per_second: 20,
                minimum_click_interval_ms: 100,
                focus_loss: FocusLossPolicy::Stop,
            },
        }
    }

    #[test]
    fn text_projection_exposes_detector_controls_and_safe_intents() {
        let projection = project_inspector(&definition(), "observe", &[]);
        let InspectorProjection::Text(text) = projection else {
            panic!("text inspector");
        };
        assert_eq!(text.expected, "Salvage");
        assert_eq!(text.region_id, "scan");
        assert_eq!(text.poll_interval_ms, 250);
        assert!(text.actions.contains(&InspectorIntent::TestOcr {
            block_id: "observe".into()
        }));
        assert!(text.actions.contains(&InspectorIntent::RecaptureRegion {
            region_id: "scan".into()
        }));
    }

    #[test]
    fn problem_navigation_selects_exact_structural_owner() {
        let problems = vec![ValidationProblem {
            code: "x".into(),
            message: "fix it".into(),
            block_id: Some("observe".into()),
        }];
        assert_eq!(
            problem_navigation_target(&problems, 0).as_deref(),
            Some("observe")
        );
        assert_eq!(problem_navigation_target(&problems, 1), None);
    }

    #[test]
    fn watch_lane_projection_exposes_persisted_priority() {
        let mut definition = definition();
        let lane = |id: &str| WatchLane {
            id: id.into(),
            enabled: true,
            condition: PassiveCondition::Text {
                source_block_id: "observe".into(),
                rule_id: "rule".into(),
            },
            then_body: vec![],
        };
        definition.blocks.push(Block {
            id: "watch".into(),
            enabled: true,
            kind: BlockKind::WatchGroup {
                group: WatchGroup {
                    lanes: vec![lane("lane-1"), lane("lane-2")],
                    timeout_ms: Limit::Finite(3_000),
                    timeout_outcome: TimeoutOutcome::Continue,
                    cooldown_ms: 125,
                },
            },
        });

        let InspectorProjection::Text(text) = project_inspector(&definition, "lane-2", &[]) else {
            panic!("text lane inspector");
        };
        assert_eq!(text.lane_priority, Some(2));
        assert!(!text.supports_observe_mode);
    }

    #[test]
    fn flow_projection_exposes_repeat_and_watch_limits() {
        let mut definition = definition();
        definition.blocks.extend([
            Block {
                id: "repeat".into(),
                enabled: true,
                kind: BlockKind::RepeatUntil {
                    condition: Condition::Text {
                        source_block_id: "observe".into(),
                        rule_id: "rule".into(),
                        mode: ObserveMode::CheckNow,
                    },
                    max_iterations: Limit::Finite(9),
                    body: vec![],
                },
            },
            Block {
                id: "watch".into(),
                enabled: true,
                kind: BlockKind::WatchGroup {
                    group: WatchGroup {
                        lanes: vec![],
                        timeout_ms: Limit::Finite(4_000),
                        timeout_outcome: TimeoutOutcome::Continue,
                        cooldown_ms: 175,
                    },
                },
            },
        ]);

        let InspectorProjection::Text(repeat) = project_inspector(&definition, "repeat", &[])
        else {
            panic!("repeat detector inspector");
        };
        assert!(
            repeat
                .flow_fields
                .contains(&("Max iterations".into(), "9".into()))
        );
        let InspectorProjection::Flow(watch) = project_inspector(&definition, "watch", &[]) else {
            panic!("watch flow inspector");
        };
        assert!(watch.fields.contains(&("Timeout".into(), "4000".into())));
        assert!(watch.fields.contains(&("Cooldown".into(), "175 ms".into())));
    }

    #[test]
    fn nested_timeout_body_is_available_to_selection() {
        let mut definition = definition();
        let BlockKind::Observe { condition } = &mut definition.blocks[0].kind else {
            panic!("observe fixture");
        };
        let Condition::Text { mode, .. } = condition else {
            panic!("text fixture");
        };
        *mode = ObserveMode::WaitForTrue {
            timeout_ms: Limit::Finite(500),
            timeout_outcome: TimeoutOutcome::RunBody {
                body: vec![Block {
                    id: "timeout-child".into(),
                    enabled: true,
                    kind: BlockKind::Comment {
                        text: "fallback".into(),
                    },
                }],
            },
        };

        assert!(matches!(
            project_inspector(&definition, "timeout-child", &[]),
            InspectorProjection::Flow(_)
        ));
    }

    #[test]
    fn image_scale_edits_require_nonempty_unique_bounded_percentages() {
        assert_eq!(validate_image_scales(&[95, 100, 105]), Ok(()));
        assert!(validate_image_scales(&[]).is_err());
        assert!(validate_image_scales(&[100, 100]).is_err());
        assert!(validate_image_scales(&[0]).is_err());
        assert!(validate_image_scales(&vec![100; DEFAULT_MAX_SCALES + 1]).is_err());
    }

    #[test]
    fn required_text_editor_enums_cycle_through_every_canonical_choice() {
        let mut mode = TextMatchMode::Exact;
        for expected in [
            TextMatchMode::Contains,
            TextMatchMode::Fuzzy,
            TextMatchMode::Absent,
            TextMatchMode::Exact,
        ] {
            mode = next_text_match_mode(mode);
            assert_eq!(mode, expected);
        }
        let mut profile = PreprocessProfile::Original;
        for expected in [
            PreprocessProfile::Grayscale,
            PreprocessProfile::HighContrast,
            PreprocessProfile::SmallText,
            PreprocessProfile::Original,
        ] {
            profile = next_preprocess(profile);
            assert_eq!(profile, expected);
        }
        let mut policy = MatchSelectionPolicy::ExactlyOne;
        for expected in [
            MatchSelectionPolicy::HighestScore,
            MatchSelectionPolicy::FirstReadingOrder,
            MatchSelectionPolicy::Topmost,
            MatchSelectionPolicy::Bottommost,
            MatchSelectionPolicy::ExactlyOne,
        ] {
            policy = next_policy(policy);
            assert_eq!(policy, expected);
        }
    }

    #[test]
    fn observe_timeout_edit_preserves_timeout_outcome_body() {
        let current = ObserveMode::WaitForTrue {
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
        let edited = observe_mode_with_timeout(&current, Limit::Unlimited).unwrap();
        assert!(matches!(
            edited,
            ObserveMode::WaitForTrue {
                timeout_ms: Limit::Unlimited,
                timeout_outcome: TimeoutOutcome::RunBody { ref body },
            } if body[0].id == "fallback"
        ));
    }

    #[test]
    fn detector_mode_cycle_preserves_wait_configuration() {
        let current = ObserveMode::WaitForTrue {
            timeout_ms: Limit::Finite(444),
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
        let next = next_observe_mode(&current);
        assert!(matches!(
            next,
            ObserveMode::WaitForFalse {
                timeout_ms: Limit::Finite(444),
                timeout_outcome: TimeoutOutcome::RunBody { ref body },
            } if body[0].id == "fallback"
        ));
    }

    #[test]
    fn empty_inspector_prompt_refers_to_a_canvas_block() {
        assert_eq!(EMPTY_INSPECTOR_PROMPT, "Select a canonical canvas block.");
    }
}
