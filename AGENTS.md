# AGENTS.md — conventions for AI agents working in this repo

This file is the source of truth for how automated agents (Claude Code, Codex, etc.)
should behave in the Keitora repo. Human-facing contribution docs live in `README.md`.

## No plan or task identifiers in code or commit messages

Plan/task identifiers must **never** appear in source code or in commit messages. This
includes:

- Task/issue tags: `[#0036]`, `#0036`, `#36`
- Plan-stage back-references: "Stage 2", "#0034 Stage 6", "(#0035, Stage 4)"
- In-repo plan paths in comments: `plans/0001-example`

**Why:** these identifiers are churn that outlives its usefulness — a stage number is
meaningful for the length of one piece of work and then just clutters the code and the
history. Traceability belongs in durable places, not inline:

- the **branch name** (`llm/<user>/keitora/<NNNN>-<desc>`),
- the **`plans/` documents** (proposal + stage files),
- the **task CLI** (`./keitora task`).

### What to write instead

- **Code comments:** explain the *what/why* on its own terms. `// validate the pane is
  live before binding a bridge` — not `// #0036 Stage 2: validate the pane is live`.
- **Commit messages:** plain [Conventional Commits](https://www.conventionalcommits.org/) —
  `type(scope): description`. No trailing `[#NNNN]`. Keep the
  `Co-Authored-By:` trailer when one is configured for the session.

### Enforcement

- Before committing, check your diff: `git grep -nE '#[0-9]{3,4}' -- <changed files>` and
  `git log -1 --format=%B | grep -E '#[0-9]'` should both be empty for source/commits.
- Plan documents under `plans/` are exempt — they are *about* a numbered task, so the
  number belongs there.

## Placeholder names only — no real projects, people, or machines

This repo is public. Test fixtures, example paths, mock data, doc and plan examples, and
commit/PR text must **never** use the names of real projects, games, companies, internal
services, repos, worktrees, people, or the contributor's own machine. That includes names
copied from whatever you happen to be working on alongside Keitora (other repos in a
workspace, the projects a contributor dogfoods it with, folders seen in their `D:/repos`).

**Why:** a fixture like `groupAriaLabel("my-employers-game")` or a path like
`C:/Users/<real-user>/src/<internal-repo>` leaks information the moment it is pushed — and
once a commit is in a public repo (or a GitHub PR ref), it can't be fully taken back.

### Use these instead

| Kind | Placeholders |
|---|---|
| Projects / repos | `proj`, `other-proj`, `my-game`, `shared-tools`, `projectA`, `keitora` |
| Worktrees | `main`, `alpha`, `bravo`, `charlie`, `wt` |
| Unreal targets | `MyGame`, `MyGameEditor`, `MyPlayerController.cpp` |
| Users / homes | `me`, `x`, `u` — `C:/Users/x/…`, `/Users/me/…`, `~/…` |
| Other paths | `D:/repos/proj`, `D:/scratch`, `~/oss/some-tool`, `//srv/share/a` |
| Services / backends | `backend`, `api`, `example.com` |
| People / email | `Alice`, `Bob`, `user@example.com` |

When a test needs a realistic-looking value (a long path, a UBT log, a branch name), build
it from these pieces rather than pasting real output. If you paste real output (logs,
`git worktree list`, a screenshot description), scrub it first.

### Enforcement

- The pre-commit hook rejects staged lines matching a **local, never-committed** denylist
  at `$(git rev-parse --git-common-dir)/info/sensitive-terms` — one extended regex per
  line, `#` comments allowed. It lives outside the tree on purpose: a committed list of the
  names would itself be the leak. Contributors keep their own.
- Before pushing, also check commit messages and PR descriptions — the hook only sees the
  diff.

## Workflow

Large efforts run through the `/workflow` command (`.claude/commands/workflow.md`): staged
Plan → Refine → … → Finalization with human gates. Follow it when driving multi-stage work.

## Tests

New logic in the Rust crates ships with `#[cfg(test)]` coverage in the same change —
untested new logic is not "done". `cargo test --workspace` and `yarn test` must stay green.
