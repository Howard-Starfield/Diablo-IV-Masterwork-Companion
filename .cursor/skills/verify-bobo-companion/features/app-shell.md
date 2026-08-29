# App shell

App shell is the always-visible chrome: window title `BoBo Companion`, `Enchant` / `Macro` tabs, and `Always on top`, including persistence of that toggle in UI preferences.

## Sub-features

- `shell-launch` opens a single owned window at a readable size.
- `shell-tabs` switches between Enchant and Macro pages.
- `shell-aot-toggle` turns Always on top on and off from the chrome checkbox.
- `shell-aot-persist` keeps Always on top across restart via sandbox `ui-state.json`.
- `shell-single-instance` refuses a second launch while a window exists.

## How to get to it (user POV)

- Start the app (verification: `control-bobo.ps1 launch`).
- Choose `Enchant` or `Macro` in the top chrome.
- Toggle `Always on top` in the top chrome.
- Quit and reopen to confirm the toggle persisted.

## Driving it with control-bobo

Preconditions:

- No foreign `BoBo Companion` window.
- `control-bobo.ps1 doctor` reports `owned_instance: true` after launch.

- **Launch.** Run `control-bobo.ps1 launch`. Doctor shows `owned_instance: true` and `window_title` `BoBo Companion`.
- **Enchant chrome.** Run `control-bobo.ps1 click-label-approx -Label enchant` then `screenshot -Path .../artifacts/app-shell/enchant.png`. The capture shows `Enchant assistant` or Enchant calibration labels such as `Select game window`.
- **Macro tab.** Run `click-label-approx -Label macro` then screenshot `.../artifacts/app-shell/macro.png`. The capture shows Macro library chrome (`New Macro` or `Search`).
- **Always on top on.** Run `click-label-approx -Label always-on-top`, wait ~300ms, run `read-ui-state`. JSON includes `"always_on_top": true` (or the toggled value relative to the previous read). Save the JSON text under `artifacts/app-shell/ui-state-after-aot.json`.
- **Persistence (optional longer proof).** `cleanup`, `launch` again with the same sandbox only if you copied ui-state back — default launch wipes `run/LocalAppData`. For persistence, copy `ui-state.json` aside before cleanup, restore it into the new sandbox after launch, or toggle then read without wiping. Prefer: toggle, `read-ui-state`, quit via `cleanup` only after copying evidence.
- **Proof.** Keep both screenshots and the ui-state snippet. `notes.txt` should list display resolution and scaling.

## Gotchas

- Single-instance mutex: launch fails if the user already has the app open.
- Default `launch` resets the sandbox LocalAppData; copy ui-state into `artifacts/` before `cleanup` if you need it.
- `click-label-approx` for `macro` is calibrated near client `(0.36, 0.035)` on a ~916×1120 window; `(0.30, 0.028)` can miss and leave Enchant selected. Use `window-info` and adjust if the tab does not switch.
- Never `Stop-Process -Name diablo_masterwork_companion`.
