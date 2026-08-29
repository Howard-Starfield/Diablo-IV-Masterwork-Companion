### Components Found

**Page entry (`src/main.rs`)**
- `AppPage::{Enchant, Macro}` at 519–522. Default page is Enchant (`NativeApp::new`, 622).
- Top chrome: `selectable_label` Enchant/Macro (1983–1994). Enchant shows `status_pill`; Macro shows `Saved target:` + `Draft only` / `Saved rN` (2019–2043).
- Bottom chrome is page-specific (`bottom_surface`, 530–534): Enchant `exact_height(112)` action bar; Macro resizable `184–520` (default `276`) panel calling `MacroPage::show_bottom` (2057–2065). Constants: `MacroPage::BOTTOM_*` in `src/macro_ui/mod.rs` 1282–1284.
- Central body: Enchant `NativeApp::content`; Macro hydrates layout then `MacroPage::show` (2081–2086). After paint, Macro-only: `dispatch_macro_intents`, `take_wizard_request`, `take_editor_authoring_request` (2091–2100).
- Window default `APP_WIDTH=900`, `APP_HEIGHT=1080` (`main.rs` 76–77). `pane_mode(900.0)` is `ThreePaneCompact` (test 3963–3965).

**`MacroPageState` (`src/macro_ui/mod.rs` 309–349)**
Owns: `EditorDraft`, `SavedMacroIdentity`, library rows/search/rename/package path, `image_package_reverification`, `MacroCanvasLayout` + `UiEditHistory`, `MacroIntentQueue` (cap 64, Stop retained under pressure, 257–284), canvas selection/`pending_canvas_port`, `pending_inspector_intent`, `pending_conversion`, wizard + authoring request IDs, editor authoring + image-negative samples.
Does **not** own `MacroStore`, `MacroController`, HWND, or capture. Comments at 183–184, 306–307, 1341–1342.

**Layout composition (`MacroPage::show`, 1286–1338)**
Vertical stack:
1. `image_package_reverification` banner (1365–1407) if import-in-progress
2. `wizard::show` overlay (`GUIDED MACRO WIZARD`, 9 steps) if `state.wizard` is Some
3. `status_strip` (1575–1694)
4. `workspace` (1934–2030): three panes or canvas+drawers

`pane_mode` (1215–1222): `>=1100` ThreePane, `>=720` ThreePaneCompact, else `CanvasWithDrawers` (library/inspector become collapsed headers 2018–2027). Spec originally wanted a **timeline**; `timeline.rs` was deleted; canvas is the center.

**Status strip (1575–1694)** — not Enchant’s “Occultist Affix Reroll” header.
Facts: TARGET (draft title or “No target selected”), **WINDOW `"Not connected"` hardcoded 1599**, **FOREGROUND `"Unknown"` hardcoded 1600**, DISPLAY (`WxH | DPI` or `--`), **GEOMETRY `"Snapshot only"` hardcoded 1618**, REVISION `draft | saved | running`, VALIDATION.
Buttons: Create starter draft / Guided wizard / Close wizard / Capture Target|Retarget / Save / Open first problem.
Run buttons are **not** here (design spec §3 put Validate/Dry Run/Run in this strip; code put them in the bottom bar).

**Library (`library_pane` 2033–2137 + `library.rs`)**
Search, `New Macro` → `create_starter_draft` (2139–2145), `library::show` rows emit `MacroIntent::Select`. Collapsed “Manage selected macro”: Enabled, rename/duplicate, import/export path text field, confirm-delete.
Empty state copy (`library.rs` 107–124): **“No macros yet” / “The guided creator arrives in the next phase.”** — wizard already exists.

**Canvas (`workspace` 1994–2003, `canvas.rs`, `canvas_model.rs`, `canvas_layout.rs`)**
- Section title `EVENT CANVAS`. Toolbar then `canvas::show` with **fixed height `CANVAS_HEIGHT = 430`** (`canvas.rs` 14, 238).
- Empty: `"Create or select a macro to inspect its canonical blocks."` (`mod.rs` 2154–2156).
- Nodes 280×88 (`canvas_layout.rs` 11–12). Categories Observe/Decide/Act/Repeat (`ui_theme.rs` 32–73).
- Gestures: pan (empty/space/middle), wheel/pinch zoom, node drag, connector drag. Shortcuts Cmd+Z/Y, F=fit (`mod.rs` 2158–2180). Toolbar Fit uses height **600** (2449–2450) vs F using **430**.
- Layout persisted in `UiStateStore.macro_layouts` (`mod.rs` 523–545, `ui_state.rs` 26–33). **`library_width`/`inspector_width` are stored (defaults 220/320) but `workspace` SidePanels use hardcoded 175/250 compact/full (1967–1983) — widths are not applied.** README claims drawer-width persistence.

**Inspector (`inspector.rs`)**
Empty: `"No block selected"` + `"Select a canonical canvas block."` (12, 511–517).
- Text/Image detectors: Expected/Region/polling/timeout, Advanced collapsed, Test OCR/Image, Recapture, negatives.
- If / RepeatUntil with a condition project as **TEXT/IMAGE DETECTOR** plus `flow_fields` `"Branches" = "THEN / ELSE"` (386–392, 157–193) — not a dedicated If editor.
- Wait / RepeatN / Watch / Continuous → `FlowInspector`. Continuous has **no** `FlowEdit` (370–373). Action/Stop/Comment fall through `"BLOCK"` with **no fields** (375–376).
- Comment text is not editable. SafetyPolicy (max clicks, etc.) is never shown.

**Monitor (`show_bottom` 1343–1361, `monitor.rs` 541–607)**
Below 720px: run controls stacked over monitor; else 2 columns. Grid of Active block / Branch / Loop / Iteration / Candidates / scores / Action state / observation / Run mode / Stop reason. Default empty values `"--"`. `"No stop reason reported"` when idle (589–595).

**Run chrome (`run_controls` 1697–1798)**
Always shows eight buttons: Validate, Dry Run (Observe only), Run Once, Run (Observe only), Run Live, Pause, Resume, Stop.
Availability (`run_control_availability` 1240–1275): starts require `selected_saved` + `enabled` + `Idle`. Pause=Running, Resume=Paused, Stop=Running|Paused|Stopping.
`primary_label`/`primary_detail` are computed (1253–1258) and **never rendered** (only tested, 3973–3993).

**Wizard (`wizard.rs`)**
9 steps: Target, Region, Rule, DetectorTest, Action, Repetition, Failure, DryRun, Finish (`WizardStep::ORDER` 29–39).
DryRun UI is a **checkbox** “I reviewed…” (973–985), label “Review only: no live runtime starts and zero input is injected.” Spec §4 step 8 said “Perform a dry run.”
Finish emits canonical `MacroDefinition` with If-gated click, optional RepeatN / RepeatUntil+Wait / Continuous (`finish` 270–455). ID always `"wizard-macro"` (461).

**Shared chrome vs Enchant**
- Shared: title “BoBo Companion”, tab switcher, Always on top, `ui_theme::apply`, 12px central margin, `Color32::from_rgb(9,11,13)` fill.
- Enchant (`content` 2106–2118): PAGE_TITLE 22 “Occultist Affix Reroll”, numbered 1–N `step_button`s 138×38, live OCR result, 112px Start/Stop. Mouse-movement recording is Enchant-only (`begin_mouse_movement_recording` 1484–1504).
- Macro: IDE panes, 16px monospace section labels, 430px graph, 8 run modes, wizard group, debug-ish feedback.

---

### Flow

**Authoring If/Else/Repeat/Observe/Action/Wait**

Palette (`editor_toolbar` 2487–2520): `+ Observe`, `+ Action`, `+ IF`, `+ Repeat`, `+ Watch`, `+ Wait`, `+ Stop`, plus `+ Note`. There is **no `+ Else`**. ELSE is an owned container of If (`canvas_model.rs` 349–368; spec §5.3).

`palette_command_for_selection` (`mod.rs` 3521–3641):
| Button | Inserts | Preconditions |
|---|---|---|
| Observe | `BlockKind::Observe` using **first** `text_rules` else first `image_rules` | Else error “Add a text or image rule…” |
| Action | `ClickTextMatch`/`ClickImageMatch` Left | Needs a matched Observe source |
| IF | `If { then_body:[], else_body:[] }` copying selected Observe condition | Needs an observation |
| Repeat | **`RepeatN { count: 2, body: [] }` only** | — |
| Watch | `WatchGroup` with **one** lane | Needs observation; **no Add Lane command in editor.rs** |
| Wait | `Wait { duration_ms: 250 }` | — |
| Stop | `StopSuccess` only | — |

Insertion target (`insertion_target` 3644–3711): into IfThen / LoopBody / first Watch lane if a container is selected; else next sibling; else root. Connector drop sets `pending_canvas_port`; next palette insert is retargeted (`retarget_insert_command` 2803–2813). Drop on background → `OpenAddStep` (`canvas.rs` 199–210) — **no picker widget**, only feedback “Choose a block below…” (`mod.rs` 2212–2216).

Conversions on selected block (`conversion_choices` 3046–3207): Observe CheckNow/Wait true/false; click Left/Right; RepeatN ↔ RepeatUntil. Replacements (`replacement_choices` 3388–3433): swap detector/action/loop/wait/note/stop. Loop replacement is RepeatN, not Continuous.

**Missing from palette/conversion vs model (`model.rs` 206–308) and spec §5:** `Continuous` (wizard only), `MoveOnly` (canvas label exists `canvas_model.rs` 740, no `ConversionTarget`), extra text/image rules (no insert-rule UI; Observe reuses first rule), extra Watch lanes, `StopError`, Comment text edit, SafetyPolicy.

**Canvas ↔ canonical tree**
`project_canvas` walks `definition.blocks` and **derives** nodes/groups/edges (`canvas_model.rs` 127–140, 305–336). Sequence edges = sibling Next ports. IfThen/IfElse/LoopBody/WatchLane/Timeout are groups. `LoopReturn` is visual-only, `editable: false` (446–451); connecting it is `InvalidPort` (211).
Dropping a connector on an existing node → `connection_command` → `EditorCommand::MoveBlock` (215–234). Layout (`node_positions`, pan, zoom) is **not** in the executable draft (`mod.rs` 324–325, `canvas.rs` 227–228).

**Intents → engine**
UI `enqueue_intent` → `NativeApp::dispatch_macro_intents` (`main.rs` 1172–1407).
`macro_run_request` (84–102):
- `DryRun` → `ControllerRunRequest::once(RunMode::DryRun)`
- `RunOnce` → `once(ObservationOnly)`
- `Run` → `continuous(ObservationOnly)`
- `RunLive` → `continuous(Live)`
`start_saved_macro` (823–871) refuses unsaved drafts: requires store revision+hash match **and** a live `CapturedTargetBinding` for the draft session — else “Capture the exact saved target before running…”. Then `MacroController::start_saved`. Pause/Resume/Stop call controller methods (1189–1205). Validate is local `EditorCommand::MarkValidated` (1183). Save goes through `store.save_validated` (787–820).
`ShowHistory` / `CleanupOrphans` are handled in dispatch (1296–1404) but **never enqueued from any widget**.

Authoring (capture/OCR/image test) is request/result, not MacroIntent: `EditorAuthoringKind` / `WizardAuthoringKind` spawned on a thread (`begin_editor_authoring` 1447–1481), results applied via `apply_editor_authoring_result` / `apply_wizard_result`. Wizard blocks inspector edits (`handle_inspector_intent` 2291–2293).

---

### Files Read

- `src/main.rs` (AppPage, NativeApp chrome, `macro_run_request`, dispatch/start/sync/authoring, Enchant `content`/`header`/`bottom_bar`)
- `src/macro_ui/mod.rs` (state, show, workspace, toolbar, palette, conversions, intents, pane_mode)
- `src/macro_ui/canvas.rs`, `canvas_model.rs`, `canvas_layout.rs`
- `src/macro_ui/inspector.rs`, `library.rs`, `monitor.rs`, `wizard.rs`, `editor.rs` (commands/families)
- `src/engine/macro_engine/model.rs` (BlockKind/Action)
- `src/engine/macro_engine/runtime.rs` (RunMode, ControllerRunRequest, MacroController pause/resume)
- `src/ui_theme.rs`, `src/ui_state.rs`
- `docs/superpowers/specs/2026-07-12-macro-tab-design.md` §§1–5
- `README.md` Macro workspace / UAT
- Grep-only: `library_width` unused in workspace; no InsertLane; `ShowHistory` unused in UI

---

### Boundaries

- **Enchant vs Macro:** separate `AppPage`, separate bottom surface, separate runtime (`BotState` + `EscStopSignal` vs `MacroController` + intent queue). Shared window, theme, Always-on-top, capture primitives. Enchant mouse-path recording is not Macro authoring.
- **UI vs engine:** widgets emit `MacroIntent` / authoring requests only. NativeApp owns store/controller/Windows. Runs always use **saved** identity, never in-memory draft (`SavedMacroIdentity` comment 111–113).
- **Canvas vs definition:** projection + layout store. Connections cannot invent edges; they become Move/Insert into `ContainerPath`.
- **Wizard vs editor:** wizard writes the same `BlockKind` tree; opening wizard disables mutations (`editor_mutations_allowed` 751–759). Finish replaces draft and **clears** `selected_saved` (1501–1503) — unsaved.
- **Product goal vs popular mouse macros (spec §1–2, §5.4):** this is a **validated observation→explicit click** tree (OCR + `imageproc` templates), not TinyTask-style record/playback, not keyboard macros, not pixel-color search, no Goto/labels. “Clicker-style playback” is structural loops, not recorded paths.

---

### Non-Obvious Things

1. **Density / “out of place” vs Enchant:** Enchant is one vertical checklist + one Start/Stop bar. Macro at the same 900×1080 window is compact 150–230px side panes + 430px canvas **inside** a page `ScrollArea` (`main.rs` 2076) **plus** a 276px default bottom panel of 8 wrapped buttons and a 12-field monitor. Toolbar is 3+ wrapped rows (undo/arrange/palette/up-down/delete/conversions/replacements). Wizard group sits **on top of** that workspace. Status strip duplicates library “New” and still shows dead WINDOW/FOREGROUND.
2. **Stale / debug copy:** library empty-state “guided creator arrives in the next phase”; `Pending observation intent: {intent:?}` Debug dump (2424–2428); `{:?}` on `RunStatus`/`RunMode` in monitor (557, 667–668).
3. **Wizard Dry Run is not Dry Run.** Checkbox + fingerprint, no `MacroIntent::DryRun`.
4. **Run Live still needs a session-bound recapture** even after save (`start_saved_macro` 844–852).
5. **If is how wizard “gates” clicks** (`wizard.rs` 357–373 comment): else_body left empty. Palette IF is the same pattern. Users looking for a separate Else block will not find one.
6. **RepeatUntil vs Wait:** Observe conversions “Wait true/false” change `ObserveMode` (poll until match). Palette Wait is a **fixed delay**. RepeatUntil is a conversion of RepeatN, not a palette item.
7. **Continuous Loop and Watch extra lanes** exist in the engine/canvas projection but cannot be created from the canvas palette; UAT (`README.md` 52) lists “Continuous Loop” as if it were a canvas add.
8. **History/export UAT vs UI:** export/import exist as path text fields; `ShowHistory` has no button (feedback would only be a count string, 1296–1304).
9. **Fit-view height mismatch** 600 vs 430.
10. **Spec vs shipped chrome:** timeline→canvas; run controls moved to bottom; status-strip live window/foreground never wired; persisted pane widths unused.

**Wired today (frontend):** library CRUD intents, starter + wizard, canvas projection/gestures/undo, If then/else **ports**, RepeatN + convert to RepeatUntil, Wait, Observe/Action (match-click), Watch (one lane), StopSuccess, OCR/image inspector test/recapture, validate/save, four start modes + pause/resume/stop, monitor projection, image-package re-verify banner.

**Still missing / stubbed (frontend):** live WINDOW/FOREGROUND/GEOMETRY; wizard actual dry run; Else as its own insert (by design, but easy to misread); extra detector rules; extra Watch lanes; Continuous from palette; MoveOnly; StopError; comment/safety editors; history/orphan UI; pane-width persistence; connector “add step” picker; unused `primary_label` chrome.

---

### Open Questions

- Are `library_width`/`inspector_width` leftover from the readability plan, or should SidePanels bind them?
- Should `OpenAddStep` spawn a family picker using `allowed` (`canvas.rs` 204–207), or is the toolbar-retarget path the intended UX?
- Is Continuous supposed to be a palette/conversion target (README UAT says yes; palette says no)?
- Should Watch “Add lane” exist, or is one-lane Watch the v1 product?
- WINDOW/FOREGROUND: is live connection state owned by NativeApp and just never projected, or never implemented?
- Wizard DryRun: intentional review gate, or unfinished engine hook?
- `ShowHistory`: dead API for a later panel, or omitted from Manage?
- Nested `ScrollArea` around SidePanels+canvas+bottom: known layout bug at 900×1080, or accepted?