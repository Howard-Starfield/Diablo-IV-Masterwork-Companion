# Diablo Masterwork Companion

Native Rust companion for the Diablo IV enchant/reroll workflow.

## Active Project

- `enchant_ocr_native`: egui desktop app and calibration UI.
- `enchant_ocr_backend`: OCR, matching, mouse input, screen capture, and enchant loop logic.

The old Python implementation and Tauri frontend have been removed from this workspace. The Rust native GUI is the supported app.

## Development

```powershell
cargo run -p enchant_ocr_native
```

## Test

```powershell
cargo test
```

## Release Build

```powershell
cargo build --release -p enchant_ocr_native
```

The release exe is written to:

```text
target/release/enchant_ocr_native.exe
```

User settings are stored next to the running `.exe` as `enchant_config_native.json`.
