# 003 — Section cards

**Goal:** projects read as distinct sections.

**Implementation:** `--proj-h` on `.rail-group`; card container; monogram tile that swaps
to the chevron on hover/focus-visible; two-line 44px header with `shortPath(root)` +
last-activity subtitle; spine down open lanes; compact single-line overlay chip; light
theme + reduced motion; hsl fallbacks before oklch. Shared minute `now` tick + `homeDir`
hook in `RailProjects`.

**Validation:** `yarn test`, `yarn tsc --noEmit`, `yarn lint`.

**Commit:** `feat(rail): identity-colored project cards with path and last activity`
