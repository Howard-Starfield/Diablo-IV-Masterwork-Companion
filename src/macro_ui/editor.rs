use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use crate::engine::macro_engine::{
    Action, Block, BlockKind, Condition, ImageRule, Limit, MacroDefinition, MouseButton,
    ObserveMode, PassiveCondition, TextRule, TimeoutOutcome, ValidationProblem,
};
use crate::engine::types::RectRatio;
use std::ops::{Deref, DerefMut};

const EDITOR_UNDO_LIMIT: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftStatus {
    Ready,
    NeedsValidation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftEditability {
    Editable,
    Running { revision: u64 },
}

#[derive(Debug, Clone, PartialEq)]
struct UndoEntry {
    definition: MacroDefinition,
    invalidated_source_ids: BTreeSet<String>,
}

/// Editor-only state. The canonical definition never stores removed conversion data or undo data.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorDraft {
    pub definition: MacroDefinition,
    pub status: DraftStatus,
    pub editability: DraftEditability,
    invalidated_source_ids: BTreeSet<String>,
    undo: VecDeque<UndoEntry>,
}

impl EditorDraft {
    pub fn new(definition: MacroDefinition) -> Self {
        Self {
            definition,
            status: DraftStatus::Ready,
            editability: DraftEditability::Editable,
            invalidated_source_ids: BTreeSet::new(),
            undo: VecDeque::new(),
        }
    }

    pub fn invalidated_source_ids(&self) -> &BTreeSet<String> {
        &self.invalidated_source_ids
    }

    pub fn watch_lane_ids(&self, group_id: &str) -> Vec<&str> {
        find_block(&self.definition.blocks, group_id)
            .and_then(|block| match &block.kind {
                BlockKind::WatchGroup { group } => {
                    Some(group.lanes.iter().map(|lane| lane.id.as_str()).collect())
                }
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }
}

impl Deref for EditorDraft {
    type Target = MacroDefinition;
    fn deref(&self) -> &Self::Target {
        &self.definition
    }
}
impl DerefMut for EditorDraft {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.definition
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ContainerPath {
    Root,
    IfThen { if_id: String },
    IfElse { if_id: String },
    LoopBody { loop_id: String },
    WatchLaneBody { watch_id: String, lane_id: String },
    TimeoutBody { owner_id: String },
}

impl ContainerPath {
    fn owner_ids(&self) -> impl Iterator<Item = &str> {
        let mut owners = [None, None];
        match self {
            Self::Root => {}
            Self::IfThen { if_id } | Self::IfElse { if_id } => {
                owners[0] = Some(if_id.as_str());
            }
            Self::LoopBody { loop_id } => owners[0] = Some(loop_id.as_str()),
            Self::WatchLaneBody { watch_id, lane_id } => {
                owners[0] = Some(watch_id.as_str());
                owners[1] = Some(lane_id.as_str());
            }
            Self::TimeoutBody { owner_id } => owners[0] = Some(owner_id.as_str()),
        }
        owners.into_iter().flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlockPath {
    pub container: ContainerPath,
    pub block_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InsertionTarget {
    pub container: ContainerPath,
    /// Index in the destination after removal for a move operation.
    pub index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfBranch {
    Then,
    Else,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopDeletionChoice {
    DeleteWithContents,
    KeepContents,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildDisposition {
    DeleteOwnedContents,
    KeepOwnedContents,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockFamily {
    TextObservation,
    ImageObservation,
    TextMatchedClick,
    ImageMatchedClick,
    SavedLocationClick,
    Loop,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversionPreview {
    Compatible {
        preserved_fields: Vec<&'static str>,
        required_fields: Vec<&'static str>,
        removed_fields: Vec<&'static str>,
    },
    ReplaceRequired {
        from: BlockFamily,
        to: BlockFamily,
        reason: &'static str,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConversionTarget {
    TextObservation {
        mode: ObserveMode,
    },
    ImageObservation {
        mode: ObserveMode,
    },
    ClickTextMatch {
        button: MouseButton,
    },
    ClickImageMatch {
        button: MouseButton,
    },
    ClickPoint {
        point_id: String,
        button: MouseButton,
    },
    ClickRegion {
        region_id: String,
        button: MouseButton,
    },
    RepeatN {
        count: u32,
    },
    RepeatUntil {
        condition: Condition,
        max_iterations: Limit<u64>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditorCommand {
    InsertBlock {
        target: InsertionTarget,
        block: Block,
    },
    RemoveBlock {
        path: BlockPath,
        loop_choice: Option<LoopDeletionChoice>,
    },
    DuplicateBlock {
        source: BlockPath,
        target: InsertionTarget,
    },
    SetBlockEnabled {
        path: BlockPath,
        enabled: bool,
    },
    SetLaneEnabled {
        group_id: String,
        lane_id: String,
        enabled: bool,
    },
    ReorderSibling {
        path: BlockPath,
        to_index: usize,
    },
    MoveBlock {
        source: BlockPath,
        target: InsertionTarget,
    },
    TransferIfBranch {
        if_id: String,
        branch: IfBranch,
        block_id: String,
        to_index: usize,
    },
    MoveLane {
        group_id: String,
        lane_id: String,
        to_index: usize,
    },
    ConvertBlock {
        path: BlockPath,
        target: ConversionTarget,
    },
    SetConditionMode {
        path: BlockPath,
        mode: ObserveMode,
    },
    SetWaitDuration {
        path: BlockPath,
        duration_ms: u64,
    },
    SetRepeatCount {
        path: BlockPath,
        count: u32,
    },
    SetRepeatUntilMax {
        path: BlockPath,
        max_iterations: Limit<u64>,
    },
    SetWatchSettings {
        path: BlockPath,
        timeout_ms: Limit<u64>,
        cooldown_ms: u64,
    },
    ReplaceBlock {
        path: BlockPath,
        replacement: Block,
        children: ChildDisposition,
    },
    ReplaceTextRule {
        rule: TextRule,
    },
    ReplaceImageRule {
        rule: ImageRule,
    },
    RecaptureRegion {
        region_id: String,
        rect: RectRatio,
    },
    MarkValidated,
    Undo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    Changed,
    NoChange,
    Validated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorError {
    RunInProgress,
    DuplicateIdentity(String),
    MissingContainer,
    MissingBlock(String),
    MissingLane(String),
    MissingRule(String),
    MissingRegion(String),
    InvalidIndex,
    IllegalDescendantMove,
    NestedWatchGroup,
    LoopDeletionChoiceRequired,
    InvalidLoopDeletionChoice,
    IncompatibleConversion,
    ReplacementIdMismatch,
    ValidationFailed,
    NothingToUndo,
}

pub fn preview_conversion(block: &Block, target: BlockFamily) -> ConversionPreview {
    let from = block_family(block);
    if from != target {
        return ConversionPreview::ReplaceRequired {
            from,
            to: target,
            reason: "Unrelated block families require Replace Block so removed settings are explicit.",
        };
    }

    let (preserved_fields, required_fields, removed_fields) = match from {
        BlockFamily::TextObservation | BlockFamily::ImageObservation => (
            vec!["source", "rule", "timeout policy"],
            vec![],
            vec!["incompatible wait mode fields"],
        ),
        BlockFamily::TextMatchedClick | BlockFamily::ImageMatchedClick => {
            (vec!["matched source"], vec![], vec!["mouse button"])
        }
        BlockFamily::SavedLocationClick => (
            vec!["mouse button"],
            vec!["saved point or region"],
            vec!["previous saved target"],
        ),
        BlockFamily::Loop => (
            vec!["complete child body"],
            vec!["count or until condition"],
            vec!["previous loop limit"],
        ),
        BlockFamily::Other => (vec![], vec![], vec![]),
    };
    ConversionPreview::Compatible {
        preserved_fields,
        required_fields,
        removed_fields,
    }
}

fn block_family(block: &Block) -> BlockFamily {
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

pub fn apply_editor_command(
    draft: &mut EditorDraft,
    command: EditorCommand,
) -> Result<EditOutcome, EditorError> {
    if matches!(draft.editability, DraftEditability::Running { .. }) {
        return Err(EditorError::RunInProgress);
    }
    ensure_unique_structural_ids(&draft.definition)?;

    if command == EditorCommand::MarkValidated {
        if !crate::engine::macro_engine::validate_macro(&draft.definition).is_empty() {
            return Err(EditorError::ValidationFailed);
        }
        if draft.status == DraftStatus::Ready && draft.invalidated_source_ids.is_empty() {
            return Ok(EditOutcome::NoChange);
        }
        draft.invalidated_source_ids.clear();
        draft.status = DraftStatus::Ready;
        return Ok(EditOutcome::Validated);
    }

    if command == EditorCommand::Undo {
        let Some(previous) = draft.undo.pop_back() else {
            return Err(EditorError::NothingToUndo);
        };
        let revision = draft.definition.revision.saturating_add(1);
        draft.definition = previous.definition;
        draft.definition.revision = revision;
        draft.invalidated_source_ids = previous.invalidated_source_ids;
        draft.status = DraftStatus::NeedsValidation;
        return Ok(EditOutcome::Changed);
    }

    let before = UndoEntry {
        definition: draft.definition.clone(),
        invalidated_source_ids: draft.invalidated_source_ids.clone(),
    };
    let mut candidate = before.definition.clone();
    let mut invalidated = before.invalidated_source_ids.clone();
    apply_to_definition(&mut candidate, &mut invalidated, command)?;
    ensure_unique_structural_ids(&candidate)?;
    reject_nested_watch_groups(&candidate.blocks, false)?;

    if candidate == before.definition && invalidated == before.invalidated_source_ids {
        return Ok(EditOutcome::NoChange);
    }

    candidate.revision = before.definition.revision.saturating_add(1);
    if draft.undo.len() == EDITOR_UNDO_LIMIT {
        draft.undo.pop_front();
    }
    draft.undo.push_back(before);
    draft.definition = candidate;
    draft.invalidated_source_ids = invalidated;
    draft.status = DraftStatus::NeedsValidation;
    Ok(EditOutcome::Changed)
}

/// Runtime validation plus editor-only dependency invalidations. Saved canonical definitions do not
/// carry undo or transient invalidation metadata; callers must clear these only after validation.
pub fn editor_validation_problems(draft: &EditorDraft) -> Vec<ValidationProblem> {
    let mut problems = crate::engine::macro_engine::validate_macro(&draft.definition);
    collect_invalidated_action_problems(
        &draft.definition.blocks,
        true,
        &draft.invalidated_source_ids,
        &mut problems,
    );
    problems
}

fn collect_invalidated_action_problems(
    blocks: &[Block],
    ancestors_enabled: bool,
    invalidated: &BTreeSet<String>,
    problems: &mut Vec<ValidationProblem>,
) {
    for block in blocks {
        let enabled = ancestors_enabled && block.enabled;
        if enabled {
            let source = match &block.kind {
                BlockKind::Action {
                    action:
                        Action::ClickTextMatch {
                            source_block_id, ..
                        }
                        | Action::ClickImageMatch {
                            source_block_id, ..
                        },
                } => Some(source_block_id),
                BlockKind::Action {
                    action:
                        Action::MoveOnly {
                            target:
                                crate::engine::macro_engine::ActionTarget::TextMatch { source_block_id }
                                | crate::engine::macro_engine::ActionTarget::ImageMatch {
                                    source_block_id,
                                },
                        },
                } => Some(source_block_id),
                _ => None,
            };
            if let Some(source) = source.filter(|source| invalidated.contains(*source)) {
                problems.push(ValidationProblem {
                    code: "editor.dependent_action_needs_revalidation".to_string(),
                    message: format!(
                        "action '{}' depends on changed observation source '{source}'; validate the source before running",
                        block.id
                    ),
                    block_id: Some(block.id.clone()),
                });
            }
        }
        for child in child_containers(block) {
            collect_invalidated_action_problems(child, enabled, invalidated, problems);
        }
    }
}

fn apply_to_definition(
    definition: &mut MacroDefinition,
    invalidated: &mut BTreeSet<String>,
    command: EditorCommand,
) -> Result<(), EditorError> {
    match command {
        EditorCommand::InsertBlock { target, block } => {
            let container = find_container_mut(&mut definition.blocks, &target.container)
                .ok_or(EditorError::MissingContainer)?;
            if target.index > container.len() {
                return Err(EditorError::InvalidIndex);
            }
            container.insert(target.index, block);
        }
        EditorCommand::RemoveBlock { path, loop_choice } => {
            let container = find_container_mut(&mut definition.blocks, &path.container)
                .ok_or(EditorError::MissingContainer)?;
            let index = container
                .iter()
                .position(|block| block.id == path.block_id)
                .ok_or_else(|| EditorError::MissingBlock(path.block_id.clone()))?;
            let removed = container.remove(index);
            match removed.kind {
                BlockKind::RepeatN { body, .. }
                | BlockKind::RepeatUntil { body, .. }
                | BlockKind::Continuous { body } => match loop_choice {
                    None => return Err(EditorError::LoopDeletionChoiceRequired),
                    Some(LoopDeletionChoice::DeleteWithContents) => {}
                    Some(LoopDeletionChoice::KeepContents) => {
                        container.splice(index..index, body);
                    }
                },
                _ if loop_choice == Some(LoopDeletionChoice::KeepContents) => {
                    return Err(EditorError::InvalidLoopDeletionChoice);
                }
                _ => {}
            }
        }
        EditorCommand::DuplicateBlock { source, target } => {
            let block = find_block_in_container(&definition.blocks, &source)?
                .cloned()
                .ok_or_else(|| EditorError::MissingBlock(source.block_id.clone()))?;
            let mut used = structural_id_set(definition);
            let duplicate = duplicate_with_new_ids(block, &mut used);
            let container = find_container_mut(&mut definition.blocks, &target.container)
                .ok_or(EditorError::MissingContainer)?;
            if target.index > container.len() {
                return Err(EditorError::InvalidIndex);
            }
            container.insert(target.index, duplicate);
        }
        EditorCommand::SetBlockEnabled { path, enabled } => {
            let block = find_block_in_container_mut(&mut definition.blocks, &path)?
                .ok_or_else(|| EditorError::MissingBlock(path.block_id.clone()))?;
            block.enabled = enabled;
        }
        EditorCommand::SetLaneEnabled {
            group_id,
            lane_id,
            enabled,
        } => {
            let lane = find_lane_mut(&mut definition.blocks, &group_id, &lane_id)
                .ok_or(EditorError::MissingLane(lane_id))?;
            lane.enabled = enabled;
        }
        EditorCommand::ReorderSibling { path, to_index } => {
            move_within_container(
                &mut definition.blocks,
                &path.container,
                &path.block_id,
                to_index,
            )?;
        }
        EditorCommand::MoveBlock { source, target } => {
            let moving = find_block_in_container(&definition.blocks, &source)?
                .ok_or_else(|| EditorError::MissingBlock(source.block_id.clone()))?;
            let descendants = block_owned_identity_set(moving);
            if target
                .container
                .owner_ids()
                .any(|owner| descendants.contains(owner))
            {
                return Err(EditorError::IllegalDescendantMove);
            }
            let source_container = find_container_mut(&mut definition.blocks, &source.container)
                .ok_or(EditorError::MissingContainer)?;
            let source_index = source_container
                .iter()
                .position(|block| block.id == source.block_id)
                .ok_or_else(|| EditorError::MissingBlock(source.block_id.clone()))?;
            let moving = source_container.remove(source_index);
            let target_container = find_container_mut(&mut definition.blocks, &target.container)
                .ok_or(EditorError::MissingContainer)?;
            if target.index > target_container.len() {
                return Err(EditorError::InvalidIndex);
            }
            target_container.insert(target.index, moving);
        }
        EditorCommand::TransferIfBranch {
            if_id,
            branch,
            block_id,
            to_index,
        } => {
            let block = find_block_mut(&mut definition.blocks, &if_id)
                .ok_or_else(|| EditorError::MissingBlock(if_id.clone()))?;
            let BlockKind::If {
                then_body,
                else_body,
                ..
            } = &mut block.kind
            else {
                return Err(EditorError::MissingContainer);
            };
            let (source, target) = match branch {
                IfBranch::Then => (then_body, else_body),
                IfBranch::Else => (else_body, then_body),
            };
            let index = source
                .iter()
                .position(|block| block.id == block_id)
                .ok_or_else(|| EditorError::MissingBlock(block_id.clone()))?;
            if to_index > target.len() {
                return Err(EditorError::InvalidIndex);
            }
            let moved = source.remove(index);
            target.insert(to_index, moved);
        }
        EditorCommand::MoveLane {
            group_id,
            lane_id,
            to_index,
        } => {
            let block = find_block_mut(&mut definition.blocks, &group_id)
                .ok_or_else(|| EditorError::MissingBlock(group_id.clone()))?;
            let BlockKind::WatchGroup { group } = &mut block.kind else {
                return Err(EditorError::MissingContainer);
            };
            if to_index >= group.lanes.len() {
                return Err(EditorError::InvalidIndex);
            }
            let from = group
                .lanes
                .iter()
                .position(|lane| lane.id == lane_id)
                .ok_or_else(|| EditorError::MissingLane(lane_id.clone()))?;
            let lane = group.lanes.remove(from);
            group.lanes.insert(to_index, lane);
        }
        EditorCommand::ConvertBlock { path, target } => {
            let block = find_block_in_container_mut(&mut definition.blocks, &path)?
                .ok_or_else(|| EditorError::MissingBlock(path.block_id.clone()))?;
            let source = block.id.clone();
            convert_block(block, target)?;
            invalidated.insert(source);
        }
        EditorCommand::SetConditionMode { path, mode } => {
            let block = find_block_in_container_mut(&mut definition.blocks, &path)?
                .ok_or_else(|| EditorError::MissingBlock(path.block_id.clone()))?;
            let condition = match &mut block.kind {
                BlockKind::Observe { condition }
                | BlockKind::If { condition, .. }
                | BlockKind::RepeatUntil { condition, .. } => condition,
                _ => return Err(EditorError::IncompatibleConversion),
            };
            match condition {
                Condition::Text { mode: current, .. } | Condition::Image { mode: current, .. } => {
                    *current = mode
                }
            }
            invalidated.insert(path.block_id);
        }
        EditorCommand::SetWaitDuration { path, duration_ms } => {
            let block = find_block_in_container_mut(&mut definition.blocks, &path)?
                .ok_or_else(|| EditorError::MissingBlock(path.block_id.clone()))?;
            let BlockKind::Wait {
                duration_ms: current,
            } = &mut block.kind
            else {
                return Err(EditorError::IncompatibleConversion);
            };
            *current = duration_ms;
        }
        EditorCommand::SetRepeatCount { path, count } => {
            let block = find_block_in_container_mut(&mut definition.blocks, &path)?
                .ok_or_else(|| EditorError::MissingBlock(path.block_id.clone()))?;
            let BlockKind::RepeatN { count: current, .. } = &mut block.kind else {
                return Err(EditorError::IncompatibleConversion);
            };
            *current = count;
        }
        EditorCommand::SetRepeatUntilMax {
            path,
            max_iterations,
        } => {
            let block = find_block_in_container_mut(&mut definition.blocks, &path)?
                .ok_or_else(|| EditorError::MissingBlock(path.block_id.clone()))?;
            let BlockKind::RepeatUntil {
                max_iterations: current,
                ..
            } = &mut block.kind
            else {
                return Err(EditorError::IncompatibleConversion);
            };
            *current = max_iterations;
        }
        EditorCommand::SetWatchSettings {
            path,
            timeout_ms,
            cooldown_ms,
        } => {
            let block = find_block_in_container_mut(&mut definition.blocks, &path)?
                .ok_or_else(|| EditorError::MissingBlock(path.block_id.clone()))?;
            let BlockKind::WatchGroup { group } = &mut block.kind else {
                return Err(EditorError::IncompatibleConversion);
            };
            group.timeout_ms = timeout_ms;
            group.cooldown_ms = cooldown_ms;
        }
        EditorCommand::ReplaceBlock {
            path,
            replacement,
            children,
        } => {
            if replacement.id != path.block_id {
                return Err(EditorError::ReplacementIdMismatch);
            }
            let container = find_container_mut(&mut definition.blocks, &path.container)
                .ok_or(EditorError::MissingContainer)?;
            let index = container
                .iter()
                .position(|block| block.id == path.block_id)
                .ok_or_else(|| EditorError::MissingBlock(path.block_id.clone()))?;
            let old = std::mem::replace(&mut container[index], replacement);
            if children == ChildDisposition::KeepOwnedContents {
                container.splice(index + 1..index + 1, owned_contents(old));
            }
            invalidated.insert(path.block_id);
        }
        EditorCommand::ReplaceTextRule { mut rule } => {
            let old = definition
                .text_rules
                .iter_mut()
                .find(|item| item.id == rule.id)
                .ok_or_else(|| EditorError::MissingRule(rule.id.clone()))?;
            rule.revision = old.revision;
            if *old != rule {
                rule.revision = old.revision.saturating_add(1);
                let id = rule.id.clone();
                *old = rule;
                invalidated.extend(source_ids_for_rule(&definition.blocks, &id));
            }
        }
        EditorCommand::ReplaceImageRule { mut rule } => {
            let old = definition
                .image_rules
                .iter_mut()
                .find(|item| item.id == rule.id)
                .ok_or_else(|| EditorError::MissingRule(rule.id.clone()))?;
            rule.revision = old.revision;
            if *old != rule {
                rule.revision = old.revision.saturating_add(1);
                rule.verification = None;
                let id = rule.id.clone();
                *old = rule;
                invalidated.extend(source_ids_for_rule(&definition.blocks, &id));
            }
        }
        EditorCommand::RecaptureRegion { region_id, rect } => {
            let region = definition
                .regions
                .iter_mut()
                .find(|item| item.id == region_id)
                .ok_or_else(|| EditorError::MissingRegion(region_id.clone()))?;
            if region.rect != rect {
                region.rect = rect;
                region.revision = region.revision.saturating_add(1);
                let rule_ids: Vec<String> = definition
                    .text_rules
                    .iter()
                    .filter(|r| r.region_id == region_id)
                    .map(|r| r.id.clone())
                    .chain(
                        definition
                            .image_rules
                            .iter_mut()
                            .filter(|r| r.region_id == region_id)
                            .map(|r| {
                                r.verification = None;
                                r.id.clone()
                            }),
                    )
                    .collect();
                for id in rule_ids {
                    invalidated.extend(source_ids_for_rule(&definition.blocks, &id));
                }
            }
        }
        EditorCommand::MarkValidated | EditorCommand::Undo => unreachable!(),
    }
    Ok(())
}

fn convert_block(block: &mut Block, target: ConversionTarget) -> Result<(), EditorError> {
    match (&mut block.kind, target) {
        (
            BlockKind::Observe {
                condition: Condition::Text { mode, .. },
            },
            ConversionTarget::TextObservation { mode: next },
        )
        | (
            BlockKind::Observe {
                condition: Condition::Image { mode, .. },
            },
            ConversionTarget::ImageObservation { mode: next },
        ) => *mode = next,
        (
            BlockKind::Action {
                action: Action::ClickTextMatch { button, .. },
            },
            ConversionTarget::ClickTextMatch { button: next },
        )
        | (
            BlockKind::Action {
                action: Action::ClickImageMatch { button, .. },
            },
            ConversionTarget::ClickImageMatch { button: next },
        ) => *button = next,
        (BlockKind::Action { action }, ConversionTarget::ClickPoint { point_id, button })
            if matches!(
                action,
                Action::ClickPoint { .. } | Action::ClickRegion { .. }
            ) =>
        {
            *action = Action::ClickPoint { point_id, button }
        }
        (BlockKind::Action { action }, ConversionTarget::ClickRegion { region_id, button })
            if matches!(
                action,
                Action::ClickPoint { .. } | Action::ClickRegion { .. }
            ) =>
        {
            *action = Action::ClickRegion { region_id, button }
        }
        (
            BlockKind::RepeatN { body, .. },
            ConversionTarget::RepeatUntil {
                condition,
                max_iterations,
            },
        ) => {
            let body = std::mem::take(body);
            block.kind = BlockKind::RepeatUntil {
                condition,
                max_iterations,
                body,
            };
        }
        (BlockKind::RepeatUntil { body, .. }, ConversionTarget::RepeatN { count }) => {
            let body = std::mem::take(body);
            block.kind = BlockKind::RepeatN { count, body };
        }
        _ => return Err(EditorError::IncompatibleConversion),
    }
    Ok(())
}

fn owned_contents(block: Block) -> Vec<Block> {
    match block.kind {
        BlockKind::If {
            mut then_body,
            else_body,
            ..
        } => {
            then_body.extend(else_body);
            then_body
        }
        BlockKind::RepeatN { body, .. }
        | BlockKind::RepeatUntil { body, .. }
        | BlockKind::Continuous { body } => body,
        BlockKind::WatchGroup { group } => group
            .lanes
            .into_iter()
            .flat_map(|lane| lane.then_body)
            .collect(),
        _ => vec![],
    }
}

fn move_within_container(
    blocks: &mut Vec<Block>,
    path: &ContainerPath,
    id: &str,
    to: usize,
) -> Result<(), EditorError> {
    let list = find_container_mut(blocks, path).ok_or(EditorError::MissingContainer)?;
    if to >= list.len() {
        return Err(EditorError::InvalidIndex);
    }
    let from = list
        .iter()
        .position(|b| b.id == id)
        .ok_or_else(|| EditorError::MissingBlock(id.into()))?;
    let block = list.remove(from);
    list.insert(to, block);
    Ok(())
}

fn find_block_in_container<'a>(
    blocks: &'a [Block],
    path: &BlockPath,
) -> Result<Option<&'a Block>, EditorError> {
    Ok(find_container(blocks, &path.container)
        .ok_or(EditorError::MissingContainer)?
        .iter()
        .find(|b| b.id == path.block_id))
}
fn find_block_in_container_mut<'a>(
    blocks: &'a mut Vec<Block>,
    path: &BlockPath,
) -> Result<Option<&'a mut Block>, EditorError> {
    Ok(find_container_mut(blocks, &path.container)
        .ok_or(EditorError::MissingContainer)?
        .iter_mut()
        .find(|b| b.id == path.block_id))
}

fn find_container<'a>(blocks: &'a [Block], target: &ContainerPath) -> Option<&'a [Block]> {
    if *target == ContainerPath::Root {
        return Some(blocks);
    }
    for block in blocks {
        if let Some(found) = direct_container(block, target) {
            return Some(found);
        }
        for child in child_containers(block) {
            if let Some(found) = find_container(child, target) {
                return Some(found);
            }
        }
    }
    None
}
fn find_container_mut<'a>(
    blocks: &'a mut Vec<Block>,
    target: &ContainerPath,
) -> Option<&'a mut Vec<Block>> {
    if *target == ContainerPath::Root {
        return Some(blocks);
    }
    for block in blocks {
        if direct_container(block, target).is_some() {
            return direct_container_mut(block, target);
        }
        for child in child_containers_mut(block) {
            if let Some(found) = find_container_mut(child, target) {
                return Some(found);
            }
        }
    }
    None
}

fn direct_container<'a>(block: &'a Block, target: &ContainerPath) -> Option<&'a [Block]> {
    match (&block.kind, target) {
        (BlockKind::If { then_body, .. }, ContainerPath::IfThen { if_id })
            if block.id == *if_id =>
        {
            Some(then_body)
        }
        (BlockKind::If { else_body, .. }, ContainerPath::IfElse { if_id })
            if block.id == *if_id =>
        {
            Some(else_body)
        }
        (
            BlockKind::RepeatN { body, .. }
            | BlockKind::RepeatUntil { body, .. }
            | BlockKind::Continuous { body },
            ContainerPath::LoopBody { loop_id },
        ) if block.id == *loop_id => Some(body),
        (BlockKind::WatchGroup { group }, ContainerPath::WatchLaneBody { watch_id, lane_id })
            if block.id == *watch_id =>
        {
            group
                .lanes
                .iter()
                .find(|l| l.id == *lane_id)
                .map(|l| l.then_body.as_slice())
        }
        (BlockKind::WatchGroup { group }, ContainerPath::TimeoutBody { owner_id })
            if block.id == *owner_id =>
        {
            timeout_body(&group.timeout_outcome)
        }
        (
            BlockKind::Observe { condition }
            | BlockKind::If { condition, .. }
            | BlockKind::RepeatUntil { condition, .. },
            ContainerPath::TimeoutBody { owner_id },
        ) if block.id == *owner_id => condition_timeout_body(condition),
        _ => None,
    }
}
fn direct_container_mut<'a>(
    block: &'a mut Block,
    target: &ContainerPath,
) -> Option<&'a mut Vec<Block>> {
    let owner = target.owner_ids().next()?;
    if block.id != owner {
        return None;
    }
    match target {
        ContainerPath::IfThen { .. } => {
            if let BlockKind::If { then_body, .. } = &mut block.kind {
                Some(then_body)
            } else {
                None
            }
        }
        ContainerPath::IfElse { .. } => {
            if let BlockKind::If { else_body, .. } = &mut block.kind {
                Some(else_body)
            } else {
                None
            }
        }
        ContainerPath::LoopBody { .. } => match &mut block.kind {
            BlockKind::RepeatN { body, .. }
            | BlockKind::RepeatUntil { body, .. }
            | BlockKind::Continuous { body } => Some(body),
            _ => None,
        },
        ContainerPath::WatchLaneBody { lane_id, .. } => {
            if let BlockKind::WatchGroup { group } = &mut block.kind {
                group
                    .lanes
                    .iter_mut()
                    .find(|l| l.id == *lane_id)
                    .map(|l| &mut l.then_body)
            } else {
                None
            }
        }
        ContainerPath::TimeoutBody { .. } => match &mut block.kind {
            BlockKind::WatchGroup { group } => timeout_body_mut(&mut group.timeout_outcome),
            BlockKind::Observe { condition }
            | BlockKind::If { condition, .. }
            | BlockKind::RepeatUntil { condition, .. } => condition_timeout_body_mut(condition),
            _ => None,
        },
        ContainerPath::Root => None,
    }
}

fn child_containers(block: &Block) -> Vec<&[Block]> {
    let mut out = vec![];
    match &block.kind {
        BlockKind::If {
            condition,
            then_body,
            else_body,
        } => {
            out.extend([then_body.as_slice(), else_body.as_slice()]);
            if let Some(b) = condition_timeout_body(condition) {
                out.push(b);
            }
        }
        BlockKind::RepeatN { body, .. } | BlockKind::Continuous { body } => out.push(body),
        BlockKind::RepeatUntil {
            condition, body, ..
        } => {
            out.push(body);
            if let Some(b) = condition_timeout_body(condition) {
                out.push(b);
            }
        }
        BlockKind::Observe { condition } => {
            if let Some(b) = condition_timeout_body(condition) {
                out.push(b);
            }
        }
        BlockKind::WatchGroup { group } => {
            out.extend(group.lanes.iter().map(|l| l.then_body.as_slice()));
            if let Some(b) = timeout_body(&group.timeout_outcome) {
                out.push(b);
            }
        }
        _ => {}
    }
    out
}
fn child_containers_mut(block: &mut Block) -> Vec<&mut Vec<Block>> {
    let mut out = vec![];
    match &mut block.kind {
        BlockKind::If {
            condition,
            then_body,
            else_body,
        } => {
            out.push(then_body);
            out.push(else_body);
            if let Some(b) = condition_timeout_body_mut(condition) {
                out.push(b);
            }
        }
        BlockKind::RepeatN { body, .. } | BlockKind::Continuous { body } => out.push(body),
        BlockKind::RepeatUntil {
            condition, body, ..
        } => {
            out.push(body);
            if let Some(b) = condition_timeout_body_mut(condition) {
                out.push(b);
            }
        }
        BlockKind::Observe { condition } => {
            if let Some(b) = condition_timeout_body_mut(condition) {
                out.push(b);
            }
        }
        BlockKind::WatchGroup { group } => {
            out.extend(group.lanes.iter_mut().map(|l| &mut l.then_body));
            if let Some(b) = timeout_body_mut(&mut group.timeout_outcome) {
                out.push(b);
            }
        }
        _ => {}
    }
    out
}
fn timeout_body(outcome: &TimeoutOutcome) -> Option<&[Block]> {
    if let TimeoutOutcome::RunBody { body } = outcome {
        Some(body)
    } else {
        None
    }
}
fn timeout_body_mut(outcome: &mut TimeoutOutcome) -> Option<&mut Vec<Block>> {
    if let TimeoutOutcome::RunBody { body } = outcome {
        Some(body)
    } else {
        None
    }
}
fn condition_timeout_body(condition: &Condition) -> Option<&[Block]> {
    match condition {
        Condition::Text { mode, .. } | Condition::Image { mode, .. } => match mode {
            ObserveMode::WaitForTrue {
                timeout_outcome, ..
            }
            | ObserveMode::WaitForFalse {
                timeout_outcome, ..
            } => timeout_body(timeout_outcome),
            ObserveMode::CheckNow => None,
        },
    }
}
fn condition_timeout_body_mut(condition: &mut Condition) -> Option<&mut Vec<Block>> {
    match condition {
        Condition::Text { mode, .. } | Condition::Image { mode, .. } => match mode {
            ObserveMode::WaitForTrue {
                timeout_outcome, ..
            }
            | ObserveMode::WaitForFalse {
                timeout_outcome, ..
            } => timeout_body_mut(timeout_outcome),
            ObserveMode::CheckNow => None,
        },
    }
}

fn find_block<'a>(blocks: &'a [Block], id: &str) -> Option<&'a Block> {
    for block in blocks {
        if block.id == id {
            return Some(block);
        }
        for child in child_containers(block) {
            if let Some(x) = find_block(child, id) {
                return Some(x);
            }
        }
    }
    None
}
fn find_block_mut<'a>(blocks: &'a mut Vec<Block>, id: &str) -> Option<&'a mut Block> {
    for block in blocks {
        if block.id == id {
            return Some(block);
        }
        for child in child_containers_mut(block) {
            if let Some(x) = find_block_mut(child, id) {
                return Some(x);
            }
        }
    }
    None
}
fn find_lane_mut<'a>(
    blocks: &'a mut Vec<Block>,
    group: &str,
    lane: &str,
) -> Option<&'a mut crate::engine::macro_engine::WatchLane> {
    let block = find_block_mut(blocks, group)?;
    if let BlockKind::WatchGroup { group } = &mut block.kind {
        group.lanes.iter_mut().find(|l| l.id == lane)
    } else {
        None
    }
}

fn ensure_unique_structural_ids(definition: &MacroDefinition) -> Result<(), EditorError> {
    fn visit(blocks: &[Block], seen: &mut HashSet<String>) -> Result<(), EditorError> {
        for b in blocks {
            if !seen.insert(b.id.clone()) {
                return Err(EditorError::DuplicateIdentity(b.id.clone()));
            }
            if let BlockKind::WatchGroup { group } = &b.kind {
                for l in &group.lanes {
                    if !seen.insert(l.id.clone()) {
                        return Err(EditorError::DuplicateIdentity(l.id.clone()));
                    }
                }
            }
            for c in child_containers(b) {
                visit(c, seen)?
            }
        }
        Ok(())
    }
    visit(&definition.blocks, &mut HashSet::new())
}
fn structural_id_set(definition: &MacroDefinition) -> HashSet<String> {
    let mut ids = HashSet::new();
    fn visit(bs: &[Block], ids: &mut HashSet<String>) {
        for b in bs {
            ids.insert(b.id.clone());
            if let BlockKind::WatchGroup { group } = &b.kind {
                ids.extend(group.lanes.iter().map(|l| l.id.clone()))
            }
            for c in child_containers(b) {
                visit(c, ids)
            }
        }
    }
    visit(&definition.blocks, &mut ids);
    ids
}
fn block_owned_identity_set(block: &Block) -> HashSet<&str> {
    let mut ids = HashSet::new();
    fn visit<'a>(b: &'a Block, ids: &mut HashSet<&'a str>) {
        ids.insert(&b.id);
        if let BlockKind::WatchGroup { group } = &b.kind {
            ids.extend(group.lanes.iter().map(|l| l.id.as_str()))
        }
        for c in child_containers(b) {
            for x in c {
                visit(x, ids)
            }
        }
    }
    visit(block, &mut ids);
    ids
}
fn reject_nested_watch_groups(blocks: &[Block], inside: bool) -> Result<(), EditorError> {
    for b in blocks {
        let watch = matches!(b.kind, BlockKind::WatchGroup { .. });
        if inside && watch {
            return Err(EditorError::NestedWatchGroup);
        }
        for c in child_containers(b) {
            reject_nested_watch_groups(c, inside || watch)?
        }
    }
    Ok(())
}

fn duplicate_with_new_ids(mut block: Block, used: &mut HashSet<String>) -> Block {
    fn assign(b: &Block, used: &mut HashSet<String>, map: &mut HashMap<String, String>) {
        map.insert(b.id.clone(), unique_copy_id(&b.id, used));
        if let BlockKind::WatchGroup { group } = &b.kind {
            for l in &group.lanes {
                map.insert(l.id.clone(), unique_copy_id(&l.id, used));
            }
        }
        for c in child_containers(b) {
            for x in c {
                assign(x, used, map)
            }
        }
    }
    let mut map = HashMap::new();
    assign(&block, used, &mut map);
    rewrite_ids(&mut block, &map);
    block
}
fn unique_copy_id(base: &str, used: &mut HashSet<String>) -> String {
    for n in 1.. {
        let id = if n == 1 {
            format!("{base}-copy")
        } else {
            format!("{base}-copy-{n}")
        };
        if used.insert(id.clone()) {
            return id;
        }
    }
    unreachable!()
}
fn rewrite_ids(block: &mut Block, map: &HashMap<String, String>) {
    block.id = map
        .get(&block.id)
        .cloned()
        .unwrap_or_else(|| block.id.clone());
    rewrite_kind_sources(&mut block.kind, map);
    match &mut block.kind {
        BlockKind::If {
            then_body,
            else_body,
            ..
        } => {
            for b in then_body.iter_mut().chain(else_body) {
                rewrite_ids(b, map)
            }
        }
        BlockKind::RepeatN { body, .. }
        | BlockKind::RepeatUntil { body, .. }
        | BlockKind::Continuous { body } => {
            for b in body {
                rewrite_ids(b, map)
            }
        }
        BlockKind::WatchGroup { group } => {
            for l in &mut group.lanes {
                l.id = map.get(&l.id).cloned().unwrap_or_else(|| l.id.clone());
                rewrite_passive_source(&mut l.condition, map);
                for b in &mut l.then_body {
                    rewrite_ids(b, map)
                }
            }
            if let TimeoutOutcome::RunBody { body } = &mut group.timeout_outcome {
                for b in body {
                    rewrite_ids(b, map)
                }
            }
        }
        _ => {}
    }
}
fn rewrite_kind_sources(kind: &mut BlockKind, map: &HashMap<String, String>) {
    match kind {
        BlockKind::Observe { condition }
        | BlockKind::If { condition, .. }
        | BlockKind::RepeatUntil { condition, .. } => rewrite_condition_source(condition, map),
        BlockKind::Action { action } => match action {
            Action::ClickTextMatch {
                source_block_id, ..
            }
            | Action::ClickImageMatch {
                source_block_id, ..
            } => rewrite_source(source_block_id, map),
            Action::MoveOnly {
                target:
                    crate::engine::macro_engine::ActionTarget::TextMatch { source_block_id }
                    | crate::engine::macro_engine::ActionTarget::ImageMatch { source_block_id },
            } => rewrite_source(source_block_id, map),
            _ => {}
        },
        _ => {}
    }
}
fn rewrite_condition_source(c: &mut Condition, map: &HashMap<String, String>) {
    match c {
        Condition::Text {
            source_block_id, ..
        }
        | Condition::Image {
            source_block_id, ..
        } => rewrite_source(source_block_id, map),
    }
}
fn rewrite_passive_source(c: &mut PassiveCondition, map: &HashMap<String, String>) {
    match c {
        PassiveCondition::Text {
            source_block_id, ..
        }
        | PassiveCondition::Image {
            source_block_id, ..
        } => rewrite_source(source_block_id, map),
    }
}
fn rewrite_source(id: &mut String, map: &HashMap<String, String>) {
    if let Some(next) = map.get(id) {
        *id = next.clone()
    }
}
fn source_ids_for_rule(blocks: &[Block], rule: &str) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    fn visit(bs: &[Block], rule: &str, ids: &mut BTreeSet<String>) {
        for b in bs {
            match &b.kind {
                BlockKind::Observe { condition }
                | BlockKind::If { condition, .. }
                | BlockKind::RepeatUntil { condition, .. } => match condition {
                    Condition::Text {
                        source_block_id,
                        rule_id,
                        ..
                    }
                    | Condition::Image {
                        source_block_id,
                        rule_id,
                        ..
                    } if rule_id == rule => {
                        ids.insert(source_block_id.clone());
                    }
                    _ => {}
                },
                BlockKind::WatchGroup { group } => {
                    for l in &group.lanes {
                        match &l.condition {
                            PassiveCondition::Text {
                                source_block_id,
                                rule_id,
                            }
                            | PassiveCondition::Image {
                                source_block_id,
                                rule_id,
                            } if rule_id == rule => {
                                ids.insert(source_block_id.clone());
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            for c in child_containers(b) {
                visit(c, rule, ids)
            }
        }
    }
    visit(blocks, rule, &mut ids);
    ids
}

pub fn locate_block_path(definition: &MacroDefinition, id: &str) -> Option<BlockPath> {
    fn scan(blocks: &[Block], container: ContainerPath, id: &str) -> Option<BlockPath> {
        for block in blocks {
            if block.id == id {
                return Some(BlockPath {
                    container: container.clone(),
                    block_id: id.into(),
                });
            }
            let mut candidates: Vec<(ContainerPath, &[Block])> = vec![];
            match &block.kind {
                BlockKind::If {
                    then_body,
                    else_body,
                    condition,
                } => {
                    candidates.push((
                        ContainerPath::IfThen {
                            if_id: block.id.clone(),
                        },
                        then_body,
                    ));
                    candidates.push((
                        ContainerPath::IfElse {
                            if_id: block.id.clone(),
                        },
                        else_body,
                    ));
                    if let Some(body) = condition_timeout_body(condition) {
                        candidates.push((
                            ContainerPath::TimeoutBody {
                                owner_id: block.id.clone(),
                            },
                            body,
                        ));
                    }
                }
                BlockKind::RepeatN { body, .. } | BlockKind::Continuous { body } => candidates
                    .push((
                        ContainerPath::LoopBody {
                            loop_id: block.id.clone(),
                        },
                        body,
                    )),
                BlockKind::RepeatUntil {
                    condition, body, ..
                } => {
                    candidates.push((
                        ContainerPath::LoopBody {
                            loop_id: block.id.clone(),
                        },
                        body,
                    ));
                    if let Some(timeout) = condition_timeout_body(condition) {
                        candidates.push((
                            ContainerPath::TimeoutBody {
                                owner_id: block.id.clone(),
                            },
                            timeout,
                        ));
                    }
                }
                BlockKind::Observe { condition } => {
                    if let Some(body) = condition_timeout_body(condition) {
                        candidates.push((
                            ContainerPath::TimeoutBody {
                                owner_id: block.id.clone(),
                            },
                            body,
                        ));
                    }
                }
                BlockKind::WatchGroup { group } => {
                    for lane in &group.lanes {
                        candidates.push((
                            ContainerPath::WatchLaneBody {
                                watch_id: block.id.clone(),
                                lane_id: lane.id.clone(),
                            },
                            &lane.then_body,
                        ));
                    }
                    if let Some(body) = timeout_body(&group.timeout_outcome) {
                        candidates.push((
                            ContainerPath::TimeoutBody {
                                owner_id: block.id.clone(),
                            },
                            body,
                        ));
                    }
                }
                _ => {}
            }
            for (path, body) in candidates {
                if let Some(found) = scan(body, path, id) {
                    return Some(found);
                }
            }
        }
        None
    }
    scan(&definition.blocks, ContainerPath::Root, id)
}

pub fn block_at_path<'a>(definition: &'a MacroDefinition, path: &BlockPath) -> Option<&'a Block> {
    find_block_in_container(&definition.blocks, path)
        .ok()
        .flatten()
}

pub fn sibling_position(definition: &MacroDefinition, path: &BlockPath) -> Option<(usize, usize)> {
    let list = find_container(&definition.blocks, &path.container)?;
    let index = list.iter().position(|b| b.id == path.block_id)?;
    Some((index, list.len()))
}
pub fn container_len(definition: &MacroDefinition, container: &ContainerPath) -> Option<usize> {
    Some(find_container(&definition.blocks, container)?.len())
}

pub fn locate_watch_lane(
    definition: &MacroDefinition,
    lane_id: &str,
) -> Option<(String, usize, usize)> {
    fn scan(blocks: &[Block], lane_id: &str) -> Option<(String, usize, usize)> {
        for block in blocks {
            if let BlockKind::WatchGroup { group } = &block.kind {
                if let Some(index) = group.lanes.iter().position(|lane| lane.id == lane_id) {
                    return Some((block.id.clone(), index, group.lanes.len()));
                }
            }
            for child in child_containers(block) {
                if let Some(found) = scan(child, lane_id) {
                    return Some(found);
                }
            }
        }
        None
    }
    scan(&definition.blocks, lane_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::macro_engine::{
        FocusLossPolicy, MACRO_SCHEMA_VERSION, MatchSelectionPolicy, PreprocessProfile,
        RegionDefinition, SafetyPolicy, TargetProfile, TextMatchMode, WatchGroup, WatchLane,
    };

    fn def(blocks: Vec<Block>) -> MacroDefinition {
        MacroDefinition {
            schema_version: MACRO_SCHEMA_VERSION,
            id: "m".into(),
            name: "m".into(),
            revision: 4,
            target: TargetProfile {
                process_path: "g".into(),
                window_class: "w".into(),
                title_contains: "d".into(),
                captured_client_width: 1280,
                captured_client_height: 720,
                captured_dpi: 96,
            },
            regions: vec![],
            points: vec![],
            text_rules: vec![],
            image_rules: vec![],
            blocks,
            safety: SafetyPolicy {
                max_runtime_ms: Limit::Finite(1000),
                max_clicks: Limit::Finite(2),
                max_observation_retries: Limit::Finite(2),
                max_observations_per_second: 20,
                minimum_click_interval_ms: 50,
                focus_loss: FocusLossPolicy::Stop,
            },
        }
    }
    fn comment(id: &str) -> Block {
        Block {
            id: id.into(),
            enabled: true,
            kind: BlockKind::Comment { text: id.into() },
        }
    }
    fn path(id: &str) -> BlockPath {
        BlockPath {
            container: ContainerPath::Root,
            block_id: id.into(),
        }
    }
    fn mode_wait_true() -> ObserveMode {
        ObserveMode::WaitForTrue {
            timeout_ms: Limit::Finite(50),
            timeout_outcome: TimeoutOutcome::Continue,
        }
    }
    fn mode_wait_false() -> ObserveMode {
        ObserveMode::WaitForFalse {
            timeout_ms: Limit::Unlimited,
            timeout_outcome: TimeoutOutcome::Continue,
        }
    }

    #[test]
    fn all_compatible_conversion_directions_preserve_identity_and_owned_bodies() {
        let obs = |id: &str, text: bool| Block {
            id: id.into(),
            enabled: true,
            kind: BlockKind::Observe {
                condition: if text {
                    Condition::Text {
                        source_block_id: id.into(),
                        rule_id: "r".into(),
                        mode: ObserveMode::CheckNow,
                    }
                } else {
                    Condition::Image {
                        source_block_id: id.into(),
                        rule_id: "i".into(),
                        mode: ObserveMode::CheckNow,
                    }
                },
            },
        };
        let mut draft = EditorDraft::new(def(vec![
            obs("text", true),
            obs("image", false),
            Block {
                id: "tc".into(),
                enabled: true,
                kind: BlockKind::Action {
                    action: Action::ClickTextMatch {
                        source_block_id: "text".into(),
                        button: MouseButton::Left,
                    },
                },
            },
            Block {
                id: "ic".into(),
                enabled: true,
                kind: BlockKind::Action {
                    action: Action::ClickImageMatch {
                        source_block_id: "image".into(),
                        button: MouseButton::Left,
                    },
                },
            },
            Block {
                id: "fixed".into(),
                enabled: true,
                kind: BlockKind::Action {
                    action: Action::ClickPoint {
                        point_id: "p".into(),
                        button: MouseButton::Left,
                    },
                },
            },
            Block {
                id: "loop".into(),
                enabled: true,
                kind: BlockKind::RepeatN {
                    count: 2,
                    body: vec![comment("owned")],
                },
            },
        ]));
        let convert = |draft: &mut EditorDraft, id: &str, target| {
            apply_editor_command(
                draft,
                EditorCommand::ConvertBlock {
                    path: path(id),
                    target,
                },
            )
            .unwrap()
        };
        for mode in [mode_wait_true(), mode_wait_false(), ObserveMode::CheckNow] {
            convert(
                &mut draft,
                "text",
                ConversionTarget::TextObservation { mode },
            );
        }
        for mode in [mode_wait_false(), mode_wait_true(), ObserveMode::CheckNow] {
            convert(
                &mut draft,
                "image",
                ConversionTarget::ImageObservation { mode },
            );
        }
        convert(
            &mut draft,
            "tc",
            ConversionTarget::ClickTextMatch {
                button: MouseButton::Right,
            },
        );
        convert(
            &mut draft,
            "ic",
            ConversionTarget::ClickImageMatch {
                button: MouseButton::Right,
            },
        );
        convert(
            &mut draft,
            "fixed",
            ConversionTarget::ClickRegion {
                region_id: "region".into(),
                button: MouseButton::Right,
            },
        );
        convert(
            &mut draft,
            "fixed",
            ConversionTarget::ClickPoint {
                point_id: "point2".into(),
                button: MouseButton::Left,
            },
        );
        convert(
            &mut draft,
            "loop",
            ConversionTarget::RepeatUntil {
                condition: Condition::Text {
                    source_block_id: "loop".into(),
                    rule_id: "r".into(),
                    mode: ObserveMode::CheckNow,
                },
                max_iterations: Limit::Finite(9),
            },
        );
        convert(&mut draft, "loop", ConversionTarget::RepeatN { count: 3 });
        assert!(matches!(
            draft.definition.blocks[2].kind,
            BlockKind::Action {
                action: Action::ClickTextMatch {
                    button: MouseButton::Right,
                    ..
                }
            }
        ));
        assert!(matches!(
            draft.definition.blocks[3].kind,
            BlockKind::Action {
                action: Action::ClickImageMatch {
                    button: MouseButton::Right,
                    ..
                }
            }
        ));
        assert!(
            matches!(draft.definition.blocks[4].kind,BlockKind::Action{action:Action::ClickPoint{ref point_id,..}} if point_id=="point2")
        );
        assert!(
            matches!(draft.definition.blocks[5].kind,BlockKind::RepeatN{ref body,..} if body[0].id=="owned")
        );
    }

    #[test]
    fn structural_commands_are_transactional_and_revisioned() {
        let lane = |id: &str| WatchLane {
            id: id.into(),
            enabled: true,
            condition: PassiveCondition::Text {
                source_block_id: id.into(),
                rule_id: "r".into(),
            },
            then_body: vec![comment(&format!("{id}-body"))],
        };
        let watch = Block {
            id: "watch".into(),
            enabled: true,
            kind: BlockKind::WatchGroup {
                group: WatchGroup {
                    lanes: vec![lane("a"), lane("b"), lane("c")],
                    timeout_ms: Limit::Finite(100),
                    timeout_outcome: TimeoutOutcome::Continue,
                    cooldown_ms: 25,
                },
            },
        };
        let mut draft = EditorDraft::new(def(vec![watch]));
        assert_eq!(
            apply_editor_command(
                &mut draft,
                EditorCommand::MoveLane {
                    group_id: "watch".into(),
                    lane_id: "c".into(),
                    to_index: 0
                }
            ),
            Ok(EditOutcome::Changed)
        );
        assert_eq!(draft.watch_lane_ids("watch"), vec!["c", "a", "b"]);
        assert_eq!(draft.definition.revision, 5);
        assert_eq!(draft.status, DraftStatus::NeedsValidation);
        let before = draft.clone();
        let nested = Block {
            id: "nested".into(),
            enabled: true,
            kind: BlockKind::WatchGroup {
                group: WatchGroup {
                    lanes: vec![],
                    timeout_ms: Limit::Finite(1),
                    timeout_outcome: TimeoutOutcome::Continue,
                    cooldown_ms: 0,
                },
            },
        };
        assert_eq!(
            apply_editor_command(
                &mut draft,
                EditorCommand::InsertBlock {
                    target: InsertionTarget {
                        container: ContainerPath::WatchLaneBody {
                            watch_id: "watch".into(),
                            lane_id: "a".into()
                        },
                        index: 0
                    },
                    block: nested
                }
            ),
            Err(EditorError::NestedWatchGroup)
        );
        assert_eq!(draft, before);
    }

    #[test]
    fn running_and_duplicate_ids_reject_without_mutation() {
        let mut running = EditorDraft::new(def(vec![comment("x")]));
        running.editability = DraftEditability::Running { revision: 4 };
        let before = running.clone();
        assert_eq!(
            apply_editor_command(
                &mut running,
                EditorCommand::SetBlockEnabled {
                    path: path("x"),
                    enabled: false
                }
            ),
            Err(EditorError::RunInProgress)
        );
        assert_eq!(running, before);
        let mut duplicate = EditorDraft::new(def(vec![comment("x"), comment("x")]));
        let before = duplicate.clone();
        assert_eq!(
            apply_editor_command(
                &mut duplicate,
                EditorCommand::SetBlockEnabled {
                    path: path("x"),
                    enabled: false
                }
            ),
            Err(EditorError::DuplicateIdentity("x".into()))
        );
        assert_eq!(duplicate, before);
    }

    #[test]
    fn rule_and_recapture_invalidate_dependent_matched_click_until_validation() {
        let source = Block {
            id: "source".into(),
            enabled: true,
            kind: BlockKind::Observe {
                condition: Condition::Text {
                    source_block_id: "source".into(),
                    rule_id: "r".into(),
                    mode: ObserveMode::CheckNow,
                },
            },
        };
        let click = Block {
            id: "click".into(),
            enabled: true,
            kind: BlockKind::Action {
                action: Action::ClickTextMatch {
                    source_block_id: "source".into(),
                    button: MouseButton::Left,
                },
            },
        };
        let mut definition = def(vec![source, click]);
        definition.regions.push(RegionDefinition {
            id: "region".into(),
            revision: 1,
            rect: RectRatio {
                x: 0.1,
                y: 0.1,
                width: 0.2,
                height: 0.2,
            },
        });
        definition.text_rules.push(TextRule {
            id: "r".into(),
            revision: 1,
            region_id: "region".into(),
            language: "en-US".into(),
            preprocess: PreprocessProfile::Grayscale,
            expected: "Salvage".into(),
            match_mode: TextMatchMode::Contains,
            threshold: 0.9,
            case_sensitive: false,
            allow_cross_line: false,
            match_policy: MatchSelectionPolicy::ExactlyOne,
            poll_interval_ms: 250,
            timeout_ms: Limit::Finite(1000),
            stable_frames: 1,
        });
        let mut draft = EditorDraft::new(definition);
        let mut rule = draft.definition.text_rules[0].clone();
        rule.expected = "Retry".into();
        apply_editor_command(&mut draft, EditorCommand::ReplaceTextRule { rule }).unwrap();
        assert!(draft.invalidated_source_ids().contains("source"));
        assert!(
            editor_validation_problems(&draft)
                .iter()
                .any(|p| p.code == "editor.dependent_action_needs_revalidation"
                    && p.block_id.as_deref() == Some("click"))
        );
        apply_editor_command(
            &mut draft,
            EditorCommand::RecaptureRegion {
                region_id: "region".into(),
                rect: RectRatio {
                    x: 0.2,
                    y: 0.1,
                    width: 0.2,
                    height: 0.2,
                },
            },
        )
        .unwrap();
        assert_eq!(draft.definition.regions[0].revision, 2);
        assert_eq!(draft.definition.text_rules[0].expected, "Retry");
    }

    #[test]
    fn unrelated_conversion_requires_replace_preview() {
        let block = Block {
            id: "o".into(),
            enabled: true,
            kind: BlockKind::Observe {
                condition: Condition::Text {
                    source_block_id: "o".into(),
                    rule_id: "r".into(),
                    mode: ObserveMode::CheckNow,
                },
            },
        };
        assert!(matches!(
            preview_conversion(&block, BlockFamily::Loop),
            ConversionPreview::ReplaceRequired { .. }
        ));
    }

    #[test]
    fn moving_container_into_descendant_is_transactional() {
        let outer = Block {
            id: "outer".into(),
            enabled: true,
            kind: BlockKind::RepeatN {
                count: 2,
                body: vec![Block {
                    id: "inner".into(),
                    enabled: true,
                    kind: BlockKind::RepeatN {
                        count: 2,
                        body: vec![comment("leaf")],
                    },
                }],
            },
        };
        let mut draft = EditorDraft::new(def(vec![outer]));
        let before = draft.clone();
        let result = apply_editor_command(
            &mut draft,
            EditorCommand::MoveBlock {
                source: path("outer"),
                target: InsertionTarget {
                    container: ContainerPath::LoopBody {
                        loop_id: "inner".into(),
                    },
                    index: 0,
                },
            },
        );
        assert_eq!(result, Err(EditorError::IllegalDescendantMove));
        assert_eq!(draft, before);
    }

    #[test]
    fn explicit_then_else_transfer_preserves_whole_loop() {
        let branch = Block {
            id: "if".into(),
            enabled: true,
            kind: BlockKind::If {
                condition: Condition::Text {
                    source_block_id: "if".into(),
                    rule_id: "r".into(),
                    mode: ObserveMode::CheckNow,
                },
                then_body: vec![Block {
                    id: "loop".into(),
                    enabled: true,
                    kind: BlockKind::RepeatN {
                        count: 2,
                        body: vec![comment("inside")],
                    },
                }],
                else_body: vec![comment("fallback")],
            },
        };
        let mut draft = EditorDraft::new(def(vec![branch]));
        apply_editor_command(
            &mut draft,
            EditorCommand::TransferIfBranch {
                if_id: "if".into(),
                branch: IfBranch::Then,
                block_id: "loop".into(),
                to_index: 1,
            },
        )
        .unwrap();
        let BlockKind::If {
            then_body,
            else_body,
            ..
        } = &draft.definition.blocks[0].kind
        else {
            panic!()
        };
        assert!(then_body.is_empty());
        assert_eq!(else_body[1].id, "loop");
        assert!(matches!(else_body[1].kind,BlockKind::RepeatN{ref body,..}if body[0].id=="inside"));
    }

    #[test]
    fn loop_deletion_requires_choice_and_can_keep_contents() {
        let loop_block = Block {
            id: "loop".into(),
            enabled: true,
            kind: BlockKind::RepeatN {
                count: 2,
                body: vec![comment("a"), comment("b")],
            },
        };
        let mut draft = EditorDraft::new(def(vec![loop_block]));
        let before = draft.clone();
        assert_eq!(
            apply_editor_command(
                &mut draft,
                EditorCommand::RemoveBlock {
                    path: path("loop"),
                    loop_choice: None
                }
            ),
            Err(EditorError::LoopDeletionChoiceRequired)
        );
        assert_eq!(draft, before);
        apply_editor_command(
            &mut draft,
            EditorCommand::RemoveBlock {
                path: path("loop"),
                loop_choice: Some(LoopDeletionChoice::KeepContents),
            },
        )
        .unwrap();
        assert_eq!(
            draft
                .definition
                .blocks
                .iter()
                .map(|b| b.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn no_op_preserves_revision_status_and_undo() {
        let mut draft = EditorDraft::new(def(vec![comment("x")]));
        assert_eq!(
            apply_editor_command(
                &mut draft,
                EditorCommand::SetBlockEnabled {
                    path: path("x"),
                    enabled: true
                }
            ),
            Ok(EditOutcome::NoChange)
        );
        assert_eq!(draft.definition.revision, 4);
        assert_eq!(draft.status, DraftStatus::Ready);
        assert_eq!(draft.undo_len(), 0);
    }

    #[test]
    fn inspector_flow_commands_edit_mode_repeat_and_watch_settings() {
        let condition = || Condition::Text {
            source_block_id: "observe".into(),
            rule_id: "r".into(),
            mode: ObserveMode::CheckNow,
        };
        let mut draft = EditorDraft::new(def(vec![
            Block {
                id: "observe".into(),
                enabled: true,
                kind: BlockKind::Observe {
                    condition: condition(),
                },
            },
            Block {
                id: "repeat-n".into(),
                enabled: true,
                kind: BlockKind::RepeatN {
                    count: 2,
                    body: vec![],
                },
            },
            Block {
                id: "repeat-until".into(),
                enabled: true,
                kind: BlockKind::RepeatUntil {
                    condition: condition(),
                    max_iterations: Limit::Finite(10),
                    body: vec![],
                },
            },
            Block {
                id: "watch".into(),
                enabled: true,
                kind: BlockKind::WatchGroup {
                    group: WatchGroup {
                        lanes: vec![],
                        timeout_ms: Limit::Finite(1_000),
                        timeout_outcome: TimeoutOutcome::Continue,
                        cooldown_ms: 100,
                    },
                },
            },
        ]));

        apply_editor_command(
            &mut draft,
            EditorCommand::SetConditionMode {
                path: path("observe"),
                mode: ObserveMode::WaitForTrue {
                    timeout_ms: Limit::Unlimited,
                    timeout_outcome: TimeoutOutcome::Continue,
                },
            },
        )
        .unwrap();
        apply_editor_command(
            &mut draft,
            EditorCommand::SetRepeatCount {
                path: path("repeat-n"),
                count: 7,
            },
        )
        .unwrap();
        apply_editor_command(
            &mut draft,
            EditorCommand::SetRepeatUntilMax {
                path: path("repeat-until"),
                max_iterations: Limit::Unlimited,
            },
        )
        .unwrap();
        apply_editor_command(
            &mut draft,
            EditorCommand::SetWatchSettings {
                path: path("watch"),
                timeout_ms: Limit::Unlimited,
                cooldown_ms: 275,
            },
        )
        .unwrap();

        let BlockKind::Observe { condition } = &draft.blocks[0].kind else {
            panic!()
        };
        assert!(matches!(
            condition,
            Condition::Text {
                mode: ObserveMode::WaitForTrue {
                    timeout_ms: Limit::Unlimited,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            draft.blocks[1].kind,
            BlockKind::RepeatN { count: 7, .. }
        ));
        assert!(matches!(
            draft.blocks[2].kind,
            BlockKind::RepeatUntil {
                max_iterations: Limit::Unlimited,
                ..
            }
        ));
        let BlockKind::WatchGroup { group } = &draft.blocks[3].kind else {
            panic!()
        };
        assert_eq!(group.timeout_ms, Limit::Unlimited);
        assert_eq!(group.cooldown_ms, 275);
    }

    #[test]
    fn duplication_remaps_internal_dependency_and_undo_is_bounded() {
        let subtree = Block {
            id: "loop".into(),
            enabled: true,
            kind: BlockKind::RepeatN {
                count: 1,
                body: vec![
                    Block {
                        id: "source".into(),
                        enabled: true,
                        kind: BlockKind::Observe {
                            condition: Condition::Text {
                                source_block_id: "source".into(),
                                rule_id: "r".into(),
                                mode: ObserveMode::CheckNow,
                            },
                        },
                    },
                    Block {
                        id: "click".into(),
                        enabled: true,
                        kind: BlockKind::Action {
                            action: Action::ClickTextMatch {
                                source_block_id: "source".into(),
                                button: MouseButton::Left,
                            },
                        },
                    },
                ],
            },
        };
        let mut draft = EditorDraft::new(def(vec![subtree]));
        apply_editor_command(
            &mut draft,
            EditorCommand::DuplicateBlock {
                source: path("loop"),
                target: InsertionTarget {
                    container: ContainerPath::Root,
                    index: 1,
                },
            },
        )
        .unwrap();
        let BlockKind::RepeatN { body, .. } = &draft.definition.blocks[1].kind else {
            panic!()
        };
        assert!(
            matches!(body[1].kind,BlockKind::Action{action:Action::ClickTextMatch{ref source_block_id,..}}if source_block_id=="source-copy")
        );
        for _ in 0..40 {
            let enabled = !draft.definition.blocks[0].enabled;
            apply_editor_command(
                &mut draft,
                EditorCommand::SetBlockEnabled {
                    path: path("loop"),
                    enabled,
                },
            )
            .unwrap();
        }
        assert_eq!(draft.undo_len(), EDITOR_UNDO_LIMIT);
    }

    #[test]
    fn sibling_reorder_and_lane_enable_are_revisioned() {
        let mut draft = EditorDraft::new(def(vec![comment("a"), comment("b"), comment("c")]));
        apply_editor_command(
            &mut draft,
            EditorCommand::ReorderSibling {
                path: path("c"),
                to_index: 0,
            },
        )
        .unwrap();
        assert_eq!(
            draft
                .definition
                .blocks
                .iter()
                .map(|b| b.id.as_str())
                .collect::<Vec<_>>(),
            vec!["c", "a", "b"]
        );
        assert_eq!(draft.definition.revision, 5);
    }
}
