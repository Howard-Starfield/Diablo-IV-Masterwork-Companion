# Diablo Masterwork Companion

A Windows companion for Diablo IV enchanting. It watches the affix result, compares it with your target, and repeats rerolls until it finds a match or you stop it.
![Description of the GIF](https://imgur.com/a/gHw9iVu)
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

This project is not affiliated with or endorsed by Blizzard Entertainment.
