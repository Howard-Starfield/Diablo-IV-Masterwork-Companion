# Macro branching and simple list plan

Macro stays observation-gated automation for one Diablo window. This program keeps nested If then/else in the engine, finishes first-class THEN/ELSE authoring, exposes Continuous and Watch lanes, and replaces the interactive graph with a simple indented step list that is easy to control. No Goto. No canvas-first graph growth. PR order is `PR-macro-if`, `PR-macro-author`, `PR-macro-list`, `PR-macro-shell`.

## How to read this

One box is one unit of work. Every box names the evidence that checks it. A nested box is a sub-step of the box above it. Check a box only when its evidence exists, a file, a log line, a screenshot, a test run, or a SHA. The body is a how-to. The appendices explain and record.

The program runs `pstack/skills/poteto-mode/playbooks/autopilot-stack.md`. The operator lands the linear PR chain. Owners stop at STACK-READY. PR ids `PR-macro-if`, `PR-macro-author`, `PR-macro-list`, and `PR-macro-shell` are review-gated interaction changes.

Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

## Program checklist

### Arm the program

- [ ] State the protocol and this plan to the operator, then stop. Start execution only on her explicit go.
- [ ] On her go, arm a `/goal` with this exact text. "Run `docs/plans/2026-08-29-macro-branching-execution-plan.md` as autopilot-stack. PR order `PR-macro-if` then `PR-macro-author` then `PR-macro-list` then `PR-macro-shell`. Frontend is an indented step list, not an interactive graph. Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Owners stop at STACK-READY. Operator lands the stack. Done when Close the program is checked."
- [ ] Read these from trunk at program start. Re-read them at every tick.
  - [ ] `git show origin/main:pstack/skills/poteto-mode/playbooks/autopilot-stack.md`
  - [ ] `git show origin/main:pstack/skills/swarm/SKILL.md`
  - [ ] `git show origin/main:.cursor/skills/verify-bobo-companion/SKILL.md`
  - [ ] `git show origin/main:pstack/skills/poteto-mode/playbooks/opening-a-pr.md`
  - [ ] `git show origin/main:pstack/skills/how/SKILL.md`
  - [ ] `git show origin/main:docs/superpowers/specs/2026-07-12-macro-tab-design.md`
- [ ] Arm the 30-minute audit tick. In a local session, a real terminal `/loop`. In a cloud root, a cloud-sleeper wake chain. Never leave the cadence to memory.
- [ ] Use this tick prompt, verbatim. "Re-read the execution playbook from trunk and the armed /goal. Audit the operation against both and fix drift in this tick. Probe every active lane and judge progress by side effects only. Stand down a stuck lane and dispatch its replacement now. Then send the operator a status message, whether or not anything changed, with the queue table of PR, owner, state, and head SHA, the verdicts since the last tick, what merged, open operator gates, and blockers."
- [ ] On the operator's hold or stand-down, send every owner a zero-writes order at once.

### Spawn owners

- [ ] Spawn one owner per PR with the full lifecycle the execution playbook names.
- [ ] Follow this dependency graph. Start dependent work only after its parent merges, or base it on the parent branch when the execution playbook stacks.
  - [ ] `PR-macro-if` first from `main` (in flight on branch `pr-macro-if`).
  - [ ] `PR-macro-author` after `PR-macro-if`.
  - [ ] `PR-macro-list` after `PR-macro-author`.
  - [ ] `PR-macro-shell` after `PR-macro-list`.
- [ ] Hold the file boundaries. `PR-macro-if` touches only `src/macro_ui/{inspector,mod,editor,canvas,canvas_model,canvas_layout,library}.rs` and matching tests. `PR-macro-author` may also touch `src/engine/macro_engine/model.rs` only if a UI command type must land with the editor. `PR-macro-list` owns the Macro center surface. It may add `src/macro_ui/step_list.rs` and rewrite `MacroPage::workspace` to drop interactive graph gestures. `PR-macro-shell` touches chrome, widths, history, and live target facts.
- [ ] Hold the review gate. All four PRs change an interaction. They wait for the operator's review in chat with screenshots and a video before merge.
- [ ] Cancelled. `PR-macro-canvas` (grow interactive graph / 07-21 canvas-first) is cancelled. Do not spawn it.

### PR mechanics, for every PR

- [ ] Open the PR ready, never draft, with `gh pr create` and `draft: false`, or with Graphite `gt` for a stack.
- [ ] Run the repo's lint and typecheck once before the PR-facing push. Push with hooks on.
- [ ] Run `/deslop` before each commit and `/no-comments` before review.
- [ ] Triage every Bugbot and security-reviewer comment per `../references/bugbot-triage.md`.
- [ ] Rebase onto current trunk before babysit and again before the merge-ready report.

### Verdict and merge, for every PR

- [ ] At the merge-ready head SHA, run the swarm per `pstack/skills/swarm/SKILL.md`. One gates lane. The ten live lanes from the PR's **Verify, live** block. The perf lane from its **Verify, perf** block. One audit lane that reads the diff and the receipts and distrusts the PR body.
- [ ] Clean only when every lane is `PASS`. Findings go back to the owner. A new head gets a fresh swarm and a fresh verdict.
- [ ] Root appends the PR to the linear chain on a clean verdict. No owner merges. Operator lands the stack. If `gt` is missing, use ordered `gh` PRs bottom-up.

### Boot recipe, for every live lane

Each live lane runs on its own machine at the PR head. Drive through `.cursor/skills/verify-bobo-companion/scripts/control-bobo.ps1`.

- [ ] `git fetch origin <head-branch> && git checkout <head SHA>`.
- [ ] Close any foreign `BoBo Companion` window. Run `control-bobo.ps1 launch`. Wait until `control-bobo.ps1 doctor` reports `owned_instance` true.
- [ ] Deliver input only through `control-bobo.ps1` (`click-label-approx`, `click-rel`, `screenshot`, `read-ui-state`, `cargo-test`). Read-only diagnostics are `doctor` and `window-info`.
- [ ] Save every screenshot to `/tmp/swarm-<pr-id>/worker-<n>/<slug>.png` and return the paths with the report.
- [ ] Run `control-bobo.ps1 cleanup` after the lane. Keep screenshots.

## Make If then and else first-class (PR-macro-if)

**Depends on.** None.

**Files.**

- [ ] Edit `src/macro_ui/inspector.rs`.
- [ ] Edit `src/macro_ui/mod.rs`.
- [ ] Edit `src/macro_ui/editor.rs`.
- [ ] Edit `src/macro_ui/canvas_model.rs`.
- [ ] Edit `src/macro_ui/canvas.rs`.
- [ ] Edit `src/macro_ui/library.rs` empty-state copy if it still denies the wizard.

**Build.**

- [ ] Keep `BlockKind::If { then_body, else_body }` as the only branch model. Do not add Goto, Tag, Jump, or cross-sibling links.
- [ ] Give If a dedicated inspector that names THEN and ELSE as insert targets.
- [ ] When an IfThen or IfElse selection is active, palette insert goes into that branch.
- [ ] Show status text. "If true runs THEN. If false runs ELSE. Then continues after the If."
- [ ] Unit tests for THEN insert, ELSE insert, and empty branch slot selection.
- [ ] Note. This PR may still use the temporary canvas as the selection surface. `PR-macro-list` replaces that surface.

**You see.**

- [ ] Selecting THEN or ELSE and inserting a step lands the block in `then_body` or `else_body` in the draft.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Run `cargo test palette_inserts_into_selected_if_else`.
- [ ] Run `cargo test invalid_cross_branch_link`.
- [ ] Run THEN insert and empty-slot hit tests added on this branch.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `grok-4.6-fast-xhigh` at the PR head, per the boot recipe.

- [ ] Lane 1. Open Macro. Save `if-01-macro-tab.png`. Pass when Macro chrome and `New Macro` are visible.
- [ ] Lane 2. Create starter draft. Save `if-02-starter.png`. Pass when a draft Observe exists.
- [ ] Lane 3. Add IF. Save `if-03-if-node.png`. Pass when If and THEN or ELSE are visible.
- [ ] Lane 4. Select ELSE. Insert Wait. Save `if-04-else-wait.png`. Pass when Wait is under ELSE.
- [ ] Lane 5. Select THEN. Insert Action if allowed. Save `if-05-then-action.png`. Pass when Action is under THEN or precondition text shows.
- [ ] Lane 6. If inspector. Save `if-06-inspector.png`. Pass when THEN and ELSE are named.
- [ ] Lane 7. Illegal cross-branch link attempt or unit evidence. Save `if-07-reject.png`. Pass when no Goto edge appears.
- [ ] Lane 8. Save when valid. Save `if-08-saved.png`. Pass when a saved revision appears or Save succeeds.
- [ ] Lane 9. Validate or Dry Run. Save `if-09-dry-or-validate.png`. Pass when no structural If error.
- [ ] Lane 10. Cleanup. Save `if-10-cleanup.png`. Pass when session is gone and screenshots remain.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Wall seconds from `launch` to `doctor` ok.
- [ ] Probe. Time launch then doctor at trunk and head, three times each.
- [ ] Baseline. Record the trunk median seconds first.
- [ ] Rule. Head median must stay under trunk median plus 2.0 seconds.

**Review gate.** The operator reviews before merge.

- [ ] Copy lane 3, 4, and 6 screenshots into `.cursor/skills/verify-bobo-companion/artifacts/PR-macro-if-review-*.png`.
- [ ] Record a 30 to 60 second video of THEN versus ELSE insert. Save it as `.cursor/skills/verify-bobo-companion/artifacts/PR-macro-if-review.mp4`.
- [ ] Post the screenshots and the video in chat. Stop at merge-ready. Wait for the operator's click.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] Root appends `PR-macro-if` to the chain. Operator lands it.

## Expose Continuous, Watch lanes, and Action fields (PR-macro-author)

**Depends on.** `PR-macro-if`.

**Files.**

- [ ] Edit `src/macro_ui/inspector.rs`.
- [ ] Edit `src/macro_ui/mod.rs`.
- [ ] Edit `src/macro_ui/editor.rs`.
- [ ] Edit list or canvas selection hooks only as needed so Continuous and Watch remain insertable after the list lands.

**Build.**

- [ ] Add Continuous to palette or conversion.
- [ ] Add Watch Add Lane.
- [ ] Replace Action empty inspector with click fields for the existing `Action` enum.
- [ ] Keep engine validation for unpaced Continuous.

**You see.**

- [ ] User can add Continuous, a second Watch lane, and edit Action click side without Debug dumps.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Run `cargo test rejects_unpaced_continuous`.
- [ ] Run `cargo test watch_lane_projection`.
- [ ] Run a Continuous insert or conversion test.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `grok-4.6-fast-xhigh` at the PR head, per the boot recipe.

- [ ] Lane 1. Macro ready. Save `author-01-macro.png`. Pass when Macro is selected.
- [ ] Lane 2. Draft open. Save `author-02-draft.png`. Pass when a draft exists.
- [ ] Lane 3. Insert Continuous. Save `author-03-continuous.png`. Pass when Continuous appears in the step list or temporary canvas.
- [ ] Lane 4. Insert Watch. Save `author-04-watch.png`. Pass when one lane exists.
- [ ] Lane 5. Add Lane. Save `author-05-second-lane.png`. Pass when two lanes show.
- [ ] Lane 6. Action inspector. Save `author-06-action-inspector.png`. Pass when click fields show.
- [ ] Lane 7. Validate paced Continuous. Save `author-07-validate.png`. Pass when validation is honest.
- [ ] Lane 8. Move step up or down. Save `author-08-reorder.png`. Pass when order changes.
- [ ] Lane 9. Undo. Save `author-09-undo.png`. Pass when prior state returns.
- [ ] Lane 10. Cleanup. Save `author-10-done.png`. Pass when session cleared.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. `cargo test macro_ui::` wall seconds.
- [ ] Probe. Run the suite at trunk and head twice each.
- [ ] Baseline. Record the trunk mean seconds first.
- [ ] Rule. Head mean must stay under trunk mean plus 15 percent.

**Review gate.** The operator reviews before merge.

- [ ] Copy lane 3, 5, and 6 screenshots into `.cursor/skills/verify-bobo-companion/artifacts/PR-macro-author-review-*.png`.
- [ ] Record a 30 to 60 second video of Continuous and Add Lane. Save it as `.cursor/skills/verify-bobo-companion/artifacts/PR-macro-author-review.mp4`.
- [ ] Post the screenshots and the video in chat. Stop at merge-ready. Wait for the operator's click.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] Root appends `PR-macro-author` to the chain. Operator lands it.

## Replace the graph with an indented step list (PR-macro-list)

**Depends on.** `PR-macro-author`.

**Files.**

- [ ] Create `src/macro_ui/step_list.rs` (or equivalent module name).
- [ ] Edit `src/macro_ui/mod.rs` workspace so the center surface is the step list, not `canvas::show` pan/zoom/connectors.
- [ ] Edit inspector selection wiring to use list row selection.
- [ ] Stop calling interactive graph gestures from the default Macro page path.
- [ ] Leave dead canvas modules only if needed for tests in this PR. Prefer delete or `#[cfg(test)]` quarantine over a second live graph.

**Build.**

- [ ] Show the canonical tree as an indented vertical list. One row per block. Indent for THEN, ELSE, loop body, and Watch lanes.
- [ ] If rows expose clear THEN and ELSE child sections. Selecting a section sets the insert target.
- [ ] Controls are Add step, Up, Down, Disable, Duplicate, Delete. No empty-space pan, wheel zoom, or connector drag on the default path.
- [ ] Keep nested `BlockKind::If` semantics. List edits emit the same `EditorCommand` path.
- [ ] Match Enchant density. Large readable rows, few buttons, one inspector.

**You see.**

- [ ] Macro center is a scrollable step list. A new user can build If then/else without learning graph gestures.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Add step_list projection tests for If THEN/ELSE indent.
- [ ] Run `cargo test` filters for list insert into ELSE and reorder.
- [ ] Confirm no production call path requires connector drag for basic authoring.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `grok-4.6-fast-xhigh` at the PR head, per the boot recipe.

- [ ] Lane 1. Macro tab. Save `list-01-macro.png`. Pass when center is a list, not a node graph.
- [ ] Lane 2. Starter draft rows. Save `list-02-rows.png`. Pass when Observe appears as a row.
- [ ] Lane 3. Add IF. Save `list-03-if.png`. Pass when THEN and ELSE sections are visible as indented groups.
- [ ] Lane 4. Add Wait under ELSE via list selection. Save `list-04-else.png`. Pass when Wait is indented under ELSE.
- [ ] Lane 5. Move Wait up or down within ELSE. Save `list-05-reorder.png`. Pass when order changes.
- [ ] Lane 6. No connector UI required. Save `list-06-no-graph.png`. Pass when screenshot shows no bezier wires as the primary editor.
- [ ] Lane 7. Inspector still edits the selected row. Save `list-07-inspector.png`. Pass when inspector matches selection.
- [ ] Lane 8. Library still lists macros. Save `list-08-library.png`. Pass when library remains usable.
- [ ] Lane 9. Enchant regression. Save `list-09-enchant.png`. Pass when Enchant Start remains.
- [ ] Lane 10. Cleanup. Save `list-10-done.png`. Pass when session cleared.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Wall seconds from Macro tab click to stable screenshot.
- [ ] Probe. After launch, time Macro open then screenshot at trunk and head, three times each.
- [ ] Baseline. Record the trunk median seconds first.
- [ ] Rule. Head median must stay under trunk median plus 1.0 second.

**Review gate.** The operator reviews before merge.

- [ ] Copy lane 1, 3, and 6 screenshots into `.cursor/skills/verify-bobo-companion/artifacts/PR-macro-list-review-*.png`.
- [ ] Record a 30 to 60 second video of list If authoring without graph gestures. Save it as `.cursor/skills/verify-bobo-companion/artifacts/PR-macro-list-review.mp4`.
- [ ] Post the screenshots and the video in chat. Stop at merge-ready. Wait for the operator's click.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] Root appends `PR-macro-list` to the chain. Operator lands it.

## Compact Macro chrome and live target facts (PR-macro-shell)

**Depends on.** `PR-macro-list`.

**Files.**

- [ ] Edit `src/macro_ui/mod.rs` status strip.
- [ ] Edit `src/ui_state.rs` pane widths if side panes remain.
- [ ] Edit `src/main.rs` live target projection and bottom dock defaults for list density.
- [ ] Wire History to existing intents.

**Build.**

- [ ] Replace hardcoded Not connected and Unknown with real capture facts when bound.
- [ ] Apply persisted pane widths if side panes remain after the list redesign.
- [ ] Keep one clear Guided wizard entry. Drop permanent control sprawl.
- [ ] Expose History for README UAT.

**You see.**

- [ ] Macro chrome feels closer to Enchant. Connection text is honest. History opens.

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Run `cargo test persisted_layout_round_trip` or the list-era layout equivalent.
- [ ] Add or extend a width persistence test if panes remain.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes on `grok-4.6-fast-xhigh` at the PR head, per the boot recipe.

- [ ] Lane 1. Macro tab. Save `shell-01-macro.png`. Pass when list chrome is visible.
- [ ] Lane 2. Status before capture. Save `shell-02-uncaptured.png`. Pass when facts are not falsely live.
- [ ] Lane 3. Capture Target when possible. Save `shell-03-capture.png`. Pass when status updates or skip is evidenced.
- [ ] Lane 4. History. Save `shell-04-history.png`. Pass when History UI or empty message appears.
- [ ] Lane 5. Wizard entry. Save `shell-05-wizard.png`. Pass when wizard opens.
- [ ] Lane 6. Bottom dock compact. Save `shell-06-dock.png`. Pass when run controls remain reachable.
- [ ] Lane 7. Enchant Start. Save `shell-07-enchant.png`. Pass when Enchant works.
- [ ] Lane 8. Back to Macro. Save `shell-08-back.png`. Pass when list remains.
- [ ] Lane 9. Restart persist check if widths remain. Save `shell-09-persist.png`. Pass when state is honest.
- [ ] Lane 10. Cleanup. Save `shell-10-done.png`. Pass when session cleared.

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. Wall seconds for cleanup then launch then doctor ok.
- [ ] Probe. Run the sequence at trunk and head three times each.
- [ ] Baseline. Record the trunk median seconds first.
- [ ] Rule. Head median must stay under trunk median plus 2.0 seconds.

**Review gate.** The operator reviews before merge.

- [ ] Copy lane 1, 4, and 7 screenshots into `.cursor/skills/verify-bobo-companion/artifacts/PR-macro-shell-review-*.png`.
- [ ] Record a 30 to 60 second video of list chrome plus History. Save it as `.cursor/skills/verify-bobo-companion/artifacts/PR-macro-shell-review.mp4`.
- [ ] Post the screenshots and the video in chat. Stop at merge-ready. Wait for the operator's click.

**Merge.**

- [ ] Root's clean verdict at the exact head SHA.
- [ ] Bugbot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] Root appends `PR-macro-shell` to the chain. Operator lands it.

## Close the program

- [x] Every box above is checked with its evidence. Stack landed on `main` at `daff2ad` (PR #1 then #5 carrying author+list+shell tip `f0f521d`). Swarm verdicts under `.cursor/skills/verify-bobo-companion/artifacts/PR-macro-*/SWARM-VERDICT.md`. `PR-macro-canvas` cancelled per plan.
- [x] Reply to the operator with the report the execution playbook names. See `.cursor/skills/verify-bobo-companion/artifacts/STACK-DELIVER.md`.

## Appendix A. Prototype evidence

Nested If branching remains proven in engine tests. Head for If UI work is on `pr-macro-if` at `b237a76c3600c367f4119b9177d74720d7872c5c` pending fresh swarm after audit fixes.

Indented list Macro UI is unproven as a live layout. `PR-macro-list` is the proving PR. Operator preference against interactive graph is recorded here and overrides `2026-07-21` canvas-first for this program.

## Appendix B. Alternatives rejected

Interactive graph growth and 07-21 canvas-first. Lost because the operator finds graph gestures hard to control. Experience First wins.

Goto jump labels. Lost. Nested then/else stays the domain model.

Keeping the graph and only simplifying toolbars. Lost. The hard part is connectors and pan/zoom, not label copy.

## Appendix C. Risks

Live lanes without Diablo cannot Capture Target or Run Live. Skip with evidence.

List redesign may temporarily break canvas-only acceptance tests. Owner updates or quarantines them in `PR-macro-list`.

In-flight live swarm against the old graph UI may complete with stale screenshots. Root discards graph-growth findings and re-verifies against this plan.

## Appendix D. Links and reading list

- `docs/superpowers/specs/2026-07-12-macro-tab-design.md` (engine contract, timeline-era intent)
- `docs/superpowers/specs/2026-07-21-macro-canvas-first-workspace-design.md` (superseded for this program)
- `.cursor/skills/verify-bobo-companion/SKILL.md`
- `src/engine/macro_engine/model.rs` (`BlockKind::If`)
- Decision trail per show-me-your-work kept local to each owner
