# Enchant calibration

Enchant calibration lets a user mark the game window and control points, set a target affix, probe OCR on the text area, and Start/Stop the reroll loop. Automated verification stays on companion chrome and safe probes unless the user requests a game-attached run.

## Sub-features

- `enchant-labels` shows the calibration step buttons with their fixed labels.
- `enchant-check-text` offers `Check text area` when a region exists (may error without a real capture).
- `enchant-start-stop-chrome` shows `Start` / `Stop` on the bottom action bar.
- `enchant-esc` documents ESC as the user stop path (manual or game-attached only).

## How to get to it (user POV)

- Open the app on the `Enchant` tab (default).
- Use `Select game window`, `Capture text area`, `Set Enchant button`, `Set Replace button`, `Set Close button`.
- Enter `Target affix`, optionally `Check text area`, then `Start` / `Stop` or ESC.

## Driving it with control-bobo

Preconditions:

- Owned instance on the Enchant page (`click-label-approx -Label enchant` if needed).
- Do not require Diablo IV for the default proof.

- **Visible calibration.** Screenshot `artifacts/enchant-calibration/chrome.png`. Image shows labels from `ENCHANT_LABELS`: `Select game window`, `Capture text area`, `Target affix`, `Set Enchant button`, `Set Replace button`, `Set Close button`.
- **Action bar.** Screenshot includes `Start` and `Stop` (bottom bar).
- **No game injection.** Do not click `Start` in CI-style runs without an explicit user request and a prepared enchant window; if Start is clicked without calibration, expect a status/error rather than success — record that as chrome behavior, not a full bot proof.
- **Logic regression (optional).** `control-bobo.ps1 cargo-test -Filter enchant` only if matching tests exist; otherwise rely on UI chrome screenshots plus any OCR unit tests already in the suite.

## Gotchas

- Capture flows enter a drag-selection mode that can steal the session; prefer screenshots over starting capture in unattended runs.
- Config is written next to the staged exe as `enchant_config_native.json`; the sandbox launch starts from `{}`.
- Full README Manual UAT route for Enchant is a human regression when OCR/game focus matter.
