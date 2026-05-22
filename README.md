# Diablo Masterwork Companion

A small Windows companion for Diablo IV enchanting. It watches the affix result, compares it with your target, and repeats the reroll flow until it finds a match or you stop it.

## How To Use

1. Open the app.
2. Select the enchant window and mark the buttons/affix area.
3. Enter the affix you want.
4. Start the bot.
5. Press `ESC` at any time to stop.

Set max attempts to `0` to keep rerolling until a match is found.

## Build

```powershell
cargo build --release -p enchant_ocr_native
```

The app is created at:

```text
target/release/enchant_ocr_native.exe
```
