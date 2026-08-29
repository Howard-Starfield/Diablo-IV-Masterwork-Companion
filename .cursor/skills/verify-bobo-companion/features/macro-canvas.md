# Macro canvas

Macro canvas is the visual editor for the canonical block tree: pan, zoom, node move, connectors, Fit view, Auto arrange, and Undo/Redo, with layout persisted separately from the definition.

## Sub-features

- `canvas-fit` runs Fit view without changing the saved definition identity.
- `canvas-arrange` runs Auto arrange.
- `canvas-layout-persist` stores node positions in ui-state keyed by macro id.
- `canvas-accept` covers connection validation and layout-vs-definition separation in tests.

## How to get to it (user POV)

- Open `Macro`, create or select a macro with blocks on the canvas.
- Use empty-space / middle-button pan, wheel zoom, drag nodes, connect ports.
- Choose `Fit view` or `Auto arrange`; use Undo/Redo for edits.

## Driving it with control-bobo

Preconditions:

- Owned instance with a draft or saved macro that has canvas nodes (`New Macro` is enough for a starter observe block).
- For definition/layout proofs, prefer cargo tests when DPI makes live gestures unreliable.

- **Live chrome (smoke).** After `New Macro`, screenshot `artifacts/macro-canvas/draft.png` showing the canvas. If `Fit view` is visible, note it in `notes.txt`; live click of Fit view is optional and coordinate-fragile.
- **Layout vs definition.** Run `control-bobo.ps1 cargo-test -Filter layout_edits_do_not_change_saved_or_running_identity`. Exit code 0.
- **Persisted layout.** Run `cargo-test -Filter persisted_layout_round_trip_leaves_definition_unchanged`. Exit code 0.
- **Corrupt layout recovery.** Run `cargo-test -Filter corrupt_layout_recovery_preserves_canonical_definition`. Exit code 0.
- **Proof.** Save cargo test transcript lines (or a short `artifacts/macro-canvas/cargo-test.txt` you write from the output) plus the optional screenshot.

## Gotchas

- Canvas gestures conflict with verification clicks; do not flail random `click-rel` on the canvas.
- A damaged layout is rebuilt from the canonical definition — proving recovery is a test path, not a hand-corrupted live file in the user's store.
- Reveal-on-run must not rewrite saved layout; covered in acceptance tests / monitor projection, not the default smoke.
