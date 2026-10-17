---
disable-model-invocation: true
---
Cut or fill a GitHub release for the current platform using `yarn release` (`scripts/release.ts`).

Keitora releases are **local and per-platform**: each machine builds and uploads *its own* artifact
(Windows → NSIS `.exe`, macOS → signed/notarized `.dmg`) into a single release keyed by `v{version}`.
The typical flow is: rev the version on one machine (which creates the release), then run again on the
other OS with no bump to add that platform's artifact to the same release.

## Steps

1. **Preflight** — verify the environment:
   - `gh auth status` — must be authenticated (repo is `rudoi/keitora`, ambient via `origin`).
   - Platform: `node -e "console.log(process.platform, process.arch)"`. On **macOS**, a real (non-dry) run
     requires Apple signing env vars (`APPLE_SIGNING_IDENTITY` + a notarization flow — see
     `scripts/build-dmg.sh`); confirm they're set or the signed build will fail fast.
   - `git status --porcelain` — a version rev commits the three version files; make sure the working tree
     is clean enough that the release commit won't sweep up unrelated changes.

2. **Decide the flags with the user:**
   - **Revving the version?** Pick exactly one: `--major` / `--minor` / `--patch` / `--set X.Y.Z`.
     A rev bumps `package.json` + `src-tauri/tauri.conf.json` + `src-tauri/Cargo.toml`, commits, tags
     `v{version}`, and pushes — then creates the release.
   - **Filling an existing release?** No bump flag. The tool targets `v{current}` (creating it at the
     current version from HEAD if it doesn't exist) and uploads this platform's artifact only if the
     release doesn't already have one.
   - **Replacing an artifact?** Add `--overwrite` (no-bump mode) to clobber an existing same-platform asset.

3. **Always dry-run first:** run `yarn release {flags} --dry-run` and show the user the planned actions
   (version writes, git commit/tag/push, release create, upload decision). **Wait for confirmation.**

4. **Execute:** run `yarn release {flags}` for real. Surface the build/sign output; a macOS build can take
   a while.

5. **Report** the release URL printed at the end, and state which artifact was uploaded / replaced /
   skipped. If skipped ("already has …"), remind the user that `--overwrite` replaces it.

## Notes
- Never pass a bump flag together with `--overwrite` expecting a clobber — a fresh rev creates an empty
  release, so there's nothing to overwrite.
- A `--set` to a version not greater than the current one requires interactive confirmation; it will
  refuse in a non-interactive context.
