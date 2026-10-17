# Stage 1 — tailPath

**Goal:** a pure display helper that keeps a project path to root + 2 folders, or the
trailing 2 folders.

**Prerequisites:** none.

**Implementation:** `src/lib/format.ts`: `export function tailPath(path: string, levels = 2)`
per the proposal's rules (trailing slash, verbatim prefix strip, roots `~` / `X:` /
`//server/share` / `/`). Doc comment in the file's style.

**Validation:** `src/lib/format.test.ts` `describe("tailPath")` covering every rule; `yarn test`
and `tsc --noEmit` green.

**Commit:** `feat(rail): tailPath keeps project paths to two folders`
