# Frontend Readability and Macro Canvas Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a readable 900x1080 Windows UI with persisted Always on top behavior, an independently implemented n8n-style Macro canvas backed only by the canonical block tree, and a simpler Enchant presentation.

**Architecture:** Keep `MacroDefinition.blocks` as the sole executable source of truth. Project it into a pure canvas model, store only rebuildable positions/pan/zoom in a separate bounded UI-state file, and translate connection gestures back into existing checked `EditorCommand` mutations. Use only the pinned `eframe/egui 0.27.2` APIs; add no graph or web frontend dependency.

**Tech Stack:** Rust 2024, eframe/egui 0.27.2, serde/serde_json, tempfile atomic replacement, Windows monitor/window APIs already enabled by the `windows` crate.

## Global Constraints

- Execution prerequisite: resume `docs/superpowers/plans/2026-07-12-macro-v1-implementation.md` at Task 13 and accept its outstanding package and live-runtime reviews before starting Task 1 here.
- The approved UI contract is `docs/superpowers/specs/2026-07-13-frontend-readability-macro-canvas-design.md`.
- Existing Macro runtime, validation, persistence, cancellation, and package semantics do not change in this plan.
- Existing Enchant capture, matcher, configuration, and action behavior do not change in this plan.
- Preferred initial inner size is 900x1080 logical pixels, clamped to the cursor monitor work area; the window remains resizable.
- Always on top defaults Off, applies immediately, and persists independently from Enchant and Macro definitions.
- Page title is 22 px, section title 16 px semibold, body/control text 16 px, supporting text 14 px, and optional metadata no smaller than 12 px.
- The editor may use Observe blue, Decide purple, Act orange, and Repeat teal, but color is never the only category or status signal.
- Do not copy n8n CSS, Vue code, icons, trademarks, source files, or visual assets.
- Do not add arbitrary edges, Goto, Tag, Jump, cross-container cycles, or disconnected executable blocks.
- Node movement and viewport changes never alter the executable definition, revision, validation state, saved hash, or active run snapshot.
- ESC remains a direct emergency path outside the egui command queue.
- Use TDD, focused touched-file formatting, `git diff --check`, and one reviewable commit per task.

## File and ownership map

- `src/ui_theme.rs` — shared type scale, colors, control spacing, and category presentation.
- `src/ui_state.rs` — bounded rebuildable UI state, canvas layout records, atomic load/save, and sanitization.
- `src/engine/platform/windows_impl.rs` — cursor-monitor work-area query and pure placement clamping.
- `src/macro_ui/canvas_model.rs` — canonical tree projection, groups, derived edges, ports, and checked connection-to-command translation.
- `src/macro_ui/canvas_layout.rs` — deterministic auto-layout, viewport transforms, fit/reset, visible-node culling, and layout history.
- `src/macro_ui/canvas.rs` — egui painting, hit testing, pan/zoom/node/connector gestures, selection, and active-run highlighting.
- `src/macro_ui/editor.rs` — canonical definition Undo/Redo and existing structural mutations.
- `src/macro_ui/mod.rs` — Macro page orchestration, responsive panes, unified edit history, and UI intent queue.
- `src/macro_ui/library.rs` — readable searchable library and secondary management actions.
- `src/macro_ui/inspector.rs` — readable contextual inspector, preview/test actions, and collapsed Advanced settings.
- `src/macro_ui/monitor.rs` — semantic status monitor with 12 px minimum metadata.
- `src/macro_ui/test_support.rs` — shared canonical fixtures for canvas/editor tests; compiled only under `cfg(test)`.
- `src/main.rs` — native shell, preference ownership, viewport commands, bottom composition, and Enchant presentation-only cleanup.
- `src/macro_ui/acceptance_tests.rs` — cross-module UI/canonical-boundary regression tests.
- `README.md` — user-facing navigation, canvas gestures, Always on top, and safety wording.

---

### Task 1: Shared Readable Theme

**Files:**
- Create: `src/ui_theme.rs`
- Modify: `src/main.rs:15-23,448,1090-1121,1555-1650`
- Modify: `src/macro_ui/mod.rs`
- Modify: `src/macro_ui/library.rs`
- Modify: `src/macro_ui/inspector.rs`
- Modify: `src/macro_ui/monitor.rs`
- Modify: `src/macro_ui/wizard.rs`

**Interfaces:**
- Produces: `ui_theme::apply(&Context)`, `ui_theme::text`, `ui_theme::colors`, and `ui_theme::category_style(BlockCategory)`.
- Consumes: `egui::Style`, `TextStyle`, `FontId`, `FontFamily`, `Spacing`, `Visuals`.

- [ ] **Step 1: Write failing theme token tests**

Create `src/ui_theme.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_type_scale_never_drops_below_twelve_points() {
        let style = build_style();
        assert_eq!(style.text_styles[&egui::TextStyle::Heading].size, 22.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Body].size, 16.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Button].size, 16.0);
        assert_eq!(style.text_styles[&egui::TextStyle::Small].size, 12.0);
        assert!(style.text_styles.values().all(|font| font.size >= 12.0));
    }

    #[test]
    fn category_style_has_text_and_icon_not_just_color() {
        for category in BlockCategory::ALL {
            let style = category_style(category);
            assert!(!style.label.is_empty());
            assert!(!style.icon.is_empty());
        }
    }
}
```

- [ ] **Step 2: Run the tests and verify the missing implementation**

Run: `cargo test ui_theme::tests -- --nocapture`

Expected: compile failure because `build_style`, `BlockCategory`, and `category_style` are not defined.

- [ ] **Step 3: Implement the shared style**

Use these exact public contracts:

```rust
use eframe::egui::{self, Color32, Context, FontFamily, FontId, TextStyle};

pub mod text {
    pub const PAGE_TITLE: f32 = 22.0;
    pub const SECTION_TITLE: f32 = 16.0;
    pub const BODY: f32 = 16.0;
    pub const SUPPORTING: f32 = 14.0;
    pub const META: f32 = 12.0;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockCategory { Observe, Decide, Act, Repeat }

impl BlockCategory {
    pub const ALL: [Self; 4] = [Self::Observe, Self::Decide, Self::Act, Self::Repeat];
}

pub struct CategoryStyle {
    pub label: &'static str,
    pub icon: &'static str,
    pub accent: Color32,
}

pub fn build_style() -> egui::Style {
    let mut style = egui::Style::default();
    style.text_styles.insert(TextStyle::Heading, FontId::new(text::PAGE_TITLE, FontFamily::Proportional));
    style.text_styles.insert(TextStyle::Body, FontId::new(text::BODY, FontFamily::Proportional));
    style.text_styles.insert(TextStyle::Button, FontId::new(text::BODY, FontFamily::Proportional));
    style.text_styles.insert(TextStyle::Monospace, FontId::new(text::META, FontFamily::Monospace));
    style.text_styles.insert(TextStyle::Small, FontId::new(text::META, FontFamily::Proportional));
    style.spacing.button_padding = egui::vec2(12.0, 9.0);
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.visuals = app_visuals();
    style
}

pub fn apply(ctx: &Context) { ctx.set_style(build_style()); }
```

`category_style` returns labels and icons `Observe/◉`, `Decide/◇`, `Act/▶`, and `Repeat/↻` with the approved restrained colors. Replace `configure_style` in `main.rs` with `ui_theme::apply`.

- [ ] **Step 4: Replace sub-12 px and scattered body sizes**

Replace literal `.size(9.0)`, `.size(10.0)`, and `.size(11.0)` throughout the listed UI files with `ui_theme::text::META`; replace supporting `.size(12.0)` uses with `SUPPORTING` unless the content is genuinely optional metadata. Use `TextStyle::Body` or `ui_theme::text::BODY` for normal labels, inputs, and buttons. Do not change detector thresholds, timing values, geometry values, or runtime constants.

- [ ] **Step 5: Run focused verification**

Run:

```powershell
cargo test ui_theme::tests
rg -n "\.size\((9|10|11)\.0\)" src/main.rs src/macro_ui
rustfmt --edition 2024 --check --config skip_children=true src/ui_theme.rs src/main.rs src/macro_ui/mod.rs src/macro_ui/library.rs src/macro_ui/inspector.rs src/macro_ui/monitor.rs src/macro_ui/wizard.rs
git diff --check
```

Expected: tests pass, the `rg` command returns no matches, formatting passes for touched files, and diff check is clean.

- [ ] **Step 6: Commit**

```powershell
git add src/ui_theme.rs src/main.rs src/macro_ui
git commit -m "style: add readable shared ui theme"
```

---

### Task 2: Window Placement and Persisted UI State

**Files:**
- Create: `src/ui_state.rs`
- Modify: `src/main.rs:148-188,426-490,1085-1121`
- Modify: `src/engine/platform/windows_impl.rs`
- Modify: `src/engine/platform/mod.rs`

**Interfaces:**
- Produces: `AppUiState`, `MacroCanvasLayout`, `UiStateStore`, `WindowPlacement`, `preferred_window_placement()`.
- Consumes: `ViewportBuilder::with_inner_size`, `ViewportBuilder::with_position`, `ViewportBuilder::with_window_level`, `ViewportCommand::WindowLevel`, `WindowLevel::{Normal, AlwaysOnTop}`.

- [ ] **Step 1: Write failing UI-state and placement tests**

```rust
#[test]
fn ui_state_defaults_always_on_top_off() {
    assert!(!AppUiState::default().always_on_top);
}

#[test]
fn corrupt_ui_state_recovers_without_macro_data() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("ui-state.json"), b"not json").unwrap();
    let (store, warning) = UiStateStore::open(temp.path().join("ui-state.json"));
    assert_eq!(store.state, AppUiState::default());
    assert!(warning.is_some());
}

#[test]
fn preferred_size_is_clamped_inside_work_area() {
    let placement = clamp_window_placement([900.0, 1080.0], [0.0, 0.0, 1920.0, 1040.0], 1.0);
    assert_eq!(placement.inner_size, [900.0, 992.0]); // reserve 48 px for frame/title bar
    assert!(placement.outer_position[1] >= 0.0);
}
```

- [ ] **Step 2: Run failing tests**

Run: `cargo test ui_state::tests engine::platform::windows_impl::tests::preferred_size_is_clamped_inside_work_area`

Expected: compile failure because the state and placement types do not exist.

- [ ] **Step 3: Implement bounded rebuildable UI state**

Use one non-executable state file under `%LOCALAPPDATA%/BoBo Companion/ui-state.json`:

```rust
pub const UI_STATE_SCHEMA_VERSION: u32 = 1;
pub const MAX_UI_STATE_BYTES: u64 = 1_048_576;
pub const MAX_LAYOUT_MACROS: usize = 128;
pub const MAX_LAYOUT_NODES: usize = 2_048;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AppUiState {
    pub schema_version: u32,
    #[serde(default)]
    pub always_on_top: bool,
    #[serde(default)]
    pub macro_layouts: std::collections::BTreeMap<String, MacroCanvasLayout>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MacroCanvasLayout {
    pub node_positions: std::collections::BTreeMap<String, [f32; 2]>,
    pub pan: [f32; 2],
    pub zoom: f32,
    pub library_width: f32,
    pub inspector_width: f32,
}

impl Default for AppUiState {
    fn default() -> Self {
        Self {
            schema_version: UI_STATE_SCHEMA_VERSION,
            always_on_top: false,
            macro_layouts: Default::default(),
        }
    }
}

impl Default for MacroCanvasLayout {
    fn default() -> Self {
        Self {
            node_positions: Default::default(),
            pan: [0.0, 0.0],
            zoom: 1.0,
            library_width: 220.0,
            inspector_width: 320.0,
        }
    }
}

impl MacroCanvasLayout {
    pub fn is_finite(&self) -> bool {
        self.pan.iter().all(|value| value.is_finite())
            && self.zoom.is_finite()
            && self.node_positions.values().flatten().all(|value| value.is_finite())
    }
}

pub struct UiStateStore {
    path: std::path::PathBuf,
    pub state: AppUiState,
    dirty: bool,
}

impl UiStateStore {
    pub fn open(path: std::path::PathBuf) -> (Self, Option<String>);
    pub fn mark_dirty(&mut self);
    pub fn save_if_dirty(&mut self) -> anyhow::Result<()>;
}
```

`UiStateStore::open` reads at most `MAX_UI_STATE_BYTES + 1`, defaults on missing/corrupt/unsupported data, drops non-finite coordinates, clamps zoom to `0.5..=1.75`, limits maps deterministically, and returns a warning without touching any MacroStore file. `save_if_dirty` uses `tempfile::NamedTempFile`, `write_all`, `flush`, `sync_all`, and `persist` in the destination directory.

- [ ] **Step 4: Implement cursor-monitor placement**

In `windows_impl.rs`, split Win32 lookup from pure clamping:

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowPlacement {
    pub inner_size: [f32; 2],
    pub outer_position: [f32; 2],
}

pub fn clamp_window_placement(
    preferred: [f32; 2],
    work_area_physical: [f32; 4],
    scale: f32,
) -> WindowPlacement;

pub fn preferred_window_placement(preferred: [f32; 2]) -> WindowPlacement;
```

`preferred_window_placement` uses `GetCursorPos`, `MonitorFromPoint`, `GetMonitorInfoW.rcWork`, and `GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, ...)` to center the clamped window. It reserves 48 logical pixels for native decorations and never returns a size below 720x680 unless the work area itself is smaller.

- [ ] **Step 5: Own preferences in the native shell**

Load `AppUiState` before building `NativeOptions`; initialize `ViewportBuilder` with the placement and the stored window level. Add these `NativeApp` fields:

```rust
ui_state_store: UiStateStore,
ui_state_warning: Option<String>,
```

The top-bar toggle performs:

```rust
if ui.toggle_value(&mut self.ui_state_store.state.always_on_top, "Always on top").changed() {
    let level = if self.ui_state_store.state.always_on_top {
        egui::WindowLevel::AlwaysOnTop
    } else {
        egui::WindowLevel::Normal
    };
    ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
    self.ui_state_store.mark_dirty();
}
```

Save dirty UI state independently from `NativeConfig`. A save failure keeps the in-memory choice, restores no unrelated state, and exposes one non-blocking warning.

- [ ] **Step 6: Verify and commit**

```powershell
cargo test ui_state::tests
cargo test engine::platform::windows_impl::tests
rustfmt --edition 2024 --check --config skip_children=true src/ui_state.rs src/main.rs src/engine/platform/windows_impl.rs src/engine/platform/mod.rs
git diff --check
git add src/ui_state.rs src/main.rs src/engine/platform
git commit -m "feat: persist window presentation preferences"
```

Expected: focused tests and formatting pass; first launch is Off and round-trip tests restore On.

---

### Task 3: Canonical Canvas Projection and Checked Connections

**Files:**
- Create: `src/macro_ui/canvas_model.rs`
- Create: `src/macro_ui/test_support.rs`
- Modify: `src/macro_ui/editor.rs:25-40,246-371,433-507`
- Modify: `src/macro_ui/mod.rs:1-18,89-140`
- Delete after porting tests: `src/macro_ui/timeline.rs`

**Interfaces:**
- Produces: `CanvasProjection`, `CanvasNode`, `CanvasGroup`, `CanvasEdge`, `CanvasSelection`, `OutputPort`, `connection_command`, and `EditorCommand::Redo`.
- Consumes: `MacroDefinition`, `BlockPath`, `ContainerPath`, `InsertionTarget`, `locate_block_path`, `EditorCommand::MoveBlock`.

- [ ] **Step 1: Write projection tests before rendering code**

```rust
#[test]
fn continuous_loop_projects_as_owned_group_with_generated_return_edge() {
    let definition = fixture_continuous_with_observe_and_action();
    let graph = project_canvas(&definition);
    assert!(graph.groups.iter().any(|g| g.id.kind == CanvasGroupKind::LoopBody));
    assert!(graph.edges.iter().any(|e| e.kind == CanvasEdgeKind::LoopReturn && !e.editable));
    assert!(graph.node("observe").unwrap().groups.contains(&CanvasGroupId::loop_body("loop")));
}

#[test]
fn if_ports_are_fixed_then_and_else_roles() {
    let graph = project_canvas(&fixture_if());
    let node = graph.node("if-1").unwrap();
    assert_eq!(node.outputs, vec![OutputPort::IfThen("if-1".into()), OutputPort::IfElse("if-1".into())]);
}

#[test]
fn cross_descendant_connection_is_rejected_without_mutation() {
    let draft = fixture_nested_loop_draft();
    let before = draft.definition.clone();
    assert_eq!(connection_command(&draft, OutputPort::Next("child".into()), "loop"), Err(CanvasConnectionError::IllegalDescendant));
    assert_eq!(draft.definition, before);
}
```

- [ ] **Step 2: Run failing projection tests**

Run: `cargo test macro_ui::canvas_model::tests -- --nocapture`

Expected: compile failure because `canvas_model` does not exist.

- [ ] **Step 3: Define the pure projection**

Use stable IDs and explicit structural roles:

```rust
pub struct CanvasProjection {
    pub nodes: Vec<CanvasNode>,
    pub groups: Vec<CanvasGroup>,
    pub edges: Vec<CanvasEdge>,
}

pub struct CanvasNode {
    pub id: String,
    pub selection: CanvasSelection,
    pub category: crate::ui_theme::BlockCategory,
    pub title: String,
    pub summary: String,
    pub outputs: Vec<OutputPort>,
    pub groups: Vec<CanvasGroupId>,
}

pub struct CanvasGroup {
    pub id: CanvasGroupId,
    pub label: &'static str,
    pub member_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanvasGroupKind { IfThen, IfElse, LoopBody, WatchLaneThen, TimeoutBody }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasEdgeKind { Sequence, Branch, LoopReturn, WatchLane, Timeout }

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
        Self { owner_id: owner_id.into(), kind: CanvasGroupKind::LoopBody }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CanvasSelection {
    Block(String),
    Lane { group_id: String, lane_id: String },
    TimeoutBody { owner_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OutputPort {
    Next(String),
    IfThen(String),
    IfElse(String),
    LoopBody(String),
    WatchLane { group_id: String, lane_id: String },
    TimeoutBody(String),
}

pub fn project_canvas(definition: &MacroDefinition) -> CanvasProjection;
pub fn insertion_target_for_port(definition: &MacroDefinition, port: &OutputPort) -> Result<InsertionTarget, CanvasConnectionError>;
pub fn connection_command(draft: &EditorDraft, port: OutputPort, target_block_id: &str) -> Result<EditorCommand, CanvasConnectionError>;
```

Add `CanvasProjection::node(&self, id: &str) -> Option<&CanvasNode>` as a stable lookup used by layout, rendering, and tests.

`connection_command` moves the target block to the checked insertion target represented by the source port. It never stores an arbitrary edge. THEN/ELSE, loop body, Watch lane, and timeout-body ports map to owned `ContainerPath` variants. Loop-return edges are derived and non-editable. Port filtering omits any block type that would violate the existing validator.

Move the existing canonical fixture builders from `timeline.rs` tests and `mod.rs` tests into `src/macro_ui/test_support.rs`, exported under `cfg(test)` with these exact signatures so every later task uses the same definitions:

```rust
pub fn fixture_definition() -> MacroDefinition;
pub fn fixture_draft() -> EditorDraft;
pub fn fixture_if() -> MacroDefinition;
pub fn fixture_continuous_with_observe_and_action() -> MacroDefinition;
pub fn fixture_nested_loop_draft() -> EditorDraft;
pub fn fixture_large_definition() -> MacroDefinition;
pub fn fixture_ready_state(mode: RunMode) -> MacroPageState;
pub fn fixture_with_pinned_run() -> MacroPageState;
pub fn corrupt_layout() -> MacroCanvasLayout;
```

`fixture_large_definition` creates 500 enabled Comment blocks with stable IDs `comment-000` through `comment-499`; `corrupt_layout` starts from `MacroCanvasLayout::default()` and inserts NaN/Infinity for `observe`; the remaining helpers reuse the existing complete constructors rather than inventing a second test schema.

- [ ] **Step 4: Add definition Redo**

Add `redo: VecDeque<UndoEntry>`, `redo_len`, `EditorCommand::Redo`, and `EditorError::NothingToRedo`. Undo pushes the current snapshot onto `redo`; Redo pushes the current snapshot onto `undo`; every successful non-history mutation clears `redo`. Both paths assign a new monotonic draft revision and `NeedsValidation`.

Add tests proving Undo → Redo restores structure but never reuses the old revision number, and a new edit after Undo clears Redo.

- [ ] **Step 5: Port timeline projection tests and remove list rendering**

Move semantic summary and nesting tests from `timeline.rs` to `canvas_model.rs`. Keep the tested labels plain-language (`Wait for text`, `Left-click text match`, `Continuous Loop`) and remove `timeline.rs` only after all equivalent cases pass.

- [ ] **Step 6: Verify and commit**

```powershell
cargo test macro_ui::canvas_model
cargo test macro_ui::editor
rustfmt --edition 2024 --check --config skip_children=true src/macro_ui/canvas_model.rs src/macro_ui/test_support.rs src/macro_ui/editor.rs src/macro_ui/mod.rs
git diff --check
git add src/macro_ui
git commit -m "feat: project canonical macros onto checked canvas"
```

---

### Task 4: Deterministic Layout, Viewport, and Unified History

**Files:**
- Create: `src/macro_ui/canvas_layout.rs`
- Create: `src/macro_ui/history.rs`
- Modify: `src/macro_ui/mod.rs`
- Modify: `src/ui_state.rs`

**Interfaces:**
- Produces: `CanvasLayoutEngine`, `CanvasViewport`, `LayoutEdit`, `LayoutHistory`, `UiEditHistory`, `fit_view`, `visible_nodes`.
- Consumes: `CanvasProjection`, `MacroCanvasLayout`, `EditorCommand::{Undo,Redo}`.

- [ ] **Step 1: Write failing layout invariance tests**

```rust
#[test]
fn moving_node_changes_layout_only() {
    let draft = fixture_draft();
    let executable_before = serde_json::to_vec(&draft.definition).unwrap();
    let mut layout = MacroCanvasLayout::default();
    CanvasLayoutEngine::move_node(&mut layout, "observe", [320.0, 180.0]);
    assert_eq!(serde_json::to_vec(&draft.definition).unwrap(), executable_before);
    assert_eq!(layout.node_positions["observe"], [320.0, 180.0]);
}

#[test]
fn fit_view_contains_every_projected_node() {
    let graph = project_canvas(&fixture_large_definition());
    let layout = auto_arrange(&graph);
    let viewport = fit_view([900.0, 700.0], graph_bounds(&graph, &layout));
    assert!(graph.nodes.iter().all(|node| viewport.visible_world_rect().contains_rect(node_rect(node, &layout))));
}

#[test]
fn corrupt_positions_are_rebuilt_not_applied() {
    let mut saved = MacroCanvasLayout::default();
    saved.node_positions.insert("observe".into(), [f32::NAN, f32::INFINITY]);
    let repaired = reconcile_layout(&project_canvas(&fixture_definition()), saved);
    assert!(repaired.node_positions.values().flatten().all(|v| v.is_finite()));
}
```

- [ ] **Step 2: Run failing layout tests**

Run: `cargo test macro_ui::canvas_layout::tests macro_ui::history::tests`

Expected: compile failure because the modules do not exist.

- [ ] **Step 3: Implement pure layout and viewport math**

Use these bounds and contracts:

```rust
pub const MIN_ZOOM: f32 = 0.5;
pub const MAX_ZOOM: f32 = 1.75;
pub const NODE_WIDTH: f32 = 280.0;
pub const LAYER_GAP: f32 = 72.0;
pub const SIBLING_GAP: f32 = 36.0;

pub struct CanvasViewport { pub pan: egui::Vec2, pub zoom: f32 }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanvasLayoutError { MissingNode(String), NonFinitePosition }

impl CanvasViewport {
    pub fn screen_from_world(&self, canvas: egui::Rect, world: egui::Pos2) -> egui::Pos2;
    pub fn world_from_screen(&self, canvas: egui::Rect, screen: egui::Pos2) -> egui::Pos2;
    pub fn zoom_around(&mut self, canvas: egui::Rect, pointer: egui::Pos2, factor: f32);
}

pub fn auto_arrange(graph: &CanvasProjection) -> MacroCanvasLayout;
pub fn reconcile_layout(graph: &CanvasProjection, saved: MacroCanvasLayout) -> MacroCanvasLayout;
pub fn node_rect(node: &CanvasNode, layout: &MacroCanvasLayout) -> egui::Rect;
pub fn graph_bounds(graph: &CanvasProjection, layout: &MacroCanvasLayout) -> egui::Rect;
pub fn fit_view(canvas_size: [f32; 2], world_bounds: egui::Rect) -> CanvasViewport;
pub fn visible_nodes<'a>(graph: &'a CanvasProjection, layout: &MacroCanvasLayout, viewport: &CanvasViewport, canvas: egui::Rect) -> Vec<&'a CanvasNode>;
```

`auto_arrange` uses deterministic top-to-bottom layers, places IF branches side by side, encloses loop bodies in group bounds, and sorts Watch lanes by canonical priority. `reconcile_layout` retains finite positions for current stable IDs, drops stale IDs, and auto-positions missing nodes. `visible_nodes` returns only rectangles intersecting the clipped world viewport.

- [ ] **Step 4: Implement bounded unified Undo/Redo ordering**

Use a domain sequence so Ctrl+Z respects the order of layout and definition edits:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditDomain { Definition, Layout }

pub struct UiEditHistory {
    undo_domains: VecDeque<EditDomain>,
    redo_domains: VecDeque<EditDomain>,
    layout: LayoutHistory,
}
```

Successful structural edits record `Definition`; completed node drags/pane resizes record one `Layout` entry. Undo pops the newest domain and invokes either `EditorCommand::Undo` or `LayoutHistory::undo`; Redo mirrors it. Pan and zoom persistence do not enter Undo history. Cap each history at 100 entries.

- [ ] **Step 5: Wire layouts to rebuildable UI state**

`MacroPageState::move_canvas_node(&mut self, block_id: &str, position: [f32; 2]) -> Result<(), CanvasLayoutError>` changes only layout state and records a `Layout` history entry. `MacroPageState` borrows the selected macro's `MacroCanvasLayout` for editing and marks only `UiStateStore` dirty after layout changes. Reopening a macro reconciles stored layout against the current projection. Save failures never set `DraftStatus`, change `saved_revision`, or stop a run.

- [ ] **Step 6: Verify and commit**

```powershell
cargo test macro_ui::canvas_layout
cargo test macro_ui::history
cargo test ui_state
rustfmt --edition 2024 --check --config skip_children=true src/macro_ui/canvas_layout.rs src/macro_ui/history.rs src/macro_ui/mod.rs src/ui_state.rs
git diff --check
git add src/macro_ui src/ui_state.rs
git commit -m "feat: persist rebuildable macro canvas layouts"
```

---

### Task 5: Native egui Canvas Interaction and Rendering

**Files:**
- Create: `src/macro_ui/canvas.rs`
- Modify: `src/macro_ui/mod.rs`
- Modify: `src/ui_theme.rs`

**Interfaces:**
- Produces: `canvas::show`, `CanvasResponse`, `CanvasAction`, `CanvasInputFrame`, `reduce_canvas_input`.
- Consumes: `CanvasProjection`, `MacroCanvasLayout`, `CanvasViewport`, `Ui::allocate_rect`, `Sense::click_and_drag`, `Response::{dragged_by,drag_delta}`, `InputState::{raw_scroll_delta,zoom_delta}`, `Painter::with_clip_rect`.

- [ ] **Step 1: Write input-reducer tests**

```rust
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
    assert_eq!(action, CanvasAction::MoveNode { id: "observe".into(), delta: egui::vec2(24.0, -8.0) });
}

#[test]
fn invalid_connector_drop_returns_reason_without_command() {
    let response = finish_connection(&fixture_draft(), OutputPort::LoopBody("loop".into()), CanvasHit::Node("loop".into()));
    assert!(matches!(response.action, Some(CanvasAction::RejectedConnection(_))));
    assert!(response.editor_command.is_none());
}
```

- [ ] **Step 2: Run failing interaction tests**

Run: `cargo test macro_ui::canvas::tests`

Expected: compile failure because `canvas` does not exist.

- [ ] **Step 3: Implement the egui-to-pure-input adapter**

Allocate one clipped canvas response and translate egui input into `CanvasInputFrame`. Plain wheel zooms while hovered; `zoom_delta()` handles pinch/Ctrl-wheel. Zoom stays pointer-centered. Primary blank drag, middle drag, and Space+drag pan. Node hit testing wins over background panning. Ctrl+primary blank drag is reserved for selection rectangle.

Use these exact interaction types:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum CanvasHit {
    Background,
    Node(String),
    Input(String),
    Output(OutputPort),
}

#[derive(Debug, Clone, PartialEq)]
pub enum CanvasAction {
    Pan(egui::Vec2),
    Zoom { pointer: egui::Pos2, factor: f32 },
    MoveNode { id: String, delta: egui::Vec2 },
    Select(CanvasSelection),
    FinishConnection { source: OutputPort, target_block_id: String },
    OpenAddStep { source: OutputPort, world_position: [f32; 2], allowed: Vec<BlockFamily> },
    RejectedConnection(String),
    CancelGesture,
}

#[derive(Debug, Clone)]
pub struct CanvasInputFrame {
    pub hovered: bool,
    pub hit: CanvasHit,
    pub pointer: Option<egui::Pos2>,
    pub primary_drag_delta: egui::Vec2,
    pub middle_drag_delta: egui::Vec2,
    pub wheel_y: f32,
    pub pinch_zoom: f32,
    pub space_down: bool,
    pub command_down: bool,
}

pub struct CanvasResponse {
    pub action: Option<CanvasAction>,
    pub selection: Option<CanvasSelection>,
    pub editor_command: Option<EditorCommand>,
    pub layout_changed: bool,
}
```

Implement `Default for CanvasHit` as `Background` and `Default for CanvasInputFrame` with `pinch_zoom = 1.0`.

```rust
impl Default for CanvasHit {
    fn default() -> Self { Self::Background }
}

impl Default for CanvasInputFrame {
    fn default() -> Self {
        Self {
            hovered: false,
            hit: CanvasHit::Background,
            pointer: None,
            primary_drag_delta: egui::Vec2::ZERO,
            middle_drag_delta: egui::Vec2::ZERO,
            wheel_y: 0.0,
            pinch_zoom: 1.0,
            space_down: false,
            command_down: false,
        }
    }
}
```

Use `ui.ctx().input_mut` to consume wheel delta only when the canvas is hovered so the parent page does not scroll simultaneously.

- [ ] **Step 4: Render groups, edges, nodes, and handles**

Paint in this order through `ui.painter().with_clip_rect(canvas_rect)`:

1. subtle grid/background;
2. owned group frames and THEN/ELSE/LOOP labels;
3. derived cubic edges;
4. visible nodes only;
5. category chips with icon and label;
6. connector handles and drag preview;
7. selection outline and validation badge;
8. active-run edge/node overlay.

Use `egui::epaint::CubicBezierShape` for edges. Generated loop returns use a distinct non-editable stroke. Active animation requests repaint only while a run is active. A stopped or idle canvas has no continuous repaint loop.

- [ ] **Step 5: Implement connection and Add-step gestures**

Dragging a compatible output to a node input returns the checked `EditorCommand` from `connection_command`. Dropping on empty space returns:

```rust
CanvasAction::OpenAddStep {
    source: OutputPort,
    world_position: [f32; 2],
    allowed: Vec<BlockFamily>,
}
```

The existing palette creates one canonical block and dispatches `EditorCommand::InsertBlock` at `insertion_target_for_port`. Rejected drops make no edit and display `CanvasConnectionError::message()` next to the pointer and in `editor_feedback`.

- [ ] **Step 6: Add keyboard and recovery controls**

Toolbar actions: Fit view, Reset zoom, Auto arrange, Undo, Redo, and visible zoom percentage. Keyboard: `Ctrl+Z`, `Ctrl+Y`, Delete with existing confirmation rules, `F` for Fit view when an input is not focused, and Escape to cancel an in-progress connector drag. Escape cancellation in the editor must not consume or delay the runtime emergency-stop signal.

- [ ] **Step 7: Verify and commit**

```powershell
cargo test macro_ui::canvas
cargo test macro_ui::canvas_layout
rustfmt --edition 2024 --check --config skip_children=true src/macro_ui/canvas.rs src/macro_ui/mod.rs src/ui_theme.rs
git diff --check
git add src/macro_ui src/ui_theme.rs
git commit -m "feat: add native macro node canvas"
```

---

### Task 6: Approved Macro Page Composition

**Files:**
- Modify: `src/macro_ui/mod.rs`
- Modify: `src/macro_ui/library.rs`
- Modify: `src/macro_ui/inspector.rs`
- Modify: `src/macro_ui/monitor.rs`
- Modify: `src/main.rs:1085-1167`

**Interfaces:**
- Produces: `MacroUiIntent`, `MacroPage::show`, `MacroPage::show_bottom`, `PaneMode`, `RunControlAvailability`.
- Consumes: accepted Task 13 `MacroController`/store lifecycle, `canvas::show`, existing inspector intents, monitor projection.

- [ ] **Step 1: Write responsive and control-state tests**

```rust
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
    let controls = run_control_availability(&fixture_ready_state(RunMode::DryRun));
    assert_eq!(controls.primary_label, "Dry Run");
    assert_eq!(controls.primary_detail, "Observe only");
}
```

- [ ] **Step 2: Run failing composition tests**

Run: `cargo test macro_ui::tests::nine_hundred_pixel_window_keeps_three_compact_panes macro_ui::tests::dry_run_is_never_reported_as_live_run`

Expected: compile failure because the responsive/control projections do not exist.

- [ ] **Step 3: Introduce a bounded UI intent queue**

Define exact user intents without giving widgets store/controller ownership:

```rust
pub enum MacroUiIntent {
    Create,
    Select(String),
    Rename { macro_id: String, name: String },
    Duplicate(String),
    Import(std::path::PathBuf),
    Export { macro_id: String, destination: std::path::PathBuf },
    Delete(String),
    Validate,
    Save,
    DryRun,
    RunOnce,
    RunContinuous,
    Pause,
    Resume,
    Stop,
}
```

`MacroPageState::push_intent` rejects additional intents after a bound of 64 and preserves Stop by replacing the oldest non-stop intent. `NativeApp` drains intents and calls the accepted Task 13 controller/store APIs. UI modules never hold `MacroStore`, `MacroController`, Windows input, or HWND values.

Expose `MacroPageState::pending_intent_count(&self) -> usize` for projection/tests and `MacroPageState::drain_intents(&mut self) -> impl Iterator<Item = MacroUiIntent> + '_` for the native shell.

- [ ] **Step 4: Match the approved page hierarchy**

Remove the separate `MACRO FORGE` hero. Use the native top bar for product identity, tabs, target, Always on top, and saved state. `MacroPage::show` composes resizable library/canvas/inspector panes; `show_bottom` composes Run Controls and Status Monitor. At widths below 720, keep the canvas visible and expose Library/Inspector as labeled drawers.

Use a pure breakpoint projection:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneMode { ThreePane, ThreePaneCompact, CanvasWithDrawers }

pub fn pane_mode(width: f32) -> PaneMode {
    if width >= 1100.0 { PaneMode::ThreePane }
    else if width >= 720.0 { PaneMode::ThreePaneCompact }
    else { PaneMode::CanvasWithDrawers }
}
```

- [ ] **Step 5: Refine Library and Inspector**

Library shows Search, New Macro, readable status badges, and secondary Rename/Duplicate/Import/Export/Delete actions. Inspector uses 16 px fields and groups retry, normalization, multiple-match, preprocessing detail, and failure policy under `CollapsingHeader::new("Advanced settings")`. OCR inspector keeps Expected text, Match type, Preprocessing, Polling interval, Timeout, Recapture region, Test detector, and the latest non-clicking result visible.

- [ ] **Step 6: Build semantic bottom controls and monitor**

Run controls render Validate, Dry Run (`Observe only`), Run Once, Run (`Live`), Pause/Resume, and Stop from `RunControlAvailability`. Disabled controls include a tooltip reason. Monitor retains current block, branch/loop, iteration, latest observation, last action, elapsed time, and exact stop reason without an unbounded poll list.

```rust
pub struct RunControlAvailability {
    pub can_validate: bool,
    pub can_dry_run: bool,
    pub can_run_once: bool,
    pub can_run_live: bool,
    pub can_pause: bool,
    pub can_resume: bool,
    pub can_stop: bool,
    pub primary_label: &'static str,
    pub primary_detail: &'static str,
    pub disabled_reason: Option<String>,
}

pub fn run_control_availability(state: &MacroPageState) -> RunControlAvailability;
```

- [ ] **Step 7: Verify and commit**

```powershell
cargo test macro_ui
cargo test routing_tests
rustfmt --edition 2024 --check --config skip_children=true src/macro_ui/mod.rs src/macro_ui/library.rs src/macro_ui/inspector.rs src/macro_ui/monitor.rs src/main.rs
git diff --check
git add src/main.rs src/macro_ui
git commit -m "feat: compose readable macro workspace"
```

---

### Task 7: Enchant Hierarchy and Diagnostics Cleanup

**Files:**
- Modify: `src/main.rs:1170-1553`
- Modify: `src/ui_theme.rs`

**Interfaces:**
- Produces: `EnchantDiagnosticsState`, task-oriented labels, collapsed diagnostics UI.
- Consumes: existing `NativeConfig`, `TestOcrResult`, `BotState`, and unchanged event/capture/start/stop methods.

- [ ] **Step 1: Write presentation-boundary regression tests**

```rust
#[test]
fn diagnostics_visibility_does_not_change_ready_config() {
    let config = NativeConfig::default();
    let before = serde_json::to_vec(&config).unwrap();
    let mut diagnostics = EnchantDiagnosticsState::default();
    diagnostics.open = !diagnostics.open;
    assert_eq!(serde_json::to_vec(&config).unwrap(), before);
}

#[test]
fn beginner_labels_are_task_oriented() {
    assert_eq!(ENCHANT_LABELS.capture_text_area, "Capture text area");
    assert_eq!(ENCHANT_LABELS.target_affix, "Target affix");
    assert!(!ENCHANT_LABELS.capture_text_area.contains("OCR"));
}
```

- [ ] **Step 2: Run failing cleanup tests**

Run: `cargo test routing_tests::diagnostics_visibility_does_not_change_ready_config routing_tests::beginner_labels_are_task_oriented`

Expected: compile failure because the presentation state and labels do not exist.

- [ ] **Step 3: Reorder the Enchant page without changing handlers**

Keep prominent, in order: page title/help, Current result, Target affix, numbered Setup checklist, one primary status message, Start/Stop. Rename beginner-facing labels to `Select game window`, `Capture text area`, `Set Enchant button`, `Set Replace button`, and `Set Close button`. Continue calling the existing capture and action methods unchanged.

Define the copy once:

```rust
pub struct EnchantLabels {
    pub select_window: &'static str,
    pub capture_text_area: &'static str,
    pub target_affix: &'static str,
    pub enchant_button: &'static str,
    pub replace_button: &'static str,
    pub close_button: &'static str,
}

pub const ENCHANT_LABELS: EnchantLabels = EnchantLabels {
    select_window: "Select game window",
    capture_text_area: "Capture text area",
    target_affix: "Target affix",
    enchant_button: "Set Enchant button",
    replace_button: "Set Replace button",
    close_button: "Set Close button",
};
```

- [ ] **Step 4: Collapse technical diagnostics**

Move normalized OCR text, raw/captured rectangles, score timing, coordinate ratios, mouse movement details, and workflow trace into:

```rust
egui::CollapsingHeader::new("Diagnostics")
    .default_open(false)
    .show(ui, |ui| self.show_enchant_diagnostics(ui));
```

Do not serialize whether Diagnostics is open. Keep status pill meaning and one nearby status message; remove duplicate prose that reports the same state.

- [ ] **Step 5: Run Enchant regression verification and commit**

```powershell
cargo test routing_tests
cargo test engine::enchant_loop
rustfmt --edition 2024 --check --config skip_children=true src/main.rs src/ui_theme.rs
git diff --check
git add src/main.rs src/ui_theme.rs
git commit -m "style: simplify enchant setup hierarchy"
```

Expected: existing Enchant tests pass and no engine file changes are staged.

---

### Task 8: Cross-Boundary Acceptance, Documentation, and Manual UAT

**Files:**
- Create: `src/macro_ui/acceptance_tests.rs`
- Modify: `src/macro_ui/mod.rs`
- Modify: `README.md`

**Interfaces:**
- Consumes: all preceding UI contracts plus accepted Task 13 runtime/store contracts.
- Produces: adversarial boundary evidence, user instructions, and manual acceptance record.

- [ ] **Step 1: Add cross-module boundary tests**

Register `#[cfg(test)] mod acceptance_tests;` and cover these exact invariants:

```rust
#[test]
fn layout_edits_do_not_change_saved_or_running_identity() {
    let mut state = fixture_with_pinned_run();
    let saved = state.saved_identity().unwrap().clone();
    let running = state.running_snapshot.as_ref().unwrap().clone();
    state.move_canvas_node("observe", [440.0, 210.0]).unwrap();
    assert_eq!(state.saved_identity().unwrap(), &saved);
    assert_eq!(state.running_snapshot.as_ref().unwrap(), &running);
}

#[test]
fn corrupt_layout_recovery_preserves_definition() {
    let definition = fixture_definition();
    let recovered = reconcile_layout(&project_canvas(&definition), corrupt_layout());
    assert_eq!(validate_macro(&definition), Vec::<ValidationProblem>::new());
    assert!(recovered.is_finite());
}

#[test]
fn stop_replaces_oldest_non_stop_intent_when_queue_is_full() {
    let mut state = fixture_ready_state(RunMode::ObservationOnly);
    for index in 0..64 {
        state.push_intent(MacroUiIntent::Rename {
            macro_id: "macro".into(),
            name: format!("name-{index}"),
        });
    }
    state.push_intent(MacroUiIntent::Stop);
    assert_eq!(state.pending_intent_count(), 64);
    assert!(state.drain_intents().any(|intent| matches!(intent, MacroUiIntent::Stop)));
}
```

Also test invalid cross-branch links, generated loop returns, Watch-lane order, UI intent bounds, fit-view recovery, disabled-action reasons, and active-node reveal without saved-layout mutation. Keep the existing direct `EscStopSignal`/controller tests as the proof that emergency stop bypasses `MacroUiIntent`; do not add an EmergencyStop enum variant.

- [ ] **Step 2: Run focused and full automated verification**

```powershell
cargo test macro_ui::acceptance_tests
cargo test macro_ui
cargo test
cargo build --release
rustfmt --edition 2024 --check --config skip_children=true src/ui_theme.rs src/ui_state.rs src/main.rs src/macro_ui/mod.rs src/macro_ui/canvas.rs src/macro_ui/canvas_model.rs src/macro_ui/canvas_layout.rs src/macro_ui/history.rs src/macro_ui/library.rs src/macro_ui/inspector.rs src/macro_ui/monitor.rs src/macro_ui/wizard.rs src/macro_ui/test_support.rs src/macro_ui/acceptance_tests.rs src/engine/platform/windows_impl.rs src/engine/platform/mod.rs
git diff --check
```

Expected: all tests pass, release build succeeds, touched-file formatting passes, and diff check is clean. If broad `cargo fmt --check` still reports the pre-existing `src/engine/enchant_loop.rs` drift, do not modify that unrelated file in this plan.

- [ ] **Step 3: Update user documentation**

Document the 900x1080 preferred workspace, responsive drawers, Always on top default/persistence, empty-space pan, wheel/pinch zoom, node movement, checked connectors, Fit view, Auto arrange, Undo/Redo, Dry Run observation-only wording, Stop, and ESC. State that the UI is independently implemented and that node connections cannot bypass canonical validation.

- [ ] **Step 4: Perform the user-assisted manual path**

Ask the user to test on their normal Windows/DPI setup:

```text
launch -> confirm readable size -> toggle Always on top -> restart ->
Enchant calibration/OCR/start/stop -> Macro create -> bind target ->
add Observe/IF/THEN/ELSE/Act/Continuous Loop -> pan -> zoom -> move ->
connect -> reject invalid connection -> Fit view -> Auto arrange -> save -> reopen ->
validate -> dry run -> run once -> live run -> pause -> resume -> Stop -> ESC ->
history -> export -> delete -> import -> local image re-verification
```

Record display resolution, Windows scaling, whether the whole window remained reachable, whether any text still felt too small, and any canvas gesture conflict.

- [ ] **Step 5: Commit**

```powershell
git add src/macro_ui README.md
git commit -m "test: verify readable macro canvas experience"
```

---

## Spec Coverage Matrix

| Design requirement | Owning task |
| --- | --- |
| Shared 22/16/14/12 type scale and contrast | Task 1 |
| 900x1080 clamped window | Task 2 |
| Persisted Always on top, default Off | Task 2 |
| Canonical tree remains runtime truth | Tasks 3, 8 |
| Continuous Loop visually owns its repeated body | Tasks 3, 5 |
| n8n-style pan, zoom, node drag, connectors | Tasks 4, 5 |
| No copied n8n source/CSS/assets | Global Constraints, Tasks 5, 8 |
| Separate rebuildable layout state | Tasks 2, 4, 8 |
| Undo/Redo and Auto arrange | Tasks 3, 4, 5 |
| Library/canvas/inspector + bottom controls/monitor | Task 6 |
| Responsive drawers with canvas priority | Task 6 |
| Inspector preview, recapture, test, Advanced settings | Task 6 |
| Run/Dry Run/Pause/Stop semantic distinction | Task 6 |
| Enchant presentation cleanup without behavior change | Task 7 |
| Accessibility, recovery, bounded state/intents | Tasks 1, 2, 4, 6, 8 |
| Automated and user-assisted acceptance | Task 8 |

## Plan Self-Review Result

- Every section of `2026-07-13-frontend-readability-macro-canvas-design.md` maps to at least one task above.
- The plan adds no general-purpose graph, web frontend, CSS, or n8n dependency.
- `MacroDefinition` and its checked editor commands remain the sole executable mutation path.
- Persisted layout types use arrays rather than egui geometry types, avoiding a new serde feature.
- `WindowLevel`, viewport input, `Painter::with_clip_rect`, `Response::dragged_by`, and `InputState::zoom_delta` names match the locally pinned egui 0.27.2 source.
- The implementation starts only after the paused Task 13 backend findings and live-core review are accepted.
- No placeholder step or intentionally deferred requirement remains in this plan.
