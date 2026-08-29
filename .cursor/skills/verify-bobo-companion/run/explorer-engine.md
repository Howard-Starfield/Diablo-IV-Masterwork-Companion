### Components Found

**Canonical definition (`MACRO_SCHEMA_VERSION = 1`)** — `MacroDefinition` in `src/engine/macro_engine/model.rs` (L24–36): `TargetProfile`, named `regions`/`points`, `text_rules`, `image_rules`, nested `blocks`, `SafetyPolicy`.

**Block tree (`Block` + `BlockKind`, model.rs L198–243)** — matches the v1 spec (`docs/superpowers/specs/2026-07-12-macro-tab-design.md` §5):

| Kind | Role |
|---|---|
| `Observe { condition }` | Detector poll; stores an `ObservationToken` under `condition.source_block_id` |
| `Action { action }` | Mouse only: `ClickTextMatch`, `ClickImageMatch`, `ClickPoint`, `ClickRegion`, `MoveOnly` |
| `If { condition, then_body, else_body }` | One evaluation per entry (`if_once_decision`) |
| `Wait { duration_ms }` | Cooperative sleep (1–10 ms slices) |
| `RepeatN { count, body }` | Count-bounded; N=0 skips; 1 ms yield per iteration |
| `RepeatUntil { condition, max_iterations, body }` | Pre-check (`evaluate_repeat_until_before_body`); `Limit::Unlimited` allowed |
| `Continuous { body }` | Infinite until stop; validation forbids unpaced bodies |
| `WatchGroup { group }` | Concurrent passive lanes; one winner body; cooldown |
| `StopSuccess` / `StopError { message }` | Macro-wide terminate |
| `Comment { text }` | No-op |

There is **no** keyboard, hotkey, drag, wheel, pixel-color, variable, goto/label, or record/replay block.

**Conditions** — `Condition` (Observe/If/RepeatUntil) has `ObserveMode`: `CheckNow`, `WaitForTrue { timeout_ms, timeout_outcome }`, `WaitForFalse {…}` (model.rs L247–285). `PassiveCondition` (Watch lanes) has **no** wait/timeout (`deny_unknown_fields`, tests at L403–432). Timeouts: `TimeoutOutcome::{StopError, Continue, RunBody}`.

**Text OCR rules (`TextRule`, L63–78)** — region, Windows OCR language, `PreprocessProfile::{Original, Grayscale, HighContrast, SmallText}`, expected string, `TextMatchMode::{Exact, Contains, Fuzzy, Absent}`, threshold, case, `allow_cross_line`, `MatchSelectionPolicy`, `poll_interval_ms`, **`timeout_ms`**, `stable_frames`.

**Image rules (`ImageRule`, L81–96)** — region, template `AssetRef` + optional mask, threshold, `scales_percent`, stability/drift/runner-up margin, **required** `ImageRuleVerificationArtifact` (`IMAGE_RULE_VERIFICATION_VERSION = 2`), same poll/timeout/policy fields.

**Safety (`SafetyPolicy`, L144–151)** — `max_runtime_ms`, `max_clicks`, `max_observation_retries`, `max_observations_per_second` (>0), `minimum_click_interval_ms` (>0), `focus_loss: Pause|Stop`. `Limit<T> = Finite|Unlimited`. Unlimited never disables cancellation, pacing, or event-queue bounds.

**Runtime** — `MacroRuntime` (`runtime.rs` L2803–3043): capture + `ConditionDetector` + `Clock`; optional live via `with_live_input`. `RunMode::{ObservationOnly, DryRun, Live}` (L692–696). `RunEvent` tagged stream (L2092–2204). `MacroController` (L3356+) owns a worker thread, journals, and Once vs Continuous extent.

**Detectors** — `ConditionDetector` trait (`observation.rs` L157–170). `ConditionDetectorRouter` splits Text vs Image. Production: `TextDetector` + `WindowsTextRecognizer` (`windows_ocr.rs`, `Windows.Media.Ocr`), `ImageDetector` + `ImageMatcher` (imageproc NCC / custom masked NCC).

**Watch** — `WatchGroupRunner` + `arbitrate_candidates` (`watch_group.rs`): 25 ms window (`ARBITRATION_WINDOW_MS`), winner = lowest persisted lane index among candidates in window. Global `WatchDetectorPool` in `runtime.rs`: **1 text + 2 image workers**, 256 resident + 256 delayed lanes. `DetectorScheduler`/`CaptureCoordinator` in `watch_group.rs` are **unit-test/scheduler prototypes**; production Watch uses `CapturedCycle` + the pool.

**Persistence** — `MacroStore` (`persistence.rs` L954+): `{root}/macro_data/{definitions,assets,runs,asset_identities.json}`. Immutable revision JSON + SHA sidecar; `save_validated` compiles before publish. Image packages **cannot** import until local re-verification (`reject_portable_image_rules`, L2745–2756).

**Native composition** — `build_windows_macro_runtime` (`windows_impl.rs` L380–403): captured HWND binding must match saved `TargetProfile` (path/class/title/client/DPI); xcap capture + `WindowsTargetGuard` + `WindowsInputSink` + ESC emergency watcher.

---

### Flow

**1. Author → persist → compile**

- UI draft → `validate_macro` (`validate.rs` L33) → `MacroStore::save_validated` (persistence.rs L1020–1043).
- `CompiledMacro::compile` (`runtime.rs` L2309–2365):
  1. `validate_macro` (structural IDs, rule/region refs, source bindings, image verification fingerprint, busy-loop, Watch limits).
  2. Canonical JSON SHA must equal `SavedRevision.definition_hash`.
  3. Pinned assets must be **exactly** the referenced image templates/masks; bytes hash-checked.
  4. Decode templates/masks and `image_verification::validate_decoded_rule`.

Runs **only** use `store.acquire_current_for_run` / `load_current`; drafts do not execute.

**2. App start of a run** (`main.rs` L84–103, L823–871)

```
MacroIntent → ControllerRunRequest
  DryRun     → once(DryRun)
  RunOnce    → once(ObservationOnly)
  Run        → continuous(ObservationOnly)   // "Run Observe only"
  RunLive    → continuous(Live)
```

`start_saved_macro` always builds a **live-capable** `WindowsMacroRuntimeBundle`, then `MacroController::start_saved`. Live input is only used if `mode == Live`.

`ControllerRunExtent::Once` rejects reachable `Continuous` (`blocks_reach_continuous`, runtime.rs L3416–3419, L3704+).

**3. Execute** (`MacroRuntime::run_acquired` L2922–3006)

- Compile → `EventEmitter::run_started` + `StatusChanged(Running)`.
- Live: construct `ActionCommitter` (click budget + attempt ledger).
- `RunExecution::execute_blocks` walks enabled blocks.

Per block (`execute_block` L3923–4013):

- **Observe / If / RepeatUntil** → `evaluate_condition` (L4775–4886): paced by `max_observations_per_second`; polls at `rule.poll_interval_ms`; wait modes use **`ObserveMode.timeout_ms`**, not `TextRule`/`ImageRule.timeout_ms`; success → token in `observations[source_block_id]`.
- **If** → `if_once_decision` then execute then/else.
- **RepeatN / RepeatUntil / Continuous** → body then `LoopYielded` then `cooperative_wait(1)`.
- **Wait** → `cooperative_wait(duration_ms)`.
- **WatchGroup** → `execute_watch_group` (L4020–4652): union-region `CapturedCycle`, submit per-lane jobs, latch on rising-edge match, 25 ms arbitration, validate capture freshness, run winner `then_body`, then `cooldown_ms`. Generation change (pause/resume/stop) discards candidates (`ArbitrationDiscardReason::GenerationChanged`).
- **Action** → `plan_action` (L5026–5110): always emits `ActionPlanned { state: Planned }`. If `live.is_some()`, `dispatch_live_action`. Else (DryRun **and** ObservationOnly) no SendInput. Then wait `minimum_click_interval_ms` even for non-clicks.
- **Stop\*** → `StopReason::StopSuccess` / `StopError`.

**4. DryRun vs ObservationOnly vs Live**

- Both DryRun and ObservationOnly set `live = None` (`run_acquired` L2936–2954). Both **plan clicks** and both enforce the simulated `max_clicks` counter (`non_authoritative_planned_clicks`). Tests at runtime.rs L7774–7817 treat them the same for click planning.
- Difference is the `RunEvent::RunStarted.mode` tag + UI labels. Comment on `ControllerRunRequest` (L3054–3056) that Live is “intentionally absent” is **stale**; Live exists.
- Live: `authorize_action` (L2372–2486) ties destination to compiled block + current token/frame HWND/geometry/DPI; `ActionCommitter::prepare`/`commit_observed` is the **only** SendInput gate (`LiveActionInput` L846–861). `MoveOnly` → `StopReason::UnsupportedBlock` (L5119–5131). After a real click, `invalidate_after_side_effect` bumps `side_effect_epoch` and clears tokens.

**5. Events**

`RunStarted → StatusChanged → BlockEntered → ObservationCompleted/ConditionEvaluated/ObservationProgress → ActionPlanned[/Blocked/StateChanged] → LoopYielded → ArbitrationCompleted/PollingDelayed → Error → StatusChanged(Stopping) → RunStopped`.

Progress events coalesce; capacity exhaustion → `SafetyLimit`. `ControllerEventSink` journals via `MacroStore::open_journal` (8 MiB / 128 runs in controller, runtime.rs L3438). Semantic projection keeps one latest event per category.

**6. Detectors (hot path)**

- Sequential Observe: detector `capture.capture_frame(region)`.
- Text (`text.rs` `observe_text` L812–935): preprocess → OCR words → `match_text` → stability count ≥ `stable_frames`. Absent → no geometry (validation forbids click).
- Image (`image_match.rs` `observe_image` L484–671): grayscale NCC (`MatchTemplateMethod::CrossCorrelationNormalized`) or masked cosine; multi-scale; local maxima ≥ threshold; cluster; reject if runner-up margin too small; stability + `maximum_center_drift_px`. Client size/DPI must match `TargetProfile` or the detector **bails** (technical failure).
- Stale generation/epoch → unmatched (watermark), not a crash.

**7. Validation highlights** (`validate.rs`)

- Unique block/lane IDs globally (`timeline.duplicate_identity`).
- Consumers must bind the **same rule** as their source (`source_rule_mismatch`); enabled consumer cannot use disabled source.
- Image rules must have a fingerprint-matching verification artifact (negative corpus SHA, sample count > 0, `threshold - best_negative ≥ margin`, variance ≥ 16).
- Watch: ≥1 enabled lane, ≤256 lanes (disabled count), no nesting, no `Continuous` in lane bodies, non-blank lane IDs.
- Continuous must contain Observe/If/RepeatUntil/Watch/Stop/click/Wait>0.

---

### Files Read

- `src/engine/macro_engine/{mod,model,semantics,validate,observation,runtime,persistence,text,image_match,image_verification,watch_group}.rs` (runtime sampled by section; ~13k lines)
- `src/engine/platform/{windows_impl.rs,windows_ocr.rs}` (bundle + OCR)
- `src/engine/mod.rs`, `src/main.rs` (run request mapping + `start_saved_macro`)
- `src/macro_ui/mod.rs` (run buttons / `MacroRunIntent`)
- `docs/superpowers/specs/2026-07-12-macro-tab-design.md`
- `.planning/phases/13-macro-v1-runtime-library/.continue-here.md`

---

### Boundaries

| Layer | Owns | Does not own |
|---|---|---|
| **UI** (`src/macro_ui/`) | Draft tree, inspector (including unused rule timeouts), wizard detector tests, run intents | Execution, SendInput, compile |
| **App** (`src/main.rs`) | Store open, `save_validated`, target recapture, `build_windows_macro_runtime`, maps intents → `ControllerRunRequest` | Block semantics |
| **Engine** (`macro_engine`) | Definition, validation, compile, sequential + Watch execution, events, journals, image proof | HWND, OCR engine, SendInput |
| **Platform** | WGC/xcap capture, `Windows.Media.Ocr`, `WindowsInputSink`, ESC hook, target guard | Macro AST |
| **Enchant** (`enchant_loop`) | Separate calibrated occultist loop | Macro blocks |

`RuntimeCommand::{Start,Pause,…,DryRun,TestDetector}` (`runtime.rs` L1804–1821) is a **channel protocol used only in tests**. Production uses `MacroController` methods + `RuntimeControlHandle`. Wizard `TestDetector` is UI authoring, not `RuntimeCommand::TestDetector`.

`mod.rs` marks `runtime`, `observation`, `watch_group` `#[allow(dead_code)]` even though they are the production path (binary crate unused-item noise).

---

### Non-Obvious Things

1. **Default “Run” does not click.** UI “Run / Observe only” → `RunMode::ObservationOnly` continuous. Only “Run Live” injects mouse. DryRun and ObservationOnly both **plan** actions without input.

2. **`TextRule.timeout_ms` / `ImageRule.timeout_ms` are dead at runtime.** Inspector edits them (`inspector.rs` ~L550/606). `evaluate_condition` uses only `ObserveMode.timeout_ms`. Watch uses `WatchGroup.timeout_ms`. Rule timeouts are serialized and tested in UI only (`macro_ui/mod.rs` L4451+).

3. **Live `find_block_by_id` does not walk `TimeoutOutcome::RunBody` or Watch timeout bodies** (`runtime.rs` L2489–2514). Timeout-body clicks work in DryRun; Live `authorize_action` can fail with “action block is not compiled”.

4. **`MoveOnly` is in the model and DryRun-planned; Live rejects it** as `UnsupportedBlock`. Validation also treats MoveOnly as **not** a pacing operation (`continuous.busy_loop`).

5. **No keyboard/recording.** Actions are click-or-move. Popular “mouse macro” record-replay, keys, scroll, drag, pixel search, variables are absent by design (spec §2: Diablo-window visual automation, not a scripting platform).

6. **Watch is not a loop of concurrent clicks.** Lanes observe concurrently; **one** winner body runs; mouse is serialized. Global pool is 1 OCR + 2 image workers for the whole process.

7. **Image rules are not portable.** Compile requires a bound verification artifact (negative corpus). Import of image packages always demands local recapture (`LocalReverificationRequired`).

8. **Geometry/DPI lock.** Detectors bail if live client size or DPI ≠ `TargetProfile`. Run also requires a recaptured binding matching the saved target (`main.rs` L844–852).

9. **If/RepeatUntil/Watch lanes are observation sources**, not only `Observe`. Actions click the last token for that `source_block_id`. Tokens die on pause, side-effect epoch, or unmatched poll.

10. **Two click budgets.** Simulated `non_authoritative_planned_clicks` vs live `ActionCommitter.committed_clicks`. DryRun cannot consume the live ledger (test L7802+).

11. **`minimum_click_interval_ms` waits after every Action**, including DryRun and MoveOnly.

12. **Product vs Enchant.** Spec: beginner timeline next to Enchant. Engine is a compile/verify/journal runtime with SHA-pinned assets, Watch arbitration, and three run modes. That weight (verification, recapture, Live vs Observe) is why the page feels like a different product sitting on the same chrome.

**Wired vs missing vs product goal (if/else, loops, waits, OCR, image like mouse macros)**

| Goal | Status |
|---|---|
| If/else | Wired (`If` + `if_once_decision`) |
| Loops | Wired: RepeatN, RepeatUntil (pre-check), Continuous + busy-loop check |
| Waits | Wired: `Wait`; condition WaitForTrue/False; Watch timeout |
| OCR | Wired: Windows OCR, 4 preprocess profiles, Exact/Contains/Fuzzy/Absent, stability |
| Image recognition | Wired: multi-scale NCC, mask, clustering, ambiguity margin, verification artifact |
| Mouse click on match/point/region | Wired in Live; planned-only in DryRun/ObservationOnly |
| Concurrent “watch until any” | Wired as Watch Group (observe-concurrent, act-serial) |
| Keyboard, record/replay, drag, wheel, pixel, variables, hotkey trigger | Missing from `BlockKind`/`Action` |
| Move without click | Model + DryRun only; Live unsupported |
| Rule-level detector timeout | Persisted, not executed |
| Live timeout-body actions | Execute path yes; authorize lookup incomplete |
| Default Run = live clicking | Missing; explicit Live button only |

---

### Open Questions

- Is `TextRule`/`ImageRule.timeout_ms` leftover from an earlier design, or intended to cap detector polls independently of `ObserveMode`?
- Should Live `find_block_by_id` include timeout `RunBody` (Observe/If/RepeatUntil/Watch) so those actions can commit?
- Is ObservationOnly vs DryRun meant to stay identical for `ActionPlanned`, or should ObservationOnly skip planning (comment vs tests disagree on intent, not on code)?
- Will `MoveOnly` get a Live path, or should it be stripped from the v1 action set?
- `DetectorScheduler` in `watch_group.rs` vs `WatchDetectorPool` in `runtime.rs`: is the former retired, or a future replacement?
- Phase 13 handoff (`.continue-here.md`) still lists package P1s and live-core review; some of that may already be in this tree (handle-based package reads, `build_windows_macro_runtime`) — worth a separate “what landed vs handoff” pass.