# GitHub Projects task backend

Keitora can back a project's task tracking + dispatch with a **GitHub Projects v2 board** instead of
the default local SQLite store. Select it per-project in `keitora.toml`.

This backend is **eventually consistent** (`consistency() → Eventual`): GitHub has no atomic
compare-and-set, so `claim` is advisory (best-effort). Keitora surfaces this — see
[Claiming & consistency](#claiming--consistency).

## Prerequisites

- **`gh` CLI** installed and on `PATH`. The backend shells out to `gh`; if it is missing or you are
  not logged in, Keitora reports `Unavailable` ("install gh" / "gh auth login") — not a config error.
- **`project` scope** on your `gh` token. Reads + writes to a Projects v2 board need it:
  ```sh
  gh auth refresh -s project
  ```
  A missing scope surfaces as `Unauthorized` with exactly this remediation.

## Configuration

In the project's `keitora.toml`:

```toml
[tasks]
backend = "github"

[tasks.github]
# All optional. owner/repo are parsed from `git remote origin` when omitted;
# project_number is auto-resolved from the repo's linked projects when omitted.
owner = "acme"
repo = "rpg-game"
project_number = 3
# Local SQLite sidecar for dispatch_log + reservations (machine-local coordination
# GitHub can't model). Relative paths resolve against the project root. When omitted,
# a per-project file under the user config dir is used (stable even if the working
# copy isn't checked out).
sidecar_path = "github-dispatch.db"

# Optional — only for boards whose Status columns don't match the conventional names.
[tasks.github.status_map]
active = "In progress"
open   = "Ready"
backlog = "Backlog"
done   = "Done"
```

Existing `keitora.toml` files without `[tasks.github]` are unaffected (the field defaults to absent).

## Data-model mapping

| Keitora | GitHub |
|---|---|
| task id | **issue number** (so `Fixes #N` in a PR auto-closes the task) |
| title / description | issue title / body (Keitora metadata in an HTML-comment block, preserved around your prose) |
| status | project **Status** single-select column (see below) |
| `active` (claimed) | "In progress" |
| `open` | "Ready" |
| `backlog` | "Backlog" |
| `done` | "Done" column, then issue **closed as completed** |
| `cancelled` | issue **closed as not planned** |
| worktree ownership | **`keitora:<worktree>` label** (e.g. `keitora:idris`) — added on assign/claim, removed on release |
| priority | `priority:<p>` label |
| tags | issue labels (excluding the reserved `keitora:*` / `priority:*`) |
| dependencies | Keitora-managed `blocked_by` list inside the metadata block |

**Status columns.** Field + option ids are resolved at runtime from the board
(`gh project view` / `gh project field-list`). By convention Keitora expects distinct **"In progress"**,
**"Ready"**, **"Backlog"**, and **"Done"** columns; `open` → "Ready" is kept distinct from `backlog`
→ "Backlog" so a ready-to-pick task is distinguishable from a deliberately-shelved one. If your board
uses different names, map them with `[tasks.github.status_map]`. A required column the board lacks is
a clear `Config` error at first use.

On read, `done`/`cancelled` are derived from the issue's **closed state + close reason** (not the
column), so closing an issue on GitHub is reflected in Keitora; the Status column is kept in sync for
board visibility.

## Claiming & consistency

Because GitHub offers no atomic CAS, `claim` is **read → decide → write** and returns
`ClaimOutcome::ClaimedUnverified` — the write was submitted but exclusivity is not guaranteed (two
worktrees could both believe they own a task). The CLI prints a "claim submitted — verify it stuck"
note; the desktop surfaces it via the structured claim result. A returned `Conflict` is confirmed by
a fresh re-read (not a stale snapshot). Per-machine **dispatch reservations stay strongly consistent**
in the local sidecar.

## Resilience

- Every `gh` call is wrapped in bounded **retry with backoff**, honouring `Retry-After` on rate
  limits. Non-retryable errors (`Conflict`/`Unauthorized`/`Config`/`Unavailable`/`NotFound`) are not
  retried.
- A mutation whose write may have applied (GraphQL `200 + errors[]`, HTTP 409) is **never blindly
  retried**; the backend re-reads and reconciles. `create` dedupes a retried call via a hidden
  idempotency marker (find-before-create).
- Errors reach you with structure: rate-limit waits, the `gh auth refresh -s project` remediation,
  and "gh missing" are distinct, not flattened to a generic failure.

## What stays local

Worktree sessions and resume sessions are **always** local SQLite, regardless of `backend` —
they model machine-local state GitHub doesn't represent.

## Limitations

- Eventually consistent: `claim` is advisory (above).
- `gh`-subprocess latency per operation; list/deps may issue multiple calls.
- No background poller yet — `revision()` (a composed GitHub-cursor + sidecar-counter) is exposed for
  a future change-watcher, but the live fs-watcher remains SQLite-specific.
