use crate::engine::macro_engine::{
    Action, Block, BlockKind, Condition, Limit, MacroDefinition, ObserveMode, TimeoutOutcome,
};
use crate::macro_ui::{
    BlockPath, ContainerPath, EditorCommand, EditorDraft, InsertionTarget, locate_block_path,
    sibling_position,
};
use crate::ui_theme::BlockCategory;

#[derive(Debug, Clone, PartialEq)]
pub struct CanvasProjection {
    pub nodes: Vec<CanvasNode>,
    pub groups: Vec<CanvasGroup>,
    pub edges: Vec<CanvasEdge>,
}

impl CanvasProjection {
    pub fn node(&self, id: &str) -> Option<&CanvasNode> {
        self.nodes.iter().find(|node| node.id == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanvasNode {
    pub id: String,
    pub selection: CanvasSelection,
    pub category: BlockCategory,
    pub title: String,
    pub summary: String,
    pub outputs: Vec<OutputPort>,
    pub groups: Vec<CanvasGroupId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanvasGroup {
    pub id: CanvasGroupId,
    pub label: &'static str,
    pub member_ids: Vec<String>,
    pub selection: Option<CanvasSelection>,
    pub lane_priority: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanvasGroupKind {
    IfThen,
    IfElse,
    LoopBody,
    WatchLaneThen,
    TimeoutBody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasEdgeKind {
    Sequence,
    Branch,
    LoopReturn,
    WatchLane,
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanvasEdge {
    pub from: OutputPort,
    pub to: String,
    pub kind: CanvasEdgeKind,
    pub editable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanvasGroupId {
    pub owner_id: String,
    pub kind: CanvasGroupKind,
}

impl CanvasGroupId {
    pub fn loop_body(owner_id: impl Into<String>) -> Self {
        Self {
            owner_id: owner_id.into(),
            kind: CanvasGroupKind::LoopBody,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CanvasSelection {
    Block(String),
    Lane { group_id: String, lane_id: String },
    TimeoutBody { owner_id: String },
    IfThen { if_id: String },
    IfElse { if_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OutputPort {
    Next(String),
    IfThen(String),
    IfElse(String),
    LoopBody(String),
    WatchLane {
        group_id: String,
        lane_id: String,
    },
    TimeoutBody(String),
    /// A generated visual port for the return edge; never an editable insertion target.
    LoopReturn(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanvasConnectionError {
    MissingSource(String),
    MissingTarget(String),
    InvalidPort,
    IllegalSelf,
    IllegalDescendant,
}

impl CanvasConnectionError {
    pub fn message(&self) -> &'static str {
        match self {
            Self::MissingSource(_) => "The connector source no longer exists.",
            Self::MissingTarget(_) => "The drop target no longer exists.",
            Self::InvalidPort => "That connector is not available for this block.",
            Self::IllegalSelf => "A block cannot connect to itself.",
            Self::IllegalDescendant => "A container cannot be moved into its own body.",
        }
    }
}

pub fn project_canvas(definition: &MacroDefinition) -> CanvasProjection {
    let mut projection = CanvasProjection {
        nodes: Vec::new(),
        groups: Vec::new(),
        edges: Vec::new(),
    };
    append_container(
        &definition.blocks,
        &[],
        &mut projection,
        CanvasEdgeKind::Sequence,
    );
    projection
}

pub fn insertion_target_for_port(
    definition: &MacroDefinition,
    port: &OutputPort,
) -> Result<InsertionTarget, CanvasConnectionError> {
    match port {
        OutputPort::Next(id) => {
            let path = locate_block_path(definition, id)
                .ok_or_else(|| CanvasConnectionError::MissingSource(id.clone()))?;
            let block = crate::macro_ui::block_at_path(definition, &path)
                .ok_or_else(|| CanvasConnectionError::MissingSource(id.clone()))?;
            if !output_ports(block).contains(port) {
                return Err(CanvasConnectionError::InvalidPort);
            }
            let (index, _) = sibling_position(definition, &path)
                .ok_or_else(|| CanvasConnectionError::MissingSource(id.clone()))?;
            Ok(InsertionTarget {
                container: path.container,
                index: index + 1,
            })
        }
        OutputPort::IfThen(id) => owned_container_target(
            definition,
            id,
            ContainerPath::IfThen { if_id: id.clone() },
            |block| matches!(block.kind, BlockKind::If { .. }),
        ),
        OutputPort::IfElse(id) => owned_container_target(
            definition,
            id,
            ContainerPath::IfElse { if_id: id.clone() },
            |block| matches!(block.kind, BlockKind::If { .. }),
        ),
        OutputPort::LoopBody(id) => owned_container_target(
            definition,
            id,
            ContainerPath::LoopBody {
                loop_id: id.clone(),
            },
            |block| {
                matches!(
                    block.kind,
                    BlockKind::RepeatN { .. }
                        | BlockKind::RepeatUntil { .. }
                        | BlockKind::Continuous { .. }
                )
            },
        ),
        OutputPort::WatchLane { group_id, lane_id } => owned_container_target(
            definition,
            group_id,
            ContainerPath::WatchLaneBody {
                watch_id: group_id.clone(),
                lane_id: lane_id.clone(),
            },
            |block| match &block.kind {
                BlockKind::WatchGroup { group } => {
                    group.lanes.iter().any(|lane| lane.id == *lane_id)
                }
                _ => false,
            },
        ),
        OutputPort::TimeoutBody(id) => owned_container_target(
            definition,
            id,
            ContainerPath::TimeoutBody {
                owner_id: id.clone(),
            },
            has_timeout_body,
        ),
        OutputPort::LoopReturn(_) => Err(CanvasConnectionError::InvalidPort),
    }
}

pub fn connection_command(
    draft: &EditorDraft,
    port: OutputPort,
    target_block_id: &str,
) -> Result<EditorCommand, CanvasConnectionError> {
    let target = locate_block_path(&draft.definition, target_block_id)
        .ok_or_else(|| CanvasConnectionError::MissingTarget(target_block_id.into()))?;
    let mut destination = insertion_target_for_port(&draft.definition, &port)?;
    let anchor_id = port_owner_id(&port);
    if target.block_id == anchor_id {
        return Err(CanvasConnectionError::IllegalSelf);
    }
    if block_contains(&draft.definition.blocks, target_block_id, anchor_id) {
        return Err(CanvasConnectionError::IllegalDescendant);
    }
    adjust_move_index(&draft.definition, &target, &mut destination);
    Ok(EditorCommand::MoveBlock {
        source: target,
        target: destination,
    })
}

fn owned_container_target(
    definition: &MacroDefinition,
    owner_id: &str,
    container: ContainerPath,
    accepts: impl FnOnce(&Block) -> bool,
) -> Result<InsertionTarget, CanvasConnectionError> {
    let path = locate_block_path(definition, owner_id)
        .ok_or_else(|| CanvasConnectionError::MissingSource(owner_id.into()))?;
    let owner = crate::macro_ui::block_at_path(definition, &path)
        .ok_or_else(|| CanvasConnectionError::MissingSource(owner_id.into()))?;
    if !accepts(owner) {
        return Err(CanvasConnectionError::InvalidPort);
    }
    let index = crate::macro_ui::container_len(definition, &container)
        .ok_or(CanvasConnectionError::InvalidPort)?;
    Ok(InsertionTarget { container, index })
}

fn adjust_move_index(
    definition: &MacroDefinition,
    source: &BlockPath,
    destination: &mut InsertionTarget,
) {
    if source.container != destination.container {
        return;
    }
    let Some((source_index, _)) = sibling_position(definition, source) else {
        return;
    };
    if source_index < destination.index {
        destination.index -= 1;
    }
}

fn port_owner_id(port: &OutputPort) -> &str {
    match port {
        OutputPort::Next(id)
        | OutputPort::IfThen(id)
        | OutputPort::IfElse(id)
        | OutputPort::LoopBody(id)
        | OutputPort::TimeoutBody(id)
        | OutputPort::LoopReturn(id) => id,
        OutputPort::WatchLane { group_id, .. } => group_id,
    }
}

fn block_contains(blocks: &[Block], ancestor_id: &str, id: &str) -> bool {
    for block in blocks {
        if block.id == ancestor_id {
            return descendants_contain(block, id);
        }
        if child_bodies(block)
            .into_iter()
            .any(|body| block_contains(body, ancestor_id, id))
        {
            return true;
        }
    }
    false
}

fn descendants_contain(block: &Block, id: &str) -> bool {
    child_bodies(block)
        .into_iter()
        .flatten()
        .any(|child| child.id == id || descendants_contain(child, id))
}

fn append_container(
    blocks: &[Block],
    groups: &[CanvasGroupId],
    projection: &mut CanvasProjection,
    sequence_kind: CanvasEdgeKind,
) {
    for block in blocks {
        let (title, summary) = block_presentation(&block.kind);
        projection.nodes.push(CanvasNode {
            id: block.id.clone(),
            selection: CanvasSelection::Block(block.id.clone()),
            category: block_category(&block.kind),
            title,
            summary,
            outputs: output_ports(block),
            groups: groups.to_vec(),
        });
        append_owned_containers(block, groups, projection);
    }

    for pair in blocks.windows(2) {
        let from = OutputPort::Next(pair[0].id.clone());
        if output_ports(&pair[0]).contains(&from) {
            projection.edges.push(CanvasEdge {
                from,
                to: pair[1].id.clone(),
                kind: sequence_kind,
                editable: true,
            });
        }
    }
}

fn append_owned_containers(
    block: &Block,
    parent_groups: &[CanvasGroupId],
    projection: &mut CanvasProjection,
) {
    match &block.kind {
        BlockKind::If {
            condition,
            then_body,
            else_body,
        } => {
            append_group(
                block,
                CanvasGroupKind::IfThen,
                "THEN",
                then_body,
                parent_groups,
                projection,
                OutputPort::IfThen(block.id.clone()),
                CanvasEdgeKind::Branch,
            );
            append_group(
                block,
                CanvasGroupKind::IfElse,
                "ELSE",
                else_body,
                parent_groups,
                projection,
                OutputPort::IfElse(block.id.clone()),
                CanvasEdgeKind::Branch,
            );
            append_condition_timeout(block, condition, parent_groups, projection);
        }
        BlockKind::RepeatN { body, .. } | BlockKind::Continuous { body } => {
            append_loop_group(block, body, parent_groups, projection);
        }
        BlockKind::RepeatUntil {
            condition, body, ..
        } => {
            append_loop_group(block, body, parent_groups, projection);
            append_condition_timeout(block, condition, parent_groups, projection);
        }
        BlockKind::WatchGroup { group } => {
            for (index, lane) in group.lanes.iter().enumerate() {
                let group_id = CanvasGroupId {
                    owner_id: format!("{}:{}", block.id, lane.id),
                    kind: CanvasGroupKind::WatchLaneThen,
                };
                append_group_with_id(
                    group_id,
                    "THEN",
                    &lane.then_body,
                    parent_groups,
                    projection,
                    OutputPort::WatchLane {
                        group_id: block.id.clone(),
                        lane_id: lane.id.clone(),
                    },
                    CanvasEdgeKind::WatchLane,
                    Some(CanvasSelection::Lane {
                        group_id: block.id.clone(),
                        lane_id: lane.id.clone(),
                    }),
                    Some(index + 1),
                );
            }
            if let TimeoutOutcome::RunBody { body } = &group.timeout_outcome {
                append_group(
                    block,
                    CanvasGroupKind::TimeoutBody,
                    "ON TIMEOUT",
                    body,
                    parent_groups,
                    projection,
                    OutputPort::TimeoutBody(block.id.clone()),
                    CanvasEdgeKind::Timeout,
                );
            }
        }
        BlockKind::Observe { condition } => {
            append_condition_timeout(block, condition, parent_groups, projection);
        }
        BlockKind::Action { .. }
        | BlockKind::Wait { .. }
        | BlockKind::StopSuccess
        | BlockKind::StopError { .. }
        | BlockKind::Comment { .. } => {}
    }
}

fn append_loop_group(
    block: &Block,
    body: &[Block],
    parent_groups: &[CanvasGroupId],
    projection: &mut CanvasProjection,
) {
    let id = CanvasGroupId::loop_body(block.id.clone());
    append_group_with_id(
        id,
        "LOOP BODY",
        body,
        parent_groups,
        projection,
        OutputPort::LoopBody(block.id.clone()),
        CanvasEdgeKind::Sequence,
        None,
        None,
    );
    projection.edges.push(CanvasEdge {
        from: OutputPort::LoopReturn(block.id.clone()),
        to: block.id.clone(),
        kind: CanvasEdgeKind::LoopReturn,
        editable: false,
    });
}

fn append_condition_timeout(
    block: &Block,
    condition: &Condition,
    parent_groups: &[CanvasGroupId],
    projection: &mut CanvasProjection,
) {
    if let Some(body) = condition_timeout_body(condition) {
        append_group(
            block,
            CanvasGroupKind::TimeoutBody,
            "ON TIMEOUT",
            body,
            parent_groups,
            projection,
            OutputPort::TimeoutBody(block.id.clone()),
            CanvasEdgeKind::Timeout,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn append_group(
    owner: &Block,
    kind: CanvasGroupKind,
    label: &'static str,
    body: &[Block],
    parent_groups: &[CanvasGroupId],
    projection: &mut CanvasProjection,
    output: OutputPort,
    edge_kind: CanvasEdgeKind,
) {
    append_group_with_id(
        CanvasGroupId {
            owner_id: owner.id.clone(),
            kind,
        },
        label,
        body,
        parent_groups,
        projection,
        output,
        edge_kind,
        match kind {
            CanvasGroupKind::TimeoutBody => Some(CanvasSelection::TimeoutBody {
                owner_id: owner.id.clone(),
            }),
            CanvasGroupKind::IfThen => Some(CanvasSelection::IfThen {
                if_id: owner.id.clone(),
            }),
            CanvasGroupKind::IfElse => Some(CanvasSelection::IfElse {
                if_id: owner.id.clone(),
            }),
            CanvasGroupKind::LoopBody | CanvasGroupKind::WatchLaneThen => None,
        },
        None,
    );
}

#[allow(clippy::too_many_arguments)]
fn append_group_with_id(
    id: CanvasGroupId,
    label: &'static str,
    body: &[Block],
    parent_groups: &[CanvasGroupId],
    projection: &mut CanvasProjection,
    output: OutputPort,
    edge_kind: CanvasEdgeKind,
    selection: Option<CanvasSelection>,
    lane_priority: Option<usize>,
) {
    projection.groups.push(CanvasGroup {
        id: id.clone(),
        label,
        member_ids: body.iter().map(|block| block.id.clone()).collect(),
        selection,
        lane_priority,
    });
    if let Some(first) = body.first() {
        projection.edges.push(CanvasEdge {
            from: output,
            to: first.id.clone(),
            kind: edge_kind,
            editable: true,
        });
    }
    let mut nested_groups = parent_groups.to_vec();
    nested_groups.push(id);
    append_container(body, &nested_groups, projection, CanvasEdgeKind::Sequence);
}

fn output_ports(block: &Block) -> Vec<OutputPort> {
    let mut ports = match &block.kind {
        BlockKind::If { .. } => vec![
            OutputPort::IfThen(block.id.clone()),
            OutputPort::IfElse(block.id.clone()),
            OutputPort::Next(block.id.clone()),
        ],
        BlockKind::RepeatN { .. }
        | BlockKind::RepeatUntil { .. }
        | BlockKind::Continuous { .. } => vec![
            OutputPort::LoopBody(block.id.clone()),
            OutputPort::Next(block.id.clone()),
            OutputPort::LoopReturn(block.id.clone()),
        ],
        BlockKind::WatchGroup { group } => {
            let mut ports = group
                .lanes
                .iter()
                .map(|lane| OutputPort::WatchLane {
                    group_id: block.id.clone(),
                    lane_id: lane.id.clone(),
                })
                .collect::<Vec<_>>();
            ports.push(OutputPort::Next(block.id.clone()));
            ports
        }
        BlockKind::StopSuccess | BlockKind::StopError { .. } => Vec::new(),
        BlockKind::Observe { .. }
        | BlockKind::Action { .. }
        | BlockKind::Wait { .. }
        | BlockKind::Comment { .. } => vec![OutputPort::Next(block.id.clone())],
    };
    if has_timeout_body(block) {
        ports.push(OutputPort::TimeoutBody(block.id.clone()));
    }
    ports
}

fn has_timeout_body(block: &Block) -> bool {
    match &block.kind {
        BlockKind::Observe { condition }
        | BlockKind::If { condition, .. }
        | BlockKind::RepeatUntil { condition, .. } => condition_timeout_body(condition).is_some(),
        BlockKind::WatchGroup { group } => {
            matches!(group.timeout_outcome, TimeoutOutcome::RunBody { .. })
        }
        _ => false,
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
            } => match timeout_outcome {
                TimeoutOutcome::RunBody { body } => Some(body),
                TimeoutOutcome::StopError { .. } | TimeoutOutcome::Continue => None,
            },
            ObserveMode::CheckNow => None,
        },
    }
}

fn child_bodies(block: &Block) -> Vec<&[Block]> {
    match &block.kind {
        BlockKind::If {
            condition,
            then_body,
            else_body,
        } => {
            let mut bodies = vec![then_body.as_slice(), else_body.as_slice()];
            if let Some(timeout) = condition_timeout_body(condition) {
                bodies.push(timeout);
            }
            bodies
        }
        BlockKind::RepeatN { body, .. } | BlockKind::Continuous { body } => vec![body],
        BlockKind::RepeatUntil {
            condition, body, ..
        } => {
            let mut bodies = vec![body.as_slice()];
            if let Some(timeout) = condition_timeout_body(condition) {
                bodies.push(timeout);
            }
            bodies
        }
        BlockKind::Observe { condition } => condition_timeout_body(condition).into_iter().collect(),
        BlockKind::WatchGroup { group } => {
            let mut bodies: Vec<&[Block]> = group
                .lanes
                .iter()
                .map(|lane| lane.then_body.as_slice())
                .collect();
            if let TimeoutOutcome::RunBody { body } = &group.timeout_outcome {
                bodies.push(body);
            }
            bodies
        }
        _ => Vec::new(),
    }
}

pub(crate) fn block_category(kind: &BlockKind) -> BlockCategory {
    match kind {
        BlockKind::Observe { .. } | BlockKind::WatchGroup { .. } => BlockCategory::Observe,
        BlockKind::If { .. } => BlockCategory::Decide,
        BlockKind::RepeatN { .. }
        | BlockKind::RepeatUntil { .. }
        | BlockKind::Continuous { .. } => BlockCategory::Repeat,
        BlockKind::Action { .. }
        | BlockKind::Wait { .. }
        | BlockKind::StopSuccess
        | BlockKind::StopError { .. }
        | BlockKind::Comment { .. } => BlockCategory::Act,
    }
}

pub(crate) fn block_presentation(kind: &BlockKind) -> (String, String) {
    match kind {
        BlockKind::Observe { condition } => condition_presentation(condition),
        BlockKind::Action { action } => {
            let text = action_presentation(action);
            (text.clone(), text)
        }
        BlockKind::If { condition, .. } => ("If".into(), condition_summary(condition)),
        BlockKind::Wait { duration_ms } => ("Wait".into(), format_duration(*duration_ms)),
        BlockKind::RepeatN { count, .. } => ("Repeat".into(), format!("{count} iterations")),
        BlockKind::RepeatUntil {
            condition,
            max_iterations,
            ..
        } => (
            "Repeat Until".into(),
            format!(
                "{} | {} iterations",
                condition_summary(condition),
                format_limit(max_iterations)
            ),
        ),
        BlockKind::Continuous { .. } => ("Continuous Loop".into(), "Until stopped".into()),
        BlockKind::WatchGroup { group } => (
            "Watch Group".into(),
            format!(
                "{} lanes | {} timeout",
                group.lanes.len(),
                format_limit(&group.timeout_ms)
            ),
        ),
        BlockKind::StopSuccess => ("Stop".into(), "Complete successfully".into()),
        BlockKind::StopError { message } => ("Stop Error".into(), message.clone()),
        BlockKind::Comment { text } => ("Note".into(), text.clone()),
    }
}

fn condition_presentation(condition: &Condition) -> (String, String) {
    let title = match condition {
        Condition::Text { mode, .. } => match mode {
            ObserveMode::CheckNow => "Check text",
            ObserveMode::WaitForTrue { .. } => "Wait for text",
            ObserveMode::WaitForFalse { .. } => "Wait until text is absent",
        },
        Condition::Image { mode, .. } => match mode {
            ObserveMode::CheckNow => "Check image",
            ObserveMode::WaitForTrue { .. } => "Wait for image",
            ObserveMode::WaitForFalse { .. } => "Wait until image is absent",
        },
    };
    (title.into(), condition_summary(condition))
}

fn condition_summary(condition: &Condition) -> String {
    match condition {
        Condition::Text { rule_id, mode, .. } => format!("{} text | {rule_id}", observe_verb(mode)),
        Condition::Image { rule_id, mode, .. } => {
            format!("{} image | {rule_id}", observe_verb(mode))
        }
    }
}

fn observe_verb(mode: &ObserveMode) -> &'static str {
    match mode {
        ObserveMode::CheckNow => "Check",
        ObserveMode::WaitForTrue { .. } => "Wait for",
        ObserveMode::WaitForFalse { .. } => "Wait until absent",
    }
}

fn action_presentation(action: &Action) -> String {
    match action {
        Action::ClickTextMatch { button, .. } => format!("{button:?}-click text match"),
        Action::ClickImageMatch { button, .. } => format!("{button:?}-click image match"),
        Action::ClickPoint { point_id, button } => format!("{button:?}-click point | {point_id}"),
        Action::ClickRegion { region_id, button } => {
            format!("{button:?}-click region | {region_id}")
        }
        Action::MoveOnly { .. } => "Move pointer without clicking".into(),
    }
}

fn format_limit<T: std::fmt::Display>(limit: &Limit<T>) -> String {
    match limit {
        Limit::Finite(value) => value.to_string(),
        Limit::Unlimited => "Unlimited".into(),
    }
}

fn format_duration(duration_ms: u64) -> String {
    if duration_ms >= 1_000 && duration_ms.is_multiple_of(1_000) {
        format!("{} seconds", duration_ms / 1_000)
    } else {
        format!("{duration_ms} ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macro_ui::test_support::{
        fixture_continuous_with_observe_and_action, fixture_if, fixture_nested_loop_draft,
    };
    use crate::macro_ui::{ContainerPath, apply_editor_command};

    #[test]
    fn continuous_loop_projects_as_owned_group_with_generated_return_edge() {
        let definition = fixture_continuous_with_observe_and_action();
        let graph = project_canvas(&definition);
        assert!(
            graph
                .groups
                .iter()
                .any(|group| group.id.kind == CanvasGroupKind::LoopBody)
        );
        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.kind == CanvasEdgeKind::LoopReturn && !edge.editable)
        );
        assert!(
            graph
                .node("observe")
                .unwrap()
                .groups
                .contains(&CanvasGroupId::loop_body("loop"))
        );
    }

    #[test]
    fn if_ports_are_fixed_then_and_else_roles() {
        let graph = project_canvas(&fixture_if());
        let node = graph.node("if-1").unwrap();
        assert_eq!(
            node.outputs,
            vec![
                OutputPort::IfThen("if-1".into()),
                OutputPort::IfElse("if-1".into()),
                OutputPort::Next("if-1".into()),
            ]
        );
    }

    #[test]
    fn if_groups_expose_typed_then_and_else_insert_targets() {
        let graph = project_canvas(&fixture_if());
        let then_group = graph
            .groups
            .iter()
            .find(|group| group.id.kind == CanvasGroupKind::IfThen)
            .unwrap();
        let else_group = graph
            .groups
            .iter()
            .find(|group| group.id.kind == CanvasGroupKind::IfElse)
            .unwrap();
        assert_eq!(then_group.label, "THEN");
        assert_eq!(else_group.label, "ELSE");
        assert_eq!(
            then_group.selection,
            Some(CanvasSelection::IfThen {
                if_id: "if-1".into()
            })
        );
        assert_eq!(
            else_group.selection,
            Some(CanvasSelection::IfElse {
                if_id: "if-1".into()
            })
        );
    }

    #[test]
    fn control_blocks_keep_structural_continuations_and_loop_return_port() {
        use crate::engine::macro_engine::{
            Block, BlockKind, Limit, PassiveCondition, WatchGroup, WatchLane,
        };

        let mut definition = fixture_continuous_with_observe_and_action();
        definition.blocks.push(Block {
            id: "after-loop".into(),
            enabled: true,
            kind: BlockKind::Comment {
                text: "after".into(),
            },
        });
        definition.blocks.push(Block {
            id: "watch".into(),
            enabled: true,
            kind: BlockKind::WatchGroup {
                group: WatchGroup {
                    lanes: vec![WatchLane {
                        id: "lane-1".into(),
                        enabled: true,
                        condition: PassiveCondition::Text {
                            source_block_id: "observe".into(),
                            rule_id: "rule".into(),
                        },
                        then_body: vec![],
                    }],
                    timeout_ms: Limit::Unlimited,
                    timeout_outcome: TimeoutOutcome::Continue,
                    cooldown_ms: 1,
                },
            },
        });
        definition.blocks.push(Block {
            id: "after-watch".into(),
            enabled: true,
            kind: BlockKind::Comment {
                text: "after".into(),
            },
        });

        let graph = project_canvas(&definition);
        let loop_node = graph.node("loop").unwrap();
        assert!(loop_node.outputs.contains(&OutputPort::Next("loop".into())));
        assert!(
            loop_node
                .outputs
                .contains(&OutputPort::LoopReturn("loop".into()))
        );
        assert!(
            graph
                .node("watch")
                .unwrap()
                .outputs
                .contains(&OutputPort::Next("watch".into()))
        );
        assert!(graph.edges.iter().any(|edge| {
            edge.from == OutputPort::Next("loop".into())
                && edge.to == "after-loop"
                && edge.kind == CanvasEdgeKind::Sequence
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.from == OutputPort::Next("watch".into())
                && edge.to == "after-watch"
                && edge.kind == CanvasEdgeKind::Sequence
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.from == OutputPort::LoopReturn("loop".into())
                && edge.to == "loop"
                && edge.kind == CanvasEdgeKind::LoopReturn
                && !edge.editable
        }));
    }

    #[test]
    fn if_next_port_derives_the_following_sibling_edge() {
        use crate::engine::macro_engine::{Block, BlockKind};

        let mut definition = fixture_if();
        definition.blocks.push(Block {
            id: "after-if".into(),
            enabled: true,
            kind: BlockKind::Comment {
                text: "after".into(),
            },
        });
        let graph = project_canvas(&definition);
        assert!(graph.edges.iter().any(|edge| {
            edge.from == OutputPort::Next("if-1".into())
                && edge.to == "after-if"
                && edge.kind == CanvasEdgeKind::Sequence
                && edge.editable
        }));
    }

    #[test]
    fn watch_lane_groups_preserve_vector_priority_and_typed_selection() {
        use crate::engine::macro_engine::{
            Block, BlockKind, Limit, PassiveCondition, WatchGroup, WatchLane,
        };

        let lane = |id: &str| WatchLane {
            id: id.into(),
            enabled: true,
            condition: PassiveCondition::Text {
                source_block_id: "observe".into(),
                rule_id: "rule".into(),
            },
            then_body: vec![],
        };
        let mut definition = fixture_if();
        definition.blocks = vec![Block {
            id: "watch".into(),
            enabled: true,
            kind: BlockKind::WatchGroup {
                group: WatchGroup {
                    lanes: vec![lane("first"), lane("second"), lane("third")],
                    timeout_ms: Limit::Unlimited,
                    timeout_outcome: TimeoutOutcome::Continue,
                    cooldown_ms: 1,
                },
            },
        }];

        let graph = project_canvas(&definition);
        let lanes = graph
            .groups
            .iter()
            .filter(|group| group.id.kind == CanvasGroupKind::WatchLaneThen)
            .collect::<Vec<_>>();
        assert_eq!(
            lanes
                .iter()
                .map(|group| group.lane_priority)
                .collect::<Vec<_>>(),
            vec![Some(1), Some(2), Some(3)]
        );
        assert_eq!(
            lanes[1].selection,
            Some(CanvasSelection::Lane {
                group_id: "watch".into(),
                lane_id: "second".into(),
            })
        );
    }

    #[test]
    fn timeout_bodies_project_as_owned_typed_groups_without_id_collisions() {
        use crate::engine::macro_engine::{Block, BlockKind, Condition, Limit, ObserveMode};

        let timeout_condition = |owner: &str, child: &str| Condition::Text {
            source_block_id: owner.into(),
            rule_id: "rule".into(),
            mode: ObserveMode::WaitForTrue {
                timeout_ms: Limit::Finite(100),
                timeout_outcome: TimeoutOutcome::RunBody {
                    body: vec![Block {
                        id: child.into(),
                        enabled: true,
                        kind: BlockKind::Comment {
                            text: "fallback".into(),
                        },
                    }],
                },
            },
        };
        let mut definition = fixture_if();
        definition.blocks = vec![
            Block {
                id: "owner".into(),
                enabled: true,
                kind: BlockKind::Observe {
                    condition: timeout_condition("owner", "timeout-child"),
                },
            },
            Block {
                id: "owner-timeout".into(),
                enabled: true,
                kind: BlockKind::Comment {
                    text: "real block".into(),
                },
            },
        ];

        let graph = project_canvas(&definition);
        assert!(graph.groups.iter().any(|group| group.selection
            == Some(CanvasSelection::TimeoutBody {
                owner_id: "owner".into(),
            })));
        assert_eq!(
            graph.node("timeout-child").unwrap().groups,
            vec![CanvasGroupId {
                owner_id: "owner".into(),
                kind: CanvasGroupKind::TimeoutBody,
            }]
        );
        assert_eq!(
            graph.node("owner-timeout").unwrap().selection,
            CanvasSelection::Block("owner-timeout".into())
        );
    }

    #[test]
    fn cross_descendant_connection_is_rejected_without_mutation() {
        let draft = fixture_nested_loop_draft();
        let before = draft.definition.clone();
        assert_eq!(
            connection_command(&draft, OutputPort::Next("child".into()), "loop"),
            Err(CanvasConnectionError::IllegalDescendant)
        );
        assert_eq!(draft.definition, before);
    }

    #[test]
    fn projection_keeps_plain_language_block_labels_and_summaries() {
        let graph = project_canvas(&fixture_continuous_with_observe_and_action());
        assert_eq!(graph.node("loop").unwrap().title, "Continuous Loop");
        assert_eq!(
            graph.node("action").unwrap().summary,
            "Left-click text match"
        );
    }

    #[test]
    fn branch_connection_translates_to_the_existing_move_command() {
        let mut draft = crate::macro_ui::EditorDraft::new(fixture_if());
        let command =
            connection_command(&draft, OutputPort::IfThen("if-1".into()), "else-note").unwrap();
        assert_eq!(
            command,
            EditorCommand::MoveBlock {
                source: BlockPath {
                    container: ContainerPath::IfElse {
                        if_id: "if-1".into(),
                    },
                    block_id: "else-note".into(),
                },
                target: InsertionTarget {
                    container: ContainerPath::IfThen {
                        if_id: "if-1".into(),
                    },
                    index: 1,
                },
            }
        );
        apply_editor_command(&mut draft, command).unwrap();
        let graph = project_canvas(&draft.definition);
        assert!(
            graph
                .node("else-note")
                .unwrap()
                .groups
                .contains(&CanvasGroupId {
                    owner_id: "if-1".into(),
                    kind: CanvasGroupKind::IfThen,
                })
        );
    }
}
