# Task 7 Report: Image Candidate Clustering and Stability

## Status

DONE

## Delivered scope

- Kept the Task 2 serial `imageproc` normalized-correlation matcher and product-owned match types; no OpenCV, ONNX, UI, live input, or Watch Group work was added.
- Added product-owned `CandidateCluster`, `ImageMatchResult`, `StabilityTracker`, `ImageRuleVerification`, and `ImageDetector` contracts.
- Added typed image-frame metadata to detector evidence and observation tokens. Unqualified evidence retains diagnostic scores and metadata but cannot retain click geometry.
- `ImageDetector` reads immutable pinned template and mask bytes from `CompiledMacro`, validates the frame against the compiled rule, and emits evidence from one atomic captured frame whose pixels and metadata were sampled together.

## Deterministic matching and work limits

- Local maxima use a deterministic 3x3 neighborhood. Equal-score plateaus retain the first point in reading order.
- Candidate clustering is score-first and deterministic. Cross-scale candidates are merged before match selection.
- A shared typed scale/work-plan validator is used by authoring verification, compile, and live matching. It rejects zero, duplicate, overflowed, and individually non-fitting scales.
- Authoring preflights resource dimensions before corpus or variance work, caps scales at 32 and negative samples at 4,096 before reserving canonicalization buffers, and computes active-pixel variance in one streaming pass.
- Matching fails closed above 750,000 generated score-map cells, 50,000,000 conservative pixel operations, 4,096 retained candidates, or 100,000 spatial-cluster comparisons.
- Decode and preprocessing also fail closed above 16,777,216 search pixels / 64 MiB BGRA, 4,194,304 template pixels / 16 MiB decoded, 4,194,304 mask pixels / 16 MiB decoded, 4,194,304 pixels in any scaled template, 8,388,608 total scaled-template pixels, or 16 MiB total scaled template-plus-mask bytes. All dimension, pixel, and byte products use checked arithmetic.
- Spatial-grid non-maximum suppression replaces unbounded all-pairs clustering. Task 14 may lower the fixed limits after named-hardware release benchmarks.
- Screen-coordinate conversion and capture-origin addition are checked; unrepresentable coordinates return typed `ImageMatchError::CoordinateOverflow`.

## Verification and mask behavior

- The dedicated `image_verification` owner constructs local artifacts, derives negative-corpus provenance from structured samples, fingerprints and validates bindings, validates decoded pinned pixels, and supplies the shared PNG decoders.
- Every locally executable image rule requires a version-2 verification artifact bound to its rule and revision, template and mask identities, DPI, region and revision, search dimensions, scales, threshold, margin, derived corpus results, active-pixel variance, canonical corpus digest, and nonzero sample count.
- Authoring supplies structured negative samples. The owner canonicalizes them, rejects malformed entries, duplicate stable IDs, and duplicate content SHA-256 values, then derives the count, digest, and best score.
- PNG IHDR dimensions and worst-case decoded color depth are validated before decoder allocation; the decoder also receives an allocation limit. Template images then decode to grayscale. RGBA and LA masks use alpha as mask activity; masks without alpha use grayscale luminance. Fully transparent masks are rejected, including transparent white, while partial transparency has identical meaning in authoring, compile, and runtime.
- Compile and runtime recompute active-pixel variance from pinned PNG bytes, so an artifact cannot make a flat template executable by claiming different decoded pixels.

## Portable package trust boundary

- Portable package image verification is untrusted data. Every image-bearing package returns typed `LocalReverificationRequired` before locking, collision remapping, asset installation, or definition save.
- The rejection is non-mutating, including packages that contain apparently valid verification artifacts or would otherwise collide with local asset identities.
- Text-only packages remain portable. Every text-only package is compiled from its in-memory definition and package bytes before installation; compile failure leaves assets and definitions unchanged.
- Image rules become executable only after a local, in-memory re-verification workflow produces artifacts bound to locally captured evidence. Task 13 owns the recapture and local re-verification UI.
- No package HMAC or portable image-execution trust was added.

## Capture and stability contracts

- Production window capture derives physical client geometry from the concrete HWND with `GetClientRect` and `ClientToScreen`. The same client rect owns local bounds, screen translation, atomic geometry identity, and captured client dimensions.
- Framed-window regressions prove xcap outer/DWM borders and title bars are excluded from client-local detector coordinates.
- Atomic capture brackets raw pixels with before-and-after target snapshots and fails closed on target, requested-region, client-geometry, display, or DPI drift.
- Atomic process identity includes the raw creation `FILETIME`; window/frame revision hashes HWND, PID, and creation time so HWND/PID reuse after a process restart cannot continue an old capture or stability sequence.
- Stability requires distinct frame IDs, the configured minimum elapsed interval, bounded center drift, the same selected scale, and matching window, geometry, display, DPI, region, and rule revisions.
- Duplicate and too-early frames do not advance stability. Incompatible eligible frames begin a new one-frame sequence, and no qualifying candidate clears stability.
- Stability state is isolated by run, generation, source block, rule, and region. Runtime completion removes only generations actually observed by the detector.

## Regression evidence

- Client-geometry tests cover framed-window offsets, title-bar exclusion, checked screen translation, captured client dimensions, and 32-bit Windows handle sign extension.
- Work-plan tests cover zero, duplicate, overflowed, non-fitting, score-cell, pixel-operation, candidate, spatial-comparison, huge-header, oversized-search, sparse-mask, per-scaled-template, total-scaled-template, decoded-byte, and arithmetic-overflow failures.
- Mask tests cover RGBA and LA alpha, grayscale fallback, fully transparent white rejection, and partial transparency parity across verification, compile, and live matching.
- Corpus tests cover order independence, malformed entries, duplicate IDs, and duplicate content hashes under different IDs.
- Persistence tests prove image packages fail with `LocalReverificationRequired` before mutation, collision reservation, or artifact inspection; valid text-only packages import, and invalid text-only packages compile-fail without mutation.
- Runtime tests cover atomic frame identity, checked coordinates, independent interleaved runs, generation-scoped cleanup, and exactly-once terminal detector completion.
- The final independent review corrected HWND reconstruction and capped the checked sum of cluster members before cloning authoring evidence.
- The final resource review moved authoring preflight ahead of corpus/mask/variance work, bounded scale and corpus counts before reservation, and confirmed no remaining allocation-before-policy path.

## Files

- `src/engine/automation.rs` - atomic capture and process-instance snapshot contracts.
- `src/engine/platform/windows_impl.rs` - Win32 client geometry, process creation identity, and atomic xcap capture.
- `src/engine/macro_engine/image_match.rs` - scale/work planning, maxima, bounded spatial clustering, selection, masked matching, stability, and live detection.
- `src/engine/macro_engine/image_verification.rs` - local artifact ownership, corpus provenance, binding and decoded-pixel validation, and shared PNG decoding.
- `src/engine/macro_engine/persistence.rs` - portable image-package rejection, unconditional text-package compile-before-install, and non-mutation regressions.
- `src/engine/macro_engine/observation.rs` - typed frame metadata on evidence and tokens.
- `src/engine/macro_engine/runtime.rs` - metadata propagation and generation-scoped detector cleanup.
- `src/bin/macro_detection_bench.rs` - release-buildable matcher benchmark.

## Final verification

- `cargo test` - 227 passed, 0 failed.
- `cargo clippy --all-targets -- -D warnings -A dead_code -A clippy::collapsible-if -A clippy::too-many-arguments -A clippy::default-constructed-unit-structs -A clippy::ptr-arg` - exit 0.
- `cargo build --release --bin macro_detection_bench` - exit 0.
- Touched Rust files passed `rustfmt --check`.
- `git diff --check 3515e43` and `git diff --check` - exit 0.

## Intentional limitations

- The benchmark executable was compiled but not run because it requires an interactive Windows capture session. Task 14 owns named-hardware calibration and corpus accuracy gates.
- Task 8 still owns final pre-action target revalidation.
- Exact immutable bytes are decoded per observation; scaled-template caching remains measured follow-up work rather than an unproven optimization.
