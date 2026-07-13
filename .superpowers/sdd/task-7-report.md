# Task 7 Report: Image Candidate Clustering and Stability

## Status

DONE

## Scope and contracts

- Kept the Task 2 serial `imageproc` normalized-correlation matcher and product-owned match types; no OpenCV, ONNX, UI, live input, or Watch Group work was added.
- Added product-owned `CandidateCluster`, `ImageMatchResult`, `StabilityTracker`, `ImageRuleVerification`, and `ImageDetector` contracts.
- Added typed `ImageFrameMetadata` to Task 5 detector evidence and observation tokens. Unqualified image evidence retains diagnostic scores/metadata but cannot retain click geometry.
- `ImageDetector` reads exact immutable pinned template/mask bytes from `CompiledMacro`, decodes in memory, validates frame rule/region/DPI identity, and emits Task 5 `DetectorEvidence` from one atomic `CapturedScreenFrame` whose pixels and metadata were sampled together.

## Fixed deterministic policies

- Local maxima use a deterministic 3x3 neighborhood. Equal-score plateaus retain the first point in reading order.
- Candidate clustering is score-first and deterministic. Candidates merge when scale delta is at most 10 percentage points and either IoU is at least 0.30 or center distance is at most 4 px. Cross-scale candidates merge before match-selection policy.
- Each cluster preserves all members and its deterministic best score/rect/scale. Runner-up and ambiguity margin always use the next distinct cluster, never adjacent score-map pixels.
- Stability requires distinct frame IDs, at least the configured poll interval between accepted frames, center drift within the rule limit, the exact same selected scale, and exact equality of window identity/revision, geometry revision, display-profile revision, DPI, region revision, and rule revision.
- Duplicate frames and frames arriving before minimum separation do not advance or replace the accepted stability state. Incompatible eligible frames start a new one-frame sequence. No qualifying candidate clears stability.
- `0.95` is exported only as `INITIAL_SIMILARITY_THRESHOLD`. Rule verification and live matching use the saved rule threshold; a verified `0.91` regression proves it is not hardcoded truth.
- The validated work plan rejects zero, duplicate, overflowed, or individually non-fitting scales. It admits at most 750,000 generated score-map cells, 50,000,000 conservative pixel operations (score cells times active scaled template/mask pixels), 4,096 retained candidates, and 100,000 deterministic spatial-cluster comparisons. Task 14 may lower these limits after named-hardware release benchmarks.

## Verification behavior

The dedicated internal `image_verification` owner constructs artifacts, derives negative-corpus provenance from structured samples, fingerprints and validates bindings, validates decoded pinned pixels, and performs trusted package-remap rewrites. `ImageRuleVerification` blocks:

- missing, unexpected, dimension-mismatched, or fully transparent masks;
- template variance below 16 grayscale intensity-squared units over active mask pixels;
- current DPI differing from captured template DPI;
- score-map work above the configured score-cell budget;
- measured threshold-to-best-negative margin below the rule's minimum runner-up margin;
- best-to-distinct-runner-up ambiguity margin below that same rule margin;
- non-finite or out-of-range thresholds.

Authoring supplies ordered-independent negative samples containing stable ID, content SHA-256, normalized measured score, and the relevant evaluation inputs. The owner canonicalizes and sorts them, rejects duplicate stable IDs, duplicate content hashes, or malformed entries, and derives count, digest, and best score; callers cannot supply those result fields. Compile and live observation share the same verification owner for binding, fingerprint, decoded-template/mask, variance, DPI, and work-budget checks. Negative-corpus margin remains an authoring/preflight input rather than being fabricated in the polling hot path.

## TDD evidence

- Baseline: `cargo test macro_engine::image_match` passed 4 tests before edits.
- First RED: `cargo test macro_engine::image_match` failed with 42 expected missing-API errors for clustering, selection, stability metadata/tracker, masks, and verification.
- First GREEN: the focused image suite passed 15 tests and observation passed 2 tests.
- Image detector RED: its end-to-end regression failed to compile because `ImageFrameMetadataSource` and `ImageDetector` did not exist. GREEN proves first-frame geometry is withheld and second stable-frame geometry is emitted from immutable pinned bytes.
- Task 5 token RED/GREEN: with the evidence-to-token metadata mapping intentionally removed, `observation_token_preserves_typed_frame_metadata` failed on `None`; restoring the mapping passed.
- Mask parity RED/GREEN: a fully opaque mask initially disagreed with Task 2 unmasked normalized correlation. The product-owned masked calculation was aligned to normalized cross-correlation energy semantics and the regression passed.
- Deterministic-coordinate RED/GREEN: Bottommost selection initially overflowed while negating `i32::MIN`; comparator-based ordering now passes without arithmetic negation.

## Files

- `src/engine/macro_engine/image_match.rs` - maxima, clustering, selection, masked matching, structured verification inputs, stability, image detector, and focused tests.
- `src/engine/macro_engine/image_verification.rs` - the internal owner for artifact construction, binding/fingerprint validation, decoded-pixel validation, and trusted package asset remapping.
- `src/engine/macro_engine/persistence.rs` - verified package collision remap through the shared owner, including export/import/validate/compile and no-install failure regressions.
- `src/engine/macro_engine/observation.rs` - typed frame metadata on evidence/tokens and safe image evidence constructor.
- `src/engine/macro_engine/runtime.rs` - metadata propagation into Task 5 tokens and regression coverage.
- `src/bin/macro_detection_bench.rs` - benchmark imports the real engine module so the expanded matcher remains release-buildable.
- `.superpowers/sdd/task-7-report.md` - this evidence report.

## Final verification

- `cargo test macro_engine::image_match` - 18 passed, 0 failed.
- `cargo test macro_engine::observation` - 2 passed, 0 failed.
- `cargo test observation_token_preserves_typed_frame_metadata` - 1 passed, 0 failed.
- `cargo test` - 177 passed, 0 failed.
- `rustfmt --edition 2024 --check --config skip_children=true src/bin/macro_detection_bench.rs src/engine/macro_engine/image_match.rs src/engine/macro_engine/observation.rs src/engine/macro_engine/runtime.rs` - exit 0, no output. `skip_children` prevents the benchmark's real-engine module import from formatting untouched legacy modules.
- `git diff --check 3515e4397880d24f1e0b1f7ec1a9e773f8e84734..HEAD` - exit 0, no output after commit.
- `cargo clippy --all-targets -- -D warnings -A dead_code -A clippy::collapsible-if -A clippy::too-many-arguments -A clippy::default-constructed-unit-structs -A clippy::ptr-arg` - exit 0.
- `cargo build --release --bin macro_detection_bench` - exit 0; optimized benchmark target compiled.

## Intentional limitations

- The manual benchmark executable was compiled but not executed because it requires an interactive Windows capture session and installed Windows OCR language support. Task 14 owns named-hardware calibration and corpus accuracy gates.
- The shared capture contract retains raw `capture` for OCR/Enchant and adds `capture_frame` for image detection. Production image detection can use `xcap_atomic_window_capture(window_id)`, which brackets raw pixels with before/after snapshots for one concrete xcap window and fails closed on any target, geometry, display, DPI, or requested-region drift. Task 8 still owns final pre-action target revalidation.
- No template cache was added. Exact immutable bytes are decoded per observation in this bounded implementation; precomputed scaled-template caching remains performance work after measured profiling.

## Review remediation appendix

The post-Task-7 safety review was resolved issue by issue:

- Stability now returns typed `Accepted`, `Ignored`, or `Reset` outcomes. Full frame identity is compared before duplicate/minimum-elapsed filtering, exact scale is required, and ignored frames can never qualify or emit match geometry. The end-to-end A-to-B identity regression proves B geometry remains withheld until two eligible B frames have stabilized.
- Image detection consumes one atomic `CapturedScreenFrame` containing pixels plus frame/window/geometry/display/DPI metadata. The detector fixture rejects the legacy raw-pixel call, so the passing detector tests prove metadata cannot be sampled separately from the matched pixels.
- Every executable image rule now requires a version-2 persisted verification artifact bound to rule/revision, template/mask identities, DPI, region/revision, search dimensions, scales, threshold, margin, derived best-negative result, active-pixel variance, canonical negative-corpus SHA-256, and derived nonzero sample count. A deterministic fingerprint covers every persisted binding/result field. Invalid provenance and transplanted or mutated artifacts fail validation.
- Authoring, macro validation, compile, live observation, and trusted package remap share the dedicated `image_verification` owner. Compile and runtime recompute active-pixel variance from immutable pinned PNG bytes; a forged artifact claiming variance for a flat template is rejected.
- Stability state is isolated by run, generation, source block, rule, and region. Interleaved runs no longer evict each other. The bounded map fails closed at capacity, and `MacroRuntime` invokes the detector's generation-scoped completion hook exactly once on normal or technical terminal paths so completed runs release state without affecting another generation.
- Screen-coordinate conversion and capture-origin addition are checked. Unrepresentable coordinates return typed `ImageMatchError::CoordinateOverflow` rather than wrapping or panicking.

### Remediation TDD evidence

- Stability outcome RED: focused compilation failed while the new tests referenced the missing `StabilityOutcome`; GREEN covers identity-before-elapsed ordering, ignored-frame geometry withholding, all identity/revision resets, duplicate frames, and exact-scale stability.
- Atomic capture RED: the detector regression could not compile before `CapturedScreenFrame` and `capture_frame` existed; GREEN uses only the paired frame contract and makes raw capture fail.
- Tracker isolation RED: `interleaved_runs_reach_image_stability_independently` failed because observing one run retained only that run's state; GREEN preserves both runs and the capacity/cleanup regression passes.
- Coordinate RED: the overflow regression could not reference the missing typed error; GREEN returns `CoordinateOverflow` for extreme capture origins.
- Verification artifact RED: serialization and validation tests failed before the artifact/binding types and required checks existed. GREEN covers missing, invalid, stale, transplanted, invalid-digest, zero-count, and fingerprint-mismatched artifacts.
- Decoded-pixel RED: `compile_rejects_forged_verification_for_flat_template_pixels` initially received `Ok(CompiledMacro)`; GREEN rejects the forged low-variance template from its pinned bytes.
- Scalar RED/GREEN covers `NaN`, infinity, negative, and greater-than-one runner-up margins, thresholds, best-negative scores, and artifact values in both authoring and macro validation.

### Remediation final verification

- `cargo test engine::automation::tests` - 5 passed, 0 failed.
- `cargo test engine::platform::windows_impl::tests` - 3 passed, 0 failed.
- `cargo test macro_engine::image_match` - 29 passed, 0 failed.
- `cargo test macro_engine::observation` - 2 passed, 0 failed.
- `cargo test macro_engine::runtime` - 40 passed, 0 failed.
- `cargo test macro_engine::validate` - 23 passed, 0 failed.
- `cargo test macro_engine::persistence` - 30 passed, 0 failed.
- `cargo test` - 202 passed, 0 failed.
- `rustfmt --edition 2024 --check` over all touched Rust modules except the pre-existing unrelated `windows_impl.rs` drift - exit 0, no output. The new Windows hunks were manually aligned with rustfmt; a whole-file Windows check reports only two unchanged pre-existing lines at current lines 466 and 996, confirmed identical in `HEAD`.
- `git diff --check 3515e43` and `git diff --check` - exit 0, no output.
- `cargo clippy --all-targets -- -D warnings -A dead_code -A clippy::collapsible-if -A clippy::too-many-arguments -A clippy::default-constructed-unit-structs -A clippy::ptr-arg` - exit 0.
- `cargo build --release --bin macro_detection_bench` - exit 0; optimized benchmark target compiled.

### Final integration TDD evidence

- Structured negative corpus RED: the inherited focused build failed with 12 expected missing-type/field/variant errors while tests referenced `NegativeCorpusSample` and `NegativeSampleEvaluationInputs` but production still accepted caller-supplied digest/count/best score. GREEN derives those fields from validated canonical samples and covers order independence, content change, duplicate stable IDs, malformed hashes, invalid scores/IDs, and evaluation-input mismatch.
- Trusted package-remap RED: `verified_image_package_collision_remap_stays_valid_and_compilable` failed because import changed the rule template identity but left its artifact stale. GREEN routes template and mask collision rewrites through `image_verification`, recomputes the fingerprint, validates the imported definition, and compiles from the imported pinned bytes.
- Invalid-remap provenance is checked before installation. A stale-fingerprint package is rejected with no remapped asset binding installed, preserving rollback/no-clobber behavior.
- Runtime lifecycle GREEN covers exactly-once detector completion for normal and technical terminal execution, more than 256 completed image runs without capacity exhaustion, and generation-scoped cleanup that preserves another active generation of the same run.
- Atomic capture GREEN brackets raw pixels with two injectable target snapshots, rejects requested-region or target drift with `StaleCapturedFrameError`, preserves the raw OCR/Enchant path, and exposes a production xcap constructor bound to one concrete window ID.

### Final independent-review remediation

- Window-coordinate RED: the production xcap wrapper accepted client-local detector regions but passed them to monitor capture as screen coordinates. `XcapWindowRegionCapture` now resolves the concrete window, bounds-checks the local crop, performs checked translation by the current window origin, and lets the atomic before/after snapshots reject motion during capture. The offset-window and outside-bounds regression is green.
- Generation-cleanup RED: after an in-flight pause/resume, the detector observed generations 1 and 3 but the completion hook received only generation 1. `RunExecution` now records every generation actually sent to the detector, sorts the unique set, and calls one terminal completion hook with that set. `ImageDetector` removes only those run/generation keys, retaining absent generations and other runs.
- Transactional-compile RED: a structurally valid artifact claiming varied pixels imported successfully when package bytes actually decoded to a flat template. Verified packages are now compiled from the in-memory remapped definition and captured package bytes before any asset or definition installation. The regression proves the import fails on decoded variance and leaves both assets and definition absent.

### Final root-review remediation

- Production window capture now derives physical client geometry from the concrete HWND with `GetClientRect` plus `ClientToScreen`. The same client rect owns local bounds, screen translation, atomic geometry identity, and captured client dimensions; framed-window tests prove xcap outer/DWM borders and title bars are excluded.
- Portable package image artifacts are untrusted data. Every package containing image rules returns typed `LocalReverificationRequired` before lock/remap/install/save, while text-only packages compile unconditionally before installation. Task 13 owns the non-mutating local recapture and re-verification UI. No package HMAC or portable execution trust was added.
- Authoring verification, immutable compile, and live matching use one typed scale/work-plan validator. Local maxima and spatial-grid NMS fail closed at fixed candidate/comparison limits rather than performing unbounded all-pairs clustering.
- One decoder owner supplies template grayscale and mask semantics to authoring, compile, and live detection. RGBA/LA masks use alpha; masks without alpha use luminance. Fully transparent white is rejected and partial transparency is preserved consistently.
- Negative corpora reject repeated content SHA-256 even when stable IDs differ.
- Final independent review corrected xcap ID reconstruction to sign-extend 32-bit Windows user handles before rebuilding HWND, and verification now caps the checked sum of cluster members before cloning authoring evidence.

### Final root-review verification

- `cargo test` - 215 passed, 0 failed.
- `cargo clippy --all-targets -- -D warnings -A dead_code -A clippy::collapsible-if -A clippy::too-many-arguments -A clippy::default-constructed-unit-structs -A clippy::ptr-arg` - exit 0.
- `cargo build --release --bin macro_detection_bench` - exit 0.
- `git diff --check 3515e43` and `git diff --check` - exit 0.
