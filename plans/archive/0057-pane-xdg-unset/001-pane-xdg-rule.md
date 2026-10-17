# 001 — pane XDG rule

## Goal
Panes never get `XDG_DATA_HOME` / `XDG_CACHE_HOME` / `XDG_RUNTIME_DIR` synthesised; unset
`XDG_CONFIG_HOME` / `XDG_STATE_HOME` become spec defaults on XDG-native platforms
(Linux/BSD) and stay `""` on macOS/Windows.

## Prerequisites
Proposal refined (see "Design" table).

## Implementation (`src-tauri/src/backend/herdr/env.rs`)
- Remove `UNSET_AS_EMPTY` and `pane_xdg_env_with(original, bool)`.
- Add a pure `pane_xdg_env_for(original, xdg_native: bool, home: Option<&Path>)`:
  for each `XDG_NAMES` entry in order — set → pass through; unset CONFIG/STATE →
  `home/.config` / `home/.local/state` when `xdg_native` and home is known, else `""`;
  unset DATA/CACHE/RUNTIME → omitted.
- `pane_xdg_env(original)` = `pane_xdg_env_for(original, cfg!(all(unix, not(target_os = "macos"))), dirs::home_dir().as_deref())`.
- Update the module docs (lines 13–17) and the `pane_xdg_env` doc comment.

## Tests
- New: native rule (defaults, omission, pass-through, never `""`), non-native rule,
  native-without-home fallback, order, no `HERDR_*`.
- Update length/all-empty assertions in `env.rs` tests and `herdr/mod.rs`
  `spawned_panes_get_the_users_xdg_env` (assert per-platform via the same predicate).

## Validation
`cargo test -p keitora --lib backend::herdr` then the full `cargo test --workspace`.

## Commit message
`fix(herdr): stop handing panes empty XDG data/cache dirs`
