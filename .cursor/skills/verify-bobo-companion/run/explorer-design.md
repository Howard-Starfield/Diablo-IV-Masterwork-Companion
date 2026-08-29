### Components Found

**Design artifacts (precedence)**

| Artifact | Status in doc | What it claims | What shipped code follows |
|---|---|---|---|
| `docs/superpowers/specs/2026-07-12-macro-tab-design.md` | Approved; GPT-audited | Top-level Macro tab; **timeline** of nested blocks; wizard; OCR + image; IF/loops/Watch; safety; **no Goto** | **Canonical model + runtime still match this.** UI is not a timeline. |
| `docs/superpowers/plans/2026-07-12-macro-v1-implementation.md` | Task checkboxes still `[ ]` | Engine + `timeline.rs` + wizard + monitor | Plan file is stale. `timeline.rs` never exists. Engine + canvas UI shipped. |
| `docs/superpowers/specs/2026-07-13-frontend-readability-macro-canvas-design.md` | Approved; awaiting written-spec review | 900×1080; Always on top; **5-region page** (library / canvas / inspector / run controls / monitor); n8n-like **LTR** canvas projection of canonical tree | **This is the shipped UX contract.** README UAT is this spec, not canvas-first. |
| `docs/superpowers/plans/2026-07-13-frontend-readability-macro-canvas.md` | Checkboxes still `[ ]` | Shared theme, UI state, canvas projection, drawers at narrow width | Largely implemented; plan not marked done. |
| `docs/superpowers/specs/2026-07-21-macro-canvas-first-workspace-design.md` | **Approved direction; awaiting written-spec review** | Supersedes 07-13 for window size, workspace composition, navigation, connections, loop viz | **Not implemented.** No matching plan file. Spec itself says “replace the current control-heavy Macro page.” |
| `.planning/phases/13-macro-v1-runtime-library/.continue-here.md` | 2026-07-13 pause note | Task 13 paused; “no honest live UI until Windows factory” | Stale vs current `src/main.rs` live run wiring. |
| `README.md` Macro + Manual UAT | User-facing | 900×1080; drawers on small displays; pan/zoom/Fit/Auto arrange/Undo; Dry Run vs Run Live; Watch is one-shot | Matches **07-13 shipped product**, not 07-21. |

**Shipped UI composition (07-13, not 07-21)**

- Shell: `AppPage::{Enchant, Macro}` in `src/main.rs`. Shared 42px top bar: identity, Enchant/Macro tabs, Always on top, Macro saved-target text.
- Window: `APP_WIDTH=900`, `APP_HEIGHT=1080`, clamped via `preferred_window_placement`. **No maximize.**
- Macro page: `MacroPage::show` (status strip + wizard + 3-pane/drawers) + `MacroPage::show_bottom` in a **resizable 184–520px bottom dock** (default **276px**).
- Canvas: `canvas::CANVAS_HEIGHT = 430.0` exact allocation. Wrapped `editor_toolbar` sits **above** it.
- Drawers: only when `pane_mode(width) < 720` → `CollapsingHeader` **below** the canvas, not overlay/side drawers. At 900px: `ThreePaneCompact` (permanent side panes).
- Bottom run dock is a full “RUN CONTROLS” + “RUN MONITOR” panel, not a one-line dock.

**Canonical engine (wired; 07-12 still authoritative)**

`BlockKind`: Observe, Action, If, Wait, RepeatN, RepeatUntil, Continuous, WatchGroup, StopSuccess, StopError, Comment.  
Detectors: Text (`Windows.Media.Ocr`) and Image (`imageproc` NCC).  
Actions: ClickTextMatch / ClickImageMatch / ClickPoint / ClickRegion / MoveOnly.  
Run modes: `ObservationOnly`, `DryRun`, `Live`.  
Watch Group: one-shot concurrent lanes, ordered arbitration, latches.  
Explicitly **not** in the model: Goto/Tag/Jump, keyboard, regex, variables, recording, OpenCV/ONNX.

---

### Flow

**Intended (07-12 product, still the engine contract)**

1. Bind one Diablo window.  
2. Capture a text or image region; configure detector; **test without clicking**.  
3. Attach an action that can only fire from a **fresh observation token**.  
4. Wrap with IF / Repeat N / Repeat Until / Continuous / Watch Group.  
5. Validate → save immutable revision → Dry Run (zero input) / Run Once / continuous observe / Run Live.  
6. Pause / Resume / Stop / ESC. Monitor shows active block, loop, observation, action state, stop reason.

Wizard (`wizard.rs` 9 steps: Target → Region → Rule → DetectorTest → Action → Repetition → Failure → DryRun → Finish) emits the **same** `MacroDefinition.blocks`.

**Intended visual workflow (07-13, shipped)**

Read the tree left-to-right on a node canvas. Edges are a **projection**; dropping a wire runs `EditorCommand::MoveBlock` / insert. Layout (positions, pan, zoom) is non-executable `MacroCanvasLayout`.

**Intended visual workflow (07-21, not shipped)**

Maximized canvas-first: tool rail + drawers; Tab / output-`+` / drag-to-empty / double-click to add; **top-to-bottom** default with per-macro LTR switch; Repeat **body-return** wire to first body task; runtime-driven n8n-like animation; compact run dock.

**What a user actually does today**

1. Macro tab → “New Macro” / “Create starter draft” / “Guided wizard” (still a **permanent status-strip row**).  
2. Wrapped palette: `+ Observe / Action / IF / Repeat / Watch / Wait / Stop / Note`, plus Up/Down, Disable, Duplicate, conversions, loop-delete choices.  
3. Inspector edits detector fields; Action/Comment/Stop inspect as generic `"BLOCK"` with **no fields**.  
4. Run from the **bottom dock** (Validate, Dry Run Observe only, Run Once, Run Observe only, Run Live, Pause, Resume, Stop).  
5. Canvas is a **430px island** inside a **vertical ScrollArea**, competing with library, inspector, status strip, wizard, and ~276px monitor.

---

### Design claim vs code (explicit)

**Shipped (matches 07-13 / README)**

| Claim | Evidence |
|---|---|
| 900×1080 preferred, clamped to work area | `src/main.rs` `APP_WIDTH/HEIGHT`; `preferred_window_placement` |
| Always on top default Off, persisted | `AppUiState.always_on_top`; `ui_state.rs` default false |
| Shared type scale ≥12px | `src/ui_theme.rs` |
| Canvas is projection of `MacroDefinition.blocks` | `project_canvas` in `canvas_model.rs` |
| Connection drop → checked `EditorCommand`, not a second graph | `connection_command` → `MoveBlock`; LoopReturn not editable |
| Empty-space / middle / Space+drag pan; wheel/pinch zoom | `canvas.rs` `gesture_for_start`, `reduce_canvas_input` |
| Fit view, Auto arrange, Undo/Redo, Ctrl+Z/Y, F=Fit | `editor_toolbar`, `show_interactive_canvas` |
| Layout persisted separately; corrupt layout discarded | `UiStateStore` + `reconcile_layout` |
| Active node thicker stroke + `request_repaint` while a run names a block | `paint_node`; **static**, not rotating animation |
| Reveal offscreen active node without saving pan | `reveal_node` comment + `acceptance_tests.rs` |
| Drawers when narrow | `PaneMode::CanvasWithDrawers` + `CollapsingHeader` |
| Loop-return edge exists, generated, non-editable | `append_loop_group` + `is_editable_output` |
| Ports LTR (input left, outputs right); bezier along X | `input_handle` / `output_handle` / `paint_curve` |
| Auto-arrange is top-to-bottom **layers** (Y by depth) | `canvas_layout.rs` `auto_arrange` |
| Wrapped command strip + selected-node mutation row | `editor_toolbar` (~2412–2800) |

**07-21 approved but not implemented**

| Claim | Code fact |
|---|---|
| Start maximized in work area | `ViewportBuilder` uses `with_inner_size` only; no `with_maximized` |
| Canvas fills space between header and run dock; never fixed 430px | `CANVAS_HEIGHT = 430.0`; Macro body is inside `ScrollArea` |
| Library + Inspector as resizable/pinnable drawers; tool rail | Permanent `SidePanel`s ≥720px; no icon rail; no pin |
| Wizard / file mgmt in overflow menu | “Guided wizard”, Capture Target, Save are **always** on the status strip |
| Compact one-line run dock; detail expands on request | Default bottom height **276px**, max 520 |
| TB default + per-macro orientation switch | No orientation field in `MacroCanvasLayout`. Ports always LTR |
| Add via output-circle `+`, drag-to-empty palette, double-click empty, **Tab** | Drop-on-empty sets `pending_canvas_port` and tells user to click **toolbar** buttons. No Tab, no double-click, no port `+`, no palette popup |
| Shared filtered node palette | Seven `+ Kind` buttons; not one palette |
| 16px node snap; Alt disables; 60px connection magnetism | `GRID_STEP = 32.0` is **paint-only**. Node drag writes raw world coords. Hit radius `HANDLE_RADIUS*1.8` ≈ 10.8px, not 24 |
| Edge-hover Insert step | No |
| Repeat return wire to **first body task**; REPEAT vs DONE; no DONE on Continuous | `LoopReturn` `to: block.id` (**self-loop on Repeat**). Continuous still has `OutputPort::Next` (DONE). Group label is always `"LOOP BODY"`, not `Repeat N` / until / continuous |
| n8n-like rotating active/waiting borders, traveling edge, loop-yield pulse, Reduce motion | Active = thicker category-colored stroke. `request_repaint()` only while `active_block.is_some()`. No Reduce motion |
| Floating Fit / 100% / + / − | Toolbar Fit / Reset zoom; **no zoom %**; Fit from toolbar uses **600** height, F-key uses **430** |
| Pointer open/closed hand | No `CursorIcon` |
| Persist drawer widths, orientation, pan, zoom | `library_width` / `inspector_width` stored and copied on auto-arrange, **never applied** to `SidePanel` (hardcoded 175/250) |
| Empty canvas “Add first step” | `"Create or select a macro to inspect its canonical blocks."` |
| Keyboard: Tab add, connect via accessible command | Only Ctrl+Z/Y and F |

**README vs 07-21:** README still documents 900×1080 and 07-13 gestures. Manual UAT does **not** include maximize, Tab, orientation, loop-back-to-first-task, drawers as default, or animation.

**Stale copy in shipped UI:** empty library still says “The guided creator arrives in the next phase.” while Guided wizard is already on the status strip (`library.rs` ~120).

---

### Backend already supports, UI under-exposes

Engine/editor **have** these; inspector/canvas do not present them as first-class authoring:

- **Full action model** (`ClickPoint`, `ClickRegion`, `MoveOnly`, left/right). Palette `+ Action` only inserts left-click on the selected observation. Inspector for Action/Comment/Stop is `"BLOCK"` with **no editors**. Mouse button / target only via toolbar **conversion** buttons or wizard.
- **Comment text** — insertable as “+ Note”; inspector cannot edit the string.
- **OCR `language`** on `TextRule` — inspector never shows it (always `"en-US"` in starters).
- **`transparent_mask`** on `ImageRule` — model + matcher; inspector has no mask capture/edit.
- **`TimeoutOutcome::{StopError, Continue, RunBody}`** — engine + canvas `ON TIMEOUT` groups if present. Inspector only edits **timeout duration**; cycling observe mode hardcodes `Continue` on CheckNow→Wait. No UI to pick timeout body vs stop vs continue (wizard/failure step is the main authoring path).
- **Watch Group `timeout_outcome`** — FlowEdit is timeout_ms + cooldown only. Lane add/reorder/enable is **toolbar**, not inspector.
- **`SafetyPolicy`** (`max_runtime_ms`, `max_clicks`, retries, pacing, `focus_loss`) — on every definition; **no inspector**.
- **Live target connection** — runtime binds a concrete HWND. Status strip hardcodes `"WINDOW" / "Not connected"`, `"FOREGROUND" / "Unknown"`, `"GEOMETRY" / "Snapshot only"`.
- **Run history** — `MacroStore::list_run_history` + `MacroIntent::ShowHistory` / `DeleteHistory` / `CleanupOrphans` handled in `main.rs`. **No button enqueues them.** README UAT still lists `history`.
- **Points list / region catalog** — recapture exists; no manager UI for all regions/points.
- **Watch nested Repeat in lane bodies** — allowed by validation; canvas groups them; authoring is awkward (palette insert into selected lane).
- **Image verification / negative corpus** — wired for import re-verification banner; not a normal inspector “benchmark profiles” flow (07-12 §8.2 deferred-looking in UI).

**Under-exposed relative to “advanced macros” the engine already has:** IF/THEN/ELSE, Repeat N / Until / Continuous, Wait, Watch Group, OCR Check/Wait/Absent, image template match, Dry Run vs Live. They are **insertable**, but the canvas is LTR cards + a dense toolbar, not a readable TB flowchart, and several knobs only exist as cycle-buttons or conversions.

---

### Product claims still lacking **engine** support

From 07-12 §18 (still the v1 exclusion list) and popular mouse-macro products:

| Popular / “advanced macro” expectation | Engine |
|---|---|
| Keyboard typing / hotkeys / modifiers | **No** (`StopReason::UnsupportedBlock` exists; no key block) |
| Record mouse path / replay | **No** (explicitly not a recorder) |
| Pixel-color wait, spiral search | Image template NCC only; no color-pick block |
| Variables, counters, expressions | **No** |
| Regex OCR | **No** |
| Arbitrary Goto / labels | **Rejected by design** (owned loops only) |
| Nested Watch Groups, parallel lane bodies | Invalid in v1 |
| Unattended schedule / global hotkey start | **No** |
| Multi-macro concurrent runs | One `MacroRuntime` |
| OpenCV / ONNX / rotation-invariant | Out of v1 |
| Focus steal, crash resume | Forbidden |

**Already in engine, matching the product goal of if/else, loops, waits, OCR, image recognition:** those block kinds and detectors are real. The gap to “like popular mouse macros” is **recorder + keyboard + simpler UX**, not missing IF/loop in the runtime.

---

### Why Macro feels out of place next to Enchant

1. **Different products in one tab strip.** Enchant is a linear Occultist checklist (window → regions → target affix → Start/Stop). Macro is a second app: library, graph editor, inspector, wizard, four run modes, package import. 07-21’s opening sentence is the diagnosis: the Macro page is **control-heavy**.
2. **Chrome sized for Macro, used by Enchant.** Window jumped from Enchant’s old ~600×760 to **900×1080** so the canvas could exist. Enchant still uses a **112px** action bar; Macro takes **276px** by default. Same shell, unrelated density.
3. **Live connection vs snapshot copy.** Enchant talks to a captured window. Macro header shows “Saved target: …” and the status strip **fakes** connection/foreground as Unknown / Not connected.
4. **Canvas is not the page.** 430px canvas + wrapping `+ Observe…` strip + 3 panes + 276px monitor + optional wizard, all inside a **scroll view**. Next to Enchant’s single-column form this reads as an unfinished IDE.
5. **Vocabulary collision.** Enchant: “Set Enchant button”, “OCR test”, Start. Macro: revision hashes, Draft/Ready, Observe/Decide/Act chips, Watch Group, Dry Run vs Run Live. Same orange theme, different job.
6. **07-21 was supposed to fix the mismatch** (maximized canvas, drawers, compact dock) and was never built. README still sells the 07-13 compromise.

---

### README Manual UAT (what it actually tests)

`README.md` lines 45–62. Path is **07-13**:

`launch → size/Always on top persist → Enchant regression → Macro create → bind target → Observe → IF → THEN → ELSE → Act → Continuous Loop → pan/zoom/move/connectors/Fit/Auto arrange/Undo/Redo → save/reopen layout → inspector recapture/test → Validate → Dry Run → Run once → Run Live → Pause/Resume/Stop/ESC → history → export/delete/import → local image re-verification`

**In UAT, not in 07-21:** maximize, Tab, orientation, loop-back-to-first-task, tool rail, animation.

**In UAT, weakly/unwired in UI:** `history` (`ShowHistory` has no control). Layout persist is real. Image re-verification banner is real.

---

### Files Read

- `docs/superpowers/specs/2026-07-12-macro-tab-design.md`
- `docs/superpowers/specs/2026-07-13-frontend-readability-macro-canvas-design.md`
- `docs/superpowers/specs/2026-07-21-macro-canvas-first-workspace-design.md`
- `docs/superpowers/plans/2026-07-12-macro-v1-implementation.md` (head + file map)
- `docs/superpowers/plans/2026-07-13-frontend-readability-macro-canvas.md` (head)
- `README.md`
- `.planning/phases/13-macro-v1-runtime-library/.continue-here.md`
- `.cursor/skills/verify-bobo-companion/features/macro-canvas.md`
- `src/macro_ui/{mod.rs, canvas.rs, canvas_model.rs, canvas_layout.rs, inspector.rs, editor.rs, library.rs, monitor.rs, wizard.rs, history.rs, acceptance_tests.rs}` (targeted)
- `src/engine/macro_engine/{mod.rs, model.rs, runtime.rs}` (surface)
- `src/ui_state.rs`, `src/ui_theme.rs`, `src/main.rs` (shell, window, run intents)
- `src/engine/platform/windows_impl.rs` (`preferred_window_placement`)

---

### Boundaries

- Did **not** audit OCR hot path, image matcher clustering, Watch arbitration internals, or live `ActionCommitter` beyond confirming they exist as engine modules.
- Did **not** run the app or UAT; claims are from source + specs.
- Other explorers likely own runtime/safety/persistence; this slice is **design vs UI/shell gap**.

---

### Non-Obvious Things

- **07-21 explicitly describes today’s Repeat viz as the thing to replace:** “This replaces the current projected self-loop on the Repeat node.” Code still does `LoopReturn → block.id`.
- **Layout is TB; wires are LTR.** Auto-arrange stacks layers on Y; ports and cubic beziers are left/right. Orientation switch would be a real geometry rewrite, not a flag.
- **`library_width` / `inspector_width` are a lie in the persistence story:** saved, sanitized, copied on arrange; SidePanels ignore them.
- **Fit view size mismatch:** toolbar Fit uses height `600.0`; keyboard F uses `CANVAS_HEIGHT` (430).
- **Grid is cosmetic.** 32px painted grid ≠ 16px snap.
- **Empty-library copy contradicts the wizard.**
- **History intents are dead UI.** Engine journal exists; README still asks testers to open history.
- **Plans and `.continue-here.md` are not a source of truth** for what shipped. Code is ahead of those files; 07-21 is ahead of the code.
- **Animation `request_repaint` while active** can spin the egui loop during a run without implementing 07-21’s 1.5s/4.5s cycles or stop-clears-motion contract beyond “no active_block → no extra repaint.”
- **v1 is not TinyTask.** Advanced control flow is structured and observation-gated. “Like popular mouse macros” in the product sense means OCR/image + IF/loop/wait, **not** record/playback.

---

### Open Questions

- Is **07-21** now the accepted UX bar, or is README/07-13 still what UAT must pass? Spec says approved direction but “awaiting written-spec review”; there is no implementation plan.
- Should Enchant inherit maximized chrome, or only Macro? 07-21 §4.1 puts both tabs on a compact header of a maximized window.
- Are status-strip `"Not connected"` / `"Unknown"` placeholders waiting on a live `TargetGuard` projection, or intentional “snapshot only” until a later phase?
- Should `ShowHistory` get a real panel, or should README drop `history` from UAT?
- For the “popular mouse macro” goal: is keyboard/recording still deferred (07-12 §18), or has the product goal moved? Engine has no types for it.
- Tab-to-add vs egui text-field focus: 07-21 wants Tab; inspector/library already own keyboard. Unspecified in code.