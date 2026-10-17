# Stage 8 — Top bar, cleanup and docs

## Goal

Slim the top bar to brand, breadcrumb and view pills, delete the project tab strip and its CSS, fix
stale comments and update the build plan. Proposal sections: §7 (Top bar, breadcrumb), §9.

## Prerequisites

- Stage 7 committed (rail owns project switching, add-project and the cycle hint).

## Implementation

`src/components/TopBar.tsx`
- Props shrink to `{ view, onView, projectLabel?, worktreeLabel? }`; remove `ProjectTabs` and
  `ProjectSummary` imports; render brand, optional `.topbar-crumb` (`project / worktree`, separator
  and worktree only when `worktreeLabel` is non-null), views. Keep exporting `View`. Update the
  header comment. Target height 36px, brand glyph about 28px, drop the tagline if it crowds (fall
  back to 40px only if the glyph reads poorly).

`src/App.tsx`
- `<TopBar view onView projectLabel={activeProjectKey} worktreeLabel={selected} />`.
- `KeyCtx.activeProjectName` fed `activeProjectKey` (cycle compares keys); drop tab-only props.
- Fix stale comments at `:149-150`, `:157-158`, `:355`, `:616`, `:950` (line numbers drift; search
  for tab strip / ProjectTabs / popover wording).

Delete `src/components/ProjectTabs.tsx`.

`src/App.css`
- Remove the `.project-tab*`, `.project-tabs`, `.project-cycle-hint` block and `.active-project`.
- Add `.topbar-crumb`, `.crumb-sep`, `.crumb-wt`; adjust top bar height and glyph size; fix the
  "project popover" comment near `:984-985`.

Comments only: `src/lib/keybindings.ts` (`KeyCtx.activeProjectName` is now the registry key),
`src/lib/useProjectList.ts` header, `src/components/SettingsDrawer.tsx` near `:819`,
`src/lib/switchProject.ts` if anything is left.

`docs/build-plan.md`
- Layout section (ASCII diagram and Rail bullet near `:102-122`) and the note near `:445-454`
  mentioning the tab strip and cycle hint: describe rail project groups instead. Keep the doc free
  of plan identifiers in prose that describes code behavior only where the file already uses them.

## Validation

```
yarn tsc --noEmit
yarn test
git grep -nE 'ProjectTabs|project-tab|project-cycle-hint' -- src docs
git grep -nE '#[0-9]{3,4}' -- src
cargo test --workspace   # once, at the very end; skip gated herdr tests (never set KEITORA_HERDR_BIN)
```
The two `git grep` commands must print nothing (except pre-existing, unrelated matches). No app launch.

## Commit message

```
refactor(topbar): replace project tabs with breadcrumb and slim bar

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```
