# Task 5 Report: Observation-Only Sequential Runtime and Dry Run

## Status

Implemented and committed the observation-only sequential macro runtime at `6ce436e` (`feat: add observation-only macro runtime`). The runtime compiles validated immutable `SavedRevision` values, pins definition and asset bytes/hashes, evaluates sequential control flow, emits ordered run-scoped events, maps events into the neutral Task 3 `JournalRecord`, and never accepts or calls an `InputSink`.

This report is committed separately after the implementation commit so it can record the implementation SHA and final verification evidence.

## Scope Delivered

- Added `observation.rs` with `DetectorKind`, `DetectorEvidence`, `ObservationToken`, `ObservationRequest`, and the `ConditionDetector` contract. Detectors receive only immutable compiled state and the shared capture interface; no input interface is reachable.
- Added `runtime.rs` with `MacroRuntime`, `RuntimeCommand`, `RunMode`, `RunEvent`, `RunStatus`, `StopReason`, `ActionState`, `CompiledMacro`, runtime control handles, and bounded command/event channel types.
- Added `From<RunEvent> for JournalRecord` without changing the neutral persistence record schema.
- Extended `semantics.rs` with the pure `observation_satisfies_mode` rule for Check Now, Wait for True, and Wait for False.
- Re-exported the Task 5 contracts from `macro_engine::mod.rs`.

## Immutable Compilation and Observation Design

- `CompiledMacro::compile` accepts only `SavedRevision`, reruns Task 1 validation, recomputes and verifies the pretty-JSON definition SHA-256, verifies every pinned asset byte hash, rejects duplicate pins, and requires the pinned identity set to match the definition references exactly.
- The compiled definition and pinned asset bytes are owned immutable snapshots (`Arc`-backed); draft mutation cannot alter an active run.
- Each qualified observation token is run- and generation-scoped and pins detector family, source block, region/rule identities and revisions, frame/capture metadata, match geometry, scores/counts, stability, and detector-specific evidence.
- The detector call is checked before and after execution. Pause/resume changes the generation, clears prior observations, discards any in-flight result that crossed that boundary, and forces a fresh observation before a matched action can be planned.
- Condition false is a normal typed result. Capture/detector/compiled-state failures stop by default with `TechnicalFailure`; explicit macro/time-out stops retain their own typed reasons.

## Runtime Semantics

- IF evaluates once and selects exactly one branch.
- Wait blocks use short cooperative slices and check stop/pause/runtime limits on every slice.
- Wait for True and Wait for False honor finite or Unlimited timeouts and the explicit Stop Error, Continue, or Run Body outcome. Continue and Run Body retain the final observed boolean for enclosing IF/loop semantics.
- Repeat N checks the count before each body and executes exactly N iterations.
- Repeat Until checks before the first body and respects finite or Unlimited iteration bounds.
- Continuous executes sequentially and always performs a cooperative yield between iterations, including paced-action-only bodies.
- Stop Success and Stop Error propagate macro-wide through every nesting level.
- Watch Group encounters produce a typed `UnsupportedBlock` stop. No lane concurrency was added.
- Only one run may own a `MacroRuntime` at a time.
- Macro safety budgets enforce max runtime, click count (Move Only does not consume click budget), per-condition observation retry count, per-rule polling delay, and the macro-wide `max_observations_per_second` ceiling.

## Zero-Input Guarantee

- Both `RunMode::DryRun` and `RunMode::ObservationOnly` plan actions as `ActionPlanned { state: Planned }` only.
- `MacroRuntime` has no `InputSink` field, constructor parameter, command payload, or dispatch call. Therefore Task 5 cannot inject mouse input by construction.
- Future Prepared/Committed/Dispatched/Uncertain Dispatch states exist in `ActionState` for the later live-action task but are never entered here.

## Ordered and Bounded Events

- Every event carries one run ID, a strictly increasing sequence, and elapsed time clamped monotonically against the injected shared clock.
- The synchronous `run` collector defaults to 4,096 slots and reserves final critical-event capacity. It coalesces/drops polling progress only; when critical events approach the bound, it stops with `SafetyLimit` instead of dropping a transition, action, error, or stop reason.
- The bounded asynchronous event queue coalesces only the newest adjacent progress record for the same run/block. It never replaces progress across an intervening critical event, preserving sequence order.
- A full queue may evict polling progress for a critical event. If the queue contains only critical events, the producer applies backpressure until the consumer frees capacity; critical events are never dropped.
- The command queue is bounded. Emergency Stop sets an atomic bypass flag even when its queue insertion reports Full, so cancellation ownership is not dependent on queue capacity.
- `JournalRecord` conversion preserves sequence/elapsed metadata, maps event classes to the neutral Task 3 journal kinds, and stores the typed event payload in `fields`.

## TDD Evidence

- RED zero-input cycle: the focused test failed to compile because `MacroRuntime`, `RunMode`, and `RunEvent` did not exist. GREEN: Dry Run planned a point click with no input-sink dependency.
- RED control-flow cycle: tests were introduced before runtime support for Repeat Until pre-check, explicit timeout bodies, IF selection, Repeat N, and nested macro-wide stops. Compilation first failed on the missing wait-mode semantics helper; after implementation, the focused runtime and semantics suites passed.
- RED safety cycle: Move Only initially consumed click budget and failed its regression; the counter was restricted to click actions. A second active run initially was accepted; active-run ownership was added.
- RED pause cycle: an in-flight detector result initially survived pause/resume until the action boundary. The new test failed on a single detector call; runtime now discards that result and re-observes under the current generation.
- RED timeout cycle: Wait for False plus timeout Continue initially discarded the last true value and selected the wrong IF branch; the explicit outcome now preserves the last condition value.
- RED review-remediation cycle: tests first failed on missing bounded synchronous collection, unsafe progress ordering, macro-wide observation pacing, and per-evaluation retry scoping. All four reviewer findings were fixed and retained as regressions.

## Review

An independent final review reported no Critical, Important, or Minor findings after remediation. The reviewer specifically rechecked immutable pinning, ordered/journal events, condition/error/outcome semantics, pause invalidation, bounded collectors/channels, zero-input architecture, and the deferred Watch Group/live-input/Windows/UI boundaries.

## Files

- `src/engine/macro_engine/observation.rs` — detector/evidence/token contracts.
- `src/engine/macro_engine/runtime.rs` — compilation, sequential execution, Dry Run, events, controls, channels, journal mapping, and 27 focused tests.
- `src/engine/macro_engine/semantics.rs` — wait-mode target semantics and focused test.
- `src/engine/macro_engine/mod.rs` — module wiring and public re-exports.
- `.superpowers/sdd/task-5-report.md` — this report.

## Exact Verification Commands and Results

- Baseline before editing: `cargo test` — 88 passed, 0 failed.
- `cargo test macro_engine::runtime` — 27 passed, 0 failed.
- `cargo test macro_engine::semantics` — 10 passed, 0 failed.
- `cargo test` — 116 passed, 0 failed.
- `rustfmt --edition 2024 --check src/engine/macro_engine/observation.rs src/engine/macro_engine/runtime.rs src/engine/macro_engine/semantics.rs src/engine/macro_engine/mod.rs` — exit 0, no output.
- `git diff --check d7a8ea6b4551b7888c48f026d7991ed059b5ef8c..HEAD` — exit 0, no output.
- `cargo clippy --all-targets -- -D warnings -A dead_code -A clippy::collapsible-if -A clippy::too-many-arguments -A clippy::default-constructed-unit-structs -A clippy::ptr-arg` — exit 0.

The unrelaxed repository-wide `cargo clippy --all-targets -- -D warnings` remains blocked by pre-existing dead-code and Clippy findings in Task 3 persistence, image matching, validation, and Windows implementation code. Two Task 5-local Clippy findings exposed during that run were fixed before the successful scoped command above.

## Intentional Limitations

- No live input dispatch, Windows foreground/target enforcement, mouse ownership, or final action commit validation is implemented. Those belong to the later live-action task.
- No Watch Group scheduler, arbitration, lane latch, or concurrent detector execution is implemented.
- No UI, background service wiring, screenshot storage, OCR implementation, or image detector implementation is added. Task 5 defines and consumes the shared capture/detector contracts only.
- Critical event backpressure can block a producer until a consumer drains the bounded event channel. This is intentional: only polling progress may be discarded.
- Reaching the synchronous event collector bound stops the observation-only run with an explicit safety outcome rather than silently losing critical history.

## Post-Review Runtime Hardening

Review remediation was implemented in `a68bd13` (`fix: harden observation runtime invariants`) without expanding Task 5 beyond observation-only execution.

- Observation state is keyed by the condition's declared source ID, so a later false observation clears the source token instead of leaving stale evidence under a different owner block. Before planning a matched action, the runtime now verifies the token's source, detector family, rule ID/revision, and region ID/revision against the compiled source identity.
- Runtime-owned bounded channels share a sticky emergency signal with the active runtime. Emergency Stop remains out of band when the command queue is full, wakes cooperative waits and pauses, and stops with `EmergencyStopped`; no input dispatch path was added.
- Finite condition deadlines are checked before retry accounting and before every subsequent detector call. Poll sleeps are capped by the remaining deadline, so 100 ms timeouts resolve through Continue, Run Body, or Stop Error without a post-deadline observation or retry-limit substitution.
- Detector results, including errors, are interpreted only after control and generation checks. An error returned across pause/resume is discarded as stale and the condition is re-observed.
- Maximum runtime is checked within the paused control loop, so a paused run cannot evade the wall-clock safety limit.
- Compilation rejects conflicting hashes for the same immutable `(asset id, revision)` identity in both referenced and pinned assets, including public deserialized `SavedRevision` input.

### Remediation TDD Evidence

- Stale source regression: RED planned an action from the earlier true token after a later false check; GREEN clears the declared source and blocks the action.
- Token identity regression: RED failed to compile because identity validation did not exist; GREEN rejects mutations to every source/detector/rule/region identity field.
- Emergency bypass regression: RED failed to compile because runtime-owned channels did not exist; GREEN proves a full command queue still wakes and stops its owning runtime.
- Deadline regressions: RED produced safety-limit outcomes after the 10-second poll interval for all three explicit 100 ms timeout modes; GREEN resolves all three in about 100 ms with exactly one detector call.
- Stale detector-error regression: RED stopped after one call with `TechnicalFailure`; GREEN discards the generation-crossing error, re-observes, and plans from current evidence.
- Paused runtime regression: RED required the test fallback stop and ended `UserStopped`; GREEN ends with the maximum-runtime `SafetyLimit` while still paused.
- Asset identity regression: RED compiled a deserialized revision containing one `(id, revision)` with two hashes; GREEN rejects it with an immutable-identity conflict.

### Remediation Verification

- `cargo test engine::macro_engine::runtime::tests` - 36 passed, 0 failed.
- `cargo test engine::macro_engine::semantics::tests` - 10 passed, 0 failed.
- `cargo test` - 125 passed, 0 failed.
- `rustfmt --edition 2024 --check src/engine/macro_engine/observation.rs src/engine/macro_engine/runtime.rs src/engine/macro_engine/semantics.rs src/engine/macro_engine/mod.rs` - exit 0, no output.
- `cargo clippy --all-targets -- -D warnings -A dead_code -A clippy::collapsible-if -A clippy::too-many-arguments -A clippy::default-constructed-unit-structs -A clippy::ptr-arg` - exit 0.
- `git diff --check` - exit 0, no output.

### Independent Review Follow-Up

The independent remediation review found one remaining emergency-ownership race. Commit `dd57e03` (`fix: make emergency bypass runtime-sticky`) closes it: command-receiver acknowledgment now uses a separate atomic notice and cannot clear the runtime's sticky emergency signal, while per-run control reset occurs under the active-ownership lock before `active = true` is published. The new regression first ended `UserStopped` after the receiver consumed the bypass; it now ends `EmergencyStopped`. Final counts after this follow-up are 37 runtime tests and 126 full-suite tests.
