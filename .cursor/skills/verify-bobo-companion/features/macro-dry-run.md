# Macro dry run

Dry Run executes a macro in observation-only mode: it advances watch/observe logic without injecting mouse/keyboard input. It is distinct from Run once, continuous observation, and Run Live.

## Sub-features

- `dry-run-label` surfaces the primary control as `Dry Run` with observe-only meaning when that mode is selected.
- `dry-run-no-inject` must not send live input (contrast with Run Live).
- `dry-run-controls` still allows Stop / ESC according to run-control availability.

## How to get to it (user POV)

- Open Macro, select a validated macro with a bound target when running for real.
- Choose `Dry Run` from the run controls.
- Watch the monitor for active block / observation results; Stop or ESC to end.

## Driving it with control-bobo

Preconditions:

- Default automated proof does **not** require a live game target.
- Prefer unit/acceptance coverage of run-mode labeling and control availability; live Dry Run against Diablo is optional and user-requested.

- **Label/mode contract.** Run `control-bobo.ps1 cargo-test -Filter primary_label` or the broader macro UI tests that assert `controls.primary_label` is `Dry Run` for dry-run mode (see `src/macro_ui/mod.rs` tests). Exit code 0; record the filter used in `artifacts/macro-dry-run/cargo-test.txt`.
- **Live smoke (optional).** With a draft open, screenshot run controls into `artifacts/macro-dry-run/controls.png` showing `Dry Run` when that intent is the primary action. Do not click Run Live in unattended verification.
- **Proof.** Test exit code 0 and/or screenshot of Dry Run chrome. Explicitly state that input injection was not exercised.

## Gotchas

- Dry Run is not a mock of the detector stack; it still observes. It must not inject. Do not trust the name alone if proving Live vs Dry — prefer mode assertions in tests.
- Unlimited limits remove user caps only; cancellation and validation still apply.
- Image rules imported from packages need local re-verification before Live use; out of scope for Dry Run label smoke.
