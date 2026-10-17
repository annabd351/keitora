# Stage 4 — Project switching plumbing

## Goal

Fix the registry-key vs resolved-name conflation in `App.tsx`, extend `handleSwitchProject` with
select/silent/action options, and add the pending-intent slot, all without changing visible UI.
Proposal sections: "Naming and keys", §6.

## Prerequisites

- Stage 3 committed (`pendingIntent.ts`, `ProjectSummary.resolved_name`, `useProjectsOverview`).

## Implementation

`src/lib/switchProject.ts`
- Add `silent?: boolean` to the opts bag; when true skip the `"Switched to ..."` success toast. The
  error toast is unchanged. Update the header comment (stale tab-strip wording).
- `switchProject.test.ts`: `silent` suppresses the success toast but not the error toast.

`src/App.tsx`
- `registryProjectsRef` mirroring `registryProjects` (like `warmRef`).
- `SwitchOpts` type and `handleSwitchProject(name /* registry key */, summary?, opts?)`, following
  §6 steps 1-7 exactly:
  1. `s = summary ?? registryProjectsRef.current.find(p => p.name === name)`; `resolved = s?.resolved_name ?? name`.
  2. Already-active check compares `project.name === resolved`; if already active and `opts` given,
     apply select/focus immediately, run `opts.action` via `runPendingAction`, return.
  3. Warm lookup/flip uses `resolved`; when `opts.select` is set replace the remembered focus restore
     with `setSelected(opts.select); setView("focus")`.
  4. `setSwitchingTo(resolved)`; the 4 s safety-net timeout compares the same string.
  5. Optimistic flip passes `summary: s && { ...s, name: s.resolved_name }`; backend still gets the key.
  6. Clear the pending slot at the start of every call; set it again when `opts.action`/`opts.select`
     need a post-settle step (cold targets apply focus only through the slot); clear on failure.
- `const [pending, setPending] = useState<PendingIntent | null>(null)` next to `switchingTo`.
- `runPendingAction(action, worktree)` `useCallback` mapping to existing handlers per §6
  (`restart`, `transfer`, `terminate`, `launch`, `diff`, `new-worktree`).
- Consuming effect declared after the "keep selected valid" and "persist focus" effects and before
  any early `return`: on `canConsume` apply selection + `setView("focus")` (when worktree non-null),
  run the action, `setPending(null)`; on `shouldDrop` clear.
- Call `useProjectsOverview()` directly under `useProjectList()` only if something consumes it in
  this commit; otherwise leave it to the next stage to avoid unused-variable lint failures.
- Keep `switchToProject` (keymap wrapper) and `SettingsDrawer`'s `onSwitchProject` type compatible.
- Do not touch rendering or props of `Rail`/`TopBar` in this stage.

Put any decision logic that is not hook-bound (for example, "should the already-active shortcut
apply", or building the `PendingIntent` from `SwitchOpts`) into `pendingIntent.ts` as a pure helper
with vitest cases rather than inlining it in `App.tsx`.

## Validation

```
yarn tsc --noEmit
yarn test src/lib/switchProject.test.ts src/lib/pendingIntent.test.ts src/lib/warmProjects.test.ts src/lib/keybindings.test.ts
```

## Commit message

```
refactor(app): key project switching on resolved names and add switch options

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```
