// Auto-install the session-state hooks into each Codex worktree's
// `.codex/hooks.json` on startup — the Codex analog of [`crate::hooks`]'s
// `.claude/settings.local.json` installer.
//
// Codex ships a native, enabled-by-default hooks engine (`codex features list` →
// `hooks stable true`). A worktree-local `.codex/hooks.json` is discovered per
// session and maps lifecycle events to shell commands, exactly like Claude Code's
// settings hooks. We register the SAME `keitora session <state>` reporting path the
// Claude hooks use, so a Codex worktree drives the daemon/rail identically.
//
// Two behaviours differ from the Claude installer and are load-bearing:
//   1. **Trust.** A freshly-registered Codex hook is UNTRUSTED and will NOT fire
//      until either a human grants trust interactively or the launch passes
//      `--dangerously-bypass-hook-trust`. Every Keitora-spawned session is
//      headless/PTY-driven with no human present, so that flag is added to the
//      Codex launch argv (see `backend::herdr::agent`). Verified empirically: without
//      the flag the hooks are silently skipped; with it they fire.
//   2. **Shell.** Codex runs a hook `command` through **PowerShell** on Windows
//      (verified: `>`-redirected output came back UTF-16LE-with-BOM, PowerShell's
//      signature, and the binary references `powershell.exe`) — NOT the Git Bash
//      that Claude Code uses. So the Windows command is PowerShell syntax (call
//      operator `&`, `*> $null`), while unix stays POSIX `/bin/sh` like Claude.
//
// Only worktrees whose resolved `CliKind` is `Codex` get this file, so a user who
// runs `codex` by hand in a Claude worktree is never prompted to trust our hooks.

use std::path::Path;

use serde_json::{json, Value};

/// (Codex event, session state). The Codex analog of `hooks::HOOK_MAP`, using the
/// same four states the Claude hooks report. Codex's event catalogue
/// (`HookEventName`: `SessionStart, UserPromptSubmit, Stop, PermissionRequest,
/// SessionEnd, …`) is near 1:1 with Claude's, so the mapping mirrors it:
///
/// | Codex event        | State   | Claude analog          |
/// |--------------------|---------|------------------------|
/// | `SessionStart`     | idle    | `SessionStart`         |
/// | `UserPromptSubmit` | busy    | `UserPromptSubmit`     |
/// | `Stop`             | idle    | `Stop`                 |
/// | `PermissionRequest`| waiting | `Notification`/perm    |
/// | `SessionEnd`       | offline | `SessionEnd`           |
///
/// `SessionStart`, `UserPromptSubmit`, `SessionEnd` are empirically confirmed to
/// fire headlessly with these payloads. `Stop` (turn-complete) and
/// `PermissionRequest` (approval-needed) could not be *fired* in the spike (the
/// account hit its usage limit before a turn could complete or request approval),
/// but their config keys are accepted by Codex's parser and are the correct
/// PascalCase `HookEventName` wire names, so they are registered here. Codex has no
/// distinct `StopFailure` analog (Claude's second idle trigger), and `SubagentStop`
/// is deliberately omitted — it fires mid-turn and would wrongly flip to idle.
const CODEX_HOOK_MAP: [(&str, &str); 5] = [
    ("SessionStart", "idle"),
    ("UserPromptSubmit", "busy"),
    ("Stop", "idle"),
    ("PermissionRequest", "waiting"),
    ("SessionEnd", "offline"),
];

/// Detectable substring identifying a hook `command` as ours, so a refresh replaces
/// rather than duplicates it (distinct from the Claude installer's marker). Carried
/// as a trailing `# …` comment — valid in both PowerShell and `/bin/sh`.
const CODEX_HOOK_MARKER: &str = "keitora:codex-session-hook";

/// The resilient hook `command` we install for one (binary, state).
///
/// Codex runs this through **PowerShell** on Windows and **`/bin/sh`** on unix, so
/// the shape is platform-specific (unlike the Claude installer, which is POSIX on
/// every platform because Claude Code always uses Git Bash):
///
/// - **Windows (PowerShell):** `if (Test-Path -LiteralPath "<bin>") { & "<bin>"
///   session <state> *> $null }`. The `Test-Path` guard short-circuits a
///   missing/not-yet-built binary; the call operator `&` runs a quoted absolute
///   path (a bare quoted string would merely be echoed); `*> $null` silences all
///   streams so a hook never leaks output or a non-zero exit into the session.
/// - **Unix (`/bin/sh`):** identical to the Claude hook — `[ -x "<bin>" ] && "<bin>"
///   session <state> >/dev/null 2>&1 || true`.
///
/// The `\\?\` extended-length prefix the resolved build path can carry is stripped
/// (PowerShell's `Test-Path`/`&` and `/bin/sh` both choke on it); UNC-safe, and a
/// no-op on unix / on a path without the prefix.
fn hook_command(task_bin: &str, state: &str) -> String {
    let bin = keitora_tasks::strip_extended_prefix_str(task_bin);
    #[cfg(windows)]
    {
        // Backslashes are literal inside PowerShell double-quotes (backtick is the
        // escape char, not backslash), so a native `C:\…\keitora.exe` is used as-is.
        format!(
            "if (Test-Path -LiteralPath \"{bin}\") {{ & \"{bin}\" session {state} *> $null }} # {CODEX_HOOK_MARKER}"
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "[ -x \"{bin}\" ] && \"{bin}\" session {state} >/dev/null 2>&1 || true # {CODEX_HOOK_MARKER}"
        )
    }
}

/// True if a Codex hook-group entry is one we manage. Matches the current marker and
/// the bare `…keitora session …` / legacy `…task session …` forms, so older installs
/// are recognized and replaced — not duplicated — on the next refresh.
fn is_ours(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|h| {
                h.get("command").and_then(|c| c.as_str()).is_some_and(|c| {
                    c.contains(CODEX_HOOK_MARKER)
                        || c.contains("keitora\" session ")
                        || c.contains("keitora session ")
                        || c.contains("task session ")
                })
            })
        })
}

/// Pure merge: return `existing` with our Codex session hooks ensured. Idempotent;
/// preserves unrelated keys and any non-ours hook entries on shared events. The
/// `.codex/hooks.json` shape mirrors Claude's settings hooks:
/// `{ "hooks": { "<Event>": [ { "hooks": [ { "type": "command", "command": … } ] } ] } }`.
pub fn merged_codex_hooks(existing: Value, task_bin: &str) -> Value {
    let mut root = match existing {
        Value::Object(m) => Value::Object(m),
        _ => json!({}),
    };
    let obj = root.as_object_mut().unwrap();

    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();

    for (event, state) in CODEX_HOOK_MAP {
        let command = hook_command(task_bin, state);
        let entry = json!({
            "hooks": [ { "type": "command", "command": command } ],
        });
        let arr = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        if !arr.is_array() {
            *arr = json!([]);
        }
        let arr = arr.as_array_mut().unwrap();
        arr.retain(|e| !is_ours(e)); // drop our prior entries → idempotent + path-change safe
        arr.push(entry);
    }
    root
}

/// Install/refresh the Codex session hooks for a single worktree at `wt_path`.
///
/// Reads `.codex/hooks.json`, merges our hooks in, and writes back only if the
/// content actually changed (preserving a clean `git status` for unchanged
/// worktrees). Callers gate this on the worktree's resolved `CliKind == Codex`.
pub fn install_codex_hooks_for(wt_path: &Path, task_bin: &str) {
    let dir = wt_path.join(".codex");
    let file = dir.join("hooks.json");

    let existing: Value = match std::fs::read_to_string(&file) {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "[keitora] codex session hooks: skip {} (parse error: {e})",
                    file.display()
                );
                return;
            }
        },
        Err(_) => json!({}), // file absent → start fresh
    };

    let merged = merged_codex_hooks(existing.clone(), task_bin);
    if merged == existing {
        return; // already installed → no write (keep git status clean)
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "[keitora] codex session hooks: mkdir {} failed: {e}",
            dir.display()
        );
        return;
    }
    let pretty = serde_json::to_string_pretty(&merged).unwrap_or_default();
    if let Err(e) = std::fs::write(&file, format!("{pretty}\n")) {
        eprintln!(
            "[keitora] codex session hooks: write {} failed: {e}",
            file.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands_for(v: &Value, event: &str) -> Vec<String> {
        v["hooks"][event]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|e| e["hooks"].as_array().unwrap().iter())
            .map(|h| h["command"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn empty_input_gets_all_five_events_mapped() {
        let out = merged_codex_hooks(json!({}), "/abs/keitora");
        for (event, state) in CODEX_HOOK_MAP {
            assert_eq!(
                commands_for(&out, event),
                vec![hook_command("/abs/keitora", state)],
                "event {event}"
            );
        }
    }

    /// The event → state mapping table itself is the load-bearing spike output;
    /// pin every pair so a future edit can't silently repoint a state.
    #[test]
    fn event_state_mapping_is_exact() {
        let map: std::collections::BTreeMap<_, _> = CODEX_HOOK_MAP.iter().copied().collect();
        assert_eq!(map.get("SessionStart"), Some(&"idle"));
        assert_eq!(map.get("UserPromptSubmit"), Some(&"busy"));
        assert_eq!(map.get("Stop"), Some(&"idle"));
        assert_eq!(map.get("PermissionRequest"), Some(&"waiting"));
        assert_eq!(map.get("SessionEnd"), Some(&"offline"));
        assert_eq!(map.len(), 5, "no unexpected events");
        // `SubagentStop` must never be mapped (fires mid-turn → false idle).
        assert!(!map.contains_key("SubagentStop"));
    }

    /// The installed command must be guarded, silenced, marker-carrying, and use the
    /// right shell per platform (PowerShell on Windows, POSIX `/bin/sh` on unix).
    #[test]
    fn command_is_guarded_silenced_and_marked() {
        let cmd = hook_command("/abs/keitora", "busy");
        assert!(cmd.contains(CODEX_HOOK_MARKER), "carries marker: {cmd}");
        assert!(cmd.contains("session busy"), "invokes state: {cmd}");
        #[cfg(windows)]
        {
            assert!(cmd.starts_with("if (Test-Path"), "guarded: {cmd}");
            assert!(cmd.contains("& \"/abs/keitora\""), "call operator: {cmd}");
            assert!(cmd.contains("*> $null"), "silenced: {cmd}");
        }
        #[cfg(not(windows))]
        {
            assert!(cmd.starts_with("[ -x \"/abs/keitora\" ]"), "guarded: {cmd}");
            assert!(cmd.contains(">/dev/null 2>&1"), "silenced: {cmd}");
            assert!(cmd.contains("|| true"), "never errors: {cmd}");
        }
    }

    #[test]
    fn is_idempotent() {
        let once = merged_codex_hooks(json!({}), "/abs/keitora");
        let twice = merged_codex_hooks(once.clone(), "/abs/keitora");
        assert_eq!(once, twice, "applying twice must not change anything");
    }

    #[test]
    fn preserves_unrelated_keys_and_hooks() {
        let existing = json!({
            "someOtherKey": { "a": 1 },
            "hooks": {
                "Stop": [ { "hooks": [ { "type": "command", "command": "/usr/bin/mine.sh" } ] } ],
                "PostToolUse": [ { "hooks": [ { "type": "command", "command": "/usr/bin/audit.sh" } ] } ]
            }
        });
        let out = merged_codex_hooks(existing, "/abs/keitora");
        // unrelated top-level key untouched
        assert_eq!(out["someOtherKey"]["a"], 1);
        // an unrelated event we don't manage is left intact
        assert_eq!(commands_for(&out, "PostToolUse"), vec!["/usr/bin/audit.sh"]);
        // the user's own Stop hook is kept, ours appended
        let stop = commands_for(&out, "Stop");
        assert!(stop.contains(&"/usr/bin/mine.sh".to_string()));
        assert!(stop.contains(&hook_command("/abs/keitora", "idle")));
        assert_eq!(stop.len(), 2);
    }

    #[test]
    fn updates_changed_binary_path_without_duplicating() {
        let first = merged_codex_hooks(json!({}), "/old/keitora");
        let second = merged_codex_hooks(first, "/new/keitora");
        assert_eq!(
            commands_for(&second, "Stop"),
            vec![hook_command("/new/keitora", "idle")]
        );
        assert_eq!(
            commands_for(&second, "UserPromptSubmit"),
            vec![hook_command("/new/keitora", "busy")]
        );
    }

    #[test]
    fn legacy_bare_command_is_replaced_not_duplicated() {
        // A worktree carrying an older unmarked `keitora session …` command must be
        // recognized as ours and upgraded to the guarded form — not left as a dupe.
        let legacy = json!({
            "hooks": {
                "SessionEnd": [ {
                    "hooks": [ { "type": "command", "command": "/abs/keitora session offline" } ]
                } ]
            }
        });
        let out = merged_codex_hooks(legacy, "/abs/keitora");
        assert_eq!(
            commands_for(&out, "SessionEnd"),
            vec![hook_command("/abs/keitora", "offline")]
        );
    }
}
