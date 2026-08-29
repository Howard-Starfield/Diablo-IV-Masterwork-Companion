# Macro Tab Design

**Status:** Approved and GPT-audited; ready for implementation planning
**Date:** 2026-07-12
**Product:** Diablo Masterwork Companion
**Target:** Windows-native Rust/egui application

## 1. Purpose

Add a top-level `Macro` tab beside the existing enchanting surface. The tab lets a beginner create, test, and run deterministic desktop macros that observe Diablo IV through text OCR or image-template matching and then perform explicitly configured mouse actions.

The product is a guided visual automation tool, not a free-form scripting environment. Its primary design goals are:

- Make execution order understandable by reading from top to bottom.
- Support text and image conditions in the first Macro release.
- Allow concurrent observation without concurrent mouse ownership.
- Keep every side effect tied to a fresh, validated observation.
- Support continuous operation while preserving cancellation, focus, geometry, pacing, and storage safeguards.
- Keep the implementation native, offline, and compatible with the existing Rust architecture.

## 2. Product boundary

The first release is Diablo-focused rather than a general Windows automation platform. Macros bind to one explicitly selected Diablo window and run only while that concrete window remains valid.

The interface must distinguish observation from input automation. It must not describe automated input as approved, safe, undetectable, or protected by human-like cursor movement. The application will not include anti-cheat bypasses, process injection, memory reading, network interception, enforcement-evasion features, unattended scheduling, remote activation, or automatic crash recovery.

## 3. Navigation and page structure

The application gains a top navigation row with at least:

- `Enchant`
- `Macro`

Selecting `Macro` opens the Macro home page. The page uses four coordinated areas:

1. **Top status strip**
   - Target window and connection state
   - Foreground, display, DPI, and geometry state
   - Draft/saved/validation state
   - `Validate`, `Dry Run`, `Run Once`, `Run`, `Pause`, and `Stop`
2. **Left macro library**
   - Create, rename, duplicate, search, enable/disable, import, export, and delete
   - Status badges: Draft, Ready, Needs Revalidation, Running, Stopped with Error, Disabled
   - Target application, last validation, and last run result
3. **Center event timeline**
   - Ordered, nested blocks with plain-language summaries
   - Drag-and-drop within structurally valid insertion points
   - Active step and enclosing control-flow containers highlighted during a run
4. **Right block inspector**
   - Type-specific settings, validation results, previews, and test actions

The bottom or side run monitor remains visible while executing and shows current step, enclosing branch and loop, iteration counts, elapsed time, latest observation, last action, next planned action, and exact stop reason.

## 4. Guided wizard and canonical editor

The New Macro wizard creates the same canonical blocks used by the timeline editor. There is no separate wizard-only macro format.

Wizard flow:

1. Select the concrete target window.
2. Capture and name a text or image search region.
3. Configure the expected text or image template.
4. Test the detector without injecting input.
5. Select a click target and mouse button.
6. Choose once, bounded repetition, condition repetition, or continuous operation.
7. Choose the default failure behavior.
8. Perform a dry run.
9. Open the generated blocks in the full timeline editor.

Wizard-created blocks remain fully editable. Compatible block conversions include:

- `Check Text` / `Wait for Text` / `Wait for Text to Disappear`
- `Check Image` / `Wait for Image` / `Wait for Image to Disappear`
- Left click / right click
- Click saved point / click saved region
- `Repeat N Times` / `Repeat Until`

Conversions preserve fields with identical meaning, request any newly required fields, and show which settings will be removed. Removed settings survive only in undo history. Unrelated type changes use `Replace Block`. Replacing an IF or loop must explicitly preserve or delete its children; children may never become hidden or unreachable.

## 5. Timeline vocabulary

### 5.1 Observation blocks

- Check Text
- Wait for Text
- Wait for Text to Disappear
- Check Image
- Wait for Image
- Wait for Image to Disappear

`Check` samples once. `Wait` polls until its condition succeeds, its configured timeout expires, or the run stops. Conditions never inject input.

### 5.2 Action blocks

- Left-click text match
- Right-click text match
- Left-click image match
- Right-click image match
- Left-click saved point
- Right-click saved point
- Left-click saved region
- Right-click saved region
- Move pointer to target without clicking

Click blocks dispatch one click and return. Fixed delays and postconditions are separate timeline blocks rather than hidden click behavior.

### 5.3 Flow blocks

- IF / THEN / ELSE
- Wait fixed duration
- Repeat N Times
- Repeat Until
- Continuous Loop
- Watch Group
- Stop Successfully
- Stop with Error
- Comment

THEN and ELSE are owned containers and cannot exist independently. Loops own their complete child bodies.

### 5.4 No arbitrary jumps

The timeline may display a non-editable `Return to loop start` marker, but v1 does not expose Tag, Label, Jump, Goto, forward jump, or arbitrary loop-back actions.

A loop may return only to its own beginning. Blocks cannot jump into or out of branches or loops. Nested loops use structural containment. Dragging a loop moves its complete body. Deleting a loop offers:

- Delete the loop and its contents.
- Keep the contents and remove repetition.

This provides familiar clicker-style playback without turning the macro into a general control-flow graph.

### 5.5 Exact control-flow semantics

- `Repeat N Times` executes exactly N times. N=0 skips the body.
- `Repeat Until` checks its condition before the first iteration. An already-satisfied condition performs no body actions.
- `Continuous Loop` repeats until stopped but must yield through a wait, detector poll, action cooldown, or scheduler yield. A body with no paced or blocking operation fails validation as a busy loop.
- IF evaluates its condition once per entry and enters exactly one branch.
- Every condition-wait timeout has an explicit outcome: stop with error, continue, or execute a configured timeout body.
- `Stop Successfully` and `Stop with Error` terminate the entire macro regardless of nesting.

## 6. Unlimited semantics

`Unlimited` removes only the selected user-configured bound. It never disables cancellation, focus checks, window identity, coordinate validation, input pacing, error handling, queue bounds, log bounds, or storage limits.

### Unlimited loop count

Continue until the condition succeeds, another configured bound ends the run, the user stops it, or a safety/error condition stops it. A loop with unlimited count and no automatic exit is labeled `Continuous Loop`.

### Unlimited wait duration

Available only for condition waits. The block continues polling at a finite rate until the condition succeeds or execution stops. A fixed-duration Wait cannot be unlimited.

### Unlimited retry count

Available only for eligible observation operations such as capture and OCR. It uses a minimum delay or bounded backoff. Clicks and other side effects are never automatically retried. Invalid configuration, wrong process, privilege mismatch, invalid coordinates, missing OCR language, cancellation, and unsupported image sizes are non-retryable.

### Unlimited total run duration

The engine imposes no elapsed-time deadline. Per-block limits still apply unless they are independently set to Unlimited. The UI labels the run `Continuous`.

## 7. Target window, regions, and revisions

Each run binds to one concrete process and window instance. The target profile records durable matching information, client dimensions, display mode, DPI, and named regions; it does not persist a reusable window handle.

Region types remain distinct:

- Text scan region
- Image search region
- Click region
- Click point
- Text-match target
- Image-match target

Every region has a stable ID and monotonic revision. Recapturing a region:

1. Replaces its geometry under the same logical ID.
2. Increments its revision.
3. Clears cached captures, OCR results, image matches, and match boxes.
4. Invalidates the compiled macro.
5. Marks dependent blocks for revalidation.
6. Shows the dependency list.
7. Preserves expected text by default.
8. Immediately offers a detector test.

Regions remain macro-local in v1.

## 8. Text detector

Text recognition uses `Windows.Media.Ocr` through an app-owned `TextDetector` interface.

Text-rule settings:

- Named scan region
- OCR language/profile
- Original, Grayscale, High Contrast, or Small Text preprocessing
- Expected text
- Exact, Contains, Fuzzy, or Text Absent matching
- Case handling
- Whitespace and line-break normalization
- Fuzzy threshold
- Polling interval and timeout
- Multiple-match policy
- Stability-frame requirement

Multiple-match policies are explicit:

- Require exactly one
- Highest score
- First in reading order
- Topmost
- Bottommost

Short strings receive stricter validation because fuzzy matching `OK`, `No`, or similarly ambiguous labels is unsafe.

Editing expected text, language, threshold, normalization, preprocessing, or match policy increments the text-rule revision and invalidates old observations and dependent matched-text clicks.

Normalized and fuzzy matches retain a mapping back to their source OCR words. A text-match rectangle is the union of the matched word boxes. Cross-line matching is disabled in v1 unless the rule explicitly enables it. Repeated identical text is grouped by its distinct word-box union before the configured multiple-match policy is applied. `Text Absent` produces no match rectangle and is incompatible with `Click Text Match`.

### 8.1 OCR hot path

The current temporary-PNG pipeline is replaced with an in-memory path:

1. Capture the named region.
2. Apply only the selected preprocessing profile.
3. Create a `SoftwareBitmap` directly from the pixel buffer.
4. Recognize it with a cached OCR engine.

OCR engines are cached by language on dedicated worker ownership. Capture and preprocessing buffers are reused. Only one OCR request per lane may be active; pending work keeps only the latest frame.

### 8.2 Preprocessing profiles

- **Original:** unmodified captured pixels
- **Grayscale:** luminance conversion only
- **High Contrast:** grayscale plus Otsu thresholding and configured/deterministic polarity
- **Small Text:** grayscale plus 2x enlargement, with optional thresholding

Three-times enlargement is not automatic. Color-channel extraction, adaptive thresholding, sharpening, morphology, and runtime multi-profile fallback are deferred until a representative corpus justifies them.

`Benchmark Profiles` evaluates saved positive and negative samples and recommends the fastest profile that meets the configured accuracy gate. It does not run every profile on every live poll.

## 9. Image detector

V1 image matching uses `imageproc::template_matching` behind an app-owned `ImageDetector` interface. Macro files persist product concepts rather than crate enums so the implementation remains replaceable.

V1 image-rule scope:

- Grayscale normalized cross-correlation
- Exact captured scale by default
- Optional 95%, 100%, and 105% checks
- Templates initially constrained to approximately 8x8 through 128x128 pixels
- Search regions initially constrained to approximately 640x360 pixels
- Starting similarity threshold of 0.95, requiring rule-specific verification
- Exactly one qualifying match by default
- Two consecutive stable matching frames
- Maximum default location drift of three pixels between stability frames
- Same detected scale across stability frames
- Minimum score margin over the strongest non-overlapping alternative
- Optional use of transparent pixels as an imported PNG mask

The raw score map is converted into visual candidates by extracting local maxima and suppressing or clustering overlapping boxes with a fixed, tested overlap/distance rule. Candidates for the same object across 95%, 100%, and 105% searches are merged before exactly-one validation. Ambiguity compares the best distinct non-overlapping cluster, not adjacent score-map pixels.

Two stable frames must have distinct frame IDs and a configured minimum elapsed separation. They must identify the same candidate cluster, scale, detector/region/rule revisions, target window, geometry, and display profile, within the configured box-center drift. Reprocessing one captured frame never counts as multiple stability frames.

The actual work cap is determined by release-mode benchmarks, not dimensions alone. Expensive configurations receive a blocking validation error rather than silently polling slowly.

Validation warns when:

- The template has very low variance.
- Grayscale removes important color distinction.
- The threshold is unusually low.
- Multiple locations approach the threshold.
- The best negative sample is close to the threshold.
- Multi-scale search introduces ambiguity.
- The template is stale for the current DPI or UI profile.

OpenCV, ONNX, rotation-invariant matching, arbitrary object detection, whole-screen search, large template libraries, automatic template learning, and live algorithm switching are out of v1 scope.

If `imageproc` misses release-mode latency or allocation targets, an app-local optimized matcher replaces it behind the same interface. The macro schema does not change.

## 10. Shared observation contract

Text and image detectors share capture and safety infrastructure but remain explicit condition families in the UI. The engine never treats an icon as OCR and never silently falls back between detectors.

Every qualified observation produces a short-lived token containing:

- Run ID
- Source condition block ID
- Detector type
- Region and rule revisions
- Frame ID and capture timestamp
- Window identity, client geometry, DPI, and display-profile revision
- Match rectangle
- Match score and match count
- Stability evidence
- Detector-specific evidence

A matched click accepts only the expected detector, source block, region revision, and rule revision. Any mismatch, stale token, side effect after capture, focus change, geometry change, recapture, edit, pause, or resume blocks the click and requires fresh observation.

## 11. Watch Group: concurrent observation

A Watch Group provides multiple simultaneously observing lanes without creating independent concurrent macro programs.

Each ordered lane contains:

- One passive text or image condition
- Polling, threshold, stability, and re-arm settings
- One sequential THEN body

All conditions observe concurrently. Exactly one qualifying lane wins arbitration, and only that lane's THEN body executes. Other lanes pause and discard stale results. A Watch Group performs one arbitration cycle; continuous monitoring places it inside Repeat Until or Continuous Loop.

### 11.1 Lifecycle

1. **Enter:** validate lanes, capture target identity and geometry, clear candidates, and arm lanes.
2. **Observe:** due lanes request frames through the shared capture coordinator.
3. **Qualify:** results must satisfy thresholds, match policy, stability, revisions, and current target geometry.
4. **Arbitrate:** collect nearly simultaneous ready candidates and select one deterministic winner.
5. **Commit:** acquire the macro-wide action lock, pause the group, and perform final safety revalidation.
6. **Execute:** run the winning THEN body sequentially.
7. **Settle:** apply cooldown and invalidate pre-action frames.
8. **Exit:** leave the Watch Group after the winning body completes.

A Watch Group is a one-shot `Wait for Any`: it arms its lanes, selects one winner, executes that lane's body, and exits. Continuous monitoring requires an enclosing Repeat Until or Continuous Loop. The group has a finite or Unlimited timeout. Timeout behavior is explicit: stop with error by default, continue after the group, or execute a configured timeout body. Detector errors remain typed errors and never silently count as no match.

Edge-trigger latch state belongs to the Watch Group block instance for the duration of the run and persists across repeated entries. Every lane that qualified true, winner or loser, becomes latched. On the next group entry it may observe only for a confirmed false state and becomes eligible again only afterward. Latches reset on a new run.

A Watch Group with zero enabled lanes is invalid. Dragging a lane changes its user-visible priority, immediately invalidates the compiled plan, and requires revalidation with a plain-language priority summary.

Nested Watch Groups and Continuous Loops inside lane bodies are disallowed in v1. Bounded Repeat blocks inside a lane body are allowed.

### 11.2 Scheduling

Initial internal defaults:

- One active capture coordinator request
- One Windows OCR job, with a measured experiment before allowing two
- Up to two image-matching jobs using serial matching
- One active detector job per lane
- One replaceable pending frame per lane
- Image polling default: 100 ms
- OCR polling default: 250 ms
- Minimum image interval: 50 ms
- Minimum OCR interval: 150 ms

Users do not configure worker counts.

V1 does not run multiple internally parallel Rayon matchers concurrently. The selected runtime policy is two concurrent serial image matches. A benchmark spike may replace that policy with one internally parallel match job if it is measurably better, but the runtime uses only one measured policy at a time through a bounded shared worker pool.

Lanes due in the same approximately 8-16 ms window may share a frame when they inspect the same or overlapping area. The coordinator captures a combined rectangle only when measured cheaper than separate captures. Each detector receives an immutable crop associated with the same frame metadata.

If a lane is already processing, its pending frame is replaced by the newest frame. Intermediate frames are dropped. No detector FIFO backlog is permitted. Under load, the scheduler increases effective polling delay, reports `Polling delayed`, and uses aging so a lane is not permanently starved.

### 11.3 Arbitration and conflicts

Candidate selection order:

1. Out-of-band system safety events immediately cancel the group and invalidate every candidate.
2. Lowest unique lane-order number wins among ordinary user-authored lanes ready within the arbitration window.

Lane order is unique and is the user-visible priority. Stable lane ID remains only an internal defensive fallback for corrupt data. A user-authored lane whose body stops the macro is an ordinary lane; users place it earlier when they want it prioritized.

The initial arbitration window is an internal implementation constant of approximately 25 ms and may be adjusted by measurement. It is not persisted or exposed as a user setting. Priority applies only among candidates that become ready in that window; the group does not wait indefinitely for a slow higher-priority detector. ESC, cancellation, focus loss, target mismatch, and other system safety events never wait for arbitration.

Once an ordinary winner begins, a later ordinary match cannot preempt it. ESC and safety/focus/window failures can still cancel any side effect not yet dispatched.

Losing lanes remain enabled but their current candidates are discarded. They never queue a click for later. Their latch state persists when the enclosing loop enters the group again.

Default lanes are edge-triggered: after qualifying true, they remain disarmed until the condition is observed false at least once. This prevents an unchanged visible control from triggering on successive Watch Group invocations.

Conflict outcomes:

- Two lanes target different controls: priority selects one; the loser may be observed again only on the next enclosing-loop entry and remains subject to its latch state.
- Two lanes target the same control: dispatch one click and log the duplicate candidate.
- A user-authored stop lane and click lane match together: unique lane order decides.
- A lane stops the macro while another detector finishes: discard the late result.
- Focus changes: invalidate every candidate.
- Editor changes during a run: the immutable saved revision continues; edits apply to the next run.

## 12. Macro runtime architecture

One application-owned `MacroRuntime` service owns exactly one active run.

The runtime uses one long-lived background service with dedicated ownership for:

- WinRT initialization and cached OCR engines
- Capture coordination
- Preprocessing buffers
- Image detector workers
- Block sequencing and runtime budgets
- Watch Group scheduling and arbitration
- Mouse movement and input ownership
- Cancellation and stop state

The egui thread edits definitions, sends commands, renders events, and never performs OCR or macro work directly.

Bounded commands include Start, Pause, Resume, Stop, Emergency Stop, Test Detector, Validate, and Dry Run. Ordered run-scoped events include block transitions, captures, detector results, arbitration outcomes, planned/dispatched/blocked actions, focus changes, errors, and the final typed stop reason.

Progress events may be coalesced. Errors, actions, state transitions, arbitration decisions, and stop reasons may not be dropped.

Normal cancellation uses a command/token path. ESC uses an atomic emergency-stop flag and wake signal that bypasses the UI command queue. Cancellation is checked before and after capture, preprocessing, detection, waits, pointer-movement segments, and immediately before input dispatch.

Action dispatch has three explicit states:

- **Prepared:** the action lock is held, but the action remains cancellable.
- **Committed:** final checks passed and the operating-system input dispatch has begun; that one input may no longer be retractable.
- **Dispatched:** the input call returned successfully.

ESC or focus loss before commit cancels the action. An event arriving after commit may not retract that one input, but no later input may occur. A process failure after commit but before a confirmed return records `Uncertain Dispatch` and is never retried.

## 13. Preflight and side-effect safety

Before every live run:

- Compile and validate the immutable saved block tree.
- Confirm one concrete target process/window instance.
- Confirm foreground, visibility, client geometry, DPI, display mode, and region bounds.
- Confirm all branches are complete.
- Confirm Unlimited semantics are explicit.
- Confirm action pacing, polling floors, and storage limits.
- Run required non-clicking startup detector samples.
- Show a plain-language maximum-impact summary.

Immediately before every click:

- Confirm cancellation is not set.
- Confirm the same process instance and window handle.
- Confirm the target is visible, foreground, and not minimized.
- Confirm client geometry, DPI, and display topology.
- Confirm destination remains inside the client area.
- Confirm the observation token is fresh and revision-compatible.
- Confirm the runtime still owns the action lock.
- Confirm input pacing and action budgets.

Focus loss pauses or stops according to macro policy. The application does not automatically steal focus. Meaningful manual pointer movement or any manual mouse-button event pauses or stops automated cursor ownership. Automated movement is covered by the action lock, checks cancellation between movement segments, and revalidates focus immediately before commit. Pause invalidates every observation, candidate, and partial stability sequence. Resume requires a fresh target-window check and fresh detector observations. A crash, sleep, lock, target restart, or application restart marks the run interrupted and never resumes or replays an uncertain action.

Dry Run performs capture, detection, matching, branching, waits, arbitration, and click visualization while injecting zero input. Continuous Dry Run has the same obvious Stop control, cancellation behavior, aggregate logging, and journal bounds as a live run.

## 14. Persistence

Definitions use versioned JSON written through atomic replacement. Runtime state is separate.

Primary entities:

- Macro definition and immutable saved revision
- Draft editor state
- Target profile
- Region
- Text rule
- Image rule/template
- Ordered block tree
- Safety policy
- Run record
- Step/observation/action records

Image templates and transparency masks are stored as separate immutable assets referenced by stable asset ID, revision, and content hash. A run snapshot pins the exact asset bytes/hash rather than a mutable file path. Recapture creates a new asset revision. Atomic save covers definition and asset-reference consistency. Missing or corrupted assets, corrupt JSON, and unsupported schema versions block validation without mutating the last usable revision. Interrupted migrations preserve the last complete revision. Export/import packages definitions and referenced assets, rejects path traversal and outside-package references, remaps duplicate IDs, and verifies hashes. Orphan cleanup removes only assets not referenced by any saved or running revision.

Run history uses a separate bounded append-oriented journal or dedicated local store. Screenshots are separate, opt-in, and disabled by default. Logs retain state changes, candidates, arbitration outcomes, actions, errors, and periodic aggregate counters rather than every poll. Runtime counters and temporary detector results are never written into the macro definition. Journal failure never disables emergency cancellation or final action validation.

## 15. Error and outcome model

Conditions and actions expose typed results. Important distinctions include:

- Condition false
- Condition timeout
- Capture failure
- OCR failure
- Image-match failure
- Ambiguous matches
- Focus loss
- Target mismatch
- Stale observation
- Action blocked
- Action prepared
- Action committed
- Click dispatched
- Uncertain dispatch
- Postcondition not satisfied
- User stopped
- Safety system stopped
- Macro completed successfully

`Click dispatched` never means the game accepted or acted on the click.

Condition false is normal branch control flow. Technical and safety errors fail closed by default. The macro-wide default failure policy is Stop Immediately. Observation-only retry may be configured; side effects are never silently retried or skipped.

## 16. Validation and performance targets

Performance is measured on a named reference PC with its CPU, Windows build, display mode, DPI, and region sizes recorded.

### Text OCR targets

For a typical region no larger than approximately 600x200:

- Measure capture, preprocessing, OCR, and end-to-end latency separately.
- Warm end-to-end P95 <= 120 ms on named reference hardware.
- A lane's polling interval is at least 1.5 times its measured P95 and never below the absolute OCR floor.
- No disk access in the hot path
- No OCR-engine creation after warm-up
- No more than one in-flight OCR request per lane

### Image matching targets

For a typical ROI around 400x250 and template up to 64x64:

- Exact-scale P95 <= 30 ms
- Three-scale P95 <= 75 ms with scaled templates precomputed
- No unbounded per-poll allocation growth
- Maximum-size rules are accepted only when measured P95 is below half their polling interval and the run remains inside its CPU budget.
- Benchmark two concurrent serial matches against one internally parallel match in release mode and select one bounded policy.

### Accuracy corpus

Maintain a versioned representative corpus with:

- 25-50 supported Diablo UI templates
- At least 1,000 positive frames
- At least 100,000 held-out negative frames per detector/template family
- Hovered, unhovered, animated, and static states
- Supported window modes, resolutions, UI scales, and Windows DPI settings
- Look-alike controls and common overlays

Targets:

- Event-level detection rate >= 99% on held-out positive sequences within 500 ms after the target becomes stable; report per-frame recall separately.
- Zero false action authorizations in the held-out negative corpus before enabling automatic clicking for that rule class, with the one-sided confidence bound reported.
- Explicit multiple-match behavior in every case
- Full corpus rerun after detector or matcher changes

### Cancellation and endurance

- Emergency flag ingestion P95 <= 50 ms.
- Pointer movement stops P95 <= 50 ms after the flag is observed.
- No action may commit after the emergency flag is observed; a previously committed input may be unretractable.
- Runtime reaches Stopped within 250 ms after any non-cancellable operating-system operation returns.
- No stale result after region, rule, focus, window, or geometry revision changes
- No automatic click retry
- No action replay after crash, sleep, or restart
- Eight-hour Continuous run with a documented post-warm-up memory ceiling and slope established from baseline measurement.
- No queue above documented bounds, stale action, lost emergency stop, definition mutation, asset leak, detector backlog, or journal/log-cap violation.

These are acceptance targets, not claims about current performance. Baseline measurements may refine size classes and thresholds without weakening safety invariants.

## 17. Testing strategy

### Unit tests

- Block-tree validation and conversions
- Unlimited semantics
- Region/rule revision invalidation
- Observation-token compatibility and freshness
- Text and image match policies
- Loop and IF/ELSE execution
- Watch Group lane lifecycle
- Watch Group one-shot timeout and persistent latch state
- Deterministic arbitration and tie-breaking
- Edge-trigger re-arming
- Log aggregation and caps

### Integration tests

- In-memory Windows OCR path with cached engine ownership
- `imageproc` matching against the versioned corpus
- Image candidate extraction, clustering, cross-scale merging, and stable-frame identity
- OCR normalized-text mapping back to word-box geometry
- Immutable template assets, hashes, import/export, and corruption handling
- Capture sharing and immutable crop metadata
- Latest-frame replacement and stale-result dropping
- Action lock and single mouse ownership
- Dry Run cannot inject input
- Focus, geometry, DPI, pause, and cancellation barriers

### Adversarial acceptance tests

- No representable unbounded CPU busy loop
- No action prepared before focus loss may commit after focus loss is observed
- No click from another condition's match
- No stale observation after edits or recapture
- No losing Watch Group action queued for later
- Out-of-band safety cancellation bypasses Watch Group arbitration
- Repeated visible matches fire once until re-armed
- Same-target simultaneous matches dispatch one click
- Action failure cannot silently continue
- Every stopped run reports one exact stop reason

## 18. Explicitly deferred

- Arbitrary Tag/Jump/Goto control flow
- Free-form expressions and scripts
- Regex matching
- Parallel lane-body execution
- Independent concurrent macros
- Nested Watch Groups
- Shared mutable variables between lanes
- User-configured worker counts
- Queued losing actions
- Dynamic or round-robin action priority
- OpenCV and ONNX backends
- Rotation-invariant or arbitrary object detection
- Keyboard text entry and complex key sequences
- Automatic focus stealing
- Scheduling, run-on-login, remote activation, and cloud sync
- Crash resumption or uncertain action replay
- Community marketplace, plugins, and AI-generated macros

## 19. Implementation order

The implementation plan should preserve these dependency boundaries:

1. Freeze executable state tables for blocks, one-shot Watch Group, latch persistence, timeouts, cancellation, pause/resume, action commit, and stop outcomes.
2. Run feasibility spikes for in-memory Windows OCR, `imageproc` candidate clustering/scale/masks/allocation/latency, and xcap capture/frame-sharing cost.
3. Lock the versioned JSON, immutable template-asset, content-hash, atomic-save, run-snapshot, migration, and bounded-journal contracts.
4. Extract reusable capture, OCR, image detection, target validation, mouse ownership, and cancellation primitives from the existing enchant path.
5. Build an observation-only sequential runtime with immutable snapshots, monotonic clocks, typed outcomes, validation, and zero-input Dry Run.
6. Add exact IF, loop, Unlimited, timeout, and busy-loop validation semantics.
7. Add the live action path with action lock, manual takeover, movement cancellation, and final target/token checks at the commit boundary.
8. Add OCR and image corpus gates, including candidate clustering and event-level benchmarks, before Watch Group work.
9. Add one-shot Watch Group scheduling, latch persistence, timeouts, arbitration, stale-result dropping, and bounded worker policy.
10. Build the canonical timeline editor and run monitor as projections of the tested engine contracts.
11. Build the guided wizard so it emits only canonical supported blocks.
12. Add bounded history and definition/asset import-export.
13. Add explicit `Enchant` and `Macro` page routing without changing enchant behavior; this shell work may proceed independently once contracts are stable.
14. Run adversarial cancellation, persistence corruption, stale-token, concurrency, latency, accuracy, and endurance gates throughout rather than reserving them for the end.
