# BoBo Companion verification map

This directory is the maintained source for verifying user-facing behavior of BoBo Companion (Diablo Masterwork Companion). Read the index before driving the app, then use the matching feature file as the recipe.

## Baseline preconditions

- Close any user-owned `BoBo Companion` window first (single-instance mutex).
- Launch via `scripts/control-bobo.ps1 launch` so `LOCALAPPDATA` is the skill `run/LocalAppData` sandbox.
- Run `control-bobo.ps1 doctor` and require `owned_instance: true`.
- Preferred window size is 900×1080; record Windows display scaling in proof notes when clicks miss.
- Never drive an instance that was not started by this verification run.
- Keep Diablo IV out of automated Enchant Start/Live proofs unless the user explicitly requests a game-attached run.

## Driving conventions

- Start every recipe from a fresh `launch` unless preconditions say otherwise.
- Prefer documented label names (`Enchant`, `Macro`, `Always on top`, `New Macro`, …) over raw pixels; use `click-label-approx` or calibrated `click-rel`.
- Treat every `control-bobo.ps1` command as literal.
- Use `cargo-test` for Macro canvas/layout/validation proofs listed in the feature files.
- Restore or discard sandbox state with `cleanup`. Do not remove proof artifacts under `artifacts/`.

## Proof and skip reporting

- Capture the user action and the resulting state, not only the final screen.
- UI proof includes a window screenshot with `BoBo Companion` chrome visible and, when relevant, sandbox `ui-state.json`.
- Test proof includes the filter name, exit code, and failing assertion text if any.
- Mutation proof for toggles includes a second read of `ui-state.json` after restart when persistence is claimed.
- Record the feature ID and entry point with every artifact.
- Report an unreachable path with the attempted command and unmet precondition.
- Do not report a skipped live entry point as verified only through unit tests without saying so.

## Feature entry contract

Each feature file starts with an H1 title and one paragraph describing the user-visible behavior. It then uses exactly four H2 sections in this order.

1. `Sub-features`
2. `How to get to it (user POV)`
3. `Driving it with control-bobo`
4. `Gotchas`

## Features

- [App shell](./app-shell.md) covers launch, chrome tabs, Always on top, and owned-instance isolation.
- [Enchant calibration](./enchant-calibration.md) covers calibration labels, Check text area, Start/Stop chrome without game injection.
- [Macro library](./macro-library.md) covers New Macro draft creation and library chrome.
- [Macro canvas](./macro-canvas.md) covers Fit view / layout persistence via live chrome plus acceptance tests.
- [Macro dry run](./macro-dry-run.md) covers Dry Run as observation-only versus Live.
