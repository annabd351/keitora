# 0057 — implementation entry point

Proposal: `plans/0057-pane-xdg-unset.md`. Branch: `llm/andrew/keitora/0057-pane-xdg-unset`.

Stages (sequential):
1. `001-pane-xdg-rule.md` — replace the unset-as-empty rule with the per-var, platform-aware rule.

Test rigor: the new rule is pure and ships with `#[cfg(test)]` coverage for both the
XDG-native and non-native behaviour, runnable on any host. `cargo test -p keitora` (src-tauri)
must pass. Tests must never touch a running herdr (the herdr backend tests use the fake).
