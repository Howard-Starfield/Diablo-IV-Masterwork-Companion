# Macro Canvas-First Workspace Design

**Status:** Approved direction; awaiting written-spec review

**Date:** 2026-07-21

**Product:** Diablo Masterwork Companion / BoBo Companion

**Target:** Windows-native Rust/egui application

## 1. Purpose and precedence

Replace the current control-heavy Macro page with a maximized, canvas-first workflow editor inspired by the useful interaction patterns of n8n and image-editing tools.

This document supersedes the window sizing, Macro workspace composition, canvas navigation, connection presentation, and loop visualization sections of `2026-07-13-frontend-readability-macro-canvas-design.md`. The canonical macro block tree, runtime behavior, validation, persistence, safety rules, ESC handling, and clean-room implementation boundary remain authoritative.

The redesign is interaction-compatible with the existing engine. It does not add arbitrary Goto, Tag, Jump, or user-authored cycles. Canvas connections remain a checked projection of the canonical block tree.

## 2. Approved product decisions

- Start the application maximized inside the active monitor's Windows work area.
- Make the canvas the dominant Macro surface.
- Move the Macro Library and Block Inspector into resizable drawers.
- Replace the large wrapped command strip with a compact tool rail and contextual controls.
- Use top-to-bottom flow as the default Macro orientation.
- Provide a per-macro top-to-bottom / left-to-right presentation switch.
- Add nodes by clicking a compatible output circle, dragging a connection to empty space, double-clicking empty canvas, or pressing `Tab`.
- Pan by dragging empty canvas with the primary mouse button. Middle-button drag and Space+drag remain equivalent.
- Snap nodes to a 16-logical-pixel world grid and connection endpoints within a 60-logical-pixel capture radius, following the current n8n interaction scale.
- Render Repeat as a visible loop-back wire to the first task in its owned loop body.
- Use n8n-like active-node motion and directional connection feedback, implemented independently in egui.

## 3. Clean-room n8n reference boundary

The implementation may reproduce these publicly observable workflow-editor patterns:

- Canvas-first editing with secondary panels.
- Output-handle `+` creation.
- Connection-drop creation at the pointer.
- Connection-line insertion.
- Grid snapping and magnetic connection targets.
- Floating zoom, fit, and arrange controls.
- Active-node execution feedback.

The implementation must not copy n8n Vue components, CSS, icons, trademarks, assets, or source text. n8n's repository uses the Sustainable Use License, and this product uses a different native UI stack. All geometry, painting, animation, hit testing, and state handling are implemented independently in Rust/egui.

Research references:

- `Canvas.vue`: grid snapping, connection radius, pan and zoom behavior.
- `CanvasNode.vue` and `CanvasHandlePlus.vue`: directional ports and output-handle creation.
- `useCanvasOperations.ts`: connection-drop creation and insertion behavior.
- `_canvasNodeStyles.scss`: active and waiting node animation timing.
- `useCanvasLayout.ts`: deterministic graph arrangement.

## 4. Maximized workspace composition

The Macro page has five layers rather than five permanently competing panels.

### 4.1 Application header

One compact row contains:

- BoBo Companion identity and Enchant / Macro navigation.
- Current macro name.
- Target-window connection state.
- Draft / saved / validation state.
- Always on top toggle.
- Save action and one overflow menu.

`Guided wizard` and secondary file-management actions live in the overflow menu or Library drawer. They do not occupy a permanent canvas row.

### 4.2 Canvas

The canvas fills all space between the application header and the bottom run dock. It never sits below a wrapping toolbar and never uses a fixed 430-pixel height.

The canvas owns:

- Node and edge rendering.
- Pan, zoom, selection, connection, and node movement gestures.
- Floating viewport controls.
- Contextual node and connection toolbars.
- Empty-state creation prompt.

### 4.3 Left tool rail and Library drawer

A narrow icon-and-tooltip rail contains:

- Select.
- Hand / Pan.
- Add step.
- Comment / Note.
- Auto arrange.
- Open Macro Library.

The Library drawer opens over or beside the canvas, depending on available width. It contains search, New Macro, saved macro rows, readiness badges, and secondary management actions. Closing it returns the width to the canvas.

### 4.4 Inspector drawer

Selecting a node opens a right-side Inspector drawer, approximately 340-400 logical pixels wide when space permits. The drawer is resizable and can be pinned open. Closing it does not clear the canvas selection.

Node mutation, conversion, detector configuration, region capture, OCR testing, duplicate, disable, and delete actions move into this drawer or the selected-node context menu. They do not appear in the global canvas toolbar.

### 4.5 Bottom run dock

A compact fixed dock contains Validate, Dry Run, Run Once, Run Live, Pause / Resume, and Stop. A one-line monitor shows current step, loop iteration, last observation, elapsed time, and stop reason. Detailed monitoring expands upward only when requested.

## 5. Responsive behavior

- Maximized desktop: canvas plus any pinned drawers.
- Reduced width: drawers overlay the canvas instead of compressing it below a useful minimum.
- Reduced height: the bottom monitor collapses before the canvas loses working height.
- The canvas remains present at every supported size.
- Reopening the app restores drawer state, pane widths, orientation, pan, and zoom only when the saved monitor geometry remains valid.

## 6. Flow orientation

### 6.1 Top-to-bottom default

- Inputs appear at the top of ordinary nodes.
- Primary outputs appear at the bottom.
- Sequence arrows point downward.
- IF exposes labeled `THEN` and `ELSE` outputs along its bottom edge.
- Watch lanes use labeled ordered outputs without relying on color alone.
- Repeat return edges travel outside the loop group and point back upward.

### 6.2 Left-to-right option

- Inputs move to the left and primary outputs to the right.
- IF branches fan vertically from the right edge.
- Repeat returns below or above the loop group toward its first task.

Changing orientation is presentation-only. It runs deterministic auto-arrange, updates port geometry, and records one undoable layout edit. It never changes execution order, the macro revision, or the active runtime snapshot.

## 7. Canvas navigation and snapping

### 7.1 Pan and zoom

- Primary drag on empty space pans.
- Primary drag beginning on a node moves the node.
- Middle drag and Space+primary drag always pan.
- Mouse-wheel or trackpad pinch zooms around the pointer.
- `Fit`, `100%`, `+`, and `-` live in a compact floating control at the lower-left.
- The pointer changes between default, open-hand, and closed-hand states so the active gesture is visible.

### 7.2 Node grid snap

- World-space grid size is 16 logical pixels.
- Node centers snap to the grid during drag, matching n8n's center-aligned approach and preventing connector drift between differently sized cards.
- Holding `Alt` temporarily disables node snap for precision placement.
- The unsnapped pointer delta remains the gesture source so nodes do not accumulate rounding error.

### 7.3 Connection magnetism

- Compatible target handles enter magnetic range at 60 logical pixels in screen-adjusted world space.
- The closest compatible target highlights and displays a ghost connection endpoint.
- Incompatible targets do not snap and expose a short rejection reason on release.
- A highlighted snap target is committed only on pointer release.
- Pressing ESC cancels the connection without changing the draft.

### 7.4 Alignment guides

V1 includes grid snap and connection magnetism. Smart alignment guides, distribution controls, minimap, and group collapsing are follow-up enhancements unless required to keep a generated layout readable.

## 8. Node creation and insertion

### 8.1 Empty macro

An empty canvas shows one centered `Add first step` action. It opens the same node palette used everywhere else.

### 8.2 Output-circle creation

- An unconnected compatible output shows a clear circular `+` affordance.
- Clicking the circle opens a categorized palette anchored near the port.
- The palette is filtered by canonical structural validity.
- Choosing a block inserts it at the next snapped position and creates the checked structural connection atomically.
- Undo removes both the inserted block and connection as one operation.

### 8.3 Drag-to-create

- Dragging from an output begins a live preview wire.
- Dropping on a compatible input creates the checked connection.
- Dropping on empty canvas opens the filtered node palette at that location.
- Canceling the palette leaves the macro unchanged.

### 8.4 Connection insertion

Hovering a normal sequence edge reveals a small `+`. Selecting a block inserts it between the two connected steps and shifts only the affected downstream layout when needed. Branch identity, loop ownership, and Watch lane order remain fixed.

## 9. Repeat visualization

Repeat remains a structured owner of its body. It does not become an arbitrary jump instruction.

### 9.1 Loop group

- Repeated tasks appear inside a restrained Repeat group boundary.
- The group header states `Repeat N`, `Repeat until ...`, or `Continuous until stopped`.
- The first task in the body is the visual loop target.
- An empty body shows `Add step to loop` rather than a misleading self-loop.

### 9.2 Generated return wire

- A generated, non-editable Repeat junction follows the last structural exit of the body.
- Its `REPEAT` path routes outside the group boundary and returns to the first body task.
- Its `DONE` path continues to the next sibling when the loop type can complete.
- Continuous loops show no false `DONE` path.
- The return wire is teal, labeled `Repeat`, and carries an arrowhead at the first repeated task.
- In top-to-bottom mode it uses the side with fewer crossings and maintains a minimum 40-pixel lane outside the group.
- In left-to-right mode it uses the clearer top or bottom lane.
- Nested loops receive distinct routing lanes so return wires do not overlap.

This replaces the current projected self-loop on the Repeat node. The engine's owned-loop semantics remain unchanged.

## 10. Execution animation

Animation communicates runtime state; it is never decorative background motion.

- Active node: independently implemented rotating orange-to-transparent border, approximately a 1.5-second cycle like n8n's current running treatment.
- Waiting detector or wait step: slower approximately 4.5-second border cycle.
- Active sequence edge: one restrained moving highlight travels in execution direction.
- Loop yield: the Repeat return wire animates once from the Repeat junction to the first repeated task as the runtime emits the next iteration.
- Completed node: settles to a static success state.
- Failed node: becomes a static error state.
- Paused run: freezes motion while retaining the active node and edge.
- Stopped, canceled, focus-lost, or completed run: removes all running animation immediately.

Animation state is derived from the bounded runtime projection, not from independent UI timers. Repaint requests run only while visible motion is required. The design must not reproduce reports of n8n animation remaining active after loops, errors, or manual stops.

A Reduce motion preference replaces traveling motion with static high-contrast active borders and arrows. Motion is never the only indication of execution direction or state.

## 11. Contextual controls

### 11.1 Selected node

A compact toolbar near the selected node may expose Configure, Run/Test, Duplicate, Disable, and More. Delete remains inside More or the Inspector and uses destructive styling.

### 11.2 Selected connection

A selected editable connection exposes Insert step and Delete. Generated branch, loop-return, and ownership edges explain why they cannot be deleted directly.

### 11.3 Global canvas controls

The only permanent canvas commands are Undo, Redo, Add step, Auto arrange, orientation, Fit, zoom, and drawer toggles.

## 12. Data and safety boundaries

- `MacroDefinition.blocks` remains the only executable source of truth.
- Orientation, positions, pan, zoom, drawer state, and routing lanes are non-executable UI state.
- Grid movement does not change validation, revision, or saved hash.
- Output-circle creation and connection insertion dispatch checked editor commands.
- Generated Repeat wires cannot be grabbed, retargeted, or deleted independently.
- Invalid cycles outside owned loops remain rejected.
- Live input safety, target focus checks, click pacing, Stop, and ESC behavior do not change.
- Canvas animation never drives runtime execution.

## 13. Accessibility and performance

- Normal text remains at least 14-16 pixels; metadata remains at least 12 pixels.
- Icon-only rail and viewport controls have names and tooltips.
- Handles have a visible target of at least 24 logical pixels even when the painted circle is smaller.
- Keyboard users can focus nodes, open the palette, connect through an accessible command path, inspect, delete with confirmation, and recover with Fit.
- Color, motion, and line style are never the sole state indicators.
- Off-screen nodes and edges are culled.
- Active animation targets 30 repaint frames per second unless a platform measurement proves 60 is inexpensive.
- Idle canvas interaction does not request continuous repaint.

## 14. Acceptance criteria

### Workspace

- The application starts maximized within the active monitor work area.
- The canvas receives the majority of the Macro page at 1920x1080 and larger.
- Opening both drawers still leaves a useful canvas; smaller widths use overlays.
- No wrapped global toolbar can push the canvas below the visible area.

### Navigation and snap

- Empty-space primary drag pans in all canvas areas not owned by a node, port, or floating control.
- Node drag never pans the canvas.
- Nodes snap to the 16-pixel world grid and `Alt` bypasses snap.
- Compatible ports snap inside the 60-pixel capture radius; incompatible ports never commit.
- Fit always restores all canonical nodes to view.

### Creation and structure

- Output-circle click, connection drop, `Tab`, and Add step use one filtered palette.
- Node creation plus connection is one undoable edit.
- Edge insertion preserves branch, loop, and Watch ownership.
- Top-to-bottom is the default for new macros; the left-to-right switch is persisted per macro.

### Repeat

- Every non-empty Repeat body visibly returns to its first repeated task.
- Repeat N and Repeat Until distinguish `REPEAT` from `DONE`.
- Continuous loops do not show a completion edge.
- Nested return wires remain distinguishable and do not become editable runtime cycles.
- The displayed return path agrees with the runtime iteration count.

### Animation and safety

- Active node and edge agree with the runtime monitor.
- A loop-yield event animates the correct return wire exactly for that transition.
- Pause freezes motion; Resume continues from current state.
- Stop, ESC, focus loss, error, and completion clear running animation promptly.
- Reduce motion preserves equivalent static state information.

## 15. Implementation sequencing

1. Replace preferred window sizing with maximized startup and valid monitor fallback.
2. Refactor the Macro page into header, dominant canvas, drawers, tool rail, and compact run dock.
3. Make canvas height derive from remaining space and repair gesture ownership/cursors.
4. Add orientation-aware ports, edge routing, and deterministic auto-arrange.
5. Add 16-pixel node snap and 60-pixel connection magnetism.
6. Replace the wrapped creation toolbar with the shared contextual node palette.
7. Replace the Repeat self-loop projection with a generated body-return wire and completion path.
8. Add runtime-driven node/edge animation and Reduce motion behavior.
9. Add focused projection, layout, gesture, snap, loop, runtime-state, and persistence tests.
10. Perform native Windows visual comparison and manual interaction UAT at 1920x1080, 2560x1440, 125% DPI, and a reduced-width fallback.
