use std::collections::{HashMap, HashSet, hash_map::Entry};

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationProblem {
    pub code: String,
    pub message: String,
    pub block_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectorFamily {
    Text,
    Image,
}

#[derive(Debug, Clone, Copy)]
struct SourceInfo<'a> {
    family: DetectorFamily,
    rule_id: &'a str,
    enabled: bool,
}

struct ValidationContext<'a> {
    region_ids: HashSet<&'a str>,
    point_ids: HashSet<&'a str>,
    text_rules: HashMap<&'a str, &'a TextRule>,
    image_rules: HashMap<&'a str, &'a ImageRule>,
    sources: HashMap<&'a str, SourceInfo<'a>>,
}

pub fn validate_macro(definition: &MacroDefinition) -> Vec<ValidationProblem> {
    let mut problems = Vec::new();

    if definition.schema_version != MACRO_SCHEMA_VERSION {
        push_problem(
            &mut problems,
            "macro.unsupported_schema_version",
            format!(
                "schema version {} is not supported; expected {MACRO_SCHEMA_VERSION}",
                definition.schema_version
            ),
            None,
        );
    }

    let region_ids = collect_unique_ids(
        definition.regions.iter().map(|region| region.id.as_str()),
        "region.duplicate_id",
        "region",
        &mut problems,
    );
    let text_rule_ids = collect_unique_ids(
        definition.text_rules.iter().map(|rule| rule.id.as_str()),
        "rule.duplicate_id",
        "text rule",
        &mut problems,
    );
    let point_ids = collect_unique_ids(
        definition.points.iter().map(|point| point.id.as_str()),
        "point.duplicate_id",
        "point",
        &mut problems,
    );
    let image_rule_ids = collect_unique_ids(
        definition.image_rules.iter().map(|rule| rule.id.as_str()),
        "rule.duplicate_id",
        "image rule",
        &mut problems,
    );

    for rule in &definition.text_rules {
        if !region_ids.contains(rule.region_id.as_str()) {
            push_problem(
                &mut problems,
                "rule.invalid_region",
                format!(
                    "text rule '{}' references missing region '{}'",
                    rule.id, rule.region_id
                ),
                None,
            );
        }
    }
    for rule in &definition.image_rules {
        if !region_ids.contains(rule.region_id.as_str()) {
            push_problem(
                &mut problems,
                "rule.invalid_region",
                format!(
                    "image rule '{}' references missing region '{}'",
                    rule.id, rule.region_id
                ),
                None,
            );
        }
    }

    if definition.safety.max_observations_per_second == 0 {
        push_problem(
            &mut problems,
            "safety.unbounded_observation_rate",
            "max_observations_per_second must be greater than zero".to_string(),
            None,
        );
    }
    if definition.safety.minimum_click_interval_ms == 0 {
        push_problem(
            &mut problems,
            "safety.unpaced_clicks",
            "minimum_click_interval_ms must be greater than zero".to_string(),
            None,
        );
    }

    let mut block_ids = HashSet::new();
    let mut lane_ids = HashSet::new();
    let mut sources = HashMap::new();
    index_blocks(
        &definition.blocks,
        true,
        &mut block_ids,
        &mut lane_ids,
        &mut sources,
        &mut problems,
    );

    let context = ValidationContext {
        region_ids,
        point_ids,
        text_rules: definition
            .text_rules
            .iter()
            .filter(|rule| text_rule_ids.contains(rule.id.as_str()))
            .map(|rule| (rule.id.as_str(), rule))
            .collect(),
        image_rules: definition
            .image_rules
            .iter()
            .filter(|rule| image_rule_ids.contains(rule.id.as_str()))
            .map(|rule| (rule.id.as_str(), rule))
            .collect(),
        sources,
    };
    validate_blocks(
        &definition.blocks,
        &context,
        true,
        false,
        false,
        &mut problems,
    );

    problems
}

fn collect_unique_ids<'a>(
    ids: impl Iterator<Item = &'a str>,
    code: &str,
    label: &str,
    problems: &mut Vec<ValidationProblem>,
) -> HashSet<&'a str> {
    let mut unique = HashSet::new();
    for id in ids {
        if !unique.insert(id) {
            push_problem(problems, code, format!("duplicate {label} id '{id}'"), None);
        }
    }
    unique
}

fn index_blocks<'a>(
    blocks: &'a [Block],
    ancestors_enabled: bool,
    block_ids: &mut HashSet<&'a str>,
    lane_ids: &mut HashSet<&'a str>,
    sources: &mut HashMap<&'a str, SourceInfo<'a>>,
    problems: &mut Vec<ValidationProblem>,
) {
    for block in blocks {
        let block_enabled = ancestors_enabled && block.enabled;
        if !block_ids.insert(block.id.as_str()) {
            push_problem(
                problems,
                "block.duplicate_id",
                format!("duplicate block id '{}'", block.id),
                Some(&block.id),
            );
        }

        match &block.kind {
            BlockKind::Observe { condition }
            | BlockKind::If { condition, .. }
            | BlockKind::RepeatUntil { condition, .. } => {
                insert_source(
                    sources,
                    block.id.as_str(),
                    source_info(condition, block_enabled),
                    problems,
                );
                index_condition_timeout_body(
                    condition,
                    block_enabled,
                    block_ids,
                    lane_ids,
                    sources,
                    problems,
                );
            }
            _ => {}
        }

        match &block.kind {
            BlockKind::If {
                then_body,
                else_body,
                ..
            } => {
                index_blocks(
                    then_body,
                    block_enabled,
                    block_ids,
                    lane_ids,
                    sources,
                    problems,
                );
                index_blocks(
                    else_body,
                    block_enabled,
                    block_ids,
                    lane_ids,
                    sources,
                    problems,
                );
            }
            BlockKind::RepeatN { body, .. }
            | BlockKind::RepeatUntil { body, .. }
            | BlockKind::Continuous { body } => {
                index_blocks(body, block_enabled, block_ids, lane_ids, sources, problems);
            }
            BlockKind::WatchGroup { group } => {
                for lane in &group.lanes {
                    if !lane_ids.insert(lane.id.as_str()) {
                        push_problem(
                            problems,
                            "watch_lane.duplicate_id",
                            format!("duplicate Watch lane id '{}'", lane.id),
                            Some(&block.id),
                        );
                    }
                    let lane_enabled = block_enabled && lane.enabled;
                    insert_source(
                        sources,
                        lane.id.as_str(),
                        passive_source_info(&lane.condition, lane_enabled),
                        problems,
                    );
                    index_blocks(
                        &lane.then_body,
                        lane_enabled,
                        block_ids,
                        lane_ids,
                        sources,
                        problems,
                    );
                }
                if let TimeoutOutcome::RunBody { body } = &group.timeout_outcome {
                    index_blocks(body, block_enabled, block_ids, lane_ids, sources, problems);
                }
            }
            _ => {}
        }
    }
}

fn insert_source<'a>(
    sources: &mut HashMap<&'a str, SourceInfo<'a>>,
    source_id: &'a str,
    source: SourceInfo<'a>,
    problems: &mut Vec<ValidationProblem>,
) {
    match sources.entry(source_id) {
        Entry::Vacant(entry) => {
            entry.insert(source);
        }
        Entry::Occupied(_) => push_problem(
            problems,
            "source.duplicate_id",
            format!("duplicate observation source id '{source_id}'"),
            Some(source_id),
        ),
    }
}

fn index_condition_timeout_body<'a>(
    condition: &'a Condition,
    condition_enabled: bool,
    block_ids: &mut HashSet<&'a str>,
    lane_ids: &mut HashSet<&'a str>,
    sources: &mut HashMap<&'a str, SourceInfo<'a>>,
    problems: &mut Vec<ValidationProblem>,
) {
    if let Some(TimeoutOutcome::RunBody { body }) = condition_timeout_outcome(condition) {
        index_blocks(
            body,
            condition_enabled,
            block_ids,
            lane_ids,
            sources,
            problems,
        );
    }
}

fn source_info(condition: &Condition, enabled: bool) -> SourceInfo<'_> {
    match condition {
        Condition::Text { rule_id, .. } => SourceInfo {
            family: DetectorFamily::Text,
            rule_id,
            enabled,
        },
        Condition::Image { rule_id, .. } => SourceInfo {
            family: DetectorFamily::Image,
            rule_id,
            enabled,
        },
    }
}

fn passive_source_info(condition: &PassiveCondition, enabled: bool) -> SourceInfo<'_> {
    match condition {
        PassiveCondition::Text { rule_id, .. } => SourceInfo {
            family: DetectorFamily::Text,
            rule_id,
            enabled,
        },
        PassiveCondition::Image { rule_id, .. } => SourceInfo {
            family: DetectorFamily::Image,
            rule_id,
            enabled,
        },
    }
}

fn validate_blocks(
    blocks: &[Block],
    context: &ValidationContext<'_>,
    ancestors_enabled: bool,
    inside_watch_group: bool,
    inside_lane_body: bool,
    problems: &mut Vec<ValidationProblem>,
) {
    for block in blocks {
        let block_enabled = ancestors_enabled && block.enabled;
        match &block.kind {
            BlockKind::Observe { condition } => {
                validate_condition(condition, context, &block.id, block_enabled, problems);
                validate_condition_timeout_body(
                    condition,
                    context,
                    block_enabled,
                    inside_watch_group,
                    inside_lane_body,
                    problems,
                );
            }
            BlockKind::Action { action } => {
                validate_action(action, context, &block.id, block_enabled, problems);
            }
            BlockKind::If {
                condition,
                then_body,
                else_body,
            } => {
                validate_condition(condition, context, &block.id, block_enabled, problems);
                validate_condition_timeout_body(
                    condition,
                    context,
                    block_enabled,
                    inside_watch_group,
                    inside_lane_body,
                    problems,
                );
                validate_blocks(
                    then_body,
                    context,
                    block_enabled,
                    inside_watch_group,
                    inside_lane_body,
                    problems,
                );
                validate_blocks(
                    else_body,
                    context,
                    block_enabled,
                    inside_watch_group,
                    inside_lane_body,
                    problems,
                );
            }
            BlockKind::RepeatN { body, .. } => validate_blocks(
                body,
                context,
                block_enabled,
                inside_watch_group,
                inside_lane_body,
                problems,
            ),
            BlockKind::RepeatUntil {
                condition, body, ..
            } => {
                validate_condition(condition, context, &block.id, block_enabled, problems);
                validate_condition_timeout_body(
                    condition,
                    context,
                    block_enabled,
                    inside_watch_group,
                    inside_lane_body,
                    problems,
                );
                validate_blocks(
                    body,
                    context,
                    block_enabled,
                    inside_watch_group,
                    inside_lane_body,
                    problems,
                );
            }
            BlockKind::Continuous { body } => {
                if inside_lane_body {
                    push_problem(
                        problems,
                        "watch_group.continuous_lane_body",
                        "Continuous blocks are not allowed inside Watch lane bodies".to_string(),
                        Some(&block.id),
                    );
                }
                if block_enabled && !contains_paced_or_blocking_operation(body) {
                    push_problem(
                        problems,
                        "continuous.busy_loop",
                        "Continuous body must contain a paced or blocking operation".to_string(),
                        Some(&block.id),
                    );
                }
                validate_blocks(
                    body,
                    context,
                    block_enabled,
                    inside_watch_group,
                    inside_lane_body,
                    problems,
                );
            }
            BlockKind::WatchGroup { group } => {
                if inside_watch_group {
                    push_problem(
                        problems,
                        "watch_group.nested",
                        "nested Watch Groups are not supported".to_string(),
                        Some(&block.id),
                    );
                }
                if !group.lanes.iter().any(|lane| lane.enabled) {
                    push_problem(
                        problems,
                        "watch_group.no_enabled_lanes",
                        "Watch Group must contain at least one enabled lane".to_string(),
                        Some(&block.id),
                    );
                }
                if matches!(
                    &group.timeout_outcome,
                    TimeoutOutcome::StopError { message } if message.trim().is_empty()
                ) {
                    push_problem(
                        problems,
                        "watch_group.timeout_outcome_missing",
                        "Watch Group timeout StopError outcome requires a message".to_string(),
                        Some(&block.id),
                    );
                }
                for lane in &group.lanes {
                    let lane_enabled = block_enabled && lane.enabled;
                    validate_passive_condition(
                        &lane.condition,
                        context,
                        &block.id,
                        lane_enabled,
                        problems,
                    );
                    validate_blocks(&lane.then_body, context, lane_enabled, true, true, problems);
                }
                if let TimeoutOutcome::RunBody { body } = &group.timeout_outcome {
                    validate_blocks(body, context, block_enabled, true, false, problems);
                }
            }
            BlockKind::Wait { .. }
            | BlockKind::StopSuccess
            | BlockKind::StopError { .. }
            | BlockKind::Comment { .. } => {}
        }
    }
}

fn validate_condition(
    condition: &Condition,
    context: &ValidationContext<'_>,
    owner_block_id: &str,
    consumer_enabled: bool,
    problems: &mut Vec<ValidationProblem>,
) {
    let (source_block_id, rule_id, expected_family, rule_exists) = match condition {
        Condition::Text {
            source_block_id,
            rule_id,
            ..
        } => (
            source_block_id,
            rule_id,
            DetectorFamily::Text,
            context.text_rules.contains_key(rule_id.as_str()),
        ),
        Condition::Image {
            source_block_id,
            rule_id,
            ..
        } => (
            source_block_id,
            rule_id,
            DetectorFamily::Image,
            context.image_rules.contains_key(rule_id.as_str()),
        ),
    };

    if !rule_exists {
        push_problem(
            problems,
            "condition.invalid_rule",
            format!("condition references missing rule '{rule_id}'"),
            Some(owner_block_id),
        );
    }

    if matches!(
        condition_timeout_outcome(condition),
        Some(TimeoutOutcome::StopError { message }) if message.trim().is_empty()
    ) {
        push_problem(
            problems,
            "condition.timeout_outcome_missing",
            "condition wait StopError outcome requires a message".to_string(),
            Some(owner_block_id),
        );
    }

    validate_source_binding(
        source_block_id,
        rule_id,
        expected_family,
        consumer_enabled,
        context,
        owner_block_id,
        "condition",
        problems,
    );
}

fn validate_passive_condition(
    condition: &PassiveCondition,
    context: &ValidationContext<'_>,
    owner_block_id: &str,
    consumer_enabled: bool,
    problems: &mut Vec<ValidationProblem>,
) {
    let (source_block_id, rule_id, expected_family, rule_exists) = match condition {
        PassiveCondition::Text {
            source_block_id,
            rule_id,
        } => (
            source_block_id,
            rule_id,
            DetectorFamily::Text,
            context.text_rules.contains_key(rule_id.as_str()),
        ),
        PassiveCondition::Image {
            source_block_id,
            rule_id,
        } => (
            source_block_id,
            rule_id,
            DetectorFamily::Image,
            context.image_rules.contains_key(rule_id.as_str()),
        ),
    };

    if !rule_exists {
        push_problem(
            problems,
            "condition.invalid_rule",
            format!("passive condition references missing rule '{rule_id}'"),
            Some(owner_block_id),
        );
    }

    validate_source_binding(
        source_block_id,
        rule_id,
        expected_family,
        consumer_enabled,
        context,
        owner_block_id,
        "condition",
        problems,
    );
}

fn validate_source_binding(
    source_block_id: &str,
    rule_id: &str,
    expected_family: DetectorFamily,
    consumer_enabled: bool,
    context: &ValidationContext<'_>,
    owner_block_id: &str,
    code_prefix: &str,
    problems: &mut Vec<ValidationProblem>,
) {
    match context.sources.get(source_block_id) {
        None => push_problem(
            problems,
            &format!("{code_prefix}.invalid_source"),
            format!("condition references missing observation source '{source_block_id}'"),
            Some(owner_block_id),
        ),
        Some(source) if source.family != expected_family => push_problem(
            problems,
            &format!("{code_prefix}.detector_family_mismatch"),
            format!("condition detector family does not match source '{source_block_id}'"),
            Some(owner_block_id),
        ),
        Some(source) if source.rule_id != rule_id => push_problem(
            problems,
            &format!("{code_prefix}.source_rule_mismatch"),
            format!(
                "consumer rule '{rule_id}' does not match source '{source_block_id}' rule '{}'",
                source.rule_id
            ),
            Some(owner_block_id),
        ),
        Some(source) if consumer_enabled && !source.enabled => push_problem(
            problems,
            &format!("{code_prefix}.disabled_source"),
            format!("enabled consumer references disabled source '{source_block_id}'"),
            Some(owner_block_id),
        ),
        Some(_) => {}
    }
}

fn condition_timeout_outcome(condition: &Condition) -> Option<&TimeoutOutcome> {
    let mode = match condition {
        Condition::Text { mode, .. } | Condition::Image { mode, .. } => mode,
    };
    match mode {
        ObserveMode::CheckNow => None,
        ObserveMode::WaitForTrue {
            timeout_outcome, ..
        }
        | ObserveMode::WaitForFalse {
            timeout_outcome, ..
        } => Some(timeout_outcome),
    }
}

fn validate_condition_timeout_body(
    condition: &Condition,
    context: &ValidationContext<'_>,
    condition_enabled: bool,
    inside_watch_group: bool,
    inside_lane_body: bool,
    problems: &mut Vec<ValidationProblem>,
) {
    if let Some(TimeoutOutcome::RunBody { body }) = condition_timeout_outcome(condition) {
        validate_blocks(
            body,
            context,
            condition_enabled,
            inside_watch_group,
            inside_lane_body,
            problems,
        );
    }
}

fn validate_action(
    action: &Action,
    context: &ValidationContext<'_>,
    block_id: &str,
    consumer_enabled: bool,
    problems: &mut Vec<ValidationProblem>,
) {
    match action {
        Action::ClickTextMatch {
            source_block_id, ..
        }
        | Action::MoveOnly {
            target: ActionTarget::TextMatch { source_block_id },
        } => validate_match_source(
            source_block_id,
            DetectorFamily::Text,
            context,
            block_id,
            consumer_enabled,
            problems,
        ),
        Action::ClickImageMatch {
            source_block_id, ..
        }
        | Action::MoveOnly {
            target: ActionTarget::ImageMatch { source_block_id },
        } => validate_match_source(
            source_block_id,
            DetectorFamily::Image,
            context,
            block_id,
            consumer_enabled,
            problems,
        ),
        Action::ClickRegion { region_id, .. }
        | Action::MoveOnly {
            target: ActionTarget::Region { region_id },
        } => {
            if !context.region_ids.contains(region_id.as_str()) {
                push_problem(
                    problems,
                    "action.invalid_region",
                    format!("action references missing region '{region_id}'"),
                    Some(block_id),
                );
            }
        }
        Action::ClickPoint { point_id, .. }
        | Action::MoveOnly {
            target: ActionTarget::Point { point_id },
        } => {
            if !context.point_ids.contains(point_id.as_str()) {
                push_problem(
                    problems,
                    "action.invalid_point",
                    format!("action references missing point '{point_id}'"),
                    Some(block_id),
                );
            }
        }
    }
}

fn validate_match_source(
    source_block_id: &str,
    expected_family: DetectorFamily,
    context: &ValidationContext<'_>,
    block_id: &str,
    consumer_enabled: bool,
    problems: &mut Vec<ValidationProblem>,
) {
    let Some(source) = context.sources.get(source_block_id) else {
        push_problem(
            problems,
            "action.invalid_source",
            format!("action references missing observation source '{source_block_id}'"),
            Some(block_id),
        );
        return;
    };

    if source.family != expected_family {
        push_problem(
            problems,
            "action.detector_family_mismatch",
            format!("action target family does not match observation source '{source_block_id}'"),
            Some(block_id),
        );
        return;
    }

    if consumer_enabled && !source.enabled {
        push_problem(
            problems,
            "action.disabled_source",
            format!("enabled action references disabled source '{source_block_id}'"),
            Some(block_id),
        );
        return;
    }

    if expected_family == DetectorFamily::Text
        && context
            .text_rules
            .get(source.rule_id)
            .is_some_and(|rule| rule.match_mode == TextMatchMode::Absent)
    {
        push_problem(
            problems,
            "action.text_absent_match",
            "Text Absent observations do not produce a match target".to_string(),
            Some(block_id),
        );
    }
}

fn contains_paced_or_blocking_operation(blocks: &[Block]) -> bool {
    blocks
        .iter()
        .filter(|block| block.enabled)
        .any(|block| match &block.kind {
            BlockKind::Observe { .. }
            | BlockKind::If { .. }
            | BlockKind::RepeatUntil { .. }
            | BlockKind::WatchGroup { .. }
            | BlockKind::StopSuccess
            | BlockKind::StopError { .. } => true,
            BlockKind::Action { action } => matches!(
                action,
                Action::ClickTextMatch { .. }
                    | Action::ClickImageMatch { .. }
                    | Action::ClickPoint { .. }
                    | Action::ClickRegion { .. }
            ),
            BlockKind::Wait { duration_ms } => *duration_ms > 0,
            BlockKind::RepeatN { count, body } => {
                *count > 0 && contains_paced_or_blocking_operation(body)
            }
            BlockKind::Continuous { body } => contains_paced_or_blocking_operation(body),
            BlockKind::Comment { .. } => false,
        })
}

fn push_problem(
    problems: &mut Vec<ValidationProblem>,
    code: &str,
    message: String,
    block_id: Option<&str>,
) {
    problems.push(ValidationProblem {
        code: code.to_string(),
        message,
        block_id: block_id.map(str::to_string),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::types::{PointRatio, RectRatio};

    fn fixture_macro(blocks: Vec<Block>) -> MacroDefinition {
        MacroDefinition {
            schema_version: MACRO_SCHEMA_VERSION,
            id: "macro".to_string(),
            name: "Fixture".to_string(),
            revision: 1,
            target: TargetProfile {
                process_path: r"C:\Game\Diablo IV.exe".to_string(),
                window_class: "Diablo IV Main Window".to_string(),
                title_contains: "Diablo IV".to_string(),
                captured_client_width: 1920,
                captured_client_height: 1080,
                captured_dpi: 96,
            },
            regions: vec![RegionDefinition {
                id: "region".to_string(),
                revision: 1,
                rect: RectRatio {
                    x: 0.1,
                    y: 0.1,
                    width: 0.2,
                    height: 0.1,
                },
            }],
            points: vec![PointDefinition {
                id: "point".to_string(),
                revision: 1,
                point: PointRatio { x: 0.5, y: 0.5 },
            }],
            text_rules: vec![text_rule("text-present", TextMatchMode::Contains)],
            image_rules: vec![image_rule("image")],
            blocks,
            safety: SafetyPolicy {
                max_runtime_ms: Limit::Finite(60_000),
                max_clicks: Limit::Finite(100),
                max_observation_retries: Limit::Finite(3),
                max_observations_per_second: 30,
                minimum_click_interval_ms: 50,
                focus_loss: FocusLossPolicy::Stop,
            },
        }
    }

    fn text_rule(id: &str, match_mode: TextMatchMode) -> TextRule {
        TextRule {
            id: id.to_string(),
            revision: 1,
            region_id: "region".to_string(),
            language: "en-US".to_string(),
            preprocess: PreprocessProfile::Original,
            expected: "expected".to_string(),
            match_mode,
            threshold: 0.9,
            case_sensitive: false,
            allow_cross_line: false,
            match_policy: MatchSelectionPolicy::HighestScore,
            poll_interval_ms: 100,
            timeout_ms: Limit::Finite(1_000),
            stable_frames: 1,
        }
    }

    fn image_rule(id: &str) -> ImageRule {
        ImageRule {
            id: id.to_string(),
            revision: 1,
            region_id: "region".to_string(),
            template_asset_id: "template".to_string(),
            transparent_mask_asset_id: None,
            threshold: 0.9,
            scales_percent: vec![100],
            stable_frames: 1,
            maximum_center_drift_px: 2,
            minimum_runner_up_margin: 0.05,
            match_policy: MatchSelectionPolicy::HighestScore,
            poll_interval_ms: 100,
            timeout_ms: Limit::Finite(1_000),
        }
    }

    fn block(id: &str, kind: BlockKind) -> Block {
        Block {
            id: id.to_string(),
            enabled: true,
            kind,
        }
    }

    fn text_condition(source_block_id: &str, rule_id: &str) -> Condition {
        Condition::Text {
            source_block_id: source_block_id.to_string(),
            rule_id: rule_id.to_string(),
            mode: ObserveMode::CheckNow,
        }
    }

    fn image_condition(source_block_id: &str, rule_id: &str) -> Condition {
        Condition::Image {
            source_block_id: source_block_id.to_string(),
            rule_id: rule_id.to_string(),
            mode: ObserveMode::CheckNow,
        }
    }

    fn passive_text_condition(source_block_id: &str, rule_id: &str) -> PassiveCondition {
        PassiveCondition::Text {
            source_block_id: source_block_id.to_string(),
            rule_id: rule_id.to_string(),
        }
    }

    fn passive_image_condition(source_block_id: &str, rule_id: &str) -> PassiveCondition {
        PassiveCondition::Image {
            source_block_id: source_block_id.to_string(),
            rule_id: rule_id.to_string(),
        }
    }

    fn has_code(problems: &[ValidationProblem], code: &str) -> bool {
        problems.iter().any(|problem| problem.code == code)
    }

    #[test]
    fn rejects_unpaced_continuous_loop() {
        let definition = fixture_macro(vec![block(
            "continuous",
            BlockKind::Continuous {
                body: vec![block(
                    "comment",
                    BlockKind::Comment {
                        text: "spin".to_string(),
                    },
                )],
            },
        )]);

        assert!(has_code(
            &validate_macro(&definition),
            "continuous.busy_loop"
        ));
    }

    #[test]
    fn rejects_watch_group_without_enabled_lanes() {
        let definition = fixture_macro(vec![block(
            "watch",
            BlockKind::WatchGroup {
                group: WatchGroup {
                    lanes: vec![],
                    timeout_ms: Limit::Finite(1_000),
                    timeout_outcome: TimeoutOutcome::Continue,
                    cooldown_ms: 0,
                },
            },
        )]);

        assert!(has_code(
            &validate_macro(&definition),
            "watch_group.no_enabled_lanes"
        ));
    }

    #[test]
    fn rejects_duplicate_block_ids_across_nested_bodies() {
        let definition = fixture_macro(vec![
            block(
                "duplicate",
                BlockKind::Comment {
                    text: "top".to_string(),
                },
            ),
            block(
                "loop",
                BlockKind::RepeatN {
                    count: 1,
                    body: vec![block(
                        "duplicate",
                        BlockKind::Comment {
                            text: "nested".to_string(),
                        },
                    )],
                },
            ),
        ]);

        assert!(has_code(&validate_macro(&definition), "block.duplicate_id"));
    }

    #[test]
    fn rejects_nested_watch_groups_and_continuous_lane_bodies() {
        let nested_watch = block(
            "nested-watch",
            BlockKind::WatchGroup {
                group: WatchGroup {
                    lanes: vec![],
                    timeout_ms: Limit::Finite(1),
                    timeout_outcome: TimeoutOutcome::Continue,
                    cooldown_ms: 0,
                },
            },
        );
        let continuous = block(
            "lane-continuous",
            BlockKind::Continuous {
                body: vec![block("wait", BlockKind::Wait { duration_ms: 1 })],
            },
        );
        let definition = fixture_macro(vec![block(
            "watch",
            BlockKind::WatchGroup {
                group: WatchGroup {
                    lanes: vec![WatchLane {
                        id: "lane".to_string(),
                        enabled: true,
                        condition: passive_text_condition("lane", "text-present"),
                        then_body: vec![nested_watch, continuous],
                    }],
                    timeout_ms: Limit::Finite(1_000),
                    timeout_outcome: TimeoutOutcome::Continue,
                    cooldown_ms: 0,
                },
            },
        )]);

        let problems = validate_macro(&definition);
        assert!(has_code(&problems, "watch_group.nested"));
        assert!(has_code(&problems, "watch_group.continuous_lane_body"));
    }

    #[test]
    fn rejects_missing_timeout_behavior() {
        let definition = fixture_macro(vec![block(
            "watch",
            BlockKind::WatchGroup {
                group: WatchGroup {
                    lanes: vec![WatchLane {
                        id: "lane".to_string(),
                        enabled: true,
                        condition: passive_text_condition("lane", "text-present"),
                        then_body: vec![],
                    }],
                    timeout_ms: Limit::Finite(1_000),
                    timeout_outcome: TimeoutOutcome::StopError {
                        message: " ".to_string(),
                    },
                    cooldown_ms: 0,
                },
            },
        )]);

        assert!(has_code(
            &validate_macro(&definition),
            "watch_group.timeout_outcome_missing"
        ));
    }

    #[test]
    fn rejects_text_absent_match_click() {
        let mut definition = fixture_macro(vec![
            block(
                "observe",
                BlockKind::Observe {
                    condition: text_condition("observe", "text-absent"),
                },
            ),
            block(
                "click",
                BlockKind::Action {
                    action: Action::ClickTextMatch {
                        source_block_id: "observe".to_string(),
                        button: MouseButton::Left,
                    },
                },
            ),
        ]);
        definition
            .text_rules
            .push(text_rule("text-absent", TextMatchMode::Absent));

        assert!(has_code(
            &validate_macro(&definition),
            "action.text_absent_match"
        ));
    }

    #[test]
    fn rejects_invalid_region_rule_and_source_references() {
        let mut definition = fixture_macro(vec![
            block(
                "observe",
                BlockKind::Observe {
                    condition: text_condition("missing-source", "missing-rule"),
                },
            ),
            block(
                "click-region",
                BlockKind::Action {
                    action: Action::ClickRegion {
                        region_id: "missing-region".to_string(),
                        button: MouseButton::Left,
                    },
                },
            ),
        ]);
        definition.text_rules[0].region_id = "missing-region".to_string();

        let problems = validate_macro(&definition);
        assert!(has_code(&problems, "rule.invalid_region"));
        assert!(has_code(&problems, "condition.invalid_rule"));
        assert!(has_code(&problems, "condition.invalid_source"));
        assert!(has_code(&problems, "action.invalid_region"));
    }

    #[test]
    fn rejects_detector_family_unsafe_match_clicks() {
        let definition = fixture_macro(vec![
            block(
                "observe-image",
                BlockKind::Observe {
                    condition: image_condition("observe-image", "image"),
                },
            ),
            block(
                "click-text",
                BlockKind::Action {
                    action: Action::ClickTextMatch {
                        source_block_id: "observe-image".to_string(),
                        button: MouseButton::Left,
                    },
                },
            ),
        ]);

        assert!(has_code(
            &validate_macro(&definition),
            "action.detector_family_mismatch"
        ));
    }

    #[test]
    fn accepts_valid_point_reference() {
        let definition = fixture_macro(vec![block(
            "click-point",
            BlockKind::Action {
                action: Action::ClickPoint {
                    point_id: "point".to_string(),
                    button: MouseButton::Left,
                },
            },
        )]);

        assert!(!has_code(
            &validate_macro(&definition),
            "action.invalid_point"
        ));
    }

    #[test]
    fn rejects_missing_point_reference() {
        let definition = fixture_macro(vec![block(
            "click-point",
            BlockKind::Action {
                action: Action::ClickPoint {
                    point_id: "missing-point".to_string(),
                    button: MouseButton::Left,
                },
            },
        )]);

        assert!(has_code(
            &validate_macro(&definition),
            "action.invalid_point"
        ));
    }

    #[test]
    fn rejects_duplicate_point_ids() {
        let mut definition = fixture_macro(vec![]);
        definition.points.push(definition.points[0].clone());

        assert!(has_code(&validate_macro(&definition), "point.duplicate_id"));
    }

    #[test]
    fn accepts_explicit_standalone_wait_timeout_outcomes() {
        let modes = vec![
            ObserveMode::WaitForTrue {
                timeout_ms: Limit::Finite(1_000),
                timeout_outcome: TimeoutOutcome::StopError {
                    message: "timeout".to_string(),
                },
            },
            ObserveMode::WaitForFalse {
                timeout_ms: Limit::Unlimited,
                timeout_outcome: TimeoutOutcome::Continue,
            },
            ObserveMode::WaitForTrue {
                timeout_ms: Limit::Finite(1_000),
                timeout_outcome: TimeoutOutcome::RunBody { body: vec![] },
            },
        ];

        for (index, mode) in modes.into_iter().enumerate() {
            let id = format!("observe-{index}");
            let definition = fixture_macro(vec![block(
                &id,
                BlockKind::Observe {
                    condition: Condition::Text {
                        source_block_id: id.clone(),
                        rule_id: "text-present".to_string(),
                        mode,
                    },
                },
            )]);
            assert!(!has_code(
                &validate_macro(&definition),
                "condition.timeout_outcome_missing"
            ));
        }
    }

    #[test]
    fn rejects_standalone_wait_without_error_message() {
        let definition = fixture_macro(vec![block(
            "observe",
            BlockKind::Observe {
                condition: Condition::Text {
                    source_block_id: "observe".to_string(),
                    rule_id: "text-present".to_string(),
                    mode: ObserveMode::WaitForTrue {
                        timeout_ms: Limit::Finite(1_000),
                        timeout_outcome: TimeoutOutcome::StopError {
                            message: " ".to_string(),
                        },
                    },
                },
            },
        )]);

        assert!(has_code(
            &validate_macro(&definition),
            "condition.timeout_outcome_missing"
        ));
    }

    #[test]
    fn rejects_text_condition_source_rule_mismatch() {
        let mut definition = fixture_macro(vec![
            block(
                "observe",
                BlockKind::Observe {
                    condition: text_condition("observe", "text-present"),
                },
            ),
            block(
                "if",
                BlockKind::If {
                    condition: text_condition("observe", "text-other"),
                    then_body: vec![],
                    else_body: vec![],
                },
            ),
        ]);
        definition
            .text_rules
            .push(text_rule("text-other", TextMatchMode::Contains));

        assert!(has_code(
            &validate_macro(&definition),
            "condition.source_rule_mismatch"
        ));
    }

    #[test]
    fn rejects_image_passive_condition_source_rule_mismatch() {
        let mut definition = fixture_macro(vec![
            block(
                "observe-image",
                BlockKind::Observe {
                    condition: image_condition("observe-image", "image"),
                },
            ),
            block(
                "watch",
                BlockKind::WatchGroup {
                    group: WatchGroup {
                        lanes: vec![WatchLane {
                            id: "lane".to_string(),
                            enabled: true,
                            condition: passive_image_condition("observe-image", "image-other"),
                            then_body: vec![],
                        }],
                        timeout_ms: Limit::Finite(1_000),
                        timeout_outcome: TimeoutOutcome::Continue,
                        cooldown_ms: 0,
                    },
                },
            ),
        ]);
        definition.image_rules.push(image_rule("image-other"));

        assert!(has_code(
            &validate_macro(&definition),
            "condition.source_rule_mismatch"
        ));
    }

    #[test]
    fn rejects_enabled_click_referencing_disabled_source() {
        let mut source = block(
            "observe",
            BlockKind::Observe {
                condition: text_condition("observe", "text-present"),
            },
        );
        source.enabled = false;
        let definition = fixture_macro(vec![
            source,
            block(
                "click",
                BlockKind::Action {
                    action: Action::ClickTextMatch {
                        source_block_id: "observe".to_string(),
                        button: MouseButton::Left,
                    },
                },
            ),
        ]);

        assert!(has_code(
            &validate_macro(&definition),
            "action.disabled_source"
        ));
    }

    #[test]
    fn rejects_enabled_condition_referencing_disabled_source() {
        let mut source = block(
            "observe",
            BlockKind::Observe {
                condition: text_condition("observe", "text-present"),
            },
        );
        source.enabled = false;
        let definition = fixture_macro(vec![
            source,
            block(
                "if",
                BlockKind::If {
                    condition: text_condition("observe", "text-present"),
                    then_body: vec![],
                    else_body: vec![],
                },
            ),
        ]);

        assert!(has_code(
            &validate_macro(&definition),
            "condition.disabled_source"
        ));
    }

    #[test]
    fn disabled_consumers_may_reference_disabled_sources() {
        let mut source = block(
            "observe",
            BlockKind::Observe {
                condition: text_condition("observe", "text-present"),
            },
        );
        source.enabled = false;
        let mut click = block(
            "click",
            BlockKind::Action {
                action: Action::ClickTextMatch {
                    source_block_id: "observe".to_string(),
                    button: MouseButton::Left,
                },
            },
        );
        click.enabled = false;
        let definition = fixture_macro(vec![source, click]);

        assert!(!has_code(
            &validate_macro(&definition),
            "action.disabled_source"
        ));
    }

    #[test]
    fn rejects_continuous_loop_paced_only_by_move_only() {
        let definition = fixture_macro(vec![block(
            "continuous",
            BlockKind::Continuous {
                body: vec![block(
                    "move",
                    BlockKind::Action {
                        action: Action::MoveOnly {
                            target: ActionTarget::Point {
                                point_id: "point".to_string(),
                            },
                        },
                    },
                )],
            },
        )]);

        assert!(has_code(
            &validate_macro(&definition),
            "continuous.busy_loop"
        ));
    }

    #[test]
    fn rejects_observe_block_and_watch_lane_source_id_collision() {
        let definition = fixture_macro(vec![
            block(
                "shared-source",
                BlockKind::Observe {
                    condition: text_condition("shared-source", "text-present"),
                },
            ),
            block(
                "watch",
                BlockKind::WatchGroup {
                    group: WatchGroup {
                        lanes: vec![WatchLane {
                            id: "shared-source".to_string(),
                            enabled: true,
                            condition: passive_text_condition("shared-source", "text-present"),
                            then_body: vec![],
                        }],
                        timeout_ms: Limit::Finite(1_000),
                        timeout_outcome: TimeoutOutcome::Continue,
                        cooldown_ms: 0,
                    },
                },
            ),
        ]);

        assert!(has_code(
            &validate_macro(&definition),
            "source.duplicate_id"
        ));
    }
}
