# Helcome to Diablo-IV-Masterwork-Companion!

A Python-based automation tool designed to assist with the masterworking process in Diablo IV by automatically detecting and selecting desired affixes.

## ⚠️ IMPORTANT DISCLAIMERS ⚠️

**READ THIS BEFORE USING:**

- This is an **unofficial third-party tool** not affiliated with or endorsed by Blizzard Entertainment
- Use of automation tools may **violate Diablo IV's Terms of Service**
- Your account could face **penalties, suspension, or permanent bans**
- **You assume all risks** associated with using this software
- The developers are **not responsible** for any account actions taken by Blizzard
- **Check the current Terms of Service** before deciding to use this tool

## Features

- **Automatic Affix Detection**: Recognizes when your desired masterwork affix appears
- **Human-like Mouse Movement**: Configurable natural movement patterns with randomization
- **Failsafe Mechanisms**: Multiple safety stops including ESC key and mouse corner failsafe
- **Customizable Settings**: Adjustable detection thresholds, movement speeds, and success targets
- **Sound Effects**: Optional audio notifications for start/completion (requires pygame)
- **Statistics Tracking**: Real-time tracking of attempts, successes, and failures
- **Developer Logs**: Detailed debugging information for troubleshooting

## System Requirements

- **Operating System**: Windows (tested)
- **Python**: Version 3.7 or higher
- **Screen Resolution**: Any (tool captures specific UI elements)
- **Game**: Diablo IV
## Installation

### Method 1: Python Installation (Recommended for developers)

1. **Install Python 3.7+** from [python.org](https://python.org)

2. **Install required dependencies:**
   ```bash
   pip install opencv-python numpy pyautogui keyboard Pillow
   ```

3. **Optional: Install pygame for sound effects:**
   ```bash
   pip install pygame
   ```

4. **Download the tool** and extract to a folder

5. **Run the application:**
   ```bash
   python "Diablo-IV-Masterwork-Companion.py"
   ```

### Method 2: Executable (If available)

1. Download the executable release
2. Extract to a folder
3. Run the `.exe` file
4. All dependencies are included

## Required Resource Files

The tool requires several reference images to function. These should be placed in a `resources/` folder:

**Required Images:**
- `upgrade_1.png` - The upgrade button
- `upgrade_2.png` - Alternate upgrade button state
- `upgrade_3.png` - Third upgrade button variant
- `skip.png` - The skip button
- `reset.png` - The reset button
- `confirm.png` - The confirmation dialog button
- `affix.png` - **YOUR DESIRED AFFIX** (captured during setup)

**Optional Images:**
- `mw_window.png` - Reference image for tooltips

**Sound Files** (optional, in `resources/sound/`):
- `start_effect.mp3` - Plays when bot starts
- `done.mp3` - Plays when masterworking completes

## Setup Guide

### 1. Initial Setup

1. **Launch Diablo IV** and navigate to the Blacksmith
2. **Open the Masterworking interface**
3. **Place your item** in the masterworking slot

### 2. Capture Your Desired Affix
This is the **most critical step**:
1. **Perform one manual upgrade** in Diablo IV to reveal potential affixes
2. Click **"Drag to Select"** button in the tool
3. When the affix selection screen appears, **drag to select ONLY your desired affix**
4. Be precise - capture only the affix text you want
5. The tool will save this as `affix.png` and keep a record of them in subfolder `screenshot_history`

**Tips for affix capture:**
- Make sure the affix is clearly visible and unobstructed
- Capture just the affix text, not surrounding elements
- Avoid capturing when other UI elements overlap
- Test different lighting/contrast if detection fails

### 3. Configure Settings

**Detection Thresholds:**
- **Affix Detection**: 0.80 (higher = more strict matching)
- **Other Elements**: 0.50 (for buttons like upgrade, skip, etc.)

**Mouse Behavior:**
- Enable human-like movement for more natural automation
- Adjust speed multipliers (higher = faster movement)
- Configure click randomness

**Success Target:**
- Choose 2 or 3 consecutive successes before stopping

## Usage Instructions

### Starting the Bot

1. **Ensure Diablo IV is running** and focused
2. **Navigate to Blacksmith > Masterworking**
3. **Place your item** in the masterworking interface
4. **Click "START BOT"** in the helper tool

### During Operation

- The bot will automatically click upgrade buttons
- It monitors for your desired affix after each attempt
- **Press ESC** at any time to stop the bot immediately
- **Move mouse to screen corner** to trigger PyAutoGUI failsafe

### When Complete

- Bot stops automatically after reaching success target
- Completion sound plays (if enabled)
- Statistics show total attempts and success rate

## Safety Features

**Multiple Failsafe Mechanisms:**
- **ESC Key**: Immediately stops all automation
- **PyAutoGUI Failsafe**: Move mouse to corner to stop
- **Window Focus Detection**: Pauses when Diablo IV loses focus
- **Maximum Attempt Limits**: Prevents infinite loops
- **Configurable Delays**: Reduces detection as automation

## Troubleshooting

### Common Issues

**"Missing required images" error:**
- Ensure all reference images are in the `resources/` folder
- Check that image files are PNG format
- Verify filenames match exactly (case-sensitive)

**Bot doesn't detect buttons:**
- Lower the "Other Elements Threshold" in settings
- Ensure Diablo IV UI language is English
- Check that UI elements aren't obstructed
- Try different screen resolution or UI scale

**Affix detection not working:**
- Recapture your desired affix with more precision
- Adjust "Affix Detection Threshold" (try 0.70-0.85)
- Ensure affix image captures only the text/icon
- Verify the affix appears exactly as captured

**Bot clicks wrong locations:**
- Check Windows display scaling settings
- Ensure Diablo IV is in Fullscreen mode
- Recapture reference images at current resolution

### Performance Tips

- **Close unnecessary applications** to improve detection speed
- **Use borderless windowed mode** for better compatibility
- **Ensure stable FPS** in Diablo IV for consistent UI
- **Avoid moving/resizing** the Diablo IV window after setup

## Development & Contribution

### Project Structure
```
Diablo-IV-Masterworker-Helper/
├── Diablo-IV-Masterwork-Companion.py  # Main application
├── resources/                      # Reference images
│   ├── *.png                      # UI element images
│   └── sound/                     # Sound effects
│       └── *.mp3
├── README.md                      # This file
└── requirements.txt               # Python dependencies
```

### For Developers

The code is structured with modular classes:
- `ImageManager`: Handles reference image loading
- `BotEngine`: Core automation logic
- `MouseUtils`: Human-like movement algorithms
- `SoundManager`: Audio effect handling
- `MasterworkerGUI`: User interface

### Contributing

1. Fork the repository
2. Create a feature branch
3. Test thoroughly with various scenarios
4. Submit a pull request with detailed description

## Known Limitations

- **English UI Tested only** - Other languages unsure
- **Windows primarily tested** - macOS/Linux may have issues
- **Screen resolution dependent** - May require recapture at different resolutions
- **Game updates** - Major UI changes may break functionality

## Version History

- **v1.0.0** - Initial release with basic automation
- **v1.1.0** - Added human-like mouse movement
- **v1.2.0** - Improved detection algorithms and GUI enhancements

## Support & Contact

- **Issues**: Report bugs via GitHub issues (if applicable)
- **Discussions**: Community forum or Discord (https://discord.gg/E2vFCTCk)
- **Updates**: Check for new releases regularly

## Legal Notice

This software is provided "as is" without warranty of any kind. The developers disclaim all liability for any damages or account actions resulting from use of this tool. Users are responsible for complying with all applicable terms of service and local laws.

**Diablo IV** is a trademark of Blizzard Entertainment, Inc. This project is not affiliated with or endorsed by Blizzard Entertainment.

---

**Remember: Use at your own risk. Game automation tools can result in account penalties or bans.**