# 0057 — Panes must not get empty `XDG_*` values

## Goal

Stop Keitora panes from handing tools an empty `XDG_*` variable for a dir the user never
set. Claude Code's self-updater joins `$XDG_DATA_HOME` with `claude` without treating an
empty value as unset. It therefore installs every new version into a **relative** `claude/`
dir in the pane's cwd. On the author's machine that is 11 repos and 7.8 GB since
2026-10-12, while the real install (`~/.local/share/claude`) stopped updating.

## Background

`src-tauri/src/backend/herdr/env.rs`:

- The Keitora herdr server runs with isolated `XDG_CONFIG_HOME` / `XDG_STATE_HOME` (+
  `HERDR_CONFIG_PATH`) under `<KEITORA_HOME>/herdr`. It does **not** set `XDG_DATA_HOME`,
  `XDG_CACHE_HOME` or `XDG_RUNTIME_DIR`, and strips contaminated values for those three
  (`PASSTHROUGH_XDG`, `env_remove`).
- Panes inherit the server env, so `pane_xdg_env` passes all five `XDG_NAMES` explicitly
  through herdr's launch env, which can set but never unset. Each one gets the user's
  original value (`OriginalXdg`). If that value was unset, it gets `""` (`UNSET_AS_EMPTY = true`).
- The comment says the XDG convention treats empty as unset. That is the spec, but not
  every tool follows it. The `UNSET_AS_EMPTY` comment already anticipated this ("Flip to
  `false` ... if a tool turns out to mishandle an empty value").

Only `claude/` dirs leaked. A scan of every repo/worktree under `D:/repos` found no stray
`config/`, `state/`, `cache/` or `data/` dirs, so the empty `CONFIG`/`STATE` values have
not caused visible harm.

## Scope

In:
- Change what panes get for an **unset** original `XDG_*` var, per var and per OS (below).
- Update the `env.rs` module docs, the `UNSET_AS_EMPTY` comment/const, and the tests.

Out:
- Changing the server's own isolation (still `XDG_CONFIG_HOME` / `XDG_STATE_HOME`).
- Set original values: they are passed through unchanged, as today.
- Automatic cleanup of already-leaked `claude/` dirs. That is a one-off manual cleanup on
  the author's machine, done separately.
- Panes already running keep their env until they are restarted.

## Design

Rule: a pane gets an explicit value for a var only when it must override something the
server carries; when it does, that value should mean "the user's default".

| Var | Original set | Original unset — Linux/BSD | Original unset — macOS / Windows |
|---|---|---|---|
| `XDG_CONFIG_HOME` | pass through | `$HOME/.config` | `""` (unchanged) |
| `XDG_STATE_HOME`  | pass through | `$HOME/.local/state` | `$HOME/.local/state` |
| `XDG_DATA_HOME`   | pass through | not passed | not passed |
| `XDG_CACHE_HOME`  | pass through | not passed | not passed |
| `XDG_RUNTIME_DIR` | pass through | not passed | not passed |

Why:
- **DATA / CACHE / RUNTIME — never synthesised.** The server does not set them (and strips
  contaminated values, `env.rs` `PASSTHROUGH_XDG` / `env_remove`), so omitting them lets the
  pane inherit "unset" exactly. This is the case that fixes the Claude Code leak, on every OS.
- **CONFIG / STATE must be overridden** — the server carries isolated values — and herdr's
  launch env can set but not unset, so the choice is "spec default" vs `""`.
  - **Linux/BSD:** XDG is the native convention; the spec default is semantically identical
    to unset for every spec-following tool and a real absolute path for naive ones.
  - **macOS and Windows:** XDG is *not* native. Several tools change layout when the var is
    merely *set*: on Windows `gh` moves from `%APPDATA%\GitHub CLI` to `$XDG_CONFIG_HOME/gh`;
    on macOS e.g. lazygit moves from `~/Library/Application Support` to `$XDG_CONFIG_HOME`.
    A synthesised default would relocate their config/logins, so these keep `""` — what Go /
    most tools treat as unset, and with no observed leak (residual risk below).
- **Linux/BSD with no resolvable home** (`dirs::home_dir()` → `None`, rare): fall back to `""`.

Shape: replace `UNSET_AS_EMPTY` / `pane_xdg_env_with(original, bool)` with a pure function
taking the original values, a platform flag (`xdg_native: bool`) and an optional home dir —
unit-testable for both behaviours on any host. `pane_xdg_env(original)` wires
`cfg!(all(unix, not(target_os = "macos")))` and `dirs::home_dir()`; callers (`pane_env`,
`pane_shell_env`, `herdr/mod.rs`) are unchanged. The result is 2–5 pairs (no longer always 5),
in `XDG_NAMES` order.

Dev runs: `scripts/dev-herdr.mjs` sets all five `XDG_*` on the dev app; `OriginalXdg::capture`
already discards values under the Keitora root, so they read as unset and follow the same rule.
No change needed there.

## Tests (`#[cfg(test)]` in `env.rs`, plus the herdr `mod.rs` launch test)

- XDG-native rule: unset CONFIG/STATE → spec defaults under the given home; unset
  DATA/CACHE/RUNTIME → absent; set values pass through; no value is ever `""`.
- Non-native rule (macOS/Windows): unset CONFIG/STATE → `""`; unset DATA/CACHE/RUNTIME →
  absent; set values pass through.
- XDG-native with no home → CONFIG/STATE `""`.
- Order stays the `XDG_NAMES` order; never a `HERDR_*` key.
- `mod.rs` launch test (currently asserts all five are `""`) asserts the per-platform shape.
- Existing tests that hard-code lengths or all-empty values are updated: `env.rs`
  `pane_env_bare_exe_is_exactly_the_xdg_pairs_on_windows`, `pane_env_bare_exe_on_unix_...`,
  `pane_env_prepends_exe_dir_to_path_then_xdg`, `pane_xdg_env_is_five_pairs_in_order`,
  `pane_xdg_env_without_unset_as_empty_skips_unset` (removed with the bool), and
  `herdr/mod.rs` `spawned_panes_get_the_users_xdg_env`.

## Risks

- **Stale server.** A herdr server started by a much older Keitora could still carry
  isolated `DATA`/`CACHE`/`RUNTIME`; panes now inherit them instead of `""`. Effect is
  contained (e.g. Claude Code installs under `<KEITORA_HOME>/herdr/data/claude`, not in repos).
  Current Keitora strips those vars from every herdr spawn, so only a server surviving from
  before that change is affected. Accepted.
- **Tools that naively join an empty `XDG_CONFIG_HOME` / `XDG_STATE_HOME` on macOS/Windows**
  would leak the same way `claude/` did. None observed (repo scan found only `claude/`).
  Fixing that fully needs herdr to support unsetting launch env vars — out of scope.

## Open questions

None. The per-var, OS-aware rule was chosen by the user over "spec defaults everywhere"
and "skip all unset"; automated review narrowed spec defaults to XDG-native platforms
(Linux/BSD) and to CONFIG/STATE only.

## Complexity

Small: one subsystem; `env.rs` (logic + tests) and one test in `herdr/mod.rs`. Recommend skipping
Fracture, Singletonize and Loss check.

## Code review revision

`/code-review high` found that Claude Code also keeps state under `$XDG_STATE_HOME/claude`
(`locks/`), and caches under `$XDG_CACHE_HOME/claude` (`staging/`). The real install on the
author's Windows machine has `~/.local/state/claude/locks` and `~/.cache/claude/staging`, and the
leaked `claude/` dirs contain `locks/` and `staging/` beside `versions/`. An empty
`XDG_STATE_HOME` on Windows therefore leaks too. `XDG_STATE_HOME` now gets the spec default
(`~/.local/state`) on every platform, which is also Claude Code's own default when unset.
`XDG_CONFIG_HOME` keeps `""` on macOS/Windows to protect `gh`/lazygit config locations.
`PASSTHROUGH_XDG` is now derived from the per-var table (`PaneDefault::Passthrough`).
