# Operator land commands (bottom-up)

All four PRs MERGEABLE. No CI checks configured. Autopilot-stack: you land.

```powershell
$env:GH_TOKEN = ...  # or gh auth login
gh pr merge 1 --merge
gh pr merge 2 --merge
gh pr merge 3 --merge
gh pr merge 4 --merge
```

Or click Merge on GitHub in order:
1. https://github.com/Howard-Starfield/Diablo-IV-Masterwork-Companion/pull/1
2. https://github.com/Howard-Starfield/Diablo-IV-Masterwork-Companion/pull/2
3. https://github.com/Howard-Starfield/Diablo-IV-Masterwork-Companion/pull/3
4. https://github.com/Howard-Starfield/Diablo-IV-Masterwork-Companion/pull/4

After all four are on `main`, reply **landed** so Close can finish.
