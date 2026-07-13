# Task 6 Report: Positioned OCR and Deterministic Text Geometry

## Status

Implemented and committed Task 6 at `84bc00a4aeb2aea849fe5c198db117ccaf49231f` (`feat: add positioned Windows OCR detection`) from base `3bfd9fb2e43df3fe8b7f6d266b643b9390903e18`.

The implementation adds positioned in-memory Windows OCR, deterministic normalized-text-to-word-box mapping, all Task 1 text match modes and selection policies, one configured preprocessing profile per live poll, a separate offline saved-sample profile benchmark, consecutive text stability qualification, and a Task 5 `ConditionDetector` adapter. It adds no image detection, input dispatch, Watch Group scheduling, or frontend behavior.

## Delivered Design

- `OcrWord` retains text, an enclosing capture-relative integer rectangle, and Windows OCR line/word order.
- Windows `OcrResult::Lines`, `OcrLine::Words`, `OcrWord::Text`, and `OcrWord::BoundingRect` are read directly from the cached in-memory `OcrEngine`; no PNG encoder, temporary file, storage file, or bitmap decoder is on the macro OCR path.
- Fractional WinRT word bounds use floor for left/top and ceil for right/bottom, so the integer rectangle encloses the complete OCR word.
- Normalization collapses whitespace, applies the rule's case policy, and keeps a normalized-character-to-source-word map. Empty OCR words do not add phantom spaces.
- Exact and Contains matches map the selected normalized character range back to its source word indices. Fuzzy matching evaluates contiguous word spans using the existing `strsim::jaro_winkler` dependency and retains the winning source word boxes.
- Phrase geometry is the checked union of only the selected word boxes. Cross-line matching is split by OCR line unless `allow_cross_line` is true.
- Candidate matches are deduplicated by distinct rectangle union before applying `ExactlyOne`, `HighestScore`, `FirstReadingOrder`, `Topmost`, or `Bottommost`.
- `ExactlyOne` with multiple distinct boxes is unqualified and carries no click rectangle. Text Absent always carries no match rectangle, whether the absence condition is true or false. The shared `DetectorEvidence::new` constructor also strips geometry from all negative evidence.
- The detector crops the compiled revision's region, runs exactly one selected profile and one OCR call, remaps Small Text coordinates to full-client capture coordinates, and emits evidence details containing the source block, rule ID/revision, region ID/revision, selected profile, OCR words, and raw text result.
- Consecutive stability is scoped to run ID, generation, source, rule revision, and region revision. Until `stable_frames` is met, the evidence is unqualified and exposes no click geometry.
- Live profiles are:
  - Original: color-preserving BGRA8 memory frame.
  - Grayscale: Gray8 luminance only.
  - High Contrast: Gray8 plus deterministic Otsu threshold and fixed light polarity.
  - Small Text: Gray8 plus exactly 2x Triangle enlargement, with enclosing inverse coordinate mapping.
- `benchmark_text_profiles` is an explicit offline API over preloaded saved positive/negative samples. It evaluates all four profiles, records correctness and elapsed time, and recommends the fastest profile meeting the requested accuracy gate. It does not call `CaptureSource` and is not part of live polling.

## Current Documentation Check

The repository Context7 rule was followed for the Windows API seam.

- Initial `npx ctx7@latest ...` failed because the PowerShell `npx.ps1` shim was blocked by execution policy.
- `npx.cmd ctx7@latest library "windows-rs" ...` resolved the high-reputation docs as `/websites/microsoft_github_io_windows-docs-rs_doc_windows`.
- `npx.cmd ctx7@latest docs "/websites/microsoft_github_io_windows-docs-rs_doc_windows" ...` confirmed the current `Lines`, `Words`, `Text`, and `BoundingRect` methods used by the installed Windows crate.

## TDD Evidence

1. Initial geometry RED
   - Command: `cargo test macro_engine::text -- --nocapture`
   - Result: compilation failed because `OcrWord`, `TextRule::contains`, `TextRule::absent`, and `match_text` did not exist.
   - GREEN: 2 passed, 0 failed.
2. Match semantics and source mapping RED
   - Command: `cargo test macro_engine::text -- --nocapture`
   - Result: 5 passed, 6 failed. Failures proved the initial implementation incorrectly unioned all words, crossed lines, treated Exact as Contains, ignored repeated-box policy, ignored geometry policies, and lacked fuzzy selection.
   - GREEN: 11 passed, 0 failed.
3. Preprocessing and WinRT geometry RED
   - Command: `cargo test macro_engine::text -- --nocapture`
   - Result: compilation failed because `OcrPixelFormat`, `preprocess_frame`, `map_processed_rect_to_capture`, and `enclosing_integer_rect` did not exist.
   - GREEN: 16 text tests and 5 Windows OCR tests passed.
4. Detector and benchmark RED
   - Command: `cargo test macro_engine::text -- --nocapture`
   - Result: compilation failed because `PositionedTextRecognizer`, `TextDetector`, `SavedTextProfileSample`, and `benchmark_text_profiles` did not exist.
   - GREEN: 19 passed, 0 failed.
5. Empty-word normalization regression RED
   - Command: `cargo test empty_ocr_words_do_not_create_extra_normalized_whitespace -- --nocapture`
   - Result: 0 passed, 1 failed because an empty OCR word inserted a second normalized space.
   - GREEN: 1 passed, 0 failed after empty normalized words were excluded from sequences.

## Exact Final Verification

- `cargo test macro_engine::text` - 20 passed, 0 failed, 129 filtered out.
- `cargo test windows_ocr` - 6 passed, 0 failed, 143 filtered out.
- `cargo test` - 149 passed, 0 failed, 0 ignored.
- `rustfmt --edition 2024 --check --config skip_children=true src/engine/macro_engine/text.rs src/engine/macro_engine/observation.rs src/engine/macro_engine/mod.rs src/engine/platform/windows_ocr.rs src/engine/platform/mod.rs` - exit 0, no output.
- `git diff --check` - exit 0, no output.
- `cargo clippy --all-targets -- -D warnings -A dead_code -A clippy::collapsible-if -A clippy::too-many-arguments -A clippy::default-constructed-unit-structs -A clippy::ptr-arg` - exit 0.

## Files

- `src/engine/macro_engine/text.rs` - text contracts, matching, geometry, profiles, stability, detector, benchmark, and focused tests.
- `src/engine/platform/windows_ocr.rs` - Gray8/BGRA8 in-memory frames, cached OCR, positioned WinRT words, enclosing integer conversion, and no-file tests.
- `src/engine/macro_engine/observation.rs` - evidence constructor that enforces no geometry on negative evidence.
- `src/engine/macro_engine/mod.rs` and `src/engine/platform/mod.rs` - module wiring and public product adapters.
- `tests/fixtures/macro/text/words.json` - saved positioned-word ordering and geometry fixture.

## Limitations and Deferred Scope

- The test gate constructs both Gray8 and BGRA8 WinRT `SoftwareBitmap` values, but it does not run live OCR against an installed language pack or a representative Diablo screenshot corpus. Corpus accuracy and profile timing remain machine- and language-pack-dependent.
- The offline benchmark accepts already loaded saved samples. Sample persistence, corpus management, and UI presentation belong to later product work.
- Fuzzy v1 compares contiguous spans with the same normalized word count as the expected phrase. Regex, adaptive thresholds, morphology, sharpening, channel extraction, automatic live multi-profile fallback, and 3x enlargement remain intentionally deferred.
- `TextDetector` rejects image conditions. Task 7 may compose explicit text and image detector ownership; there is no silent detector-family fallback.
- No live action commit, mouse input, target focus control, Watch Group runtime, image candidate clustering, screenshot storage, or frontend code was added.
- The requested model/effort setting was not exposed as a verifiable runtime control in this subagent session, so no model-selection claim is made.
