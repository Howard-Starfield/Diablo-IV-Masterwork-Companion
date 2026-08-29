# Macro library

Macro library lists saved macros, starts an unsaved starter draft via `New Macro`, and exposes manage actions (rename, duplicate, import, export, delete) for a selected macro.

## Sub-features

- `library-open` shows the Macro page library pane.
- `library-new` creates an unsaved starter draft from `New Macro`.
- `library-search` filters rows via the Search field.
- `library-manage` exposes Rename / Duplicate / Import / Export / Delete under `Manage selected macro`.

## How to get to it (user POV)

- Choose the `Macro` tab.
- Choose `New Macro`, or select an existing row by name.
- Expand `Manage selected macro` for secondary actions.

## Driving it with control-bobo

Preconditions:

- Owned instance; Macro tab active.

- **Open library.** `click-label-approx -Label macro`, then screenshot `artifacts/macro-library/pane.png`. Capture shows `Search` and `New Macro` (empty state may also show `No macros yet`).
- **New draft.** `click-label-approx -Label new-macro`, wait ~300ms, screenshot `artifacts/macro-library/new-draft.png`. Capture shows canvas/editor chrome for a draft (observe block or editor feedback), not only the empty library message.
- **Proof.** Both screenshots retained under `artifacts/macro-library/`.

## Gotchas

- `New Macro` is disabled while a wizard request is active.
- Approximate click for `new-macro` targets the left library column; on narrow/responsive layouts the library may be a drawer — adjust with `click-rel` if the draft does not appear.
- Import/export need a package folder path the user (or recipe) provides; default smoke does not delete real macros in the user's non-sandbox store because launch uses the sandbox Macro Authoring root.
