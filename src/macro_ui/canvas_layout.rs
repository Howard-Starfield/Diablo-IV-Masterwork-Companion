use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use eframe::egui::{Pos2, Rect, Vec2};

use crate::macro_ui::canvas_model::{CanvasEdgeKind, CanvasNode, CanvasProjection, OutputPort};
use crate::ui_state::MacroCanvasLayout;

pub const MIN_ZOOM: f32 = 0.5;
pub const MIN_FIT_ZOOM: f32 = 0.001;
pub const MAX_ZOOM: f32 = 1.75;
pub const NODE_WIDTH: f32 = 280.0;
pub const NODE_HEIGHT: f32 = 88.0;
pub const LAYER_GAP: f32 = 72.0;
pub const SIBLING_GAP: f32 = 36.0;
const FIT_PADDING: f32 = 32.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanvasLayoutError {
    MissingNode(String),
    NonFinitePosition,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LayoutEdit {
    NodePosition {
        id: String,
        before: Option<[f32; 2]>,
        after: Option<[f32; 2]>,
    },
    Layout {
        before: MacroCanvasLayout,
        after: MacroCanvasLayout,
    },
}

impl LayoutEdit {
    pub fn apply_before(&self, layout: &mut MacroCanvasLayout) {
        self.apply(layout, true);
    }

    pub fn apply_after(&self, layout: &mut MacroCanvasLayout) {
        self.apply(layout, false);
    }

    fn apply(&self, layout: &mut MacroCanvasLayout, before: bool) {
        match self {
            Self::NodePosition {
                id,
                before: previous,
                after: next,
            } => match if before { previous } else { next } {
                Some(position) => {
                    layout.node_positions.insert(id.clone(), *position);
                }
                None => {
                    layout.node_positions.remove(id);
                }
            },
            Self::Layout {
                before: previous,
                after: next,
            } => {
                *layout = if before {
                    previous.clone()
                } else {
                    next.clone()
                }
            }
        }
    }
}

pub struct CanvasLayoutEngine;

impl CanvasLayoutEngine {
    pub fn move_node(
        layout: &mut MacroCanvasLayout,
        block_id: &str,
        position: [f32; 2],
    ) -> Result<LayoutEdit, CanvasLayoutError> {
        if !position.iter().all(|value| value.is_finite()) {
            return Err(CanvasLayoutError::NonFinitePosition);
        }
        let before = layout.node_positions.get(block_id).copied();
        if before == Some(position) {
            return Ok(LayoutEdit::NodePosition {
                id: block_id.into(),
                before,
                after: before,
            });
        }
        layout.node_positions.insert(block_id.into(), position);
        Ok(LayoutEdit::NodePosition {
            id: block_id.into(),
            before,
            after: Some(position),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CanvasViewport {
    pub pan: Vec2,
    pub zoom: f32,
    canvas_size: [f32; 2],
}

impl Default for CanvasViewport {
    fn default() -> Self {
        Self {
            pan: Vec2::ZERO,
            zoom: 1.0,
            canvas_size: [1.0, 1.0],
        }
    }
}

impl CanvasViewport {
    pub fn from_layout(layout: &MacroCanvasLayout, canvas_size: [f32; 2]) -> Self {
        Self {
            pan: Vec2::new(layout.pan[0], layout.pan[1]),
            zoom: valid_persisted_zoom(layout.zoom),
            canvas_size: sane_size(canvas_size),
        }
    }

    pub fn write_to_layout(&self, layout: &mut MacroCanvasLayout) {
        layout.pan = [self.pan.x, self.pan.y];
        layout.zoom = valid_persisted_zoom(self.zoom);
    }

    pub fn screen_from_world(&self, canvas: Rect, world: Pos2) -> Pos2 {
        canvas.min + self.pan + world.to_vec2() * self.zoom
    }

    pub fn world_from_screen(&self, canvas: Rect, screen: Pos2) -> Pos2 {
        ((screen - canvas.min - self.pan) / self.zoom.max(f32::MIN_POSITIVE)).to_pos2()
    }

    pub fn zoom_around(&mut self, canvas: Rect, pointer: Pos2, factor: f32) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let world = self.world_from_screen(canvas, pointer);
        let next_zoom = valid_persisted_zoom(self.zoom * factor);
        self.zoom = if self.zoom >= MIN_ZOOM {
            next_zoom.max(MIN_ZOOM)
        } else {
            next_zoom
        };
        self.canvas_size = sane_size([canvas.width(), canvas.height()]);
        self.pan = pointer - canvas.min - world.to_vec2() * self.zoom;
    }

    pub fn visible_world_rect(&self) -> Rect {
        let size = sane_size(self.canvas_size);
        let zoom = self.zoom.max(f32::MIN_POSITIVE);
        Rect::from_min_size(
            ((-self.pan) / zoom).to_pos2(),
            Vec2::new(size[0] / zoom, size[1] / zoom),
        )
    }
}

pub fn auto_arrange(graph: &CanvasProjection) -> MacroCanvasLayout {
    let depths = node_depths(graph);
    let mut per_layer = BTreeMap::<usize, Vec<&CanvasNode>>::new();
    for node in &graph.nodes {
        per_layer
            .entry(*depths.get(&node.id).unwrap_or(&0))
            .or_default()
            .push(node);
    }
    let mut layout = MacroCanvasLayout::default();
    for (layer, nodes) in per_layer {
        let mut nodes = nodes;
        nodes.sort_by(|left, right| {
            node_lane_key(graph, left)
                .cmp(&node_lane_key(graph, right))
                .then_with(|| left.id.cmp(&right.id))
        });
        let width =
            nodes.len() as f32 * NODE_WIDTH + nodes.len().saturating_sub(1) as f32 * SIBLING_GAP;
        for (index, node) in nodes.iter().enumerate() {
            layout.node_positions.insert(
                node.id.clone(),
                [
                    index as f32 * (NODE_WIDTH + SIBLING_GAP) - width / 2.0,
                    layer as f32 * (NODE_HEIGHT + LAYER_GAP),
                ],
            );
        }
    }
    layout
}

pub fn reconcile_layout(
    graph: &CanvasProjection,
    mut saved: MacroCanvasLayout,
) -> MacroCanvasLayout {
    let arranged = auto_arrange(graph);
    let current_ids = graph
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<HashSet<_>>();
    saved.node_positions.retain(|id, position| {
        current_ids.contains(id.as_str()) && position.iter().all(|value| value.is_finite())
    });
    for (id, position) in arranged.node_positions {
        saved.node_positions.entry(id).or_insert(position);
    }
    if !saved.pan.iter().all(|value| value.is_finite()) {
        saved.pan = [0.0, 0.0];
    }
    saved.zoom = valid_persisted_zoom(saved.zoom);
    saved
}

pub fn node_rect(node: &CanvasNode, layout: &MacroCanvasLayout) -> Rect {
    let position = layout
        .node_positions
        .get(&node.id)
        .copied()
        .unwrap_or([0.0, 0.0]);
    Rect::from_min_size(
        Pos2::new(position[0], position[1]),
        Vec2::new(NODE_WIDTH, NODE_HEIGHT),
    )
}

pub fn graph_bounds(graph: &CanvasProjection, layout: &MacroCanvasLayout) -> Rect {
    graph
        .nodes
        .iter()
        .map(|node| node_rect(node, layout))
        .reduce(|left, right| left.union(right))
        .unwrap_or_else(|| Rect::from_min_size(Pos2::ZERO, Vec2::splat(1.0)))
}

pub fn fit_view(canvas_size: [f32; 2], world_bounds: Rect) -> CanvasViewport {
    let canvas_size = sane_size(canvas_size);
    let available = Vec2::new(
        (canvas_size[0] - FIT_PADDING * 2.0).max(1.0),
        (canvas_size[1] - FIT_PADDING * 2.0).max(1.0),
    );
    let requested_zoom = (available.x / world_bounds.width().max(1.0))
        .min(available.y / world_bounds.height().max(1.0));
    let zoom = valid_persisted_zoom(requested_zoom);
    CanvasViewport {
        pan: Vec2::new(
            (canvas_size[0] - world_bounds.width() * zoom) / 2.0 - world_bounds.min.x * zoom,
            (canvas_size[1] - world_bounds.height() * zoom) / 2.0 - world_bounds.min.y * zoom,
        ),
        zoom,
        canvas_size,
    }
}

pub fn visible_nodes<'a>(
    graph: &'a CanvasProjection,
    layout: &MacroCanvasLayout,
    viewport: &CanvasViewport,
    canvas: Rect,
) -> Vec<&'a CanvasNode> {
    let mut viewport = viewport.clone();
    viewport.canvas_size = sane_size([canvas.width(), canvas.height()]);
    let visible = viewport.visible_world_rect();
    graph
        .nodes
        .iter()
        .filter(|node| node_rect(node, layout).intersects(visible))
        .collect()
}

fn node_depths(graph: &CanvasProjection) -> HashMap<String, usize> {
    let mut next = HashMap::<String, Vec<String>>::new();
    let mut incoming = HashSet::<String>::new();
    for edge in &graph.edges {
        if edge.kind == CanvasEdgeKind::LoopReturn {
            continue;
        }
        let owner = output_owner(&edge.from);
        if graph.node(owner).is_some() && graph.node(&edge.to).is_some() {
            next.entry(owner.into()).or_default().push(edge.to.clone());
            incoming.insert(edge.to.clone());
        }
    }
    let mut depths = HashMap::new();
    let mut queue = VecDeque::new();
    for node in &graph.nodes {
        if !incoming.contains(&node.id) {
            depths.insert(node.id.clone(), 0);
            queue.push_back(node.id.clone());
        }
    }
    while let Some(id) = queue.pop_front() {
        let depth = depths[&id];
        for target in next.get(&id).into_iter().flatten() {
            let target_depth = depth + 1;
            if depths.get(target).is_none_or(|known| target_depth > *known) {
                depths.insert(target.clone(), target_depth);
                queue.push_back(target.clone());
            }
        }
    }
    depths
}

fn output_owner(port: &OutputPort) -> &str {
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

fn node_lane_key(graph: &CanvasProjection, node: &CanvasNode) -> Vec<(u8, usize, String)> {
    node.groups
        .iter()
        .map(|group_id| {
            let group = graph.groups.iter().find(|group| group.id == *group_id);
            let ordinal = group.and_then(|group| group.lane_priority).unwrap_or(0);
            let kind = match group_id.kind {
                crate::macro_ui::canvas_model::CanvasGroupKind::IfThen => 0,
                crate::macro_ui::canvas_model::CanvasGroupKind::IfElse => 2,
                crate::macro_ui::canvas_model::CanvasGroupKind::WatchLaneThen => 1,
                crate::macro_ui::canvas_model::CanvasGroupKind::LoopBody => 1,
                crate::macro_ui::canvas_model::CanvasGroupKind::TimeoutBody => 3,
            };
            (kind, ordinal, group_id.owner_id.clone())
        })
        .collect()
}

fn sane_size(size: [f32; 2]) -> [f32; 2] {
    [
        if size[0].is_finite() {
            size[0].max(1.0)
        } else {
            1.0
        },
        if size[1].is_finite() {
            size[1].max(1.0)
        } else {
            1.0
        },
    ]
}

fn valid_persisted_zoom(zoom: f32) -> f32 {
    if zoom.is_finite() && zoom > 0.0 {
        zoom.clamp(MIN_FIT_ZOOM, MAX_ZOOM)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macro_ui::canvas_model::project_canvas;
    use crate::macro_ui::test_support::{
        fixture_definition, fixture_draft, fixture_large_definition,
    };
    use crate::ui_state::MacroCanvasLayout;

    #[test]
    fn moving_node_changes_layout_only() {
        let draft = fixture_draft();
        let executable_before = serde_json::to_vec(&draft.definition).unwrap();
        let mut layout = MacroCanvasLayout::default();
        CanvasLayoutEngine::move_node(&mut layout, "observe", [320.0, 180.0]).unwrap();
        assert_eq!(
            serde_json::to_vec(&draft.definition).unwrap(),
            executable_before
        );
        assert_eq!(layout.node_positions["observe"], [320.0, 180.0]);
    }

    #[test]
    fn fit_view_contains_every_projected_node() {
        let graph = project_canvas(&fixture_large_definition());
        let layout = auto_arrange(&graph);
        let viewport = fit_view([900.0, 700.0], graph_bounds(&graph, &layout));
        assert!(graph.nodes.iter().all(|node| {
            viewport
                .visible_world_rect()
                .contains_rect(node_rect(node, &layout))
        }));
    }

    #[test]
    fn fit_view_transform_places_large_graph_inside_canvas() {
        let graph = project_canvas(&fixture_large_definition());
        let layout = auto_arrange(&graph);
        let canvas = Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 700.0));
        let viewport = fit_view(
            [canvas.width(), canvas.height()],
            graph_bounds(&graph, &layout),
        );
        assert!(graph.nodes.iter().all(|node| {
            let world = node_rect(node, &layout);
            canvas.contains_rect(Rect::from_two_pos(
                viewport.screen_from_world(canvas, world.min),
                viewport.screen_from_world(canvas, world.max),
            ))
        }));
    }

    #[test]
    fn reconcile_keeps_a_valid_positive_subminimum_fit_zoom() {
        let graph = project_canvas(&fixture_large_definition());
        let mut saved = auto_arrange(&graph);
        saved.zoom = 0.01;

        assert_eq!(reconcile_layout(&graph, saved).zoom, 0.01);
    }

    #[test]
    fn manual_zoom_from_fitted_subminimum_scale_is_continuous() {
        let graph = project_canvas(&fixture_large_definition());
        let layout = auto_arrange(&graph);
        let canvas = Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 700.0));
        let mut viewport = fit_view(
            [canvas.width(), canvas.height()],
            graph_bounds(&graph, &layout),
        );
        let before = viewport.zoom;
        assert!(before < MIN_ZOOM);

        viewport.zoom_around(canvas, canvas.center(), 1.1);

        assert!((viewport.zoom - before * 1.1).abs() < f32::EPSILON);
        assert!(viewport.zoom < MIN_ZOOM);
    }

    #[test]
    fn repeated_zoom_out_from_a_fitted_scale_stops_at_the_safe_floor() {
        let graph = project_canvas(&fixture_large_definition());
        let layout = auto_arrange(&graph);
        let canvas = Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 700.0));
        let mut viewport = fit_view(
            [canvas.width(), canvas.height()],
            graph_bounds(&graph, &layout),
        );

        for _ in 0..64 {
            viewport.zoom_around(canvas, canvas.center(), 0.5);
        }

        assert_eq!(viewport.zoom, MIN_FIT_ZOOM);
        viewport.zoom_around(canvas, canvas.center(), 1.1);
        assert!((viewport.zoom - MIN_FIT_ZOOM * 1.1).abs() < f32::EPSILON);
    }

    #[test]
    fn corrupt_positions_are_rebuilt_not_applied() {
        let mut saved = MacroCanvasLayout::default();
        saved
            .node_positions
            .insert("observe".into(), [f32::NAN, f32::INFINITY]);
        let repaired = reconcile_layout(&project_canvas(&fixture_definition()), saved);
        assert!(
            repaired
                .node_positions
                .values()
                .flatten()
                .all(|value| value.is_finite())
        );
    }
}
