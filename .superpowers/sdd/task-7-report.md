# Task 7 Report: Image Candidate Clustering and Stability

## Status

DONE

## Scope and contracts

- Kept the Task 2 serial `imageproc` normalized-correlation matcher and product-owned match types; no OpenCV, ONNX, UI, live input, or Watch Group work was added.
- Added product-owned `CandidateCluster`, `ImageMatchResult`, `StabilityTracker`, `ImageRuleVerification`, and `ImageDetector` contracts.
- Added typed `ImageFrameMetadata` to Task 5 detector evidence and observation tokens. Unqualified image evidence retains diagnostic scores/metadata but cannot retain click geometry.
- `ImageDetector` reads exact immutable pinned template/mask bytes from `CompiledMacro`, decodes in memory, validates frame rule/region/DPI identity, captures only the compiled region, and emits Task 5 `DetectorEvidence`.

## Fixed deterministic policies

- Local maxima use a deterministic 3x3 neighborhood. Equal-score plateaus retain the first point in reading order.
- Candidate clustering is score-first and deterministic. Candidates merge when scale delta is at most 10 percentage points and either IoU is at least 0.30 or center distance is at most 4 px. Cross-scale candidates merge before match-selection policy.
- Each cluster preserves all members and its deterministic best score/rect/scale. Runner-up and ambiguity margin always use the next distinct cluster, never adjacent score-map pixels.
- Stability requires distinct frame IDs, at least the configured poll interval between accepted frames, center drift within the rule limit, scale delta at most 5 percentage points, and exact equality of window identity/revision, geometry revision, display-profile revision, DPI, region revision, and rule revision.
- Duplicate frames and frames arriving before minimum separation do not advance or replace the accepted stability state. Incompatible eligible frames start a new one-frame sequence. No qualifying candidate clears stability.
- `0.95` is exported only as `INITIAL_SIMILARITY_THRESHOLD`. Rule verification and live matching use the saved rule threshold; a verified `0.91` regression proves it is not hardcoded truth.
- The initial work gate is 750,000 generated score-map cells, sized to admit the v1 640x360 three-scale envelope. Task 14 may lower it after named-hardware release benchmarks; dimensions alone are not the gate.

## Verification behavior

`ImageRuleVerification` blocks:

- missing, unexpected, dimension-mismatched, or fully transparent masks;
- template variance below 16 grayscale intensity-squared units over active mask pixels;
- current DPI differing from captured template DPI;
- score-map work above the configured score-cell budget;
- measured threshold-to-best-negative margin below the rule's minimum runner-up margin;
- best-to-distinct-runner-up ambiguity margin below that same rule margin;
- non-finite or out-of-range thresholds.

Runtime validation repeats the safety checks available from immutable live inputs: threshold, pinned mask integrity, DPI, and work budget. Negative-corpus margin remains an authoring/preflight verification input rather than being fabricated in the polling hot path.

## TDD evidence

- Baseline: `cargo test macro_engine::image_match` passed 4 tests before edits.
- First RED: `cargo test macro_engine::image_match` failed with 42 expected missing-API errors for clustering, selection, stability metadata/tracker, masks, and verification.
- First GREEN: the focused image suite passed 15 tests and observation passed 2 tests.
- Image detector RED: its end-to-end regression failed to compile because `ImageFrameMetadataSource` and `ImageDetector` did not exist. GREEN proves first-frame geometry is withheld and second stable-frame geometry is emitted from immutable pinned bytes.
- Task 5 token RED/GREEN: with the evidence-to-token metadata mapping intentionally removed, `observation_token_preserves_typed_frame_metadata` failed on `None`; restoring the mapping passed.
- Mask parity RED/GREEN: a fully opaque mask initially disagreed with Task 2 unmasked normalized correlation. The product-owned masked calculation was aligned to normalized cross-correlation energy semantics and the regression passed.
- Deterministic-coordinate RED/GREEN: Bottommost selection initially overflowed while negating `i32::MIN`; comparator-based ordering now passes without arithmetic negation.

## Files

- `src/engine/macro_engine/image_match.rs` - maxima, clustering, selection, masked matching, verification, stability, image detector, and focused tests.
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
- The current Task 5 capture trait does not itself return crop metadata. Task 7 therefore injects an `ImageFrameMetadataSource`; the source owns the immutable frame identity supplied to the detector. Task 8 still owns concrete target snapshots and final pre-action target revalidation.
- No template cache was added. Exact immutable bytes are decoded per observation in this bounded implementation; precomputed scaled-template caching remains performance work after measured profiling.
