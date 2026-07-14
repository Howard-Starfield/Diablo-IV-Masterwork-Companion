# Diablo Masterwork Companion

A Windows companion for Diablo IV enchanting. It watches the affix result, compares it with your target, and repeats rerolls until it finds a match or you stop it.

The app also includes a native Macro workspace for building validated, local text- and image-driven timelines. The Macro UI is independently implemented; its canvas is a view of the canonical block tree, and connections cannot bypass canonical validation.

![Gif](https://imgur.com/a/gHw9iVu)


## What It Offers

- Saves your button and affix-area setup.
- Checks each reroll result for your target affix.
- Stops when a match is found.
- Lets you stop any time with `ESC`.
- Supports unlimited attempts by setting max attempts to `0`.

## How To Use

1. Open the app.
2. Select the enchant window.
3. Mark the enchant button, affix result area, replace button, and close button.
4. Enter the affix you want.
5. Start the bot.
6. Press `ESC` to stop.

## Macro workspace

The preferred workspace is 900 x 1080 pixels. On smaller displays, the library and inspector become responsive drawers so the canvas keeps priority. `Always on top` starts off, applies immediately when toggled, and is persisted separately from Enchant configuration and Macro definitions.

Create or select a Macro, bind its target window, and add structured blocks in the canvas and inspector. The canvas supports:

- Empty-space or middle-button pan, plus wheel or pinch zoom.
- Node movement, checked connector drops, `Fit view`, and `Auto arrange`.
- `Undo` and `Redo` for canonical edits and layout-only canvas edits.
- Per-Macro persisted node positions, pan, zoom, and drawer widths; a damaged layout is rebuilt from the canonical definition.

Use the inspector to edit block settings, recapture target/region/template inputs, test detectors, and review validation. Image-based rules must be locally re-verified after an imported package before they can be used. `Dry Run` is observation-only and injects no input. `Run once`, continuous observation, and `Run Live` have distinct run states; `Pause`, `Resume`, `Stop`, and `ESC` remain available according to the active run state.

Watch Groups are one-shot, ordered lanes. Continuous behavior belongs in a loop. Unlimited removes only the chosen user limit; it does not remove cancellation, target checks, pacing, queue bounds, or validation.

## Manual UAT route

Run this route on the normal Windows display and DPI setup after building the app:

```text
launch -> confirm readable size and reachable window -> toggle Always on top -> restart ->
confirm Always on top persisted -> Enchant calibration -> OCR test -> Start -> Stop ->
Macro create -> bind target -> add Observe -> IF -> THEN -> ELSE -> Act -> Continuous Loop ->
empty-space pan -> wheel zoom -> node move -> valid connector -> reject invalid connector ->
Fit view -> Auto arrange -> Undo -> Redo -> save -> reopen -> confirm layout persisted ->
open inspector -> edit -> recapture -> detector test -> validate -> Dry Run -> Run once ->
Run Live -> Pause -> Resume -> Stop -> ESC -> history -> export -> delete -> import ->
local image re-verification
```

Record the display resolution, Windows scaling, whether the full window remained reachable, any text that still felt too small, and any canvas gesture conflict. The Enchant path above is a regression check: its capture, OCR, configuration, and Start/Stop behavior must remain unchanged.

## Build

```powershell
cargo build --release
```

The app is created at:

```text
target/release/diablo_masterwork_companion.exe
```

## Ownership

Copyright (c) 2026 Howard Starfield. All rights reserved.

This project is not affiliated with or endorsed by Blizzard Entertainment..
