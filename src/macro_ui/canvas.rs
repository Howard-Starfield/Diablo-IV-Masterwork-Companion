use eframe::egui::{self, Color32, Id, PointerButton, Pos2, Rect, Sense, Stroke, Vec2};

use crate::macro_ui::canvas_layout::{CanvasViewport, LayoutEdit, node_rect, visible_nodes};
use crate::macro_ui::canvas_model::{
    CanvasConnectionError, CanvasEdgeKind, CanvasProjection, CanvasSelection, OutputPort,
    connection_command,
};
use crate::macro_ui::{BlockFamily, EditorCommand, EditorDraft};
use crate::ui_state::MacroCanvasLayout;
use crate::ui_theme::{category_style, colors, text};

pub const CANVAS_HEIGHT: f32 = 430.0;
const HANDLE_RADIUS: f32 = 6.0;
const GRID_STEP: f32 = 32.0;

#[derive(Debug, Clone, PartialEq)]
pub enum CanvasHit {
    Background,
    Node(String),
    Input(String),
    Output(OutputPort),
}

impl Default for CanvasHit {
    fn default() -> Self {
        Self::Background
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CanvasAction {
    Pan(Vec2),
    Zoom {
        pointer: Pos2,
        factor: f32,
    },
    MoveNode {
        id: String,
        delta: Vec2,
    },
    Select(CanvasSelection),
    FinishConnection {
        source: OutputPort,
        target_block_id: String,
    },
    OpenAddStep {
        source: OutputPort,
        world_position: [f32; 2],
        allowed: Vec<BlockFamily>,
    },
    RejectedConnection(String),
    CancelGesture,
}

#[derive(Debug, Clone)]
pub struct CanvasInputFrame {
    pub hovered: bool,
    pub hit: CanvasHit,
    pub pointer: Option<Pos2>,
    pub primary_drag_delta: Vec2,
    pub middle_drag_delta: Vec2,
    pub wheel_y: f32,
    pub pinch_zoom: f32,
    pub space_down: bool,
    pub command_down: bool,
}

impl Default for CanvasInputFrame {
    fn default() -> Self {
        Self {
            hovered: false,
            hit: CanvasHit::Background,
            pointer: None,
            primary_drag_delta: Vec2::ZERO,
            middle_drag_delta: Vec2::ZERO,
            wheel_y: 0.0,
            pinch_zoom: 1.0,
            space_down: false,
            command_down: false,
        }
    }
}

#[derive(Debug, Default)]
pub struct CanvasResponse {
    pub action: Option<CanvasAction>,
    pub selection: Option<CanvasSelection>,
    pub editor_command: Option<EditorCommand>,
    pub layout_changed: bool,
    pub layout_edit: Option<LayoutEdit>,
}

#[derive(Debug, Clone)]
struct NodeDrag {
    id: String,
    before: [f32; 2],
}

fn node_drag_id() -> Id {
    Id::new("macro-canvas-node-drag")
}

fn connector_drag_id() -> Id {
    Id::new("macro-canvas-connector-drag")
}

pub fn reduce_canvas_input(frame: CanvasInputFrame) -> CanvasAction {
    if !frame.hovered {
        return CanvasAction::CancelGesture;
    }
    if frame.pinch_zoom.is_finite() && (frame.pinch_zoom - 1.0).abs() > f32::EPSILON {
        return CanvasAction::Zoom {
            pointer: frame.pointer.unwrap_or(Pos2::ZERO),
            factor: frame.pinch_zoom,
        };
    }
    if frame.wheel_y.abs() > f32::EPSILON {
        return CanvasAction::Zoom {
            pointer: frame.pointer.unwrap_or(Pos2::ZERO),
            factor: (frame.wheel_y * 0.01).exp(),
        };
    }
    if frame.middle_drag_delta != Vec2::ZERO
        || (frame.space_down && frame.primary_drag_delta != Vec2::ZERO)
    {
        return CanvasAction::Pan(frame.middle_drag_delta + frame.primary_drag_delta);
    }
    match (
        &frame.hit,
        frame.primary_drag_delta != Vec2::ZERO,
        frame.command_down,
    ) {
        (_, _, true) => CanvasAction::CancelGesture,
        (CanvasHit::Node(id), true, _) => CanvasAction::MoveNode {
            id: id.clone(),
            delta: frame.primary_drag_delta,
        },
        (CanvasHit::Background, true, _) => CanvasAction::Pan(frame.primary_drag_delta),
        _ => CanvasAction::CancelGesture,
    }
}

pub fn finish_connection(
    draft: &EditorDraft,
    source: OutputPort,
    target: CanvasHit,
) -> CanvasResponse {
    let target_id = match target {
        CanvasHit::Node(id) | CanvasHit::Input(id) => id,
        CanvasHit::Background | CanvasHit::Output(_) => {
            return CanvasResponse {
                action: Some(CanvasAction::OpenAddStep {
                    source,
                    world_position: [0.0, 0.0],
                    allowed: allowed_families(),
                }),
                ..Default::default()
            };
        }
    };
    match connection_command(draft, source.clone(), &target_id) {
        Ok(editor_command) => CanvasResponse {
            action: Some(CanvasAction::FinishConnection {
                source,
                target_block_id: target_id,
            }),
            editor_command: Some(editor_command),
            ..Default::default()
        },
        Err(error) => rejected_response(error),
    }
}

/// Paints the independent, native canvas projection. The supplied layout is rebuildable UI state;
/// this function never receives or mutates an executable definition.
pub fn show(
    ui: &mut egui::Ui,
    graph: &CanvasProjection,
    layout: &mut MacroCanvasLayout,
    selected: Option<&CanvasSelection>,
    active_block: Option<&str>,
    draft: Option<&EditorDraft>,
    editable: bool,
) -> CanvasResponse {
    let desired_size = Vec2::new(ui.available_width().max(1.0), CANVAS_HEIGHT);
    let (canvas_rect, response) = ui.allocate_exact_size(desired_size, Sense::click_and_drag());
    let painter = ui.painter().with_clip_rect(canvas_rect);
    let mut viewport =
        CanvasViewport::from_layout(layout, [canvas_rect.width(), canvas_rect.height()]);
    let pointer = response.hover_pos();
    let hit = pointer
        .map(|point| hit_test(graph, layout, &viewport, canvas_rect, point))
        .unwrap_or_default();
    let mut result = CanvasResponse::default();

    let (wheel_y, pinch_zoom, space_down, command_down) = ui.ctx().input_mut(|input| {
        let wheel = if response.hovered() {
            let wheel = input.raw_scroll_delta.y;
            input.raw_scroll_delta = Vec2::ZERO;
            wheel
        } else {
            0.0
        };
        (
            wheel,
            input.zoom_delta(),
            input.key_down(egui::Key::Space),
            input.modifiers.command,
        )
    });

    if response.hovered() && ui.input(|input| input.key_pressed(egui::Key::Escape)) {
        ui.ctx().data_mut(|data| {
            data.remove::<OutputPort>(connector_drag_id());
            data.remove::<NodeDrag>(node_drag_id());
        });
        result.action = Some(CanvasAction::CancelGesture);
    }

    if response.drag_started_by(PointerButton::Primary) && !space_down && !command_down {
        match &hit {
            CanvasHit::Node(id) => {
                let before = layout.node_positions.get(id).copied().unwrap_or([0.0, 0.0]);
                ui.ctx().data_mut(|data| {
                    data.insert_temp(
                        node_drag_id(),
                        NodeDrag {
                            id: id.clone(),
                            before,
                        },
                    )
                });
            }
            CanvasHit::Output(port) if editable => {
                ui.ctx()
                    .data_mut(|data| data.insert_temp(connector_drag_id(), port.clone()));
            }
            _ => {}
        }
    }

    let reducer_action = reduce_canvas_input(CanvasInputFrame {
        hovered: response.hovered(),
        hit: hit.clone(),
        pointer,
        primary_drag_delta: if response.dragged_by(PointerButton::Primary) {
            response.drag_delta()
        } else {
            Vec2::ZERO
        },
        middle_drag_delta: if response.dragged_by(PointerButton::Middle) {
            response.drag_delta()
        } else {
            Vec2::ZERO
        },
        wheel_y,
        pinch_zoom,
        space_down,
        command_down,
    });

    match reducer_action {
        CanvasAction::Pan(delta) if delta != Vec2::ZERO => {
            viewport.pan += delta;
            viewport.write_to_layout(layout);
            result.layout_changed = true;
        }
        CanvasAction::Zoom { pointer, factor } => {
            viewport.zoom_around(canvas_rect, pointer, factor);
            viewport.write_to_layout(layout);
            result.layout_changed = true;
        }
        _ => {}
    }

    let active_drag = ui
        .ctx()
        .data(|data| data.get_temp::<NodeDrag>(node_drag_id()));
    if let Some(drag) = active_drag {
        if (response.dragged_by(PointerButton::Primary)
            || response.drag_stopped_by(PointerButton::Primary))
            && !space_down
        {
            let delta = response.drag_delta() / viewport.zoom.max(f32::MIN_POSITIVE);
            let position = [drag.before[0] + delta.x, drag.before[1] + delta.y];
            if position.iter().all(|value| value.is_finite()) {
                layout.node_positions.insert(drag.id.clone(), position);
                result.layout_changed = true;
            }
        }
        if response.drag_stopped_by(PointerButton::Primary) {
            let after = layout.node_positions.get(&drag.id).copied();
            if after != Some(drag.before) {
                result.action = Some(CanvasAction::MoveNode {
                    id: drag.id.clone(),
                    delta: Vec2::new(
                        after.unwrap_or(drag.before)[0] - drag.before[0],
                        after.unwrap_or(drag.before)[1] - drag.before[1],
                    ),
                });
                result.layout_edit = Some(LayoutEdit::NodePosition {
                    id: drag.id,
                    before: Some(drag.before),
                    after,
                });
            }
            ui.ctx()
                .data_mut(|data| data.remove::<NodeDrag>(node_drag_id()));
        }
    }

    let connector = ui
        .ctx()
        .data(|data| data.get_temp::<OutputPort>(connector_drag_id()));
    if let Some(ref source) = connector {
        if response.drag_stopped_by(PointerButton::Primary) {
            ui.ctx()
                .data_mut(|data| data.remove::<OutputPort>(connector_drag_id()));
            let mut connection = draft.map_or_else(
                || rejected_response(CanvasConnectionError::MissingSource("draft".into())),
                |draft| finish_connection(draft, source.clone(), hit.clone()),
            );
            if matches!(hit, CanvasHit::Background) {
                connection.action = Some(CanvasAction::OpenAddStep {
                    source: source.clone(),
                    world_position: pointer
                        .map(|point| viewport.world_from_screen(canvas_rect, point))
                        .map(|point| [point.x, point.y])
                        .unwrap_or([0.0, 0.0]),
                    allowed: allowed_families(),
                });
                connection.editor_command = None;
            }
            result = connection;
        }
    }

    if response.clicked() {
        if let Some(selection) = selection_for_hit(graph, &hit) {
            result.selection = Some(selection.clone());
            result.action = Some(CanvasAction::Select(selection));
        }
    }

    paint_canvas(
        &painter,
        graph,
        layout,
        &viewport,
        canvas_rect,
        selected,
        active_block,
        connector.as_ref(),
        pointer,
    );
    if active_block.is_some() {
        ui.ctx().request_repaint();
    }
    result
}

fn rejected_response(error: CanvasConnectionError) -> CanvasResponse {
    CanvasResponse {
        action: Some(CanvasAction::RejectedConnection(error.message().into())),
        ..Default::default()
    }
}

fn allowed_families() -> Vec<BlockFamily> {
    vec![
        BlockFamily::TextObservation,
        BlockFamily::ImageObservation,
        BlockFamily::TextMatchedClick,
        BlockFamily::ImageMatchedClick,
        BlockFamily::SavedLocationClick,
        BlockFamily::Loop,
        BlockFamily::Other,
    ]
}

fn selection_for_hit(graph: &CanvasProjection, hit: &CanvasHit) -> Option<CanvasSelection> {
    match hit {
        CanvasHit::Node(id) | CanvasHit::Input(id) => {
            graph.node(id).map(|node| node.selection.clone())
        }
        CanvasHit::Background | CanvasHit::Output(_) => None,
    }
}

fn hit_test(
    graph: &CanvasProjection,
    layout: &MacroCanvasLayout,
    viewport: &CanvasViewport,
    canvas: Rect,
    point: Pos2,
) -> CanvasHit {
    for node in graph.nodes.iter().rev() {
        let rect = screen_rect(viewport, canvas, node_rect(node, layout));
        if !rect.contains(point) {
            continue;
        }
        if point.distance(input_handle(rect)) <= HANDLE_RADIUS * 1.8 {
            return CanvasHit::Input(node.id.clone());
        }
        for (index, port) in node.outputs.iter().enumerate() {
            if point.distance(output_handle(rect, index)) <= HANDLE_RADIUS * 1.8 {
                return CanvasHit::Output(port.clone());
            }
        }
        return CanvasHit::Node(node.id.clone());
    }
    CanvasHit::Background
}

#[allow(clippy::too_many_arguments)]
fn paint_canvas(
    painter: &egui::Painter,
    graph: &CanvasProjection,
    layout: &MacroCanvasLayout,
    viewport: &CanvasViewport,
    canvas: Rect,
    selected: Option<&CanvasSelection>,
    active_block: Option<&str>,
    connector: Option<&OutputPort>,
    pointer: Option<Pos2>,
) {
    painter.rect_filled(canvas, 4.0, colors::CANVAS);
    paint_grid(painter, viewport, canvas);
    paint_groups(painter, graph, layout, viewport, canvas);
    paint_edges(painter, graph, layout, viewport, canvas, active_block);
    for node in visible_nodes(graph, layout, viewport, canvas) {
        paint_node(
            painter,
            node,
            layout,
            viewport,
            canvas,
            selected == Some(&node.selection),
            active_block.is_some_and(|id| id == node.id),
        );
    }
    if let (Some(source), Some(pointer)) = (connector, pointer) {
        if let Some(node) = graph.node(output_owner(source)) {
            let rect = screen_rect(viewport, canvas, node_rect(node, layout));
            let origin = output_handle(
                rect,
                node.outputs
                    .iter()
                    .position(|port| port == source)
                    .unwrap_or(0),
            );
            paint_curve(
                painter,
                origin,
                pointer,
                Stroke::new(2.0, colors::DIABLO_ORANGE),
            );
        }
    }
}

fn paint_grid(painter: &egui::Painter, viewport: &CanvasViewport, canvas: Rect) {
    let zoom = viewport.zoom.max(f32::MIN_POSITIVE);
    let step = (GRID_STEP * zoom).max(12.0);
    let mut x = (canvas.left() + viewport.pan.x).rem_euclid(step) + canvas.left();
    while x < canvas.right() {
        painter.line_segment(
            [Pos2::new(x, canvas.top()), Pos2::new(x, canvas.bottom())],
            Stroke::new(1.0, Color32::from_white_alpha(10)),
        );
        x += step;
    }
    let mut y = (canvas.top() + viewport.pan.y).rem_euclid(step) + canvas.top();
    while y < canvas.bottom() {
        painter.line_segment(
            [Pos2::new(canvas.left(), y), Pos2::new(canvas.right(), y)],
            Stroke::new(1.0, Color32::from_white_alpha(10)),
        );
        y += step;
    }
}

fn paint_groups(
    painter: &egui::Painter,
    graph: &CanvasProjection,
    layout: &MacroCanvasLayout,
    viewport: &CanvasViewport,
    canvas: Rect,
) {
    for group in &graph.groups {
        let Some(bounds) = group
            .member_ids
            .iter()
            .filter_map(|id| graph.node(id))
            .map(|node| node_rect(node, layout))
            .reduce(|left, right| left.union(right))
        else {
            continue;
        };
        let rect = screen_rect(viewport, canvas, bounds.expand(18.0));
        painter.rect_filled(rect, 8.0, Color32::from_white_alpha(5));
        painter.rect_stroke(rect, 8.0, Stroke::new(1.0, Color32::from_white_alpha(44)));
        painter.text(
            rect.left_top() + Vec2::new(8.0, 6.0),
            egui::Align2::LEFT_TOP,
            group.label,
            egui::FontId::proportional(text::META),
            colors::SUPPORTING_TEXT,
        );
    }
}

fn paint_edges(
    painter: &egui::Painter,
    graph: &CanvasProjection,
    layout: &MacroCanvasLayout,
    viewport: &CanvasViewport,
    canvas: Rect,
    active_block: Option<&str>,
) {
    for edge in &graph.edges {
        let Some(source) = graph.node(output_owner(&edge.from)) else {
            continue;
        };
        let Some(target) = graph.node(&edge.to) else {
            continue;
        };
        let source_rect = screen_rect(viewport, canvas, node_rect(source, layout));
        let target_rect = screen_rect(viewport, canvas, node_rect(target, layout));
        let start = output_handle(
            source_rect,
            source
                .outputs
                .iter()
                .position(|port| port == &edge.from)
                .unwrap_or(0),
        );
        let end = input_handle(target_rect);
        let active = active_block.is_some_and(|id| id == source.id || id == target.id);
        let stroke = match edge.kind {
            CanvasEdgeKind::LoopReturn => Stroke::new(1.5, colors::REPEAT_TEAL.gamma_multiply(0.8)),
            CanvasEdgeKind::Branch => {
                Stroke::new(if active { 3.0 } else { 1.7 }, colors::DECIDE_PURPLE)
            }
            CanvasEdgeKind::WatchLane => {
                Stroke::new(if active { 3.0 } else { 1.7 }, colors::OBSERVE_BLUE)
            }
            CanvasEdgeKind::Timeout => {
                Stroke::new(if active { 3.0 } else { 1.7 }, colors::ACT_ORANGE)
            }
            CanvasEdgeKind::Sequence => Stroke::new(if active { 3.0 } else { 1.5 }, colors::BORDER),
        };
        paint_curve(painter, start, end, stroke);
    }
}

fn paint_curve(painter: &egui::Painter, start: Pos2, end: Pos2, stroke: Stroke) {
    let bend = ((end.x - start.x).abs() * 0.45).max(36.0);
    painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
        [start, start + Vec2::X * bend, end - Vec2::X * bend, end],
        false,
        Color32::TRANSPARENT,
        stroke,
    ));
}

fn paint_node(
    painter: &egui::Painter,
    node: &crate::macro_ui::canvas_model::CanvasNode,
    layout: &MacroCanvasLayout,
    viewport: &CanvasViewport,
    canvas: Rect,
    selected: bool,
    active: bool,
) {
    let rect = screen_rect(viewport, canvas, node_rect(node, layout));
    let category = category_style(node.category);
    painter.rect_filled(rect, 8.0, colors::ELEVATED_SURFACE);
    painter.rect_stroke(
        rect,
        8.0,
        Stroke::new(
            if selected || active { 2.5 } else { 1.0 },
            if selected {
                colors::DIABLO_ORANGE
            } else if active {
                category.accent
            } else {
                colors::BORDER
            },
        ),
    );
    let chip = Rect::from_min_size(rect.min + Vec2::new(10.0, 10.0), Vec2::new(86.0, 20.0));
    painter.rect_filled(chip, 4.0, category.accent.gamma_multiply(0.27));
    painter.text(
        chip.center(),
        egui::Align2::CENTER_CENTER,
        format!("{} {}", category.icon, category.label),
        egui::FontId::proportional(text::META),
        category.accent,
    );
    painter.text(
        rect.left_top() + Vec2::new(10.0, 38.0),
        egui::Align2::LEFT_TOP,
        &node.title,
        egui::FontId::proportional(text::BODY),
        colors::PRIMARY_TEXT,
    );
    painter.text(
        rect.left_bottom() + Vec2::new(10.0, -10.0),
        egui::Align2::LEFT_BOTTOM,
        truncate(&node.summary, 34),
        egui::FontId::proportional(text::SUPPORTING),
        colors::SUPPORTING_TEXT,
    );
    painter.circle_filled(input_handle(rect), HANDLE_RADIUS, colors::BORDER);
    for index in 0..node.outputs.len() {
        painter.circle_filled(output_handle(rect, index), HANDLE_RADIUS, category.accent);
    }
}

fn screen_rect(viewport: &CanvasViewport, canvas: Rect, world: Rect) -> Rect {
    Rect::from_min_max(
        viewport.screen_from_world(canvas, world.min),
        viewport.screen_from_world(canvas, world.max),
    )
}

fn input_handle(rect: Rect) -> Pos2 {
    Pos2::new(rect.left(), rect.center().y)
}

fn output_handle(rect: Rect, index: usize) -> Pos2 {
    let offset = (index as f32 - 0.5) * 18.0;
    Pos2::new(
        rect.right(),
        (rect.center().y + offset).clamp(rect.top() + 16.0, rect.bottom() - 16.0),
    )
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

fn truncate(value: &str, maximum: usize) -> String {
    let mut chars = value.chars();
    let visible = chars.by_ref().take(maximum).collect::<String>();
    if chars.next().is_some() {
        format!("{visible}…")
    } else {
        visible
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macro_ui::canvas_model::OutputPort;
    use crate::macro_ui::test_support::fixture_draft;

    #[test]
    fn primary_drag_on_empty_space_pans() {
        let action = reduce_canvas_input(CanvasInputFrame {
            hovered: true,
            hit: CanvasHit::Background,
            primary_drag_delta: egui::vec2(24.0, -8.0),
            ..Default::default()
        });
        assert_eq!(action, CanvasAction::Pan(egui::vec2(24.0, -8.0)));
    }

    #[test]
    fn node_drag_moves_node_and_does_not_pan() {
        let action = reduce_canvas_input(CanvasInputFrame {
            hovered: true,
            hit: CanvasHit::Node("observe".into()),
            primary_drag_delta: egui::vec2(24.0, -8.0),
            ..Default::default()
        });
        assert_eq!(
            action,
            CanvasAction::MoveNode {
                id: "observe".into(),
                delta: egui::vec2(24.0, -8.0),
            }
        );
    }

    #[test]
    fn invalid_connector_drop_returns_reason_without_command() {
        let response = finish_connection(
            &fixture_draft(),
            OutputPort::LoopBody("loop".into()),
            CanvasHit::Node("loop".into()),
        );
        assert!(matches!(
            response.action,
            Some(CanvasAction::RejectedConnection(_))
        ));
        assert!(response.editor_command.is_none());
    }
}
