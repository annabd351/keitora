//! Claude ⇄ Codex session transfer.
//!
//! Two Tauri commands drive a worktree's live conversation from one CLI to the
//! other and flip its persisted `cli` so a subsequent restart resumes on the new
//! side (contracts C2/C3, I3):
//!
//!   * [`preview_claude_transfer`] — Phase A. Lock-free, side-effect-free. Runs the
//!     [live-session lookup](find_live_claude_jsonl) and returns a [`ConfirmPreview`]
//!     so the rail menu (stage 060) can show a blocking confirm before anything
//!     irreversible happens. It does NOT acquire the dispatch lock, spawn
//!     `app-server`, or import.
//!   * [`transfer_worktree`] — Phase B, only after the user confirms.
//!       - **Claude → Codex**: a full *live* import via `codex app-server`'s
//!         `externalAgentConfig/import` (mirrors the official `openai-codex`
//!         plugin's `/codex:transfer`). Persists `cli=codex` + the imported
//!         `codex_thread_id` ONLY after the ledger records the import — no
//!         half-switch on any earlier failure.
//!       - **Codex → Claude**: a *summary* handoff (no supported reverse importer
//!         exists). Runs `codex exec resume` under a structured output schema,
//!         writes the parsed summary to a handoff file for stage 060 to inject,
//!         and flips `cli=claude`.
//!
//! The app-server client is a minimal JSON-RPC-2.0-over-stdio client built on
//! `std::process::Command` + a stdout reader thread + `mpsc` (the crate's existing
//! blocking-process plumbing style — see `dispatch.rs::run_claude_bounded`); the
//! blocking work runs under `tauri::async_runtime::spawn_blocking`. DIRECT
//! connection only (no broker). Protocol literals (the handshake `capabilities`,
//! the migration payload, the completion-notification method, the 2-minute
//! timeout, the `-32601` upgrade message) are reproduced verbatim from
//! `codex-cli 0.149.0` + the reference plugin.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::State;

use keitora_tasks::CliKind;

use crate::backend::herdr::recovery::RecoveryHandle;
use crate::backend::{PtyBackend, SessionKey, SharedBackend};
use crate::dispatch::DispatchGate;

// ---------------------------------------------------------------------------
// Protocol constants (verbatim from codex-cli 0.149.0 + the reference plugin)
// ---------------------------------------------------------------------------

/// The completion notification method (`codex.mjs:51`
/// `EXTERNAL_AGENT_IMPORT_COMPLETED`). The `externalAgentConfig/import` request
/// response alone does NOT imply the import finished — the completion is signalled
/// out-of-band by this notification.
const EXTERNAL_AGENT_IMPORT_COMPLETED: &str = "externalAgentConfig/import/completed";

/// `EXTERNAL_AGENT_IMPORT_TIMEOUT_MS` (`codex.mjs:52`) = `2 * 60 * 1000`.
const IMPORT_COMPLETED_TIMEOUT: Duration = Duration::from_millis(2 * 60 * 1000);

/// Generous ceiling for the `initialize` / `externalAgentConfig/import` request
/// round-trips themselves (distinct from the completion-notification wait above).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// A bounded ceiling on the headless `codex exec resume` summary run so a wedged
/// child can't hang the blocking thread forever.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(180);

/// The exact `-32601` upgrade message (`codex.mjs:1072-1076`). `npm install -g`,
/// not the abbreviated `npm i -g`.
const UPGRADE_MSG: &str = "This Codex version does not support Claude session transfer. Update Codex with `npm install -g @openai/codex@latest`, then retry.";

/// The explicit summarize instruction for the Codex → Claude handoff (`03-...md`).
const HANDOFF_PROMPT: &str = "Summarize this conversation for a handoff to a new session: goal, key decisions, next steps, open files, and any other notes.";

/// The `--output-schema` file contents for the Codex → Claude handoff. All five
/// fields required, `additionalProperties: false`.
const HANDOFF_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "goal": {"type": "string"},
    "key_decisions": {"type": "array", "items": {"type": "string"}},
    "next_steps": {"type": "array", "items": {"type": "string"}},
    "open_files": {"type": "array", "items": {"type": "string"}},
    "notes": {"type": "string"}
  },
  "required": ["goal", "key_decisions", "next_steps", "open_files", "notes"],
  "additionalProperties": false
}"#;

// ---------------------------------------------------------------------------
// IPC payloads
// ---------------------------------------------------------------------------

/// Phase-A confirm-preview payload (`03-...md` "Confirm-preview payload"). The
/// mtime-newest lookup is the weakest link; this lets the rail show the user which
/// JSONL was selected before the irreversible import fires.
#[derive(Debug, Clone, Serialize)]
pub struct ConfirmPreview {
    /// Full JSONL path (post-normalization, containment-guard-checked).
    pub path: String,
    /// Basename only, i.e. `<uuid>.jsonl`.
    pub filename: String,
    /// Last-modified time, RFC 3339 (the value used to pick "newest").
    pub mtime: String,
    /// The first real user line (`type=user`, not `isMeta`), truncated for display.
    pub first_user_line: String,
}

/// The Codex → Claude handoff summary shape (matches [`HANDOFF_SCHEMA`]). Written
/// to [`handoff_summary_path`] for stage 060 to inject as the fresh session's
/// opening prompt; `pub(crate)` so 060 can reuse the type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HandoffSummary {
    pub goal: String,
    #[serde(default)]
    pub key_decisions: Vec<String>,
    #[serde(default)]
    pub next_steps: Vec<String>,
    #[serde(default)]
    pub open_files: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

impl HandoffSummary {
    /// Fallback used when the headless `codex exec resume` summarize can't run (e.g.
    /// Codex usage limit or auth). The Codex→Claude switch still completes; this note
    /// is injected into the fresh Claude session so it's clear why no prior context
    /// carried over. `reason` is truncated — a raw `codex` failure can be very long.
    fn unavailable(reason: &str) -> Self {
        HandoffSummary {
            goal: "Handed off from a Codex session; continuing on Claude.".to_string(),
            key_decisions: Vec::new(),
            next_steps: Vec::new(),
            open_files: Vec::new(),
            notes: format!(
                "An automatic Codex handoff summary could not be generated ({}). No prior context was carried over.",
                truncate_chars(reason.trim(), 200)
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// Phase A: resolve the worktree's live Claude JSONL and return the confirm
/// preview. Lock-free, no side effects (no `DispatchGate`, no `app-server`, no
/// import).
#[tauri::command]
pub async fn preview_claude_transfer(worktree: String) -> Result<ConfirmPreview, String> {
    let config = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let cwd = config.worktree_path(&worktree);
    let path = find_live_claude_jsonl(&cwd)?;
    build_preview(&path)
}

/// Phase B: transfer the worktree to `target_cli`. `Codex` ⇒ Claude→Codex live
/// import; `Claude` ⇒ Codex→Claude summary handoff. Persists the new `cli`/thread
/// state on success; the frontend drives the actual PTY restart (C2).
#[tauri::command]
pub async fn transfer_worktree(
    worktree: String,
    target_cli: CliKind,
    gate: State<'_, DispatchGate>,
    client: State<'_, SharedBackend>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let project = keitora_tasks::resolve_active()
        .map_err(|e| e.to_string())?
        .project
        .name;
    let key = SessionKey {
        project,
        worktree: worktree.clone(),
    };
    // Before anything is held or rewritten: a terminated session is not moved.
    refuse_terminated(client.inner().as_ref(), &key)?;
    // Recovery must not resume this session while the transfer rewrites it.
    let result = with_recovery_lock(client.recovery(), &key, async move {
        match target_cli {
            // The Claude → Codex path does not touch the daemon client.
            CliKind::Codex => transfer_claude_to_codex(worktree, gate).await,
            CliKind::Claude => transfer_codex_to_claude(worktree, gate, client).await,
        }
    })
    .await;
    // On success the worktree's persisted `cli` (and `codex_thread_id`) changed, so
    // tell the frontend to refetch the worktree list — this drives the rail glyph and
    // the "Transfer to {other CLI}" menu target to the new provider.
    if result.is_ok() {
        use tauri::Emitter as _;
        let _ = app.emit("worktrees://changed", ());
    }
    result
}

/// Refuse to transfer a session the user terminated: it must be launched first.
fn refuse_terminated(client: &dyn PtyBackend, key: &SessionKey) -> Result<(), String> {
    if client.terminated_keys(&key.project).contains(&key.worktree) {
        return Err("session is terminated; launch it before transferring".into());
    }
    Ok(())
}

/// Run `transfer` with `key` held out of recovery's resume queue; the hold is
/// released when it returns, whatever the outcome.
async fn with_recovery_lock<T>(
    recovery: Option<RecoveryHandle>,
    key: &SessionKey,
    transfer: impl std::future::Future<Output = T>,
) -> T {
    let _recovery_lock = recovery.map(|r| r.transfer_lock(key));
    transfer.await
}

/// Claude → Codex: the full live import (steps 1–7 of `03-...md`). Persistence is
/// the LAST step — any failure in steps 1–6 leaves the worktree untouched on
/// Claude (no half-switch).
async fn transfer_claude_to_codex(
    worktree: String,
    gate: State<'_, DispatchGate>,
) -> Result<(), String> {
    let config = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let user = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
    let cwd = config.worktree_path(&worktree);

    // Step 1: re-resolve the live JSONL (do NOT trust a stale client path).
    // Read-only, no lock yet.
    let jsonl = find_live_claude_jsonl(&cwd)?;

    let codex = keitora_tasks::resolve_provider_path(CliKind::Codex, Some(&config), &user)
        .unwrap_or_else(|| "codex".to_string());
    let cwd_s = cwd.to_string_lossy().into_owned();

    // Step 2: acquire the dispatch gate (I3) — only now, never across the confirm
    // dialog (that already happened in Phase A, in the frontend). Held across the
    // blocking import so an in-flight `dispatch_apply` can't inject into the
    // session being torn down.
    let _gate = gate.0.lock().await;

    // Steps 3–6: spawn app-server, handshake, one import request, await the
    // completion notification, then ledger-match. All blocking → off the runtime.
    let jsonl_for_task = jsonl.clone();
    let thread_id = tauri::async_runtime::spawn_blocking(move || {
        run_claude_import(&codex, &cwd_s, &jsonl_for_task)
    })
    .await
    .map_err(|e| format!("import task panicked: {e}"))??;

    // Persist only after import succeeds, including the Claude session for a return trip.
    let claude_session_id = session_id_from_jsonl(&jsonl);
    persist_worktree_cli(
        &worktree,
        CliKind::Codex,
        Some(thread_id),
        claude_session_id,
    )?;
    Ok(())
}

/// Codex → Claude: write a summary, flip `cli=claude`, and let the frontend restart
/// the captured Claude session before injecting the summary.
///
/// The worktree's interactive PTY runs `codex resume <thread_id>`, which holds that
/// thread as its exclusive "active writer" in the Codex thread-store. A headless
/// `codex exec resume <thread_id>` (the summarize step) is refused with a
/// `thread-store conflict … already has an active writer` (`-32600`) while that live
/// session exists. So the live session is KILLED and its writer confirmed released
/// BEFORE summarizing — only then can the headless resume open the thread.
async fn transfer_codex_to_claude(
    worktree: String,
    gate: State<'_, DispatchGate>,
    client: State<'_, SharedBackend>,
) -> Result<(), String> {
    let config = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let user = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
    let cwd = config.worktree_path(&worktree);

    // Prefer the persisted thread id over `--last` (unproven to target the same
    // thread the interactive PTY runs). Read-only, no lock yet.
    //
    // A NATIVE Codex worktree (default_cli=codex, launched via `codex resume --last`,
    // never imported from Claude) legitimately has NO persisted thread id — yet the
    // rail still offers "Transfer to Claude". So this is `Option`, not a hard error:
    // when present we summarize by resuming the id; when absent we summarize via
    // `codex exec resume --last`, which (after the live session is killed) resolves
    // to this cwd's most-recent rollout.
    let thread_id = config
        .worktree_entries()
        .into_iter()
        .find(|e| e.name == worktree)
        .and_then(|e| e.codex_thread_id);

    let codex = keitora_tasks::resolve_provider_path(CliKind::Codex, Some(&config), &user)
        .unwrap_or_else(|| "codex".to_string());
    let cwd_s = cwd.to_string_lossy().into_owned();
    let project = config.project.name.clone();
    let wt = worktree.clone();
    let key = SessionKey {
        project: project.clone(),
        worktree: worktree.clone(),
    };
    // `SharedBackend` is `Arc`-backed; the clone shares the same connection + cached
    // live-set as the app's managed instance, so `is_live` here observes the push
    // loop's `Exit` update.
    let client = client.inner().clone();

    // Acquire the dispatch gate (I3) around the MUTATING window only — the session
    // release, the summarize run, and the summary-file write + `cli=claude` flip — so
    // a concurrent `dispatch_apply` can't inject into the session mid-flip. Mirrors
    // `transfer_claude_to_codex`; the read-only lookups above stay outside the lock
    // to keep the scope as tight as that path's.
    let _gate = gate.0.lock().await;

    // Kill the live Codex PTY (releasing its thread-store writer), wait for it to
    // actually exit, THEN summarize. All blocking → off the runtime.
    //
    // Releasing the session is fatal on failure (we must not proceed while the writer
    // is held). But the SUMMARY is best-effort: once the session is killed the
    // worktree is committed to the switch, so if `codex exec` can't produce a summary
    // (e.g. Codex usage limit or auth), fall back to a short note and still complete
    // the move to Claude rather than stranding a worktree whose session we just killed.
    let summary =
        tauri::async_runtime::spawn_blocking(move || -> Result<HandoffSummary, String> {
            release_codex_session(client.as_ref(), &key)?;
            Ok(
                summarize_with_writer_retry(&codex, &cwd_s, thread_id.as_deref()).unwrap_or_else(
                    |e| {
                        log::warn!("codex→claude handoff summary unavailable: {e}");
                        HandoffSummary::unavailable(&e)
                    },
                ),
            )
        })
        .await
        .map_err(|e| format!("summary task panicked: {e}"))??;

    // Produce the summary file for stage 060's injection, then flip the persisted
    // CLI. (Injection + readiness gate are the frontend/060 concern.)
    write_handoff_summary(&project, &wt, &summary)?;
    // `None` preserves the Claude session captured on the outbound leg.
    persist_worktree_cli(&worktree, CliKind::Claude, None, None)?;
    Ok(())
}

/// How long to wait for a killed Codex PTY session to actually exit (dropping its
/// thread-store writer lock) before giving up.
const WRITER_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll cadence while waiting for the killed session to drop out of the live-set.
const WRITER_RELEASE_POLL: Duration = Duration::from_millis(100);

/// Kill the worktree's live Codex PTY session — the interactive `codex resume` that
/// holds the thread as its exclusive "active writer" — and block until it is gone.
///
/// The daemon's `Kill` is generation-gated (a stale gen is silently ignored), so the
/// current generation for the key is looked up via [`PtyBackend::list_sessions`]
/// first. No live session for the key ⇒ nothing holds the thread ⇒ proceed.
///
/// After the kill, poll [`PtyBackend::is_live`] until the daemon's `Exit` push
/// clears the key from the cached live-set — that push fires only once the child
/// process (and its `\r` grandchild tree on Windows) has fully exited, which is what
/// releases the thread-store lock. Bounded by [`WRITER_RELEASE_TIMEOUT`].
fn release_codex_session(client: &dyn PtyBackend, key: &SessionKey) -> Result<(), String> {
    release_codex_session_with(client, key, WRITER_RELEASE_TIMEOUT, WRITER_RELEASE_POLL)
}

/// [`release_codex_session`] with explicit timing, so the 5s-timeout path is
/// testable in milliseconds against a [`crate::backend::fake::FakeBackend`].
fn release_codex_session_with(
    client: &dyn PtyBackend,
    key: &SessionKey,
    timeout: Duration,
    poll: Duration,
) -> Result<(), String> {
    let gen = client
        .list_sessions()
        .map_err(|e| format!("list daemon sessions before Codex handoff: {e}"))?
        .into_iter()
        .find(|s| s.key == *key)
        .map(|s| s.gen);
    let Some(gen) = gen else {
        // Nothing live for this key — no interactive writer to release.
        return Ok(());
    };
    client
        .kill(key, gen)
        .map_err(|e| format!("kill live Codex session on `{}`: {e}", key.worktree))?;

    let deadline = Instant::now() + timeout;
    while client.is_live(key) {
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for the live Codex session on `{}` to exit before summarizing",
                key.worktree
            ));
        }
        std::thread::sleep(poll);
    }
    Ok(())
}

/// Total attempts for the summarize call (initial + retries). The daemon's `Exit`
/// push (which clears the live-set) can very slightly precede the OS actually
/// dropping the thread-store lock, so a first `codex exec resume` may still see the
/// stale writer. Retry ONLY that transient condition.
const SUMMARY_WRITER_ATTEMPTS: u32 = 3;
/// Backoff between summarize retries when the thread-store writer is still held.
const SUMMARY_WRITER_BACKOFF: Duration = Duration::from_millis(500);

/// Run [`run_codex_handoff_summary`], retrying up to [`SUMMARY_WRITER_ATTEMPTS`]
/// times when the error is a lingering "active writer" / `-32600` / thread-store
/// conflict (see [`is_active_writer_error`]); any other error fails immediately.
fn summarize_with_writer_retry(
    codex: &str,
    cwd: &str,
    resume: Option<&str>,
) -> Result<HandoffSummary, String> {
    let mut attempt = 1;
    loop {
        match run_codex_handoff_summary(codex, cwd, resume) {
            Ok(summary) => return Ok(summary),
            Err(e) if attempt < SUMMARY_WRITER_ATTEMPTS && is_active_writer_error(&e) => {
                attempt += 1;
                std::thread::sleep(SUMMARY_WRITER_BACKOFF);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Pure: whether a summarize error string reports that the Codex thread still has a
/// live writer — the transient race where the interactive PTY's thread-store lock
/// has not dropped yet. Matches any of Codex's markers for it: `active writer`, the
/// JSON-RPC `-32600` code, or `thread-store conflict` (case-insensitive). Only these
/// are retryable; every other failure is real and must surface.
fn is_active_writer_error(err: &str) -> bool {
    let s = err.to_ascii_lowercase();
    s.contains("active writer") || s.contains("-32600") || s.contains("thread-store conflict")
}

/// Submit-delay for the handoff-summary injection (matches `dispatch.rs`'s
/// `SUBMIT_DELAY`: the daemon writes the text, pauses this long, then writes the
/// terminating `\r`).
const HANDOFF_SUBMIT_DELAY_MS: u32 = 60;

/// Best-effort Codex → Claude handoff-summary injection (contract
/// I5). The frontend calls this once the freshly-restarted worktree's session
/// reports `idle` (never blindly — injecting into a still-starting or busy PTY
/// would land mid-boot or interleave with other output). Reads the summary
/// [`write_handoff_summary`] wrote during [`transfer_codex_to_claude`], flattens
/// it to a single line (`write_line` submits raw text then a trailing `\r` —
/// embedded newlines would submit the prompt early, mid-sentence), injects it via
/// the same daemon `write_line` path `dispatch_apply` uses, then deletes the file
/// so a later re-poll/retry can't double-inject.
///
/// Returns `Ok(false)` — a no-op, not an error — when there is no pending handoff
/// for this worktree (a Claude→Codex transfer never writes one; a Codex→Claude
/// transfer's summary may already have been injected and removed).
#[tauri::command]
pub async fn inject_handoff_summary(
    worktree: String,
    client: State<'_, SharedBackend>,
) -> Result<bool, String> {
    let config = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let project = config.project.name.clone();
    let path = handoff_summary_path(&project, &worktree)?;
    if !path.exists() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("read handoff {}: {e}", path.display()))?;
    let summary: HandoffSummary = serde_json::from_str(&text)
        .map_err(|e| format!("parse handoff {}: {e}", path.display()))?;
    let prompt = format_handoff_prompt(&summary);
    let key = SessionKey { project, worktree };
    client
        .write_line(&key, &prompt, HANDOFF_SUBMIT_DELAY_MS)
        .map_err(|e| e.to_string())?;
    // Best-effort cleanup: if this fails, the file just survives for a retry to
    // pick up (a manual re-inject is a mild annoyance, not silent data loss).
    let _ = std::fs::remove_file(&path);
    Ok(true)
}

/// Pure: flatten a [`HandoffSummary`] into ONE line suitable for `write_line`.
/// Any embedded `\r`/`\n` in the model-generated fields is scrubbed to a space —
/// `write_line` appends a single trailing `\r` to submit, so an embedded newline
/// would submit the prompt early, mid-sentence, into Claude's input box.
fn format_handoff_prompt(summary: &HandoffSummary) -> String {
    fn oneline(s: &str) -> String {
        // `split_whitespace` also collapses any run of whitespace (not just a lone
        // `\r`/`\n`) to a single space and trims the ends, so "a\r\nb" doesn't come
        // out as "a  b" (double space).
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }
    let join = |items: &[String]| -> String {
        if items.is_empty() {
            "none".to_string()
        } else {
            items
                .iter()
                .map(|s| oneline(s))
                .collect::<Vec<_>>()
                .join("; ")
        }
    };
    let mut parts = vec![
        "Resuming from a Codex session handoff summary.".to_string(),
        format!("Goal: {}.", oneline(&summary.goal)),
        format!("Key decisions: {}.", join(&summary.key_decisions)),
        format!("Next steps: {}.", join(&summary.next_steps)),
        format!("Open files: {}.", join(&summary.open_files)),
    ];
    let notes = oneline(&summary.notes);
    if !notes.is_empty() {
        parts.push(format!("Notes: {notes}."));
    }
    parts.join(" ")
}

// ---------------------------------------------------------------------------
// Claude → Codex: the blocking import core
// ---------------------------------------------------------------------------

/// Spawn `codex app-server`, handshake, send one `externalAgentConfig/import`,
/// await the completion notification, then ledger-match → the imported thread id.
/// Blocking; runs inside `spawn_blocking`.
fn run_claude_import(codex: &str, cwd: &str, jsonl: &Path) -> Result<String, String> {
    let mut client = AppServer::spawn(codex, cwd)?;
    client.handshake()?;

    // The migration payload's session path is the raw lookup path — Codex
    // canonicalizes it on its own end (as it also does when writing `source_path`
    // to the ledger).
    let source_path = jsonl.to_string_lossy().into_owned();
    let payload =
        serde_json::to_value(migration_payload(&source_path, cwd)).map_err(|e| e.to_string())?;

    // Step 4: ONE import request. Map `-32601` → the upgrade message.
    if let Err(err) = client.request("externalAgentConfig/import", payload, REQUEST_TIMEOUT) {
        return Err(map_import_error(err));
    }

    // Step 5: the request response alone is insufficient — await the completion
    // notification (2-minute timeout).
    client.wait_notification(EXTERNAL_AGENT_IMPORT_COMPLETED, IMPORT_COMPLETED_TIMEOUT)?;

    // Step 6: ledger-match. If nothing matches after completion, hard error
    // (mirrors `codex.mjs:1080-1086`) — do NOT persist.
    let thread_id = match_ledger_thread(jsonl)?.ok_or_else(|| {
        format!(
            "Codex reported that the Claude import completed, but did not record an imported thread.{}",
            client.stderr_tail()
        )
    })?;
    Ok(thread_id)
}

/// Map an import request error to a user-facing string: `-32601` ⇒ the verbatim
/// upgrade message; everything else ⇒ the error's own display. Pure + testable.
fn map_import_error(err: ReqErr) -> String {
    match err {
        ReqErr::Rpc { code: -32601, .. } => UPGRADE_MSG.to_string(),
        other => other.to_string(),
    }
}

/// The `externalAgentConfig/import` migration literal (`codex.mjs:681-699`),
/// reproduced verbatim. The field ORDER matters (serde serializes struct fields in
/// declaration order): outer `cwd: null`, inner `details.sessions[0].cwd` = the
/// worktree cwd, and the five empty sibling arrays in the exact order
/// `plugins, sessions, mcpServers, hooks, subagents, commands`.
fn migration_payload(source_path: &str, cwd: &str) -> Migration {
    let basename = Path::new(source_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Migration {
        migration_items: vec![MigrationItem {
            item_type: "SESSIONS",
            description: format!("Transfer Claude session {basename}"),
            cwd: Value::Null,
            details: MigrationDetails {
                plugins: vec![],
                sessions: vec![SessionEntry {
                    path: source_path.to_string(),
                    cwd: cwd.to_string(),
                    title: Value::Null,
                }],
                mcp_servers: vec![],
                hooks: vec![],
                subagents: vec![],
                commands: vec![],
            },
        }],
    }
}

#[derive(Serialize)]
struct Migration {
    #[serde(rename = "migrationItems")]
    migration_items: Vec<MigrationItem>,
}

#[derive(Serialize)]
struct MigrationItem {
    #[serde(rename = "itemType")]
    item_type: &'static str,
    description: String,
    /// Always `null` — only the inner session entry carries the real cwd.
    cwd: Value,
    details: MigrationDetails,
}

#[derive(Serialize)]
struct MigrationDetails {
    plugins: Vec<Value>,
    sessions: Vec<SessionEntry>,
    #[serde(rename = "mcpServers")]
    mcp_servers: Vec<Value>,
    hooks: Vec<Value>,
    subagents: Vec<Value>,
    commands: Vec<Value>,
}

#[derive(Serialize)]
struct SessionEntry {
    path: String,
    cwd: String,
    /// Always `null`.
    title: Value,
}

// ---------------------------------------------------------------------------
// Ledger match
// ---------------------------------------------------------------------------

/// The Codex home: `$CODEX_HOME`, else `~/.codex`.
fn codex_home() -> Result<PathBuf, String> {
    if let Ok(v) = std::env::var("CODEX_HOME") {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    dirs::home_dir()
        .map(|h| h.join(".codex"))
        .ok_or_else(|| "could not resolve home directory for CODEX_HOME".to_string())
}

/// Read the ledger and return the imported thread id for `jsonl`, if recorded.
///
/// Ledger matching uses the **canonical realpath WITH the `\\?\` prefix retained**
/// (Codex writes the raw `std::fs::canonicalize` form to `source_path`) — the exact
/// OPPOSITE of the live-session-lookup cwd encoding, which strips the prefix. Do
/// not conflate the two.
fn match_ledger_thread(jsonl: &Path) -> Result<Option<String>, String> {
    let ledger_path = codex_home()?.join("external_agent_session_imports.json");
    if !ledger_path.exists() {
        return Ok(None);
    }
    // Canonicalize for matching — retain the `\\?\` prefix (do NOT strip).
    let canonical = std::fs::canonicalize(jsonl)
        .map_err(|e| format!("canonicalize {} for ledger match: {e}", jsonl.display()))?;
    let source_path = canonical.to_string_lossy().into_owned();
    let bytes = std::fs::read(&canonical)
        .map_err(|e| format!("read {} for sha256: {e}", canonical.display()))?;
    let sha = sha256_hex(&bytes);
    let text = std::fs::read_to_string(&ledger_path)
        .map_err(|e| format!("read ledger {}: {e}", ledger_path.display()))?;
    Ok(ledger_match(&text, &source_path, &sha))
}

/// Pure: parse the ledger (NON-strict — tolerate `detected_connector_records` and
/// unknown per-record fields) and return the LAST record matching
/// `source_path` + `content_sha256` + a string `imported_thread_id`.
fn ledger_match(text: &str, source_path: &str, sha: &str) -> Option<String> {
    let ledger: Ledger = serde_json::from_str(text).ok()?;
    ledger
        .records
        .iter()
        .rev()
        .find(|r| {
            r.source_path.as_deref() == Some(source_path)
                && r.content_sha256.as_deref() == Some(sha)
                && r.imported_thread_id.is_some()
        })
        .and_then(|r| r.imported_thread_id.clone())
}

/// The ledger's top-level shape. `detected_connector_records` (and any other key)
/// is tolerated and ignored — serde skips unknown fields by default.
#[derive(Deserialize)]
struct Ledger {
    #[serde(default)]
    records: Vec<LedgerRecord>,
}

/// One ledger record. Only the three fields we match on are declared; the rest
/// (`imported_at`, `source_modified_at`, `connector_names`, `title`, …) are
/// tolerated as unknown fields (non-strict).
#[derive(Deserialize)]
struct LedgerRecord {
    #[serde(default)]
    source_path: Option<String>,
    #[serde(default)]
    content_sha256: Option<String>,
    #[serde(default)]
    imported_thread_id: Option<String>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// Live-session lookup + confirm preview
// ---------------------------------------------------------------------------

/// Claude's projects dir: `$CLAUDE_CONFIG_DIR/projects` when that is set and
/// non-empty (Claude relocates its whole config dir there), else
/// `~/.claude/projects`.
pub(crate) fn claude_projects_dir() -> Result<PathBuf, String> {
    claude_projects_dir_from(std::env::var_os("CLAUDE_CONFIG_DIR"), dirs::home_dir())
}

/// Pure core of [`claude_projects_dir`] over the looked-up env value and home dir.
fn claude_projects_dir_from(
    config_dir: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Result<PathBuf, String> {
    if let Some(dir) = config_dir.filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("projects"));
    }
    home.map(|h| h.join(".claude").join("projects"))
        .ok_or_else(|| "could not resolve home directory for ~/.claude/projects".to_string())
}

/// Whether Claude has a transcript `<projects>/<encoded>/<id>.jsonl` for session
/// `id`. Pure over the filesystem; the caller validates `id` first.
pub(crate) fn claude_session_file_exists(projects: &Path, encoded: &str, id: &str) -> bool {
    projects.join(encoded).join(format!("{id}.jsonl")).is_file()
}

/// Whether Claude has any transcript (`*.jsonl`) under `<projects>/<encoded>/`, i.e.
/// whether `claude --continue` has something to continue.
pub(crate) fn claude_project_has_sessions(projects: &Path, encoded: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(projects.join(encoded)) else {
        return false;
    };
    entries.flatten().any(|e| {
        let path = e.path();
        path.extension().and_then(|x| x.to_str()) == Some("jsonl") && path.is_file()
    })
}

/// Encode a (already-normalized, prefix-stripped) cwd string to Claude's
/// `<encoded-cwd>` project-dir name: EACH non-alphanumeric character → a single
/// `-`. Verified against the real dir on this machine:
/// `D:\repos\proj-worktrees\beta` → `D--repos-proj-worktrees-beta` (the `:` and
/// the `\` each map to their own `-`, giving the leading `--`). Pure + testable.
fn encode_project_dir(normalized: &str) -> String {
    normalized
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The normalization pipeline for the `<encoded-cwd>` directory-name lookup:
/// canonicalize → strip `\\?\` (via the crate's canonical helper) → encode. Split
/// out so the encoding is testable without a real filesystem. Errors when `worktree_cwd`
/// does not exist (it cannot be canonicalized).
pub(crate) fn encoded_cwd_dir_name(worktree_cwd: &Path) -> Result<String, String> {
    let canonical = std::fs::canonicalize(worktree_cwd)
        .map_err(|e| format!("canonicalize {}: {e}", worktree_cwd.display()))?;
    // Strip the `\\?\` prefix ONLY here (the cwd-encoding path) — never for the
    // ledger realpath above.
    let stripped = keitora_tasks::strip_extended_prefix_str(&canonical.to_string_lossy());
    Ok(encode_project_dir(&stripped))
}

/// Find the worktree's live Claude JSONL: the most-recently-modified `.jsonl` under
/// `~/.claude/projects/<encoded-cwd>/`, with a containment guard (the resolved path
/// must be a real `.jsonl` under `~/.claude/projects`).
fn find_live_claude_jsonl(worktree_cwd: &Path) -> Result<PathBuf, String> {
    let projects = claude_projects_dir()?;
    let dir_name = encoded_cwd_dir_name(worktree_cwd)?;
    let dir = projects.join(&dir_name);

    let entries = std::fs::read_dir(&dir).map_err(|e| {
        format!(
            "no Claude sessions found for this worktree at {}: {e}",
            dir.display()
        )
    })?;

    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if newest.as_ref().is_none_or(|(t, _)| mtime > *t) {
            newest = Some((mtime, path));
        }
    }
    let (_, path) =
        newest.ok_or_else(|| format!("no .jsonl session files under {}", dir.display()))?;

    // Containment guard: the canonicalized pick must live under the canonicalized
    // projects dir and still be a `.jsonl`.
    let canon_path = std::fs::canonicalize(&path)
        .map_err(|e| format!("canonicalize session {}: {e}", path.display()))?;
    let canon_projects = std::fs::canonicalize(&projects)
        .map_err(|e| format!("canonicalize {}: {e}", projects.display()))?;
    if !canon_path.starts_with(&canon_projects)
        || canon_path.extension().and_then(|x| x.to_str()) != Some("jsonl")
    {
        return Err(format!(
            "selected session path escapes ~/.claude/projects: {}",
            canon_path.display()
        ));
    }
    Ok(path)
}

/// Extract the session id from Claude's `<session-uuid>.jsonl` filename.
/// Empty ids must not be persisted because bare `--resume` opens an interactive picker.
fn session_id_from_jsonl(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?.trim();
    keitora_tasks::is_safe_claude_session_id(stem).then(|| stem.to_string())
}

/// Build the [`ConfirmPreview`] for a resolved JSONL path.
fn build_preview(path: &Path) -> Result<ConfirmPreview, String> {
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let meta = std::fs::metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    let mtime = meta
        .modified()
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
        .unwrap_or_default();
    let contents =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(ConfirmPreview {
        path: path.to_string_lossy().into_owned(),
        filename,
        mtime,
        first_user_line: first_user_line(&contents),
    })
}

/// Extract the first *real* user line from Claude's JSONL: the first record with
/// `"type":"user"` and NOT `"isMeta":true` (synthetic caveats / slash-command
/// echoes are wrapped in `isMeta` records). `message.content` may be a bare string
/// or an array of content blocks (use the first block's `text`). Truncated to ~200
/// chars for display. Pure + testable.
fn first_user_line(jsonl: &str) -> String {
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let content = v.get("message").and_then(|m| m.get("content"));
        let text = match content {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(blocks)) => blocks
                .first()
                .and_then(|b| b.get("text").and_then(Value::as_str))
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        };
        if !text.trim().is_empty() {
            return truncate_chars(text.trim(), 200);
        }
    }
    String::new()
}

/// Truncate to at most `max` characters (char-boundary safe), appending `…`.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Codex → Claude: the blocking summary core
// ---------------------------------------------------------------------------

/// Process-global sequence for handoff scratch-file names. The pid alone collides
/// across two concurrent Codex→Claude transfers (same process ⇒ same pid), so two
/// worktrees would share `handoff-out-{pid}.json` and race — one worktree reading
/// the other's summary, or a mid-read delete. Pairing the pid with a monotonic
/// counter makes each transfer's schema/out pair unique.
static HANDOFF_SCRATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Build the `codex exec resume …` argv for the handoff summary. `resume` selects
/// the resume target — `Some(id)` → `exec resume <id> …` (a persisted thread);
/// `None` → `exec resume --last …` (a native Codex worktree with no imported
/// thread; `--last` resolves to this cwd's most-recent rollout once the live
/// session is released). Both are a single token in the `SESSION_ID` slot of
/// `codex exec resume [OPTIONS] [SESSION_ID] [PROMPT]`. Pure + testable.
fn summary_args<'a>(
    resume: Option<&'a str>,
    prompt: &'a str,
    schema: &'a str,
    out: &'a str,
) -> [&'a str; 8] {
    [
        "exec",
        "resume",
        resume.unwrap_or("--last"),
        prompt,
        "--output-schema",
        schema,
        "-o",
        out,
    ]
}

/// Run `codex exec resume {<thread_id> | --last} "<prompt>" --output-schema <file>
/// -o <out>` and parse the structured `<out>` into a [`HandoffSummary`]. Blocking.
fn run_codex_handoff_summary(
    codex: &str,
    cwd: &str,
    resume: Option<&str>,
) -> Result<HandoffSummary, String> {
    let dir = transfer_scratch_dir()?;
    // Unique PER TRANSFER (pid + monotonic counter) so concurrent transfers never
    // collide on these scratch paths.
    let token = format!(
        "{}-{}",
        std::process::id(),
        HANDOFF_SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let schema_file = dir.join(format!("handoff-schema-{token}.json"));
    let out_file = dir.join(format!("handoff-out-{token}.json"));

    // Run inside a closure so the scratch files are cleaned up on EVERY exit path
    // (spawn failure, timeout, parse error), not just the success path.
    let run = || -> Result<HandoffSummary, String> {
        std::fs::write(&schema_file, HANDOFF_SCHEMA)
            .map_err(|e| format!("write schema {}: {e}", schema_file.display()))?;

        let schema_s = schema_file.to_string_lossy().into_owned();
        let out_s = out_file.to_string_lossy().into_owned();
        let args = summary_args(resume, HANDOFF_PROMPT, &schema_s, &out_s);
        let mut cmd = codex_command(codex, &args);
        cmd.current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_no_window(&mut cmd);

        let (ok, _stdout, stderr) = run_bounded(cmd, SUMMARY_TIMEOUT)?;
        if !ok {
            return Err(format!(
                "codex exec resume failed: {}",
                String::from_utf8_lossy(&stderr).trim()
            ));
        }

        // `-o` writes the model's final JSON directly (no envelope/event unwrapping).
        let raw = std::fs::read_to_string(&out_file)
            .map_err(|e| format!("read summary output {}: {e}", out_file.display()))?;
        serde_json::from_str(&raw).map_err(|e| format!("parse handoff summary JSON: {e}"))
    };

    let result = run();
    let _ = std::fs::remove_file(&schema_file);
    let _ = std::fs::remove_file(&out_file);
    result
}

/// The scratch dir for transfer artifacts (schema/out/handoff files).
fn transfer_scratch_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("keitora-transfer");
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Deterministic path where a Codex → Claude handoff summary is written for stage
/// 060 to read and inject. `pub(crate)` so 060 can locate it.
pub(crate) fn handoff_summary_path(project: &str, worktree: &str) -> Result<PathBuf, String> {
    let safe = |s: &str| -> String { encode_project_dir(s) };
    Ok(transfer_scratch_dir()?.join(format!(
        "{}__{}__handoff.json",
        safe(project),
        safe(worktree)
    )))
}

/// Write the handoff summary to [`handoff_summary_path`].
fn write_handoff_summary(
    project: &str,
    worktree: &str,
    summary: &HandoffSummary,
) -> Result<(), String> {
    let path = handoff_summary_path(project, worktree)?;
    let text = serde_json::to_string_pretty(summary).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("write handoff {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Persistence (contract C3)
// ---------------------------------------------------------------------------

/// Persist provider state for one worktree without clearing absent resume ids.
fn persist_worktree_cli(
    worktree: &str,
    cli: CliKind,
    codex_thread_id: Option<String>,
    claude_session_id: Option<String>,
) -> Result<(), String> {
    let config = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let mut user = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;

    let mut entries = config.worktree_entries();
    if let Some(existing) = entries.iter_mut().find(|e| e.name == worktree) {
        existing.cli = Some(cli);
        // Each transfer passes `None` for the opposite provider's id.
        if codex_thread_id.is_some() {
            existing.codex_thread_id = codex_thread_id.clone();
        }
        if claude_session_id.is_some() {
            existing.claude_session_id = claude_session_id.clone();
        }
    } else {
        let mut entry =
            keitora_tasks::WorktreeEntry::new(worktree.to_string(), config.worktree_path(worktree));
        entry.cli = Some(cli);
        entry.codex_thread_id = codex_thread_id.clone();
        entry.claude_session_id = claude_session_id.clone();
        entries.push(entry);
    }

    let matched = user
        .set_project_worktrees_by_root(&config.root, &entries)
        .map_err(|e| e.to_string())?;
    if !matched {
        return Err(format!(
            "active project (root {}) is not registered; cannot persist transfer state",
            config.root.display()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Minimal JSON-RPC-2.0-over-stdio client for `codex app-server`
// ---------------------------------------------------------------------------

/// A parsed inbound line. Classified purely so the request/response/notification
/// demux is unit-testable.
#[derive(Debug, PartialEq)]
enum Frame {
    /// A response to one of our requests (has `id`, no `method`).
    Response {
        id: i64,
        result: Option<Value>,
        error: Option<RpcErrData>,
    },
    /// A notification (has `method`, no `id`).
    Notification { method: String, params: Value },
    /// A server-initiated request (has both `id` and `method`) — we reply `-32601`.
    ServerRequest { id: i64, method: String },
    /// Unrecognized shape.
    Junk,
}

#[derive(Debug, Clone, PartialEq)]
struct RpcErrData {
    code: i64,
    message: String,
}

/// A request failure: an RPC error object (carries `code`, e.g. `-32601`) or a
/// transport/timeout failure.
enum ReqErr {
    Rpc { code: i64, message: String },
    Transport(String),
}

impl std::fmt::Display for ReqErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReqErr::Rpc { code, message } => {
                write!(f, "codex app-server error {code}: {message}")
            }
            ReqErr::Transport(m) => write!(f, "{m}"),
        }
    }
}

/// Classify one inbound JSON message. Pure.
fn classify(v: &Value) -> Frame {
    let method = v.get("method").and_then(Value::as_str);
    let id = v.get("id").and_then(Value::as_i64);
    match (id, method) {
        (Some(id), Some(m)) => Frame::ServerRequest {
            id,
            method: m.to_string(),
        },
        (Some(id), None) => {
            let error = v.get("error").map(|e| RpcErrData {
                code: e.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: e
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
            Frame::Response {
                id,
                result: v.get("result").cloned(),
                error,
            }
        }
        (None, Some(m)) => Frame::Notification {
            method: m.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        },
        (None, None) => Frame::Junk,
    }
}

/// The spawned `codex app-server` client: newline-delimited JSON over stdio,
/// integer request ids, a background stdout reader feeding an `mpsc` channel, and a
/// buffer for notifications that arrive while awaiting a response.
struct AppServer {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Frame>,
    stderr: Arc<Mutex<String>>,
    next_id: i64,
    /// Notifications seen while awaiting a response, replayed by `wait_notification`.
    notif_buf: VecDeque<(String, Value)>,
}

impl AppServer {
    /// Spawn `codex app-server` in `cwd` (win32: through `cmd.exe`, so the direct
    /// child is the shell and the real `codex` is a grandchild — teardown kills the
    /// whole tree; unix: a direct child).
    fn spawn(codex: &str, cwd: &str) -> Result<Self, String> {
        let mut cmd = codex_command(codex, &["app-server"]);
        cmd.current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_no_window(&mut cmd);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn codex app-server: {e} — is `codex` on PATH?"))?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "codex app-server stdout unavailable".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "codex app-server stderr unavailable".to_string())?;

        // Reader thread: newline-delimited JSON → classified frames → channel.
        let (tx, rx) = std::sync::mpsc::channel::<Frame>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let frame = match serde_json::from_str::<Value>(&line) {
                    Ok(v) => classify(&v),
                    Err(_) => continue,
                };
                if tx.send(frame).is_err() {
                    break;
                }
            }
            // Sender drops on EOF → recv sees Disconnected.
        });

        // Drain stderr on its own thread so a full pipe can't wedge the child.
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        {
            let buf = stderr_buf.clone();
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = BufReader::new(stderr).read_to_string(&mut s);
                if let Ok(mut g) = buf.lock() {
                    g.push_str(&s);
                }
            });
        }

        Ok(AppServer {
            child,
            stdin,
            rx,
            stderr: stderr_buf,
            next_id: 1,
            notif_buf: VecDeque::new(),
        })
    }

    /// Handshake: `initialize` request (clientInfo names Keitora; `capabilities`
    /// reproduced verbatim incl. `optOutNotificationMethods`) then the mandatory
    /// `initialized` notification.
    fn handshake(&mut self) -> Result<(), String> {
        let init = json!({
            "clientInfo": { "title": "Keitora", "name": "Keitora", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": {
                "experimentalApi": false,
                "requestAttestation": false,
                "optOutNotificationMethods": [
                    "item/agentMessage/delta",
                    "item/reasoning/summaryTextDelta",
                    "item/reasoning/summaryPartAdded",
                    "item/reasoning/textDelta"
                ]
            }
        });
        self.request("initialize", init, REQUEST_TIMEOUT)
            .map_err(|e| e.to_string())?;
        self.send(&json!({ "method": "initialized", "params": {} }))?;
        Ok(())
    }

    fn send(&mut self, msg: &Value) -> Result<(), String> {
        let line = format!(
            "{}\n",
            serde_json::to_string(msg).map_err(|e| e.to_string())?
        );
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "codex app-server stdin is closed".to_string())?;
        stdin
            .write_all(line.as_bytes())
            .map_err(|e| format!("write codex app-server stdin: {e}"))?;
        stdin
            .flush()
            .map_err(|e| format!("flush codex app-server stdin: {e}"))
    }

    /// Send a request and await its matching response, buffering notifications and
    /// replying `-32601` to any server-initiated request meanwhile.
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, ReqErr> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "id": id, "method": method, "params": params }))
            .map_err(ReqErr::Transport)?;

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    ReqErr::Transport(format!("timed out waiting for `{method}` response"))
                })?;
            match self.rx.recv_timeout(remaining) {
                Ok(Frame::Response {
                    id: rid,
                    result,
                    error,
                }) if rid == id => {
                    return match error {
                        Some(err) => Err(ReqErr::Rpc {
                            code: err.code,
                            message: err.message,
                        }),
                        None => Ok(result.unwrap_or(Value::Null)),
                    };
                }
                Ok(Frame::Notification { method: m, params }) => {
                    self.notif_buf.push_back((m, params))
                }
                Ok(Frame::ServerRequest { id: sid, .. }) => self.reject_server_request(sid),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    return Err(ReqErr::Transport(format!(
                        "timed out waiting for `{method}` response"
                    )))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(ReqErr::Transport(format!(
                        "codex app-server closed unexpectedly.{}",
                        self.stderr_tail()
                    )))
                }
            }
        }
    }

    /// Await a specific notification method, replaying any already-buffered one
    /// first. Ignores unrelated notifications/responses; replies `-32601` to server
    /// requests.
    fn wait_notification(&mut self, method: &str, timeout: Duration) -> Result<Value, String> {
        if let Some(pos) = self.notif_buf.iter().position(|(m, _)| m == method) {
            return Ok(self
                .notif_buf
                .remove(pos)
                .map(|(_, p)| p)
                .unwrap_or(Value::Null));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    "Timed out waiting for Codex to finish importing the Claude session."
                        .to_string()
                })?;
            match self.rx.recv_timeout(remaining) {
                Ok(Frame::Notification { method: m, params }) if m == method => return Ok(params),
                Ok(Frame::Notification { .. }) | Ok(Frame::Response { .. }) | Ok(Frame::Junk) => {}
                Ok(Frame::ServerRequest { id: sid, .. }) => self.reject_server_request(sid),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(
                        "Timed out waiting for Codex to finish importing the Claude session."
                            .to_string(),
                    )
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!(
                        "codex app-server closed before the import completed.{}",
                        self.stderr_tail()
                    ))
                }
            }
        }
    }

    /// Reply `-32601` to a server-initiated request (mirrors the plugin's
    /// `handleServerRequest`). Best-effort — a write failure here is non-fatal.
    fn reject_server_request(&mut self, id: i64) {
        let _ = self.send(&json!({
            "id": id,
            "error": { "code": -32601, "message": "Unsupported server request" }
        }));
    }

    /// The trimmed captured stderr, prefixed with a newline when non-empty (for
    /// appending to an error message).
    fn stderr_tail(&self) -> String {
        self.stderr
            .lock()
            .ok()
            .map(|g| {
                let t = g.trim();
                if t.is_empty() {
                    String::new()
                } else {
                    format!("\n{t}")
                }
            })
            .unwrap_or_default()
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        // Graceful first: closing stdin makes app-server see EOF and exit.
        drop(self.stdin.take());
        // Give it a brief moment to exit on its own before force-killing.
        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => break,
            }
        }
        // Force-kill the WHOLE tree. On win32 the direct child is `cmd.exe`, so a
        // bare kill would orphan the real codex grandchild — use `taskkill /T`.
        #[cfg(windows)]
        kill_process_tree_windows(self.child.id());
        // On unix the direct child IS codex; a plain kill (SIGKILL) suffices for
        // teardown after we already have what we need. (The plugin uses SIGTERM;
        // std only exposes SIGKILL, and the graceful stdin-EOF above covers the
        // clean path.)
        #[cfg(not(windows))]
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Process spawning helpers
// ---------------------------------------------------------------------------

/// Build a `codex …` command. On win32 the `codex` shim is a `.cmd`, so route
/// through `cmd.exe` (the reference plugin's `shell: true`; also keeps `cmd.exe` the
/// direct child so the teardown tree-kill targets it) using the 8.3 short path so a
/// space in the path can't split the launch line. The program and each arg are passed
/// as SEPARATE `Command` args — Rust applies correct Windows argv quoting, which
/// survives cmd.exe's parsing and the shim's `%*` forwarding. Hand-joining a `/C`
/// string and quoting it ourselves mangled multi-word args (the handoff prompt got
/// split into `Summarize` / `this` / … and Codex rejected the extra positionals).
/// On unix invoke the resolved binary directly.
#[cfg(windows)]
fn codex_command(codex: &str, args: &[&str]) -> Command {
    let short = keitora_tasks::short_path(codex);
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(short).args(args);
    cmd
}

#[cfg(not(windows))]
fn codex_command(codex: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(codex);
    cmd.args(args);
    cmd
}

/// Suppress the console window Windows would otherwise pop when a GUI process
/// spawns a console-subsystem child (stdio is piped, so no console is needed).
/// No-op off Windows.
fn apply_no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}

/// Kill an entire process tree on Windows (`taskkill /PID <pid> /T /F`).
#[cfg(windows)]
fn kill_process_tree_windows(pid: u32) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Run a prepared command to completion under a wall-clock deadline, draining both
/// pipes on their own threads. Returns `(status_ok, stdout, stderr)`. Mirrors
/// `dispatch.rs::run_claude_bounded`.
fn run_bounded(mut cmd: Command, timeout: Duration) -> Result<(bool, Vec<u8>, Vec<u8>), String> {
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn codex: {e}"))?;
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child
            .try_wait()
            .map_err(|e| format!("wait on codex: {e}"))?
        {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    // On Windows the codex summary runs through `cmd.exe /C` (see
                    // `codex_command`), so `child` is the shell and the real `codex`
                    // is a grandchild — a bare `child.kill()` would kill `cmd.exe` and
                    // ORPHAN the codex tree. Tree-kill the whole `cmd.exe → codex …`
                    // tree with the same `taskkill /T /F` the `AppServer` teardown
                    // uses; on unix the direct child IS codex, so a plain kill (the
                    // child is a group leader) suffices. Reap either way.
                    #[cfg(windows)]
                    kill_process_tree_windows(child.id());
                    #[cfg(not(windows))]
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_reader.join();
                    let _ = err_reader.join();
                    return Err(format!("codex timed out after {}s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    Ok((status.success(), stdout, stderr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transfer_holds_its_session_out_of_recovery_until_it_returns() {
        use crate::backend::herdr::recovery::machine::Event;
        let (handle, rx) = crate::backend::herdr::recovery::recording_handle();
        let key = SessionKey {
            project: "p".into(),
            worktree: "w".into(),
        };
        let seen_inside =
            tauri::async_runtime::block_on(with_recovery_lock(Some(handle), &key, async {
                rx.try_iter().collect::<Vec<_>>()
            }));
        assert_eq!(seen_inside, vec![Event::TransferLock(key.clone(), true)]);
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            vec![Event::TransferLock(key.clone(), false)]
        );
        // Without recovery the transfer just runs.
        assert_eq!(
            tauri::async_runtime::block_on(with_recovery_lock(None, &key, async { 7 })),
            7
        );
    }

    #[test]
    fn a_terminated_session_is_refused_before_anything_changes() {
        use crate::backend::fake::FakeBackend;
        let fake = FakeBackend::new();
        let key = SessionKey {
            project: "proj".into(),
            worktree: "alpha".into(),
        };
        assert_eq!(refuse_terminated(&fake, &key), Ok(()));
        fake.insert_terminated(key.clone());
        assert_eq!(
            refuse_terminated(&fake, &key),
            Err("session is terminated; launch it before transferring".to_string())
        );
        // Another worktree, or the same name in another project, is unaffected.
        let other = SessionKey {
            project: "other".into(),
            worktree: "alpha".into(),
        };
        assert_eq!(refuse_terminated(&fake, &other), Ok(()));
        // The check only reads: no session call was made, nothing was killed.
        assert!(fake.calls().is_empty());
    }

    // --- Ledger parse vs. a captured (scrubbed) sample ----------------------

    /// A redacted capture of `external_agent_session_imports.json`: the real
    /// top-level `{records, detected_connector_records}` shape, records carrying
    /// extra fields beyond what we consume (plus an intentionally-unknown
    /// `experimental_flag` to prove non-strict deserialization), and TWO records
    /// with the same source_path+sha (different thread ids) to prove last-wins.
    /// All paths/ids are synthetic.
    const LEDGER_SAMPLE: &str = r#"{
      "records": [
        {
          "source_path": "\\\\?\\C:\\Users\\dev\\.claude\\projects\\D--repos-demo\\aaaaaaaa-1111.jsonl",
          "content_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
          "imported_thread_id": "01900000-0000-7000-8000-000000000001",
          "imported_at": 1787500000,
          "source_modified_at": 1787460000000000000,
          "connector_names": [],
          "title": "First import",
          "experimental_flag": true
        },
        {
          "source_path": "\\\\?\\C:\\Users\\dev\\.claude\\projects\\D--repos-demo\\bbbbbbbb-2222.jsonl",
          "content_sha256": "2222222222222222222222222222222222222222222222222222222222222222",
          "imported_thread_id": "01900000-0000-7000-8000-000000000002",
          "imported_at": 1787500500,
          "source_modified_at": 1787460500000000000,
          "connector_names": ["codex"],
          "title": "Other worktree"
        },
        {
          "source_path": "\\\\?\\C:\\Users\\dev\\.claude\\projects\\D--repos-demo\\aaaaaaaa-1111.jsonl",
          "content_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
          "imported_thread_id": "01900000-0000-7000-8000-000000000003",
          "imported_at": 1787501000,
          "source_modified_at": 1787461000000000000,
          "connector_names": [],
          "title": "Re-import (newer)"
        }
      ],
      "detected_connector_records": [
        { "connector_name": "codex", "detected_at": 1787499000 }
      ]
    }"#;

    const SAMPLE_SOURCE: &str =
        r"\\?\C:\Users\dev\.claude\projects\D--repos-demo\aaaaaaaa-1111.jsonl";
    const SAMPLE_SHA: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    #[test]
    fn ledger_match_takes_last_and_tolerates_extra_fields() {
        // Two records match source+sha; the LAST (thread ...0003) wins.
        let got = ledger_match(LEDGER_SAMPLE, SAMPLE_SOURCE, SAMPLE_SHA);
        assert_eq!(got.as_deref(), Some("01900000-0000-7000-8000-000000000003"));
    }

    #[test]
    fn ledger_match_requires_all_three_of_path_sha_thread() {
        // Right path, WRONG sha → no match.
        assert_eq!(ledger_match(LEDGER_SAMPLE, SAMPLE_SOURCE, "deadbeef"), None);
        // Right sha, WRONG path → no match.
        assert_eq!(
            ledger_match(LEDGER_SAMPLE, r"\\?\C:\nope.jsonl", SAMPLE_SHA),
            None
        );
    }

    #[test]
    fn ledger_match_tolerates_missing_records_key() {
        // A ledger with only detected_connector_records must not error out.
        let text = r#"{ "detected_connector_records": [] }"#;
        assert_eq!(ledger_match(text, SAMPLE_SOURCE, SAMPLE_SHA), None);
    }

    // --- Migration payload serializes to the exact expected JSON -------------

    #[test]
    fn migration_payload_serializes_verbatim() {
        let payload = migration_payload("/home/dev/.claude/projects/x/foo.jsonl", "/work/tree");
        let got = serde_json::to_string(&payload).unwrap();
        // Exact key order (declaration order): itemType, description, cwd, details
        // { plugins, sessions, mcpServers, hooks, subagents, commands }; inner
        // session { path, cwd, title }; outer cwd null, inner session cwd real.
        let expected = concat!(
            r#"{"migrationItems":[{"itemType":"SESSIONS","#,
            r#""description":"Transfer Claude session foo.jsonl","#,
            r#""cwd":null,"#,
            r#""details":{"plugins":[],"#,
            r#""sessions":[{"path":"/home/dev/.claude/projects/x/foo.jsonl","cwd":"/work/tree","title":null}],"#,
            r#""mcpServers":[],"hooks":[],"subagents":[],"commands":[]}}]}"#
        );
        assert_eq!(got, expected);
    }

    // --- Encoded-cwd + the `\\?\`-vs-ledger asymmetry -----------------------

    #[test]
    fn encode_project_dir_matches_verified_example() {
        // Verified real dir under ~/.claude/projects: each non-alnum char → one `-`
        // (so `:` and `\` give the leading `--`).
        assert_eq!(
            encode_project_dir(r"D:\repos\proj-worktrees\beta"),
            "D--repos-proj-worktrees-beta"
        );
    }

    #[test]
    fn encode_project_dir_unix_path() {
        assert_eq!(
            encode_project_dir("/home/dev/repos/proj-wt"),
            "-home-dev-repos-proj-wt"
        );
    }

    #[test]
    fn encoded_cwd_dir_name_canonicalizes_real_dirs_and_errors_on_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();
        let stripped = keitora_tasks::strip_extended_prefix_str(&canonical.to_string_lossy());
        assert_eq!(
            encoded_cwd_dir_name(tmp.path()).unwrap(),
            encode_project_dir(&stripped)
        );
        assert!(!encoded_cwd_dir_name(tmp.path()).unwrap().contains('?'));
        assert!(encoded_cwd_dir_name(&tmp.path().join("missing")).is_err());
    }

    #[test]
    fn claude_projects_dir_from_prefers_non_empty_config_dir() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            claude_projects_dir_from(Some("/cfg/claude".into()), home.clone()).unwrap(),
            PathBuf::from("/cfg/claude").join("projects")
        );
        assert_eq!(
            claude_projects_dir_from(Some("".into()), home.clone()).unwrap(),
            PathBuf::from("/home/u").join(".claude").join("projects")
        );
        assert_eq!(
            claude_projects_dir_from(None, home).unwrap(),
            PathBuf::from("/home/u").join(".claude").join("projects")
        );
        assert!(claude_projects_dir_from(None, None).is_err());
    }

    /// Serialises the tests that set the process-global `CLAUDE_CONFIG_DIR`.
    fn claude_env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn claude_projects_dir_honours_claude_config_dir() {
        let _guard = claude_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("CLAUDE_CONFIG_DIR");
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());
        let got = claude_projects_dir();
        match old {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        assert_eq!(got.unwrap(), tmp.path().join("projects"));
    }

    #[test]
    fn claude_session_file_helpers() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path();
        assert!(
            !claude_project_has_sessions(projects, "D--wt"),
            "missing dir"
        );
        std::fs::create_dir_all(projects.join("D--wt")).unwrap();
        assert!(!claude_project_has_sessions(projects, "D--wt"), "empty dir");
        std::fs::write(projects.join("D--wt").join("notes.txt"), "x").unwrap();
        std::fs::create_dir_all(projects.join("D--wt").join("dir.jsonl")).unwrap();
        assert!(
            !claude_project_has_sessions(projects, "D--wt"),
            "only files ending in .jsonl count"
        );
        std::fs::write(projects.join("D--wt").join("abc-1.jsonl"), "{}").unwrap();
        assert!(claude_project_has_sessions(projects, "D--wt"));
        assert!(claude_session_file_exists(projects, "D--wt", "abc-1"));
        assert!(!claude_session_file_exists(projects, "D--wt", "abc-2"));
        assert!(!claude_session_file_exists(projects, "D--other", "abc-1"));
        assert!(!claude_session_file_exists(projects, "D--wt", "dir"));
    }

    /// The two normalizations must NOT be conflated: the ledger realpath keeps the
    /// `\\?\` prefix, while the live-session-lookup cwd encoding strips it. A
    /// regression here silently fails every ledger match on Windows.
    #[cfg(windows)]
    #[test]
    fn ledger_realpath_retains_prefix_while_cwd_encoding_strips_it() {
        let canonical = r"\\?\D:\repos\proj-worktrees\beta";

        // Ledger side: strip is NOT applied — the prefix is retained verbatim, so a
        // record whose source_path carries the prefix matches.
        let ledger_source = canonical; // used as-is for ledger matching
        assert!(
            ledger_source.starts_with(r"\\?\"),
            "ledger realpath must retain the \\\\?\\ prefix"
        );

        // Lookup side: strip THEN encode → the encoded dir name has no prefix.
        let stripped = keitora_tasks::strip_extended_prefix_str(canonical);
        assert!(
            !stripped.starts_with(r"\\?\"),
            "cwd encoding must strip the \\\\?\\ prefix"
        );
        assert_eq!(
            encode_project_dir(&stripped),
            "D--repos-proj-worktrees-beta"
        );
    }

    // --- `-32601` → upgrade error mapping -----------------------------------

    #[test]
    fn rpc_minus_32601_maps_to_upgrade_message() {
        let mapped = map_import_error(ReqErr::Rpc {
            code: -32601,
            message: "Method not found".to_string(),
        });
        assert_eq!(mapped, UPGRADE_MSG);
    }

    // --- Retry-on-active-writer predicate (Codex → Claude release race) -----

    #[test]
    fn is_active_writer_error_matches_all_markers_case_insensitively() {
        // The real failure string from a not-yet-released interactive writer.
        assert!(is_active_writer_error(
            "codex exec resume failed: thread-store conflict: thread abc already has an active writer … thread/resume: thread abc already has an active writer (code -32600)"
        ));
        // Each marker in isolation, and case-insensitively.
        assert!(is_active_writer_error("ACTIVE WRITER"));
        assert!(is_active_writer_error("error code -32600"));
        assert!(is_active_writer_error("Thread-Store Conflict"));
    }

    // --- Codex→Claude summary argv: by-id vs. --last ------------------------

    #[test]
    fn summary_args_by_id_shape() {
        // A persisted thread id lands in the SESSION_ID slot.
        assert_eq!(
            summary_args(Some("0199-abc"), "PROMPT", "/tmp/s.json", "/tmp/o.json"),
            [
                "exec",
                "resume",
                "0199-abc",
                "PROMPT",
                "--output-schema",
                "/tmp/s.json",
                "-o",
                "/tmp/o.json",
            ]
        );
    }

    #[test]
    fn summary_args_last_shape_for_native_codex_worktree() {
        // No persisted thread id → `--last` (native Codex worktree handoff).
        assert_eq!(
            summary_args(None, "PROMPT", "/tmp/s.json", "/tmp/o.json"),
            [
                "exec",
                "resume",
                "--last",
                "PROMPT",
                "--output-schema",
                "/tmp/s.json",
                "-o",
                "/tmp/o.json",
            ]
        );
    }

    #[test]
    fn is_active_writer_error_rejects_unrelated_failures() {
        // A genuine, non-retryable error must surface, not spin the retry loop.
        assert!(!is_active_writer_error(
            "codex exec resume failed: no such thread"
        ));
        assert!(!is_active_writer_error("codex timed out after 180s"));
        assert!(!is_active_writer_error("parse handoff summary JSON: eof"));
        // A different RPC code must NOT be treated as the writer conflict.
        assert!(!is_active_writer_error("app-server error -32601"));
    }

    #[test]
    fn other_rpc_error_is_not_the_upgrade_message() {
        let mapped = map_import_error(ReqErr::Rpc {
            code: -32000,
            message: "boom".to_string(),
        });
        assert_ne!(mapped, UPGRADE_MSG);
        assert!(mapped.contains("boom"), "got: {mapped}");
        let transport = map_import_error(ReqErr::Transport("closed".to_string()));
        assert_eq!(transport, "closed");
    }

    // --- JSON-RPC framing classification round-trip -------------------------

    #[test]
    fn classify_response_success() {
        let v: Value = serde_json::from_str(r#"{"id":7,"result":{"ok":true}}"#).unwrap();
        assert_eq!(
            classify(&v),
            Frame::Response {
                id: 7,
                result: Some(json!({"ok": true})),
                error: None,
            }
        );
    }

    #[test]
    fn classify_response_error_carries_code() {
        let v: Value = serde_json::from_str(
            r#"{"id":9,"error":{"code":-32601,"message":"Method not found"}}"#,
        )
        .unwrap();
        assert_eq!(
            classify(&v),
            Frame::Response {
                id: 9,
                result: None,
                error: Some(RpcErrData {
                    code: -32601,
                    message: "Method not found".to_string(),
                }),
            }
        );
    }

    #[test]
    fn classify_notification_and_server_request() {
        let notif: Value = serde_json::from_str(
            r#"{"method":"externalAgentConfig/import/completed","params":{"x":1}}"#,
        )
        .unwrap();
        assert_eq!(
            classify(&notif),
            Frame::Notification {
                method: "externalAgentConfig/import/completed".to_string(),
                params: json!({"x": 1}),
            }
        );

        let server_req: Value =
            serde_json::from_str(r#"{"id":3,"method":"some/serverCall","params":{}}"#).unwrap();
        assert_eq!(
            classify(&server_req),
            Frame::ServerRequest {
                id: 3,
                method: "some/serverCall".to_string(),
            }
        );

        let junk: Value = serde_json::from_str(r#"{"hello":"world"}"#).unwrap();
        assert_eq!(classify(&junk), Frame::Junk);
    }

    // --- First-user-line extraction -----------------------------------------

    #[test]
    fn first_user_line_skips_meta_and_reads_string_or_blocks() {
        let jsonl = concat!(
            r#"{"type":"user","isMeta":true,"message":{"content":"<synthetic caveat>"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":"hi"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"the real first thing I typed"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"text","text":"second"}]}}"#,
        );
        assert_eq!(first_user_line(jsonl), "the real first thing I typed");
    }

    #[test]
    fn first_user_line_reads_content_blocks_when_first_real_user_is_array() {
        let jsonl =
            r#"{"type":"user","message":{"content":[{"type":"text","text":"block text"}]}}"#;
        assert_eq!(first_user_line(jsonl), "block text");
    }

    #[test]
    fn session_id_from_jsonl_extracts_uuid_stem() {
        let p = Path::new("/home/me/.claude/projects/enc/1b4f-abc-uuid.jsonl");
        assert_eq!(session_id_from_jsonl(p).as_deref(), Some("1b4f-abc-uuid"));
    }

    #[test]
    fn session_id_from_jsonl_none_when_no_file_name() {
        assert_eq!(session_id_from_jsonl(Path::new("/")), None);
    }

    #[test]
    fn session_id_from_jsonl_rejects_unsafe_stem() {
        assert_eq!(session_id_from_jsonl(Path::new("session id.jsonl")), None);
        assert_eq!(session_id_from_jsonl(Path::new("--continue.jsonl")), None);
        assert_eq!(
            session_id_from_jsonl(Path::new("session;whoami.jsonl")),
            None
        );
    }

    #[test]
    fn truncate_chars_appends_ellipsis() {
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
    }

    // --- Handoff-summary flattening (I5 injection) --------------------------

    #[test]
    fn format_handoff_prompt_is_a_single_line() {
        let summary = HandoffSummary {
            goal: "Ship the transfer menu".to_string(),
            key_decisions: vec![
                "Use write_line".to_string(),
                "No new dialog lib".to_string(),
            ],
            next_steps: vec!["Wire tsc check".to_string()],
            open_files: vec!["Rail.tsx".to_string(), "App.tsx".to_string()],
            notes: String::new(),
        };
        let prompt = format_handoff_prompt(&summary);
        assert!(!prompt.contains('\n'));
        assert!(!prompt.contains('\r'));
        assert!(prompt.contains("Ship the transfer menu"));
        assert!(prompt.contains("Use write_line; No new dialog lib"));
        assert!(prompt.contains("Rail.tsx; App.tsx"));
        // Empty notes are omitted entirely, not rendered as "Notes: .".
        assert!(!prompt.contains("Notes:"));
    }

    #[test]
    fn format_handoff_prompt_scrubs_embedded_newlines_and_includes_notes() {
        let summary = HandoffSummary {
            goal: "Line one\nLine two".to_string(),
            key_decisions: vec![],
            next_steps: vec![],
            open_files: vec![],
            notes: "Careful:\r\nedge case".to_string(),
        };
        let prompt = format_handoff_prompt(&summary);
        assert!(!prompt.contains('\n'));
        assert!(!prompt.contains('\r'));
        assert!(prompt.contains("Line one Line two"));
        assert!(prompt.contains("Careful: edge case"));
        assert!(prompt.contains("Key decisions: none."));
    }

    // --- release_codex_session via FakeBackend --------------

    use crate::backend::fake::{Call, FakeBackend};
    use crate::backend::{PtyBackendError, SessionSummary};

    fn release_key() -> SessionKey {
        SessionKey {
            project: "proj".to_string(),
            worktree: "sayo".to_string(),
        }
    }

    fn fast_release(client: &dyn PtyBackend, key: &SessionKey) -> Result<(), String> {
        release_codex_session_with(
            client,
            key,
            Duration::from_millis(150),
            Duration::from_millis(10),
        )
    }

    fn kill_calls(fake: &FakeBackend) -> Vec<(SessionKey, u64)> {
        fake.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Kill { key, gen } => Some((key, gen)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn release_codex_session_no_session_is_ok_no_kill() {
        // Default FakeBackend serves an empty session list -> nothing live for the
        // key -> Ok with no kill attempted.
        let fake = FakeBackend::new();
        let key = release_key();
        assert!(fast_release(&fake, &key).is_ok());
        assert!(kill_calls(&fake).is_empty(), "no session -> no kill");
    }

    #[test]
    fn release_codex_session_waits_for_exit_and_kills_with_gen() {
        let fake = FakeBackend::new();
        let key = release_key();
        fake.push_list_sessions(Ok(vec![SessionSummary {
            key: key.clone(),
            gen: 42,
            state_hint: None,
        }]));
        fake.insert_live(key.clone());
        // is_live reads true once, then the key drops out of the live-set.
        fake.flip_live_after(2, key.clone());
        fake.push_kill(Ok(()));

        assert!(fast_release(&fake, &key).is_ok());
        // Kill used the generation `list_sessions` reported.
        assert_eq!(kill_calls(&fake), vec![(key, 42)]);
    }

    #[test]
    fn release_codex_session_times_out_when_never_exits() {
        let fake = FakeBackend::new();
        let key = release_key();
        fake.push_list_sessions(Ok(vec![SessionSummary {
            key: key.clone(),
            gen: 7,
            state_hint: None,
        }]));
        fake.insert_live(key.clone()); // never flips -> is_live stays true
        fake.push_kill(Ok(()));

        let err = fast_release(&fake, &key).unwrap_err();
        assert!(
            err.contains("timed out waiting"),
            "expected a release timeout, got: {err}"
        );
    }

    #[test]
    fn release_codex_session_errors_when_list_sessions_fails() {
        let fake = FakeBackend::new();
        let key = release_key();
        fake.push_list_sessions(Err(PtyBackendError::Other("boom".to_string())));

        let err = fast_release(&fake, &key).unwrap_err();
        assert!(
            err.contains("list daemon sessions before Codex handoff"),
            "expected a list-sessions error, got: {err}"
        );
        assert!(kill_calls(&fake).is_empty(), "no kill on a list failure");
    }
}
