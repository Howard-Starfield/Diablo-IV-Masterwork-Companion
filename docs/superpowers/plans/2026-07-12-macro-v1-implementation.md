# Macro V1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Windows-native `Macro` tab that builds and runs deterministic text- and image-driven timelines with structured loops, one-shot concurrent Watch Groups, dry run, and serialized safe mouse actions.

**Architecture:** Keep the existing enchant feature behavior intact while adding focused `macro_engine` and `macro_ui` modules. A single run-owned runtime compiles immutable definitions, routes text observations to cached in-memory Windows OCR, routes image observations to `imageproc`, schedules passive Watch Group lanes concurrently, and serializes all side effects through one action commit boundary.

**Tech Stack:** Rust 2024, eframe/egui 0.27, windows-rs 0.62, xcap 0.9, image 0.25, imageproc 0.27 with default features disabled, serde/serde_json, standard-library threads/channels/atomics.

## Global Constraints

- Windows-only; use the existing Rust/egui application and preserve the current Enchant workflow.
- V1 supports both `Windows.Media.Ocr` text detection and `imageproc` template matching.
- `Unlimited` removes only the chosen user bound; cancellation, focus/window checks, pacing, queue/log/storage bounds, and failure handling remain mandatory.
- One macro run, one target window, one action lock, and one mouse owner at a time.
- Watch Group is one-shot `Wait for Any`; continuous observation requires an enclosing loop.
- No arbitrary Tag/Jump/Goto, scripts, regex, nested Watch Groups, concurrent lane bodies, OpenCV, ONNX, automatic click retry, focus stealing, crash replay, or background scheduling.
- Dry Run injects zero input.
- Every click requires a fresh revision-bound observation plus final cancellation, focus, target, geometry, and bounds validation.
- Image matching starts with grayscale normalized cross-correlation, exact scale or 95/100/105%, candidate clustering, and two distinct stable frames.
- All runtime queues, journals, screenshots, and asset stores are bounded even during Continuous operation.
- Baseline command before implementation: `cargo test` reports `14 passed; 0 failed`.

---

## File and Responsibility Map

### Existing files retained

- `src/main.rs` — startup and page routing; delegates Macro UI instead of absorbing engine behavior.
- `src/engine/enchant_loop.rs` — existing enchant runner; consumes extracted shared platform traits without behavior changes.
- `src/engine/platform/windows_impl.rs` — existing Windows implementation; progressively delegates focused helpers.
- `src/engine/matcher.rs` — existing enchant-specific fuzzy matching remains unchanged.
- `src/engine/types.rs` — shared point, rectangle, and captured-image primitives.

### New engine files

- `src/engine/automation.rs` — shared capture, input, stop, target, and clock contracts.
- `src/engine/macro_engine/mod.rs` — public Macro engine surface and exports.
- `src/engine/macro_engine/model.rs` — versioned definitions, blocks, limits, detector rules, assets, and policies.
- `src/engine/macro_engine/validate.rs` — structural, reference, busy-loop, and safety validation.
- `src/engine/macro_engine/semantics.rs` — pure block/loop/wait/stop transitions.
- `src/engine/macro_engine/persistence.rs` — definitions, immutable assets, hashes, journal caps, and folder-package import/export.
- `src/engine/macro_engine/observation.rs` — frame metadata, evidence, clusters, stability, and tokens.
- `src/engine/macro_engine/text.rs` — text rules and OCR-word-to-click geometry.
- `src/engine/macro_engine/image_match.rs` — imageproc adapter, clustering, ambiguity, and stability.
- `src/engine/macro_engine/runtime.rs` — immutable sequential runtime, commands/events, cancellation, and action states.
- `src/engine/macro_engine/watch_group.rs` — lane scheduling, timeout, latches, arbitration, and stale dropping.
- `src/engine/platform/windows_target.rs` — concrete window identity and geometry validation.
- `src/engine/platform/windows_ocr.rs` — cached in-memory `SoftwareBitmap` OCR.
- `src/engine/platform/windows_input.rs` — movement, takeover monitoring, and action commit.

### New UI files

- `src/macro_ui/mod.rs` — Macro page composition and UI state.
- `src/macro_ui/library.rs` — macro library and revision status.
- `src/macro_ui/timeline.rs` — ordered block tree, valid drag targets, and active highlighting.
- `src/macro_ui/inspector.rs` — editing, conversion, testing, recapture, and validation.
- `src/macro_ui/wizard.rs` — beginner wizard emitting canonical blocks.
- `src/macro_ui/monitor.rs` — run, candidate, action-state, and stop display.

### Test assets

- `tests/fixtures/macro/text/` — OCR preprocessing and word-geometry fixtures.
- `tests/fixtures/macro/images/` — template/search-region fixtures.
- `tests/fixtures/macro/packages/` — valid, corrupt, missing-asset, and traversal packages.
- `src/bin/macro_detection_bench.rs` — dependency-free manual release-mode detector timing harness.

---

### Task 1: Freeze Executable Model and Control-Flow Semantics

**Files:**
- Create: `src/engine/macro_engine/mod.rs`
- Create: `src/engine/macro_engine/model.rs`
- Create: `src/engine/macro_engine/semantics.rs`
- Create: `src/engine/macro_engine/validate.rs`
- Modify: `src/engine/mod.rs`
- Test: inline unit tests in the new files

**Interfaces:**
- Produces: `MacroDefinition`, `Block`, `BlockKind`, `Condition`, `Action`, `TextRule`, `ImageRule`, `Limit<T>`, `WatchGroup`, `WatchLane`, `ValidationProblem`, `validate_macro`, and `LoopDecision`.
- Consumes: `engine::types::{PointRatio, RectRatio}`.

- [ ] **Step 1: Add the module and write failing serialization and semantic tests**

```rust
#[test]
fn repeat_until_checks_before_first_body() {
    assert_eq!(
        evaluate_repeat_until_before_body(true, 0, Limit::Unlimited),
        LoopDecision::ExitConditionMet
    );
}

#[test]
fn zero_repeat_count_skips_body() {
    assert_eq!(repeat_n_decision(0, 0), LoopDecision::ExitCountMet);
}

#[test]
fn limit_round_trips_with_explicit_unlimited_tag() {
    let json = serde_json::to_string(&Limit::<u64>::Unlimited).unwrap();
    assert_eq!(json, r#"{"kind":"unlimited"}"#);
    assert_eq!(serde_json::from_str::<Limit<u64>>(&json).unwrap(), Limit::Unlimited);
}
```

- [ ] **Step 2: Run focused tests and verify missing types fail**

Run: `cargo test macro_engine:: -- --nocapture`

Expected: compilation fails because `macro_engine`, `Limit`, and semantic functions do not exist.

- [ ] **Step 3: Implement the versioned model and pure semantics**

```rust
pub const MACRO_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Limit<T> {
    Finite(T),
    Unlimited,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MacroDefinition {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub revision: u64,
    pub target: TargetProfile,
    pub regions: Vec<RegionDefinition>,
    pub text_rules: Vec<TextRule>,
    pub image_rules: Vec<ImageRule>,
    pub blocks: Vec<Block>,
    pub safety: SafetyPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetProfile {
    pub process_path: String,
    pub window_class: String,
    pub title_contains: String,
    pub captured_client_width: u32,
    pub captured_client_height: u32,
    pub captured_dpi: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegionDefinition {
    pub id: String,
    pub revision: u64,
    pub rect: RectRatio,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextRule {
    pub id: String,
    pub revision: u64,
    pub region_id: String,
    pub language: String,
    pub preprocess: PreprocessProfile,
    pub expected: String,
    pub match_mode: TextMatchMode,
    pub threshold: f64,
    pub case_sensitive: bool,
    pub allow_cross_line: bool,
    pub match_policy: MatchSelectionPolicy,
    pub poll_interval_ms: u64,
    pub timeout_ms: Limit<u64>,
    pub stable_frames: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageRule {
    pub id: String,
    pub revision: u64,
    pub region_id: String,
    pub template_asset_id: String,
    pub transparent_mask_asset_id: Option<String>,
    pub threshold: f32,
    pub scales_percent: Vec<u16>,
    pub stable_frames: u8,
    pub maximum_center_drift_px: u32,
    pub minimum_runner_up_margin: f32,
    pub match_policy: MatchSelectionPolicy,
    pub poll_interval_ms: u64,
    pub timeout_ms: Limit<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SafetyPolicy {
    pub max_runtime_ms: Limit<u64>,
    pub max_clicks: Limit<u64>,
    pub max_observation_retries: Limit<u64>,
    pub max_observations_per_second: u32,
    pub minimum_click_interval_ms: u64,
    pub focus_loss: FocusLossPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextMatchMode { Exact, Contains, Fuzzy, Absent }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreprocessProfile { Original, Grayscale, HighContrast, SmallText }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchSelectionPolicy { ExactlyOne, HighestScore, FirstReadingOrder, Topmost, Bottommost }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton { Left, Right }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionTarget {
    TextMatch { source_block_id: String },
    ImageMatch { source_block_id: String },
    Point { point_id: String },
    Region { region_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusLossPolicy { Pause, Stop }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub id: String,
    pub enabled: bool,
    pub kind: BlockKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockKind {
    Observe { condition: Condition },
    Action { action: Action },
    If { condition: Condition, then_body: Vec<Block>, else_body: Vec<Block> },
    Wait { duration_ms: u64 },
    RepeatN { count: u32, body: Vec<Block> },
    RepeatUntil { condition: Condition, max_iterations: Limit<u64>, body: Vec<Block> },
    Continuous { body: Vec<Block> },
    WatchGroup { group: WatchGroup },
    StopSuccess,
    StopError { message: String },
    Comment { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "detector", rename_all = "snake_case")]
pub enum Condition {
    Text { source_block_id: String, rule_id: String, mode: ObserveMode },
    Image { source_block_id: String, rule_id: String, mode: ObserveMode },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserveMode { CheckNow, WaitForTrue, WaitForFalse }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    ClickTextMatch { source_block_id: String, button: MouseButton },
    ClickImageMatch { source_block_id: String, button: MouseButton },
    ClickPoint { point_id: String, button: MouseButton },
    ClickRegion { region_id: String, button: MouseButton },
    MoveOnly { target: ActionTarget },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchGroup {
    pub lanes: Vec<WatchLane>,
    pub timeout_ms: Limit<u64>,
    pub timeout_outcome: TimeoutOutcome,
    pub cooldown_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchLane {
    pub id: String,
    pub enabled: bool,
    pub condition: Condition,
    pub then_body: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimeoutOutcome {
    StopError { message: String },
    Continue,
    RunBody { body: Vec<Block> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopDecision {
    EnterBody,
    ExitConditionMet,
    ExitCountMet,
}
```

Implement IF-once, pre-check Repeat Until, exact Repeat N, macro-wide stop propagation, and explicit wait-timeout outcomes as pure functions.

- [ ] **Step 4: Add failing validation tests**

```rust
#[test]
fn rejects_unpaced_continuous_loop() {
    let definition = fixture_macro(vec![Block::continuous(vec![Block::comment("spin")])]);
    let problems = validate_macro(&definition);
    assert!(problems.iter().any(|p| p.code == "continuous.busy_loop"));
}

#[test]
fn rejects_watch_group_without_enabled_lanes() {
    let definition = fixture_macro(vec![Block::watch_group(vec![])]);
    let problems = validate_macro(&definition);
    assert!(problems.iter().any(|p| p.code == "watch_group.no_enabled_lanes"));
}
```

- [ ] **Step 5: Implement structural and reference validation**

Reject duplicate block IDs, zero enabled Watch lanes, nested Watch Groups, Continuous inside lane bodies, missing timeout behavior, `TextAbsent -> ClickTextMatch`, invalid references, family-unsafe conversions, and Continuous bodies without a paced or blocking operation.

- [ ] **Step 6: Run formatting and tests**

Run: `cargo fmt --check; cargo test macro_engine::`

Expected: Macro tests pass and the original 14 tests remain green.

- [ ] **Step 7: Commit**

```powershell
git add src/engine/mod.rs src/engine/macro_engine
git commit -m "feat: define macro model and semantics"
```

---

### Task 2: Prove OCR, Image Matching, and Capture Feasibility

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `src/engine/macro_engine/image_match.rs`
- Create: `src/engine/platform/windows_ocr.rs`
- Create: `src/bin/macro_detection_bench.rs`
- Modify: `src/engine/platform/mod.rs`

**Interfaces:**
- Produces: `ImageMatcher`, `ImageMatchConfig`, `RawImageMatch`, `WindowsTextRecognizer`, and `OcrFrame`.
- Consumes: `ScreenImage`, `Rect`, and `image::GrayImage`.

- [ ] **Step 1: Add the matcher dependency**

```toml
imageproc = { version = "0.27.0", default-features = false }
```

Run: `cargo check`

Expected: dependency resolves with Rust 1.94.1 and the app compiles.

- [ ] **Step 2: Write a failing synthetic image-match test**

```rust
#[test]
fn normalized_correlation_reports_expected_best_location() {
    let search = fixture_search_with_icon_at(23, 17);
    let template = fixture_icon();
    let result = ImageMatcher::default()
        .match_template(&search, &template, &ImageMatchConfig::exact_scale(0.95))
        .unwrap();
    assert_eq!((result.best.rect.x, result.best.rect.y), (23, 17));
    assert!(result.best.score >= 0.95);
}
```

- [ ] **Step 3: Implement the serial normalized-correlation adapter**

Use `imageproc::template_matching::match_template` with `NormalizedCrossCorrelation`, then convert its score image into product-owned candidates. Never serialize imageproc enums.

- [ ] **Step 4: Add a failing in-memory OCR bitmap test**

```rust
#[cfg(target_os = "windows")]
#[test]
fn creates_gray8_software_bitmap_without_png_file() {
    let pixels = vec![255u8; 32 * 16];
    let bitmap = software_bitmap_from_gray8(&pixels, 32, 16).unwrap();
    assert_eq!(bitmap.PixelWidth().unwrap(), 32);
    assert_eq!(bitmap.PixelHeight().unwrap(), 16);
}
```

- [ ] **Step 5: Implement direct `SoftwareBitmap` creation and cached recognizer ownership**

Add `Security_Cryptography` and `Graphics_Imaging` Windows features. Build an `IBuffer` with `CryptographicBuffer::CreateFromByteArray`, call `SoftwareBitmap::CreateCopyFromBuffer` with `BitmapPixelFormat::Gray8`, and retain one `OcrEngine` per selected language.

- [ ] **Step 6: Add the manual benchmark harness**

Use `std::time::Instant` in a normal release binary to report capture, preprocessing, OCR, serial exact-scale match, and three-scale match separately over warm iterations. Keep it outside the default test gate.

Run: `cargo test image_match; cargo test windows_ocr; cargo build --release --bin macro_detection_bench`

Expected: detector tests pass and the benchmark target compiles.

- [ ] **Step 7: Commit**

```powershell
git add Cargo.toml Cargo.lock src/bin/macro_detection_bench.rs src/engine/macro_engine/image_match.rs src/engine/platform/windows_ocr.rs src/engine/platform/mod.rs
git commit -m "feat: prove macro detector backends"
```

---

### Task 3: Implement Immutable Definitions, Assets, and Bounded Journals

**Files:**
- Create: `src/engine/macro_engine/persistence.rs`
- Modify: `src/engine/macro_engine/model.rs`
- Create: `tests/fixtures/macro/packages/valid/macro.json`
- Create: `tests/fixtures/macro/packages/corrupt/macro.json`
- Create: `tests/fixtures/macro/packages/traversal/macro.json`

**Interfaces:**
- Produces: `MacroStore`, `SavedRevision`, `AssetRef`, `AssetStore`, `JournalRecord`, `RunJournal`, and `MacroPackage`.
- Consumes: `MacroDefinition` and template bytes. Task 5 maps `RunEvent` into the neutral `JournalRecord` contract.

- [ ] **Step 1: Write failing asset and atomic-save tests**

```rust
#[test]
fn run_snapshot_pins_template_hash() {
    let temp = tempfile::tempdir().unwrap();
    let store = MacroStore::open(temp.path()).unwrap();
    let asset = store.assets().put_png(&[1, 2, 3, 4]).unwrap();
    let saved = store.save(fixture_definition(asset.clone())).unwrap();
    store.assets().put_png(&[9, 8, 7]).unwrap();
    assert_eq!(saved.definition.image_rules[0].template.content_hash, asset.content_hash);
}
```

- [ ] **Step 2: Implement deterministic storage**

```text
macro_data/
  definitions/<macro-id>/<revision>.json
  definitions/<macro-id>/current.json
  assets/<sha256>.png
  runs/<run-id>.jsonl
```

Add `sha2 = "0.10"`. Write sibling temp files, flush, and rename. Run snapshots pin exact asset hashes and bytes.

- [ ] **Step 3: Write failing package validation tests**

```rust
#[test]
fn import_rejects_outside_asset_reference() {
    let error = MacroStore::validate_package(Path::new("tests/fixtures/macro/packages/traversal"))
        .unwrap_err();
    assert!(error.to_string().contains("outside package"));
}
```

- [ ] **Step 4: Implement folder-package import/export**

Export `manifest.json`, one definition, and referenced assets. Import canonicalizes paths under the package, verifies hashes, remaps colliding IDs, rejects unsupported schemas/corrupt JSON, and never references the source folder after import.

- [ ] **Step 5: Implement bounded journals and orphan cleanup**

Keep state changes, candidates, arbitration, actions, errors, and periodic aggregates. Enforce byte/run caps. Journal failure emits diagnostics but never alters emergency cancellation or final action validation.

- [ ] **Step 6: Run tests and commit**

Run: `cargo test macro_engine::persistence`

Expected: atomic save, hash pinning, corrupt/missing asset, traversal, ID remap, caps, and orphan cleanup pass.

```powershell
git add Cargo.toml Cargo.lock src/engine/macro_engine/model.rs src/engine/macro_engine/persistence.rs tests/fixtures/macro/packages
git commit -m "feat: persist macro revisions and assets"
```

---

### Task 4: Extract Shared Platform Contracts Without Changing Enchant Behavior

**Files:**
- Create: `src/engine/automation.rs`
- Modify: `src/engine/mod.rs`
- Modify: `src/engine/enchant_loop.rs`
- Modify: `src/engine/platform/windows_impl.rs`

**Interfaces:**
- Produces: `CaptureSource`, `InputSink`, `StopSource`, `TargetGuard`, `Clock`, `SystemClock`, and `TargetSnapshot`.
- Consumes: `Rect`, `Point`, `ScreenImage`, and `MouseMovementProfile`.

- [ ] **Step 1: Write compile-time fake implementations**

```rust
struct FakeClock(u64);
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 { self.0 }
}

struct FakeTargetGuard(TargetSnapshot);
impl TargetGuard for FakeTargetGuard {
    fn snapshot(&self) -> anyhow::Result<TargetSnapshot> { Ok(self.0.clone()) }
    fn validate(&self, expected: &TargetSnapshot) -> anyhow::Result<()> {
        anyhow::ensure!(&self.0 == expected, "target changed");
        Ok(())
    }
}
```

- [ ] **Step 2: Move shared traits and adapt enchant imports**

Define `CaptureSource::capture`, `InputSink::move_and_click`, and `StopSource::is_stopped`. Keep adapters in `enchant_loop.rs` so behavior and existing tests do not change.

- [ ] **Step 3: Run baseline behavior tests**

Run: `cargo test engine::`

Expected: original 14 tests pass unchanged.

- [ ] **Step 4: Add explicit preservation tests**

Assert Enchant -> OCR -> Replace -> Close ordering and stop-before-later-action behavior through the extracted adapters.

- [ ] **Step 5: Run all tests and commit**

Run: `cargo fmt --check; cargo test`

Expected: all tests pass.

```powershell
git add src/engine/automation.rs src/engine/mod.rs src/engine/enchant_loop.rs src/engine/platform/windows_impl.rs
git commit -m "refactor: share automation platform contracts"
```

---

### Task 5: Build the Observation-Only Sequential Runtime and Dry Run

**Files:**
- Create: `src/engine/macro_engine/observation.rs`
- Create: `src/engine/macro_engine/runtime.rs`
- Modify: `src/engine/macro_engine/mod.rs`
- Modify: `src/engine/macro_engine/semantics.rs`

**Interfaces:**
- Produces: `MacroRuntime`, `RuntimeCommand`, `RunEvent`, `RunStatus`, `StopReason`, `ObservationToken`, `DetectorEvidence`, `ActionState`, `CompiledMacro`, and `From<RunEvent> for JournalRecord`.
- Consumes: validated saved definitions, capture/clock/detector traits, and no live input in observation-only mode.

- [ ] **Step 1: Write a failing zero-input dry-run test**

```rust
#[test]
fn dry_run_plans_click_but_never_calls_input() {
    let input = RecordingInput::default();
    let runtime = fixture_runtime_with_match(input.clone());
    let events = runtime.run(fixture_click_macro(), RunMode::DryRun).unwrap();
    assert!(events.iter().any(|e| matches!(e, RunEvent::ActionPlanned { .. })));
    assert!(input.calls().is_empty());
}
```

- [ ] **Step 2: Implement immutable compilation and ordered events**

Compile only saved revisions. Pin definition/asset hashes. Use a monotonic clock and increasing event sequence. Treat condition false as normal and technical failures as stop-by-default.

- [ ] **Step 3: Add failing loop and timeout tests**

```rust
#[test]
fn repeat_until_skips_already_satisfied_body() {
    let events = fixture_runtime_true_condition()
        .run(fixture_repeat_until_macro(), RunMode::DryRun)
        .unwrap();
    assert!(!events.iter().any(|e| matches!(e, RunEvent::BlockEntered { block_id, .. } if block_id == "body-click")));
}
```

- [ ] **Step 4: Implement IF, waits, loops, yields, and macro-wide stops**

Check stop during each wait slice. Continuous execution must yield even when its body contains only paced actions. Wait timeouts follow their explicit outcome.

- [ ] **Step 5: Implement bounded command/event channels**

Commands: Start, Pause, Resume, Stop, EmergencyStop, Validate, DryRun, TestDetector. Never drop errors, actions, transitions, arbitration, or stop reasons; coalesce polling progress only.

- [ ] **Step 6: Run tests and commit**

Run: `cargo test macro_engine::runtime; cargo test macro_engine::semantics`

Expected: snapshot, event order, dry-run, timeout, loop, pause invalidation, and stop tests pass.

```powershell
git add src/engine/macro_engine
git commit -m "feat: add observation-only macro runtime"
```

---

### Task 6: Complete Text Detection and OCR Geometry Mapping

**Files:**
- Create: `src/engine/macro_engine/text.rs`
- Modify: `src/engine/platform/windows_ocr.rs`
- Modify: `src/engine/macro_engine/observation.rs`
- Create: `tests/fixtures/macro/text/words.json`

**Interfaces:**
- Produces: `TextDetector`, `OcrWord`, `TextMatch`, and `text_match_rect`.
- Consumes: Task 1 `TextRule` and `PreprocessProfile`, cached Windows OCR, captured frames, and rule/region revisions.

- [ ] **Step 1: Write failing geometry tests**

```rust
#[test]
fn normalized_phrase_maps_to_union_of_word_boxes() {
    let words = vec![
        OcrWord::new("Max", Rect::new(10, 5, 30, 12), 0, 0),
        OcrWord::new("Health", Rect::new(44, 5, 48, 12), 0, 1),
    ];
    let matched = match_text(&words, &TextRule::contains("max health")).unwrap();
    assert_eq!(matched.rect, Some(Rect::new(10, 5, 82, 12)));
}

#[test]
fn absent_condition_has_no_click_geometry() {
    let matched = match_text(&[], &TextRule::absent("Reconnect")).unwrap();
    assert!(matched.rect.is_none());
}
```

- [ ] **Step 2: Return positioned OCR words**

Read OCR lines, words, text, and bounding rectangles; convert to capture-relative integer rectangles and preserve line/word order.

- [ ] **Step 3: Implement normalization with source mapping**

Track normalized characters to source word indices. Union selected boxes. Disable cross-line matches unless explicit. Group repeated text by distinct box union before applying match policy.

- [ ] **Step 4: Implement preprocessing profiles and offline profile benchmark**

Original, Grayscale, High Contrast/Otsu, and Small Text/2x run one selected profile per live poll. Benchmark Profiles evaluates saved samples offline.

- [ ] **Step 5: Run tests and commit**

Run: `cargo test macro_engine::text; cargo test windows_ocr`

Expected: match modes, line policy, repeated text, word union, preprocessing, and no-temp-file tests pass.

```powershell
git add src/engine/macro_engine/text.rs src/engine/macro_engine/observation.rs src/engine/platform/windows_ocr.rs tests/fixtures/macro/text
git commit -m "feat: add positioned Windows OCR detection"
```

---

### Task 7: Complete Image Candidate Clustering and Stability

**Files:**
- Modify: `src/engine/macro_engine/image_match.rs`
- Modify: `src/engine/macro_engine/observation.rs`

**Interfaces:**
- Produces: `CandidateCluster`, `ImageMatchResult`, `StabilityTracker`, and `ImageRuleVerification`.
- Consumes: Task 1 `ImageRule`, Task 2 score maps, and immutable frame metadata.

- [ ] **Step 1: Write failing clustering tests**

```rust
#[test]
fn adjacent_score_peaks_form_one_visual_candidate() {
    let peaks = vec![peak(20, 20, 0.97, 1.0), peak(21, 20, 0.96, 1.0)];
    assert_eq!(cluster_peaks(peaks, ClusterPolicy::default()).len(), 1);
}

#[test]
fn same_object_across_scales_merges_before_exactly_one() {
    let peaks = vec![peak(20, 20, 0.97, 0.95), peak(20, 20, 0.98, 1.0)];
    assert_eq!(cluster_peaks(peaks, ClusterPolicy::default()).len(), 1);
}
```

- [ ] **Step 2: Implement maxima, suppression, and cross-scale merge**

Use fixed product-owned overlap and center-distance rules. Preserve best score, selected scale, distinct runner-up, and ambiguity margin.

- [ ] **Step 3: Write failing stability tests**

```rust
#[test]
fn same_frame_cannot_satisfy_two_frame_stability() {
    let mut tracker = StabilityTracker::new(2, 40, 3);
    assert!(!tracker.observe(match_on_frame(7, 100, 20, 20)));
    assert!(!tracker.observe(match_on_frame(7, 150, 20, 20)));
    assert!(tracker.observe(match_on_frame(8, 160, 21, 20)));
}
```

- [ ] **Step 4: Implement revision-aware stability and verification**

Require distinct frames, elapsed separation, same cluster/scale, drift tolerance, and identical window/geometry/region/rule revisions. Reject low variance, stale DPI, oversized work, insufficient negative margin, and ambiguity. Treat 0.95 only as a starting value.

- [ ] **Step 5: Run tests and commit**

Run: `cargo test macro_engine::image_match`

Expected: clustering, scale merge, masks, ambiguity, stability, revision, and work-limit tests pass.

```powershell
git add src/engine/macro_engine/image_match.rs src/engine/macro_engine/observation.rs tests/fixtures/macro/images
git commit -m "feat: validate stable image matches"
```

---

### Task 8: Add Target-Window Validation and the Live Action Commit Boundary

**Files:**
- Create: `src/engine/platform/windows_target.rs`
- Create: `src/engine/platform/windows_input.rs`
- Modify: `src/engine/platform/mod.rs`
- Modify: `src/engine/platform/windows_impl.rs`
- Modify: `src/engine/macro_engine/runtime.rs`
- Modify: `Cargo.toml`

**Interfaces:**
- Produces: `WindowsTargetGuard`, `ManualInputMonitor`, `WindowsInputSink`, `PreparedAction`, and `ActionCommitter` transitions for Task 5 `ActionState::{Prepared, Committed, Dispatched, UncertainDispatch}`.
- Consumes: Task 4 `TargetSnapshot`, `ObservationToken`, action lock, stop source, destination, button, and movement profile.

- [ ] **Step 1: Add Windows features and a failing target-race test**

Add namespaces for foreground-window identity, client rectangles, DPI, visibility/minimized state, process ID, and process image identity.

```rust
#[test]
fn focus_loss_before_commit_blocks_input() {
    let target = ScriptedTargetGuard::valid_then_invalid();
    let input = RecordingInput::default();
    let result = commit_action(&target, &input, fixture_prepared_action());
    assert!(matches!(result, ActionOutcome::Blocked(BlockReason::TargetChanged)));
    assert!(input.calls().is_empty());
}
```

- [ ] **Step 2: Implement concrete target snapshots**

Snapshot live HWND, process identity, executable path, client rect in screen coordinates, DPI, display profile, visibility, minimized state, and foreground status. Persist only durable matching hints; live HWND remains run-local.

- [ ] **Step 3: Implement action linearization**

Prepared acquires the action lock and remains cancellable. Commit performs final stop/target/geometry/token/bounds/pacing checks and begins `SendInput`. Dispatched records a successful return. A failure after commit records Uncertain Dispatch. Never retry committed or uncertain input.

- [ ] **Step 4: Implement manual-takeover tests and behavior**

Meaningful manual movement or any manual mouse-button event pauses/stops by policy. Runtime-owned movement checks cancellation between segments and revalidates focus immediately before commit. Pause clears observations, candidates, and partial stability; resume takes a fresh target snapshot.

- [ ] **Step 5: Run tests and commit**

Run: `cargo test windows_target; cargo test windows_input; cargo test macro_engine::runtime`

Expected: focus race, ESC before/after commit, bounds, wrong HWND, DPI, takeover, pause/resume, and uncertain-dispatch tests pass.

```powershell
git add Cargo.toml Cargo.lock src/engine/platform src/engine/macro_engine/runtime.rs
git commit -m "feat: guard live macro actions"
```

---

### Task 9: Implement One-Shot Watch Groups and Arbitration

**Files:**
- Create: `src/engine/macro_engine/watch_group.rs`
- Modify: `src/engine/macro_engine/runtime.rs`
- Modify: `src/engine/macro_engine/model.rs`
- Modify: `src/engine/macro_engine/validate.rs`

**Interfaces:**
- Produces: `WatchGroupRunner`, `LaneLatch`, `LaneState`, `CandidateEvent`, `ArbitrationResult`, and `CaptureCoordinator`.
- Consumes: passive detector jobs, tokens, bounded workers, clock, cancellation, and lane-order priority.

- [ ] **Step 1: Write failing one-shot and timeout tests**

```rust
#[test]
fn watch_group_executes_one_winner_then_exits() {
    let result = fixture_watch_runner().run_once(two_ready_lanes()).unwrap();
    assert_eq!(result.winner_lane_id.as_deref(), Some("lane-1"));
    assert_eq!(result.executed_bodies, vec!["lane-1-body"]);
}

#[test]
fn timeout_runs_explicit_timeout_body() {
    let result = fixture_watch_runner().run_once(no_matches_until_timeout()).unwrap();
    assert_eq!(result.executed_bodies, vec!["timeout-body"]);
}
```

- [ ] **Step 2: Implement one-shot lifecycle and bounded scheduling**

Enter, Observe, Qualify, Arbitrate, Commit, Execute, Settle, Exit. Use one capture request, initially one OCR job, at most two serial image jobs, one active detector plus one replaceable newest pending frame per lane, and no FIFO backlog.

- [ ] **Step 3: Write failing latch tests**

```rust
#[test]
fn losing_true_lane_remains_latched_next_entry() {
    let mut runner = fixture_watch_runner();
    runner.run_once(two_ready_lanes()).unwrap();
    assert!(runner.run_once(second_lane_still_true()).unwrap().qualified_lane_ids.is_empty());
    runner.run_once(second_lane_false()).unwrap();
    assert_eq!(
        runner.run_once(second_lane_true_again()).unwrap().winner_lane_id.as_deref(),
        Some("lane-2")
    );
}
```

- [ ] **Step 4: Implement persistent latches and fresh-frame rules**

Every true-qualified winner or loser latches for the run. Later group entries observe it only for false until re-armed. Reset on new run. Side effects, pause/resume, and target changes invalidate frames/candidates.

- [ ] **Step 5: Implement deterministic arbitration**

System safety bypasses arbitration. Lowest unique lane order wins among candidates ready in the internal arbitration window. Stable ID is a corrupt-data fallback only. Discard losing actions; never queue them. Ordinary matches cannot preempt a running body.

- [ ] **Step 6: Add sharing and overload tests**

Verify overlapping lanes share immutable frames/crops, newest pending replaces old, overload reports Polling Delayed, aging prevents detector starvation, and action priority does not grant detector CPU priority.

- [ ] **Step 7: Run tests and commit**

Run: `cargo test macro_engine::watch_group`

Expected: one-shot, timeout, latch, conflicts, safety bypass, stale drop, fairness, and queue bounds pass.

```powershell
git add src/engine/macro_engine
git commit -m "feat: add concurrent macro watch groups"
```

---

### Task 10: Add Enchant/Macro Routing and a Read-Only Macro Shell

**Files:**
- Modify: `src/main.rs`
- Create: `src/macro_ui/mod.rs`
- Create: `src/macro_ui/library.rs`
- Create: `src/macro_ui/timeline.rs`
- Create: `src/macro_ui/monitor.rs`

**Interfaces:**
- Produces: `AppPage`, `MacroPageState`, `MacroPage::show`, timeline rows, and monitor projection.
- Consumes: definitions, validation, and runtime events without changing semantics.

- [ ] **Step 1: Write pure projection tests**

```rust
#[test]
fn watch_group_rows_show_lane_order_as_priority() {
    let rows = project_timeline(&fixture_watch_group_definition());
    assert_eq!(rows.iter().filter_map(|r| r.lane_priority).collect::<Vec<_>>(), vec![1, 2, 3]);
}
```

- [ ] **Step 2: Add explicit top-level routing**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppPage { Enchant, Macro }
```

Add Enchant/Macro top buttons. Render existing content and bottom action bar only for Enchant; render Macro page for Macro. Do not rewrite enchant behavior.

- [ ] **Step 3: Render the read-only shell**

Render library, timeline, inspector empty state, and monitor from model projections. Show running revision versus draft, priorities, loop marker, active block/branch/loop, candidates, runner-up, scale, stability, action state, and stop reason.

- [ ] **Step 4: Add smoke tests and manual route**

Run: `cargo test macro_ui; cargo run`

Expected: projections pass. Manually switch Enchant -> Macro -> Enchant and confirm calibration values remain and the read-only Macro shell cannot inject input.

- [ ] **Step 5: Commit**

```powershell
git add src/main.rs src/macro_ui
git commit -m "feat: add macro page shell"
```

---

### Task 11: Build the Canonical Timeline Editor and Inspector

**Files:**
- Modify: `src/macro_ui/timeline.rs`
- Create: `src/macro_ui/inspector.rs`
- Modify: `src/macro_ui/mod.rs`
- Modify: `src/engine/macro_engine/model.rs`
- Modify: `src/engine/macro_engine/validate.rs`

**Interfaces:**
- Produces: `EditorCommand`, `apply_editor_command`, `ConversionPreview`, `InsertionTarget`, and problem navigation.
- Consumes: canonical blocks and validation.

- [ ] **Step 1: Write failing editor tests**

```rust
#[test]
fn moving_watch_lane_updates_priority_and_requires_revalidation() {
    let mut draft = fixture_three_lane_draft();
    apply_editor_command(&mut draft, EditorCommand::MoveLane { from: 2, to: 0 }).unwrap();
    assert_eq!(draft.lane_ids(), vec!["lane-3", "lane-1", "lane-2"]);
    assert_eq!(draft.status, DraftStatus::NeedsValidation);
}

#[test]
fn unrelated_conversion_requires_replace_preview() {
    assert!(matches!(
        preview_conversion(&fixture_text_wait(), BlockFamily::Loop),
        ConversionPreview::ReplaceRequired { .. }
    ));
}
```

- [ ] **Step 2: Implement model-first editor commands**

Support insert, remove, duplicate, enable, sibling reorder, whole-container move, deliberate THEN/ELSE transfer, loop deletion choice, and priority updates. Reject overlapping loops, detached ELSE, nested Watch Groups, and edits during a run.

- [ ] **Step 3: Implement family-safe conversion and dependency invalidation**

Support text check/waits, image check/waits, left/right click, point/region click, and Repeat N/Until conversions. Preserve shared fields, require new fields, keep removed data only in undo, increment revisions, and invalidate dependent matched clicks.

- [ ] **Step 4: Build type-specific inspectors**

Text: region, expected text, mode, threshold, normalization, profile, poll, timeout, policy, Test OCR, Recapture. Image: region, template, scale, threshold, policy, stability, runner-up, Test Image, Recapture. Flow: timeouts, limits, cooldown, priority.

- [ ] **Step 5: Run tests and manual editor smoke**

Run: `cargo test macro_ui::timeline; cargo test macro_ui::inspector`

Manual: create nested IF/Repeat, reorder containers, move lanes, recapture a rule, and confirm invalid drops are rejected.

- [ ] **Step 6: Commit**

```powershell
git add src/macro_ui src/engine/macro_engine/model.rs src/engine/macro_engine/validate.rs
git commit -m "feat: edit structured macro timelines"
```

---

### Task 12: Build the Guided Wizard and Region/Template Capture

**Files:**
- Create: `src/macro_ui/wizard.rs`
- Modify: `src/macro_ui/mod.rs`
- Modify: `src/macro_ui/inspector.rs`
- Modify: `src/engine/platform/windows_impl.rs`
- Modify: `src/engine/macro_engine/persistence.rs`

**Interfaces:**
- Produces: `WizardState`, `WizardStep`, `WizardOutput`, recapture commands, and template revisions.
- Consumes: drag overlay, detector tests, canonical constructors, and MacroStore.

- [ ] **Step 1: Write a failing wizard-output test**

```rust
#[test]
fn wizard_emits_canonical_editable_blocks() {
    let output = completed_text_click_wizard().finish().unwrap();
    assert!(matches!(output.blocks[0].kind, BlockKind::Continuous { .. }));
    assert!(find_block(&output.blocks, "wait-text").is_some());
    assert!(find_block(&output.blocks, "click-text").is_some());
}
```

- [ ] **Step 2: Generalize the existing drag overlay**

Expose one capture command for target-relative text regions, image search regions, click regions, click points, and templates. Preserve Enchant capture through an adapter.

- [ ] **Step 3: Implement the wizard state machine**

Target -> Region -> Rule -> Detector Test -> Action -> Repetition -> Failure -> Dry Run -> Finish. Finish uses the same constructors and validator as the editor.

- [ ] **Step 4: Implement recapture revision behavior**

Keep logical region ID; increment revision; clear observations/stability; create a new immutable template asset when applicable; invalidate plans/dependent clicks; preserve expected text; offer detector test.

- [ ] **Step 5: Run tests and manual wizard smoke**

Run: `cargo test macro_ui::wizard`

Manual: create text/image macros, recapture, edit expected text, switch mouse button, convert Wait to Check, and verify all blocks remain editable.

- [ ] **Step 6: Commit**

```powershell
git add src/macro_ui src/engine/platform/windows_impl.rs src/engine/macro_engine/persistence.rs
git commit -m "feat: add guided macro wizard"
```

---

### Task 13: Connect Live Runtime, Monitor, Library, and Packages

**Files:**
- Modify: `src/macro_ui/mod.rs`
- Modify: `src/macro_ui/library.rs`
- Modify: `src/macro_ui/monitor.rs`
- Modify: `src/engine/macro_engine/runtime.rs`
- Modify: `src/engine/macro_engine/persistence.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Produces: `MacroController`, library commands, history views, and package commands.
- Consumes: validated revisions, runtime channels, and MacroStore.

- [ ] **Step 1: Write immutable-run controller tests**

```rust
#[test]
fn editing_draft_does_not_mutate_active_run() {
    let controller = fixture_controller();
    controller.start_saved("macro-1").unwrap();
    controller.rename_draft("macro-1", "Edited").unwrap();
    assert_eq!(controller.active_revision().unwrap().name, "Original");
}
```

- [ ] **Step 2: Connect run controls**

Connect Validate, Test, Dry Run, Run Once, Run, Pause, Resume, and Stop. Only saved validated revisions run. Run Once respects nested loops. Continuous Dry Run has Stop and bounded logs.

- [ ] **Step 3: Implement monitor projection**

Show revision, status, block, branch/loops, Watch lanes/priority, iteration, candidate clusters, runner-up, scale, stability, polling delay, last/next action, action state, and exact stop reason.

- [ ] **Step 4: Implement library and package commands**

Create, rename, duplicate, enable, delete confirmation, search, folder export, validated import, history deletion, and orphan cleanup. Display all spec-defined lifecycle badges.

- [ ] **Step 5: Run tests and full manual flow**

Run: `cargo test macro_ui; cargo test macro_engine::runtime; cargo test macro_engine::persistence`

Manual: create -> save -> validate -> dry run -> live run -> pause -> resume -> ESC -> history -> export -> delete -> import -> revalidate. Confirm Enchant remains unchanged.

- [ ] **Step 6: Commit**

```powershell
git add src/main.rs src/macro_ui src/engine/macro_engine
git commit -m "feat: connect macro runtime and library"
```

---

### Task 14: Enforce Corpus, Concurrency, Cancellation, and Endurance Gates

**Files:**
- Modify: `src/bin/macro_detection_bench.rs`
- Create: `tests/macro_adversarial.rs`
- Create: `tests/macro_persistence.rs`
- Create: `tests/macro_watch_group.rs`
- Modify: `README.md`

**Interfaces:**
- Consumes: all Macro contracts.
- Produces: acceptance evidence and user documentation.

- [ ] **Step 1: Add adversarial integration tests**

Cover safety bypassing arbitration, no commit after observed stop/focus loss, committed-input logging, stale tokens, duplicate candidates, losing lane never queued, latch persistence, busy-loop rejection, pause clearing stability, corrupt assets/packages, bounded queues, and journal-failure isolation.

- [ ] **Step 2: Add corpus runner and event-level metrics**

Report stage latency, per-frame recall, event-level detection within 500 ms, false action authorizations, ambiguity margin, and one-sided confidence bound for at least 100,000 held-out negative frames per detector/template family.

- [ ] **Step 3: Enforce measured polling and work budgets**

OCR polling is at least 1.5x measured P95 and above its floor. Typical image exact P95 target is 30 ms; three-scale is 75 ms. Maximum rules require P95 below half polling interval and acceptable CPU. Record reference hardware.

- [ ] **Step 4: Run the eight-hour endurance harness**

Verify memory ceiling/slope, bounded queues/journal, no asset leak, stale action, lost emergency stop, definition mutation, or detector backlog, and successful stop. This is a manual release gate.

- [ ] **Step 5: Update README**

Document navigation, binding, detectors, loops, one-shot Watch Group, Unlimited, Dry Run, ESC, takeover, local data, and folder packages. Do not describe automation as approved, safe, or undetectable.

- [ ] **Step 6: Run final verification**

```powershell
cargo fmt --check
cargo test
cargo build --release
git diff --check
```

Expected: tests pass, release build succeeds, and diff check is clean. Record corpus/endurance results separately.

- [ ] **Step 7: Commit**

```powershell
git add src/bin/macro_detection_bench.rs tests README.md
git commit -m "test: harden macro v1 release gates"
```

---

## Spec Coverage Matrix

| Design spec section | Implementation tasks |
|---|---|
| Purpose and product boundary | Global Constraints, Tasks 10, 13, 14 |
| Navigation and page structure | Tasks 10, 13 |
| Wizard and canonical editor | Tasks 11, 12 |
| Timeline vocabulary and no jumps | Tasks 1, 5, 11 |
| Unlimited semantics | Tasks 1, 5, 9, 13 |
| Target, regions, and revisions | Tasks 3, 8, 11, 12 |
| Windows text detector and preprocessing | Tasks 2, 6, 14 |
| imageproc detector, clustering, and stability | Tasks 2, 7, 14 |
| Shared observation token | Tasks 5, 6, 7, 8 |
| One-shot Watch Group and conflicts | Tasks 1, 9, 13, 14 |
| Runtime and cancellation architecture | Tasks 4, 5, 8, 9 |
| Preflight and side-effect safety | Tasks 1, 8, 13, 14 |
| Persistence, assets, journal, packages | Tasks 3, 13, 14 |
| Typed outcomes and dispatch states | Tasks 5, 8, 13 |
| Performance and accuracy targets | Tasks 2, 7, 14 |
| Unit, integration, adversarial, endurance tests | Every task; consolidated in Task 14 |
| Explicit deferrals | Global Constraints and Task 1 validation |
| Audited implementation order | Tasks 1 through 14 in listed order |

Self-review result: all design sections have an owning task. No feature requirement is intentionally left without an implementation or verification gate.

---

## Plan Completion Gate

Before calling Macro V1 complete, confirm:

- Every spec section maps to a task above.
- Existing Enchant behavior and original tests remain green.
- No setting can create an unpaced busy loop, unbounded queue, or concurrent side effect.
- Text and image clicks consume only their own fresh detector tokens.
- Watch Groups are one-shot, timed or explicitly Unlimited, and preserve latches across loop entries.
- ESC/focus safety bypasses arbitration; Prepared/Committed/Dispatched semantics are visible and tested.
- Template assets and active revisions are immutable and hash-pinned.
- Corpus and endurance evidence meets audited gates before automatic clicking is enabled by default.
