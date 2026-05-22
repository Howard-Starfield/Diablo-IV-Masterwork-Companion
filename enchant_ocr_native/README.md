# BoBo Companion Native Enchant OCR

Native Rust frontend for the Occultist affix reroll workflow.

## Dev Run

```powershell
cd enchant_ocr_native
cargo run
```

## Workflow

1. Set the Enchant button point.
2. Set the affix OCR region.
3. Set the Replace button point.
4. Set the Close button point.

The app autosaves calibration to `enchant_config_native.json` next to the running
`.exe` and reloads it on launch. Start Bot runs:

`Enchant -> OCR scan -> stop on target -> Replace Affix -> Close -> repeat`

Press `ESC` while the bot is running to stop immediately.

Set Max Attempts to `0` for infinite attempts.

## App Icon

Save the source logo image locally, then generate the rounded-square app icons:

```powershell
python .\scripts\make_app_icon.py --source .\assets\AffixReroll.png
```

The app loads `app_icon.png` from the running `.exe` folder. Release builds also
embed `assets\app_icon.ico` into the Windows executable when that file exists.
