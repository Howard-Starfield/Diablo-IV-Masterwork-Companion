# Frontend Readability and Macro Canvas Design

**Status:** Approved design; awaiting written-spec review

**Date:** 2026-07-13

**Product:** Diablo Masterwork Companion / BoBo Companion

**Target:** Windows-native Rust/egui application

## 1. Purpose and precedence

Improve readability throughout the existing Enchant and Macro pages, enlarge the default application workspace, add a persistent `Always on top` preference, and replace the Macro page's dense structured list with a clear node canvas inspired by the interaction quality of n8n.

This document supplements `2026-07-12-macro-tab-design.md`. It supersedes that document only for typography, window presentation, Macro page layout, and editor interaction. The existing canonical block model, runtime semantics, safety boundaries, validation rules, persistence contracts, and explicit V1 exclusions remain authoritative.

The approved image reference is `ChatGPT Image Jul 13, 2026, 10_34_47 PM.png`, 1145x1374 pixels, SHA-256 `1AE81751C808061305CA7035ADD203D9BB440EC6BD4ED2CDC7E317A3EFF4D6B3`. It is a design reference, not a product asset. The implementation must preserve its layout and hierarchy while applying the semantic corrections in this document.

## 2. Design goals

- Make normal content comfortably readable without relying on Windows magnification.
- Let a new user immediately answer: what am I configuring, is it ready, and what should I do next?
- Make execution order and branching visually understandable without exposing a general-purpose programming graph.
- Keep advanced configuration available but visually secondary.
- Preserve the tested executable block tree as the only source of runtime truth.
- Keep Enchant behavior unchanged while improving its presentation and wording.
- Recreate the approved canvas experience independently in egui without copying n8n source, CSS, trademarks, or assets.

## 3. Typography and visual tokens

The application uses one shared type scale rather than scattered per-widget font sizes.

| Role | Size | Use |
| --- | ---: | --- |
| Page title | 22 px | Current tool or macro name |
| Section title | 16 px semibold | Library, Event Timeline, Block Inspector, Run Controls |
| Body and controls | 16 px | Normal labels, inputs, buttons, node titles |
| Supporting text | 14 px | Descriptions, field help, status details |
| Optional metadata | 12 px minimum | Revisions, timestamps, coordinates, performance diagnostics |

No product text may be rendered below 12 px. Category chips may use 12 px semibold text but cannot rely on uppercase styling or color alone. Inputs and primary buttons must grow vertically with the type scale rather than clipping or crowding the larger text.

The dark palette remains charcoal rather than pure black. Diablo orange identifies selection and the primary live action. Observe, Decide, Act, and Repeat use restrained blue, purple, orange, and teal accents plus icons and text labels. Secondary text must retain readable contrast; gray text cannot be made faint merely to reduce visual weight.

These tokens are implemented as app-owned Rust/egui style helpers. There is no browser CSS layer.

## 4. Window behavior

- Preferred opening size: 900x1080 logical pixels, replacing the current 600x760 size.
- Clamp the initial rectangle to the current monitor's usable work area so the title bar and controls remain reachable.
- Keep the window resizable. A smaller monitor may open below the preferred size rather than placing content off-screen.
- Preserve per-monitor DPI behavior. Do not apply a second global magnification on top of OS DPI scaling.
- Add a clearly labeled `Always on top` toggle to the application top bar.
- `Always on top` defaults to Off, applies immediately, and persists the user's last choice across launches.
- A failed preference load falls back to Off. Failure to change the window level leaves the prior level active and shows a non-blocking error; it must not affect macro runtime state.

## 5. Top-level shell

The top bar follows the approved image:

- BoBo Companion identity at the left.
- `Enchant` and `Macro` navigation, with the active tab clearly underlined or filled.
- Concrete target-window identity and connection state.
- `Always on top` toggle.
- Draft or saved state.
- Native window controls at the far right.

Target connection, saved state, and Always on top are independent states. A green target indicator must not imply that a draft is validated or that live input is safe to run.

## 6. Macro page layout

The preferred layout has five coordinated regions:

1. **Macro Library, left:** search, `New Macro`, saved macros, readiness badges, and secondary management commands.
2. **Event Canvas, center:** the dominant workspace, containing nodes and validated connectors.
3. **Block Inspector, right:** settings and tests for the selected block.
4. **Run Controls, bottom left:** Validate, Dry Run, Run Once, Run, Pause, and Stop.
5. **Status Monitor, bottom right:** readiness, current step, iteration, latest observation, last action, elapsed time, and exact stop reason.

Horizontal panes are resizable. At approximately 900 px width, compact library and inspector widths preserve a useful center canvas. On narrower work areas, the library and inspector become drawers while the canvas stays visible. The bottom monitor may stack under the run controls when horizontal space is insufficient.

## 7. Macro Library

- Search uses macro names and visible readiness states.
- `New Macro` is the primary library action.
- Each row shows name and one plain-language status: Draft, Ready, Running, Needs Attention, or Disabled.
- Rename, Duplicate, Import, Export, and Delete remain available but are visually secondary.
- Delete is separated and red; it always identifies the affected macro and uses the existing checked persistence path.
- Decorative Diablo item images from the generated reference are not required. If category icons are used, they must be app-owned or properly licensed and must not imply behavior.

## 8. Canvas presentation model

The canvas is a projection of the canonical macro block tree. It is not a second executable graph format.

- Nodes represent canonical Observe, Decide, Act, Repeat, Wait, Stop, Watch Group, and Comment blocks.
- Edges are derived from structural relationships in the canonical tree.
- A connection gesture performs a checked structural edit to that tree and then redraws the derived edges.
- Arbitrary edge records cannot bypass canonical validation or become a second runtime input.
- Spatial position, pan, zoom, collapsed state, and pane widths are non-executable editor layout state.
- Moving a node without changing connections does not alter the executable definition, saved revision hash, validation state, or pinned run snapshot.
- Structural connection changes do alter the draft and require the existing validation/save flow.

Editor layout state is stored separately from immutable executable revisions and keyed by stable macro and block IDs. Missing or corrupt layout state falls back to deterministic auto-arrangement without changing the macro definition.

## 9. Canvas interaction

### 9.1 Navigation

- Primary-button drag on empty canvas pans the viewport.
- Middle-button drag and Space+drag are equivalent pan gestures for experienced users.
- Mouse-wheel zoom is centered on the pointer. Trackpad pinch may map to the same zoom path when the platform supplies it.
- Zoom is bounded to a usable range and exposes `Fit view`, `Reset zoom`, and a visible percentage.
- Modified empty-space drag may provide a selection rectangle; it must not replace the default empty-space pan requested for V1.
- Canvas position and zoom are restored per macro. `Fit view` recovers from a lost or empty-looking viewport.

### 9.2 Nodes

- Dragging a node changes only its editor position until a connector is changed.
- Selected nodes receive a clear outline and populate the right inspector.
- Multi-selection may move related nodes together but cannot detach children from a structural container.
- `Auto arrange` produces a readable top-to-bottom flow and does not change execution order.
- Undo and Redo cover node movement, connection edits, block creation, deletion, conversion, and inspector changes.

### 9.3 Connections

- Drag from an output handle to a compatible input handle.
- Compatible targets highlight before drop; invalid targets explain why the connection is rejected.
- Dropping a connection on empty canvas opens an `Add step` menu filtered to structurally valid block types.
- IF exposes fixed, labeled THEN and ELSE outputs. Their meaning cannot be swapped by moving the nodes.
- Loops own their bodies. The editor generates and renders the loop-back edge; users do not draw arbitrary back edges.
- Watch Group lanes use ordered, labeled outputs and preserve the runtime's deterministic arbitration priority.
- During a run, highlight the active node and its current structural edge. Animation must be restrained and stop when the run stops.

### 9.4 Validation constraints

The editor rejects or reports:

- Unreachable executable blocks.
- Missing required THEN, ELSE, loop, or timeout bodies.
- Cycles outside an owned loop.
- Connections into or out of another block's owned container.
- Arbitrary Goto, Tag, Jump, or cross-loop links.
- An action whose required observation relationship is absent or stale.
- Disconnected blocks that appear runnable but are not part of the canonical entry tree.

Comments may remain disconnected because they are non-executable.

## 10. Generated-reference corrections

The approved image is followed with these explicit corrections:

- `Continuous Loop` visually contains the blocks it repeats. It cannot appear as an unrelated node after the repeated sequence.
- `Always on top` is shown Off on a first launch, despite appearing enabled in the generated reference.
- The example's ELSE body is a fixed wait action, not an Observe-category block.
- Connected numbering is derived from canonical structure and cannot suggest a false execution order.
- Run readiness is independent from target connection and saved state.

## 11. Block Inspector

The inspector uses the selected block's plain-language name and category. For an OCR block it shows:

- Region preview and region identity.
- Editable expected text.
- Match type and preprocessing profile.
- Polling interval and timeout.
- `Recapture region` and observation-only `Test detector` actions.
- Latest test outcome, confidence, and timestamp.
- A collapsed `Advanced settings` section for retry, normalization, preprocessing details, multiple-match behavior, and failure policy.

Recapture and detector-test actions continue to use the app-owned capture and detector paths. A preview is evidence for editing, not an authorization token for a later click.

## 12. Run Controls and Status Monitor

- Validate is neutral and reports actionable failures.
- Dry Run is visibly labeled `Observe only` and never resembles the live Run button.
- Run Once is distinct from continuous Run.
- Run uses the primary orange live-action treatment and includes a `Live` label.
- Pause is available only when meaningful.
- Stop is unmistakable red and remains available throughout a live or continuous run.
- ESC remains the direct emergency path and does not depend on the canvas or egui event queue.

The monitor shows semantic state rather than an unbounded event list. Current step, enclosing branch or loop, iteration count, latest observation, last action, elapsed time, and exact stop reason remain visible. Technical event history belongs in bounded diagnostics/history views.

## 13. Enchant readability cleanup

Enchant retains its behavior and configuration model but adopts the shared typography and hierarchy:

- Use task language such as `Capture text area` rather than `OCR Region` in beginner-facing controls.
- Present calibration as one numbered checklist with completion states.
- Keep current result, target affix, setup progress, and Start/Stop prominent.
- Move normalized OCR text, raw coordinates, timings, and mouse-path details into a collapsed `Diagnostics` section.
- Show one primary status message near the relevant action rather than repeating equivalent status in several places.
- Keep advanced matcher and timing settings available without presenting them as required first steps.

No Enchant engine, capture, matching, or action behavior changes as part of this UI cleanup.

## 14. Accessibility and clarity

- Color is never the only carrier of category, readiness, selection, or error state.
- All icon-only controls have tooltips and accessible names.
- Focus order follows top bar, library, canvas, inspector, run controls, and monitor.
- Keyboard users can select nodes, open the inspector, invoke Add step, delete after confirmation, and recover with Fit view.
- Long names truncate with a tooltip rather than shrinking text below the minimum.
- Error messages state what failed, why execution is blocked, and the next corrective action.

## 15. Clean-room implementation boundary

The implementation may study publicly visible workflow-editor behavior and the user-approved reference image. It must not copy n8n CSS, Vue components, source code, icons, trademarks, or proprietary visual assets.

All canvas math, rendering, hit testing, edge routing, gestures, style tokens, and tests are implemented independently in Rust/egui or through dependencies whose licenses are independently reviewed and compatible with this project. If a new canvas dependency is considered, its current API, license, maintenance status, native compatibility, and executable-size cost require a separate recorded decision before adoption.

## 16. Failure handling

- Corrupt editor layout: discard only the layout projection and auto-arrange the intact macro.
- Invalid connection drop: make no definition change and explain the structural rule.
- Inspector edit failure: preserve the prior valid value and keep the field error local.
- Preference persistence failure: keep the in-memory choice for the current session and show a non-blocking warning.
- Canvas renderer failure or empty viewport: retain the canonical definition and provide Fit view/Auto arrange recovery.
- Runtime events referring to a hidden or off-screen node: select and reveal the canonical active node without mutating the saved layout unless the user confirms the new view.

## 17. Verification and acceptance

### Typography and window

- No rendered product text is configured below 12 px.
- Body, controls, section titles, and metadata use the shared token scale.
- The preferred 900x1080 window opens fully inside each test monitor's usable area at supported DPI settings.
- Always on top defaults Off, applies immediately, and restores the last saved choice.
- Enchant remains functionally unchanged.

### Canvas model

- Node movement changes layout state but not executable hash, validation, or revision.
- Connection edits produce only valid canonical tree mutations.
- THEN/ELSE, loop ownership, Watch Group ordering, and timeout bodies survive save/load and auto-arrangement.
- Invalid cycles, cross-container links, and unreachable executable nodes cannot be saved as Ready.
- Corrupt or missing layout metadata cannot corrupt or delete a macro definition.

### Interaction

- Empty-space drag pans; node drag moves; connector drag links only compatible handles.
- Fit view always recovers all canonical nodes.
- Undo/Redo restores both structural and layout edits correctly.
- A live run highlights the correct canonical node and edge without changing execution.
- Stop and ESC remain responsive while the canvas is panning, zooming, or rendering a large macro.

### Manual acceptance path

1. Launch on a 1080p monitor and confirm the larger readable window remains fully visible.
2. Toggle Always on top, restart, and confirm the choice persists; restore it to Off.
3. Open Enchant and confirm the same workflow still calibrates, tests OCR, starts, and stops.
4. Open Macro, create a macro, and add Observe, IF/THEN/ELSE, Act, Wait, and Continuous Loop blocks.
5. Pan by dragging empty space, zoom around the pointer, move nodes, use Fit view, and Auto arrange.
6. Attempt invalid cross-branch and arbitrary cycle connections and confirm they are rejected with a reason.
7. Save, reopen, and confirm both executable structure and editor layout.
8. Validate, Dry Run, Run Once, start a live run, Pause, Resume, and stop with both the button and ESC.
9. Confirm the monitor identifies the exact active block, loop, observation, action, and stop reason.

## 18. Implementation sequencing impact

The current Macro runtime and persistence work remains ahead of UI integration. Before implementing this presentation:

1. Finish and accept the paused Task 13 backend review/remediation gates.
2. Amend the existing UI integration plan so the canonical tree remains the editor source of truth and layout metadata stays non-executable.
3. Implement shared typography, preferred window sizing, and Always on top independently of runtime behavior.
4. Build and test the clean-room canvas projection and editing rules against in-memory canonical definitions.
5. Integrate the library, inspector, run controls, and semantic monitor through the existing UI-intent/controller boundary.
6. Perform the full manual acceptance path only after native live execution and package flows are complete.
