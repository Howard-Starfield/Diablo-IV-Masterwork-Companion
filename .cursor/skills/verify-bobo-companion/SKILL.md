---
name: verify-bobo-companion
description: Drive BoBo Companion (Diablo Masterwork Companion), the Windows egui desktop app for Enchant automation and Macro authoring. Use when proving UI or behavior changes in this repo, running UAT-style checks, or capturing screenshots and ui-state evidence after a change.
---

# Verify BoBo Companion

Agent-facing control skill for the real Windows desktop app. Window title is `BoBo Companion`. Binary name is `diablo_masterwork_companion.exe`. The UI is egui (no accessibility tree): drive via `scripts/control-bobo.ps1`, screenshots, sandbox `ui-state.json`, and `cargo test` for canvas/runtime logic.

## Launch

From the repo root (or any cwd; the script resolves the repo via this skill path):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .cursor/skills/verify-bobo-companion/scripts/control-bobo.ps1 launch
```

What launch does:

1. Refuses if a foreign `BoBo Companion` window is already open (single-instance mutex `Local\BoBoCompanion.SingleInstance.v1`).
2. Builds with `cargo build` if `target/debug/diablo_masterwork_companion.exe` is missing (`-Release` for release).
3. Copies the exe into `.cursor/skills/verify-bobo-companion/run/bin/` with a blank `enchant_config_native.json` beside it.
4. Sets `LOCALAPPDATA` and `APPDATA` under `.cursor/skills/verify-bobo-companion/run/LocalAppData` so `ui-state.json`, Macro Authoring, and legacy enchant-config migration cannot touch the user's real profile.
5. Waits until the owned window appears and writes `run/session.json` with the PID.

Ready when: `doctor` exits 0 and reports `owned_instance: true`.

Teardown: `cleanup` (see Cleanup). Never start a second instance alongside the user's app.

## Doctor

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .cursor/skills/verify-bobo-companion/scripts/control-bobo.ps1 doctor
```

Read-only. Requires an owned verification session: window title `BoBo Companion`, HWND PID matches `run/session.json`, staging exe present, sandbox `LOCALAPPDATA` path recorded. Exit code `2` if unhealthy. Run doctor first whenever anything looks off. If `foreign_instance` is true, stop — do not click the user's window.

## Drive

Harness: `scripts/control-bobo.ps1`. Prefer feature files under `features/` over inventing clicks.

```powershell
$ctrl = ".cursor/skills/verify-bobo-companion/scripts/control-bobo.ps1"

# Owned-window only
powershell -NoProfile -ExecutionPolicy Bypass -File $ctrl click-label-approx -Label macro
powershell -NoProfile -ExecutionPolicy Bypass -File $ctrl click-rel -X 0.30 -Y 0.028
powershell -NoProfile -ExecutionPolicy Bypass -File $ctrl screenshot -Path .cursor/skills/verify-bobo-companion/artifacts/app-shell/macro.png
powershell -NoProfile -ExecutionPolicy Bypass -File $ctrl read-ui-state
powershell -NoProfile -ExecutionPolicy Bypass -File $ctrl cargo-test -Filter layout_edits_do_not_change_saved_or_running_identity
```

Stable user-facing labels (click targets / OCR expectations), not coordinates:

| Surface | Labels |
|---------|--------|
| Chrome | `BoBo Companion`, `Enchant`, `Macro`, `Always on top` |
| Enchant | `Select game window`, `Capture text area`, `Target affix`, `Set Enchant button`, `Set Replace button`, `Set Close button`, `Check text area`, `Start`, `Stop` |
| Macro | `New Macro`, `Search`, `Fit view`, `Auto arrange`, `Dry Run`, `Pause`, `Resume`, `Import`, `Export`, `Delete selected` |

`click-label-approx` maps those chrome/action names to client fractions calibrated for the preferred **900×1080** layout at ~100% DPI. On other DPI/sizes, use `window-info`, adjust with `click-rel`, and record the fractions in the proof notes. egui has no ARIA roles — do not invent browser selectors.

Logic-heavy Macro canvas / validation proofs: use `cargo-test` with filters from the feature map (acceptance tests in `src/macro_ui/acceptance_tests.rs`). That is the reliable path when a live canvas gesture would be DPI-fragile.

Do not drive Enchant `Start` against a live Diablo IV window in automated verification unless the user explicitly asks; default proofs stay inside the companion UI, sandbox state, Dry Run observation, and tests.

## Evidence

Proof root (survives cleanup):

`.cursor/skills/verify-bobo-companion/artifacts/<feature-id>/`

Standards:

- Exercise the real UI path for shell/tab/toggle proofs; do not poke private setters.
- Capture the action and the resulting state: screenshot before/after, plus `read-ui-state` when proving `Always on top` or layout persistence.
- Side effects: sandbox `run/LocalAppData/BoBo Companion/ui-state.json`, staged `enchant_config_native.json`, `cargo test` stdout/exit code. Copy or re-run `read-ui-state` into the artifacts folder before cleanup if you need the JSON kept.
- Never treat Dry Run as Live: Dry Run must not inject input; prove via run monitor / status text and absence of unintended focus changes when feasible.
- Record feature ID, entry point, display resolution, and Windows scaling in a short `notes.txt` beside screenshots when DPI mattered.

## Cleanup

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .cursor/skills/verify-bobo-companion/scripts/control-bobo.ps1 cleanup
```

Stops only the PID in `run/session.json`, then deletes `run/LocalAppData`, `run/bin`, and `run/session.json`. Does **not** delete `artifacts/`. Never `Stop-Process -Name diablo_masterwork_companion` — that can kill the user's session.

## Helpers

| Command | Purpose |
|---------|---------|
| `launch` | Build if needed, sandbox LOCALAPPDATA, start staged exe, record session |
| `doctor` | Owned-instance health JSON; exit 2 if bad |
| `screenshot -Path <png>` | PrintWindow capture of owned HWND |
| `click-rel -X <0..1> -Y <0..1>` | Click client-relative point |
| `click-label-approx -Label <enchant\|macro\|always-on-top\|start\|stop\|new-macro>` | Approximate label click |
| `read-ui-state` | Print sandbox `ui-state.json` |
| `window-info` | Owned window screen rect |
| `cargo-test [-Filter <name>]` | Run repo tests |
| `cleanup` | Kill owned PID; wipe run sandbox; keep artifacts |

Invocation pattern is always:

`powershell -NoProfile -ExecutionPolicy Bypass -File .cursor/skills/verify-bobo-companion/scripts/control-bobo.ps1 <command> ...`
