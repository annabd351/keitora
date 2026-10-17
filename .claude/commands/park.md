---
disable-model-invocation: true
---
Reset the current worktree to the latest `main` branch. The worktree will end up on a "parking" branch so it doesn't conflict with other worktrees on `main`.

## Steps

1. **Gather context** by running these commands:
   - `git rev-parse --abbrev-ref HEAD` — current branch name
   - `git rev-parse --show-toplevel` — repo root; the worktree name is the basename of this path
   - `git config user.name` — git username (use only the first word, lowercased, replacing spaces with hyphens)
   - `git fetch origin` — update remote refs before checking anything
   - `git status --porcelain` — detect unstaged or uncommitted changes

2. **Rebase onto `origin/main`** to drop commits already merged upstream (e.g. squash-merged PRs whose content is identical but SHAs differ):
   - Run `git rebase --empty=drop origin/main`
   - If the rebase succeeds cleanly, continue.
   - If there are **conflicts**, run `git rebase --abort` and report the conflict details to the user. Do not proceed.

3. **Check for remaining work in flight** after the rebase:
   - `git log --oneline origin/main..HEAD` — any commits still not in `origin/main`
   - `git status --porcelain` — any uncommitted changes

   If either is non-empty, stop and present the findings clearly:
   - List any dirty files (with their status letters, e.g. `M`, `??`)
   - List any remaining unmerged commits (hash + message)

   Then ask: **"Would you like to create a new branch to preserve this work before resetting to main?"**

   - If **yes**: ask for a branch name, then run `git checkout -b {new-branch}` to preserve the work, then continue.
   - If **no**: confirm they want to discard/abandon the work, then continue.

   **Do not proceed until the user explicitly responds.**

4. **Derive the parking branch name** using the format:
   `llm/{username}/{worktree-name}-parking`
   - `{username}` = git username, first word only, lowercased, spaces → hyphens
   - `{worktree-name}` = basename of `git rev-parse --show-toplevel`, lowercased

   The parking branch name is always the same — never append a number suffix. It is a throw-away placeholder, not a PR source branch.

5. **Switch to the parking branch at latest main:**
   - If the branch does not exist locally: `git checkout -b {parking-branch} origin/main`
   - If it already exists locally: `git checkout {parking-branch}` then `git reset --hard origin/main`
   - Verify with `git log -1 --oneline`

6. **Report the outcome:**
   - Confirm the worktree is now on the parking branch at the latest `main` commit
   - If a preservation branch was created in step 3, remind the user of its name
