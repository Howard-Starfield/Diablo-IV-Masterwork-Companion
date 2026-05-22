# Enchant OCR Backend

Rust backend for the Diablo IV Enchant OCR workflow.

## Scope

This crate is backend-only. It contains:

- Config persistence structures for calibrated enchant settings.
- OCR text normalization and target matching.
- The enchant loop state machine.
- Windows adapters for region capture, native Windows OCR, mouse clicks, and ESC stop polling.
- A small CLI harness for backend smoke tests.

The native egui app in `../enchant_ocr_native` is built against these same config and event shapes.

## Workflow

The live loop is:

1. Click `Enchant`.
2. Wait for the result text.
3. Capture and OCR the calibrated result region.
4. Stop if a target affix matches.
5. Otherwise click `Replace Affix`.
6. Click `Close`.
7. Continue until ESC, max attempts, or target found.

## CLI

```powershell
cargo run -- sample-config enchant_config.sample.json
cargo run -- match "Maximum Life" "Max Health"
cargo run -- ocr-file .\some-crop.png
cargo run -- ocr-region 100 100 400 120
cargo run -- run enchant_config.sample.json
```

`sample-config` writes a live-run sample. Use it only after real calibration values are saved by the UI.

## Frontend Contract

The frontend should persist or send an `EnchantConfig` with:

- `targets`
- `fuzzy_threshold`
- `max_attempts`
- `enchant_window`
- `ocr_region`
- `enchant_button`
- `replace_button`
- `close_button`
- wait timings

The loop emits `EnchantEvent` values that map cleanly to a status timeline, live OCR panel, and final outcome.
