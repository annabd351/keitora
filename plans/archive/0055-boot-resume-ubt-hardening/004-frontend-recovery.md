# 004 - Frontend recovery

## Goal
Let the UI attach panes whose sessions have settled while recovery is still running.

## Prerequisites
003 committed (the `daemon://recovery` payload carries `settled`).

## Implementation
Proposal A1 "Payload" (TypeScript bullet) and "Frontend":
- `src/lib/ipc.ts`: `RecoverySessionRef`, `RecoveryInfo.settled`, `HEALTHY_RECOVERY.settled = []`.
- `src/lib/recovery.ts`: `paneRecoveryState` keeps the `suspended` rule first, then returns `null` for a settled key during `Resuming`, otherwise today's behaviour.
- Fix every `RecoveryInfo` literal in vitest files (`src/lib/recovery.test.ts` and any other, found by `tsc`). No change to `paneStatus.ts`.

## Validation
```
yarn tsc --noEmit
yarn vitest run src/lib/recovery.test.ts
yarn vitest run src/lib/paneStatus.test.ts
```
Required tests: a settled key in `Resuming` returns `null`; failed, queued and in-flight keys stay `recovering`; a settled key in `Paused` or any other non-`Resuming` non-Healthy phase stays `recovering`; `suspended` still wins over settled; all existing `recovery.test.ts` cases are kept.

## Commit message
```
feat(ui): attach panes as their sessions finish resuming

Read the settled session list from the recovery payload and stop marking
those panes as recovering while the rest of the queue is still resuming.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```
