# 008 - Terminate / launch frontend

## Goal
Expose terminate and launch in the rail context menu and make panes honour the terminated state without auto-respawning.

## Prerequisites
007 committed (commands, `terminated` on the worktree list, `Terminated` token).

## Implementation
Proposal C3 frontend bullets (`attachErrors.ts`, `ipc.ts` `WorktreeInfo.terminated` and fixtures) and C4 in full.

Order (bottom-up so each file type-checks):
1. `src/lib/ipc.ts`: `WorktreeInfo.terminated`, `sessionTerminate` and `sessionLaunch` wrappers; add `terminated: false` to every `WorktreeInfo` literal in tests/fixtures.
2. `src/lib/attachErrors.ts` (+ test): `"terminated"` action, checked after `Recovering` and before the default.
3. `src/lib/paneStatus.ts` (+ test): `Status` `"terminated"`, in `ALL_STATUSES` only, sticky in `nextPaneStatus` before the `suspended` check.
4. `src/lib/resumeNotice.ts`: `terminateConfirmCopy()`.
5. `src/components/TerminalPane.tsx`: props, `terminatedRef`, effect on the prop, overlay with Launch / Launch fresh, `terminate()`, `launch(fresh)` via `spawn(fresh, via)`, nonce effects, `confirmKill` `"terminate"` flavour, `Terminated` token handling in `spawn` and `doAttach`.
6. `src/components/Rail.tsx` and `src/App.css`: `WorktreeMenuState.terminated`, menu items (Terminate after Transfer; Launch / Launch fresh and hidden Restart / Start fresh / Transfer when terminated), new `onTerminateSession`/`onLaunchSession` props, lane status helper with `terminated` winning, `.wt-block.terminated` style and tooltip.
7. `src/App.tsx`: two `Map<string, RestartSignal>` states via `nextRestartSignal`, handlers that focus the target pane like `onRestartSession`, wiring to `<Rail>` and `<TerminalPane>` (`terminated` from the matching `WorktreeInfo`).

## Validation
```
yarn tsc --noEmit
yarn vitest run src/lib/attachErrors.test.ts
yarn vitest run src/lib/paneStatus.test.ts
yarn vitest run src/lib/recovery.test.ts
```
Plus `yarn vitest run <file>` for any other test added or touched (for example a lane-status helper test if the helper is extractable, and any test file whose `WorktreeInfo` fixtures changed). Required tests: `nextPaneStatus` keeps `terminated` sticky (no attach; recovery cannot override it); `classifyAttachError` returns `terminated` and still prefers `Recovering`; `paneStatus` enumerates the new status; `terminateConfirmCopy` content; lane status gives `terminated` precedence over waiting/working/offline. Do not run the app; visual smoke is done later by the user.

## Commit message
```
feat(ui): terminate and launch sessions from the rail menu

Add Terminate, Launch and Launch fresh actions to the worktree context
menu, a terminated pane state with a launch overlay that never respawns on
its own, and a dimmed rail style for terminated sessions.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```
