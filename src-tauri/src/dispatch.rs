//! The advisory dispatcher: a headless `claude` proposes which open task each
//! *idle* worktree should pick up (`dispatch_suggest`), and a state-aware apply
//! reserves the task always but injects only into an idle worktree with a live
//! PTY (`dispatch_apply`). The human stays in the loop — Keitora only performs the
//! mechanical injection, and only into idle sessions.
//!
//! The IPC-facing commands are thin; the testable core is three pure functions:
//! `decide` (dispatch-vs-queue), `build_injection` (the one-line prompt), and
//! `parse_proposals` (the `claude` envelope → validated, filtered proposals).

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::State;

use keitora_tasks::dispatch::{interpolate, DispatchContext};
use keitora_tasks::{CliKind, DispatchConfig, ReserveOutcome};

use crate::backend::{
    background_set, map_write_line_result, PtyBackend, PtyBackendError, SessionKey, SharedBackend,
    WriteLineOutcome,
};

/// Serializes `dispatch_apply` so its idempotency check, reserve, PTY inject, and
/// `dispatched` log write run as one critical section. Without it, two concurrent
/// calls for the same task could both read the log before either writes its
/// `dispatched` row, both pass the guard, and both inject (the double-inject this
/// module exists to prevent). Dispatch is human-driven and rare, so a single
/// global lock held across the (bounded) inject is cheap.
///
/// Uses `tokio::sync::Mutex` because the gate is held across `.await` points after
/// migrating `dispatch_apply` to `async` (a `std::sync::Mutex` guard is `!Send`
/// across await boundaries).
#[derive(Default)]
pub struct DispatchGate(pub(crate) tokio::sync::Mutex<()>);

/// A ranked dispatch proposal from the headless `claude` call: pick up `task_id`
/// in `worktree`. Crosses the IPC boundary (Serialize) and is deserialized out of
/// the `claude` `structured_output` payload (Deserialize).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub task_id: i64,
    pub worktree: String,
    pub rationale: String,
    pub confidence: f64,
}

/// The result of `dispatch_apply`: whether the task was actually injected
/// (`dispatched`) or merely reserved (`queued`), plus a human-readable `reason`
/// when it was deferred.
#[derive(Debug, Clone, Serialize)]
pub struct DispatchOutcome {
    pub dispatched: bool,
    pub reason: Option<String>,
}

/// The dispatch-vs-defer decision (internal; never crosses IPC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Dispatched,
    Queued,
}

/// The JSON schema that forces `claude` to return a validated `{ proposals: [...] }`
/// object in its `structured_output` envelope field.
const SCHEMA: &str = r#"{"type":"object","properties":{"proposals":{"type":"array","items":{"type":"object","properties":{"task_id":{"type":"integer"},"worktree":{"type":"string"},"rationale":{"type":"string"},"confidence":{"type":"number"}},"required":["task_id","worktree","rationale","confidence"]}}},"required":["proposals"]}"#;

/// Bound the headless call so a hung `claude` fails gracefully (ms). Passed to
/// `claude` as defense-in-depth; the hard wall-clock kill below is authoritative.
const API_TIMEOUT_MS: &str = "60000";

/// Hard wall-clock deadline for the headless `claude` call. `Command::output()`
/// has no timeout, so a wedged child (stuck before it can honour `API_TIMEOUT_MS`)
/// would hang the dispatcher forever; we kill it past this.
const SUGGEST_TIMEOUT: Duration = Duration::from_secs(90);

/// Pure: dispatch (inject now) iff the worktree is idle AND we hold a live PTY for
/// it; otherwise queue (reserve only). Encodes the safety rule "never write into a
/// busy PTY" — the only state we ever inject into is a live, idle session.
fn decide(state: &str, has_live_pty: bool) -> Decision {
    if state == "idle" && has_live_pty {
        Decision::Dispatched
    } else {
        Decision::Queued
    }
}

/// Pure: map the AUTHORITATIVE final `write_line` outcome (D6) of a dispatch's
/// injection sequence into the `(audit_decision, DispatchOutcome)` pair. Kept
/// separate so the never-Err-after-reserve mapping is unit-testable. `sent_any`
/// records whether at least one earlier step's text+submit reached the pane.
///
/// NB: this NEVER yields an `Err`; the task is already reserved, and a retry would
/// re-inject. A partial inject reports `dispatched` (the agent has input and may be
/// acting); a clean miss reports `queued` (safe to retry).
fn injection_outcome(
    final_outcome: WriteLineOutcome,
    sent_any: bool,
    worktree: &str,
    detail: &str,
) -> (&'static str, DispatchOutcome) {
    match final_outcome {
        WriteLineOutcome::Dispatched => (
            "dispatched",
            DispatchOutcome {
                dispatched: true,
                reason: None,
            },
        ),
        // Partial: earlier lines already reached the pane (the agent may be acting).
        // Report dispatched so the caller doesn't re-dispatch; record for diagnostics.
        WriteLineOutcome::Queued | WriteLineOutcome::Error if sent_any => (
            "dispatched",
            DispatchOutcome {
                dispatched: true,
                reason: Some(format!("{worktree}: partially injected ({detail})")),
            },
        ),
        // Nothing reached the pane → safe to leave reserved/queued for retry.
        WriteLineOutcome::Queued | WriteLineOutcome::Error => (
            "queued",
            DispatchOutcome {
                dispatched: false,
                reason: Some(format!(
                    "{worktree}: reserved — could not inject ({detail})"
                )),
            },
        ),
    }
}

/// Why a dispatch to `worktree` was only reserved: its session is busy, or the
/// user terminated it.
fn queued_reason(worktree: &str, terminated: bool) -> String {
    if terminated {
        format!("{worktree} is terminated — reserved")
    } else {
        format!("{worktree} busy — reserved")
    }
}

/// The detail recorded for an injection's final `write_line` result: empty on
/// success; for a missing session of a terminated worktree, how to recover.
fn injection_detail(result: &Result<(), PtyBackendError>, terminated: bool) -> String {
    match result {
        Ok(()) => String::new(),
        Err(PtyBackendError::NoSuchSession) if terminated => {
            "session is terminated; launch it to dispatch".to_string()
        }
        Err(e) => e.to_string(),
    }
}

/// Inject each ritual `step` as its own submitted line via the backend's atomic
/// `write_line` (text, pause `submit_delay_ms`, then `\r`). Returns the AUTHORITATIVE
/// final `write_line` result (D6) and whether at least one step's text+submit reached
/// the pane (`sent_any`). Stops at the first `Queued`/`Error` step — nothing more
/// should land. Extracted from `dispatch_apply` so the loop is testable against a
/// `FakeBackend` without a live daemon.
fn inject_steps(
    backend: &dyn PtyBackend,
    key: &SessionKey,
    steps: &[String],
    submit_delay_ms: u32,
    inter_step: Duration,
) -> (Result<(), PtyBackendError>, bool) {
    let mut result: Result<(), PtyBackendError> = Ok(());
    let mut sent_any = false;
    for (i, line) in steps.iter().enumerate() {
        result = backend.write_line(key, line, submit_delay_ms);
        match map_write_line_result(&result) {
            WriteLineOutcome::Dispatched => {
                sent_any = true; // text + submit reached the pane
            }
            // Queued / Error: stop — nothing more should land.
            WriteLineOutcome::Queued | WriteLineOutcome::Error => break,
        }
        if i + 1 < steps.len() {
            std::thread::sleep(inter_step);
        }
    }
    (result, sent_any)
}

/// Pause between a step's text and its submit byte, passed to the daemon's atomic
/// `WriteLine` RPC (which writes the text, sleeps this long, then writes the `\r`).
/// The pause matters — a contiguous `text\r` burst reads to the TUI as a paste
/// (embedded newline, no submit), so the Enter must arrive as a distinct, later
/// keystroke. Empirically tunable; raise it if the TUI still fails to submit.
const SUBMIT_DELAY: Duration = Duration::from_millis(60);

/// Delay between dispatch steps so the target TUI finishes processing one submitted
/// line before the next arrives. Empirically tunable.
const INTER_STEP_DELAY: Duration = Duration::from_millis(80);

/// Pure: map every control char (incl. ESC, BEL, newline, tab) to a space, THEN
/// collapse whitespace runs — so an interpolated title/description can't submit the
/// line early or smuggle an escape sequence. `split_whitespace` alone leaves
/// non-whitespace C0 controls like ESC intact, hence the explicit map first.
fn collapse(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pure: Keitora's built-in dispatch prompt, used when no custom `[dispatch].steps`
/// are configured. The submit byte is appended separately at injection time.
fn default_line(ctx: &DispatchContext) -> String {
    let id = ctx.task.id;
    format!(
        "Please pick up Keitora task #{id}. Run `./keitora task claim {id}` to claim it, then implement it: {title}. {desc}. When finished, run `./keitora task done {id}`.",
        title = ctx.task.title,
        desc = ctx.task.description,
    )
}

/// Pure: the ordered lines to inject for a dispatch, given the already-selected
/// per-CLI step list (see [`select_ritual_steps`]). This function itself carries
/// NO CLI-awareness — it just renders whichever step list its caller selected;
/// the per-CLI branch lives entirely in `select_ritual_steps`. Each step is
/// interpolated against `ctx`, then control-char/whitespace collapsed, and lines
/// that render empty are dropped (no blank submits). If nothing remains — no
/// steps configured, or every configured step rendered blank — falls back to the
/// built-in default line, so a dispatch ALWAYS injects at least one line (a
/// misconfigured all-blank list can't silently produce a no-op that still
/// reports success). The submit byte is appended per line at injection time, NOT
/// here.
fn render_steps(steps: &[String], ctx: &DispatchContext) -> Vec<String> {
    let rendered: Vec<String> = steps
        .iter()
        .map(|s| collapse(&interpolate(s, ctx)))
        .filter(|l| !l.is_empty())
        .collect();
    if rendered.is_empty() {
        vec![collapse(&default_line(ctx))]
    } else {
        rendered
    }
}

/// Pure: which `DispatchConfig` step list `render_steps` should render, chosen by
/// the TARGET WORKTREE's resolved [`CliKind`] — a Claude
/// worktree gets `claude_steps`, a Codex worktree gets `codex_steps`. This is the
/// ONE place the per-CLI branch lives; `render_steps` stays CLI-agnostic, and the
/// advisory *suggest* pass (`run_claude_bounded`) never consults this at all —
/// it stays fixed on Claude/haiku regardless of any worktree's resolved CLI.
fn select_ritual_steps(cfg: &DispatchConfig, cli: CliKind) -> &[String] {
    match cli {
        CliKind::Claude => &cfg.claude_steps,
        CliKind::Codex => &cfg.codex_steps,
    }
}

/// Pure: parse the `claude --output-format json` envelope into validated proposals.
/// Errors if the envelope reports failure or is malformed; drops any proposal whose
/// `task_id` isn't a currently open/unblocked task or whose `worktree` isn't idle.
fn parse_proposals(
    stdout: &[u8],
    valid_open_ids: &[i64],
    idle_worktrees: &[String],
) -> Result<Vec<Proposal>, String> {
    let env: Value = serde_json::from_slice(stdout)
        .map_err(|e| format!("could not parse claude output as JSON: {e}"))?;

    // The envelope must report a successful result, else surface the error text.
    let is_error = env
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let subtype = env.get("subtype").and_then(Value::as_str).unwrap_or("");
    if is_error || subtype != "success" {
        let detail = env
            .get("result")
            .and_then(Value::as_str)
            .or_else(|| env.get("error").and_then(Value::as_str))
            .unwrap_or("no detail");
        return Err(format!(
            "claude returned an error (subtype={subtype}, is_error={is_error}): {detail}"
        ));
    }

    // The validated payload lives in `structured_output.proposals`.
    let items = env
        .get("structured_output")
        .and_then(|s| s.get("proposals"))
        .and_then(Value::as_array)
        .ok_or_else(|| "claude envelope missing structured_output.proposals array".to_string())?;

    // Deserialize the whole array strictly: a malformed entry (e.g. a string
    // task_id) is a schema violation, not "no matches" — surface it loudly rather
    // than silently dropping it.
    let parsed: Vec<Proposal> = serde_json::from_value(Value::Array(items.clone()))
        .map_err(|e| format!("claude structured_output.proposals did not match the schema: {e}"))?;

    // Then drop hallucinated ids / non-idle worktrees — expected filtering, not an error.
    let proposals = parsed
        .into_iter()
        .filter(|p| valid_open_ids.contains(&p.task_id) && idle_worktrees.contains(&p.worktree))
        .collect();
    Ok(proposals)
}

/// Build the self-contained prompt embedding the candidate tasks and idle worktrees,
/// so the `--bare` headless call needs no project context.
fn build_prompt(tasks: &[TaskCtx], idle: &[WorktreeCtx]) -> String {
    let mut p = String::from(
        "You are Keitora's dispatcher. Match each idle worktree to the single best open task it \
         should pick up next, considering the worktree's recent task tags (its specialties) and \
         each task's priority and tags. Propose at most one task per worktree; only use the \
         task ids and worktree names listed below. Give each proposal a confidence in [0,1] and \
         a one-sentence rationale.\n\nOpen, unblocked tasks:\n",
    );
    for t in tasks {
        p.push_str(&format!(
            "- #{} [{}] {} (tags: {})\n",
            t.id,
            t.priority,
            t.title,
            if t.tags.is_empty() {
                "none".to_string()
            } else {
                t.tags.join(", ")
            }
        ));
    }
    p.push_str("\nIdle worktrees:\n");
    for w in idle {
        p.push_str(&format!(
            "- {} (recent tags: {})\n",
            w.name,
            if w.recent_tags.is_empty() {
                "none".to_string()
            } else {
                w.recent_tags.join(", ")
            }
        ));
    }
    p
}

/// A candidate task as embedded in the prompt.
struct TaskCtx {
    id: i64,
    title: String,
    priority: String,
    tags: Vec<String>,
}

/// An idle worktree as embedded in the prompt.
struct WorktreeCtx {
    name: String,
    recent_tags: Vec<String>,
}

/// Headless `claude` → ranked proposals for the idle worktrees. Gathers open,
/// unblocked tasks and idle worktrees (reconciling DB state against live PTY
/// ownership, like `tasks::sessions_list`), runs a single `--bare` call, and
/// logs each surfaced proposal to the dispatch audit trail.
#[tauri::command]
pub async fn dispatch_suggest(client: State<'_, SharedBackend>) -> Result<Vec<Proposal>, String> {
    let (backend, config) = crate::tasks::open_backend()?;

    // Idle worktrees: DB state `idle`, reconciled against the daemon live-set (a
    // worktree with no live daemon PTY is offline regardless of its last hook
    // write). The idle rule lives in `tasks::reconciled_sessions` so the
    // dispatcher and `sessions_list` agree. Reuse the `config` already resolved by
    // `open_backend` (FIX 6 — no second resolve / Connection).
    let project = config.project.name.clone();
    // Advisory background-work set. `background_set` does a ListSessions RPC; the
    // daemon serves it from a ~1s `ps` cache so it's sub-ms in normal operation,
    // and it degrades to empty on error. This is a blocking call in an async
    // command, but it's bounded (a hung daemon caps at the RPC timeout) and this
    // command already runs the much heavier headless claude call below via
    // spawn_blocking, so the marginal exposure is small.
    let bg = background_set(client.inner().as_ref());
    let sessions =
        crate::tasks::reconciled_sessions(&client.live_set_snapshot(), &bg, &project, &config)?;
    let idle_names: Vec<String> = sessions
        .iter()
        .filter(|s| s.state == "idle")
        .map(|s| s.worktree.clone())
        .collect();

    // No idle worktree → nothing to dispatch to (the frontend also short-circuits).
    if idle_names.is_empty() {
        return Ok(vec![]);
    }

    // Fetch the task list ONCE; derive BOTH the candidate set and each idle
    // worktree's recent tags from it in memory (no per-worktree N+1 scans). Include
    // archived/done tasks (generous window) so recently-finished tasks still feed
    // specialties; backlog excluded (it can't be a candidate and shouldn't tag).
    let filter = keitora_tasks::ListFilter {
        include_archived: true,
        include_backlog: false,
        assigned_to: None,
        archive_days: 365,
    };
    let tasks = backend.list(&filter).await.map_err(|e| e.to_string())?;

    // Candidates: open, unblocked, AND not already reserved/assigned to a worktree
    // (an already-assigned task must not be proposed to another worktree — that
    // would only fail with an `assign` Conflict after the full LLM round-trip).
    let candidates: Vec<TaskCtx> = tasks
        .iter()
        .filter(|t| {
            t.task.status == "open" && t.task.assigned_to.is_none() && t.blocked_by.is_empty()
        })
        .map(|t| TaskCtx {
            id: t.task.id,
            title: t.task.title.clone(),
            priority: t.task.priority.clone(),
            tags: t.task.tags.clone(),
        })
        .collect();

    // No candidate → nothing to propose.
    if candidates.is_empty() {
        return Ok(vec![]);
    }
    let valid_ids: Vec<i64> = candidates.iter().map(|t| t.id).collect();

    let idle: Vec<WorktreeCtx> = idle_names
        .iter()
        .map(|name| WorktreeCtx {
            name: name.clone(),
            recent_tags: recent_tags_for(&tasks, name),
        })
        .collect();

    let prompt = build_prompt(&candidates, &idle);

    // Run the headless call off the async runtime — `std::process::Command` is
    // blocking. We collected everything we need above, so nothing non-Send (the
    // store, the `Ptys` guard) crosses into the closure.
    //
    // NOTE: do NOT pass `--bare`. It skips OAuth/subscription credentials, so a
    // claude.ai-authenticated user gets "Not logged in" (verified). To still keep
    // the call cheap, run it in a neutral cwd (`temp_dir`) so it doesn't load
    // keitora's project `CLAUDE.md`/hooks — auth comes from the keychain, not cwd, so
    // this trims context without breaking login. The prompt is self-contained.
    let (status_ok, stdout, stderr) =
        tauri::async_runtime::spawn_blocking(move || run_claude_bounded(&prompt))
            .await
            .map_err(|e| format!("failed to run claude: {e}"))??;

    if !status_ok {
        return Err(format!(
            "claude exited non-zero: {} — is `claude` authenticated / on PATH?",
            String::from_utf8_lossy(&stderr).trim()
        ));
    }

    let proposals = parse_proposals(&stdout, &valid_ids, &idle_names)?;

    // Log each surfaced proposal to the audit trail (the point of the dispatcher).
    for p in &proposals {
        let _ = backend
            .dispatch_log_add(p.task_id, &p.worktree, "suggested", &p.rationale)
            .await;
    }

    Ok(proposals)
}

/// Spawn the headless `claude` call with a hard wall-clock deadline. Returns
/// `(status_ok, stdout, stderr)`. stdout/stderr are drained on separate threads to
/// avoid a pipe-fill deadlock, and the child is killed if it outlives
/// `SUGGEST_TIMEOUT` (`Command::output()` has no timeout of its own).
fn run_claude_bounded(prompt: &str) -> Result<(bool, Vec<u8>, Vec<u8>), String> {
    // Resolve the configured/discovered `claude` path + login PATH — a GUI launch
    // inherits a minimal PATH that would otherwise miss `claude` entirely.
    let (claude, path_env) = crate::host::claude_runtime();
    let mut cmd = Command::new(&claude);
    cmd.arg("-p")
        .arg(prompt)
        .arg("--model")
        .arg("haiku")
        .arg("--output-format")
        .arg("json")
        .arg("--json-schema")
        .arg(SCHEMA)
        .current_dir(std::env::temp_dir())
        .env("API_TIMEOUT_MS", API_TIMEOUT_MS)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = &path_env {
        cmd.env("PATH", path);
    }
    // Windows: suppress the console window a GUI process would otherwise pop when
    // spawning the console-subsystem `claude` (stdio is piped, so no console is
    // needed). No-op off Windows.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn claude: {e} — is `claude` on PATH?"))?;

    // Drain both pipes on their own threads so a full pipe can never wedge the
    // child (which would otherwise deadlock our `try_wait` poll loop).
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

    // Poll for exit; kill past the deadline.
    let deadline = Instant::now() + SUGGEST_TIMEOUT;
    let status = loop {
        match child
            .try_wait()
            .map_err(|e| format!("failed to wait on claude: {e}"))?
        {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Reap the reader threads so their pipe handles close.
                    let _ = out_reader.join();
                    let _ = err_reader.join();
                    return Err(format!(
                        "claude timed out after {}s",
                        SUGGEST_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };

    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    Ok((status.success(), stdout, stderr))
}

// ---------------------------------------------------------------------------
// Codex proposal-parsing utility — standalone, UNWIRED.
//
// `07-headless-dispatcher.md`'s "Decision" section (and README's Decisions
// list) fixes the advisory *suggest* pass on Claude/haiku regardless of a
// worktree's resolved CLI — a cheap, structured, single-shot classification
// call that doesn't benefit from running on the user's interactive CLI, and
// keeps working for a Codex-only user (who still needs `claude` discoverable
// for this pass, same as today). Wiring the functions below into
// `dispatch_suggest`/`dispatch_apply` would silently reintroduce a per-CLI
// suggest pass that decision explicitly rules out, so they ship here as
// standalone, unit-tested infrastructure with NO production caller in this
// stage. If a future task wants a Codex-driven suggest pass, it must first
// amend that Decision section, not just add a caller here.
// ---------------------------------------------------------------------------

/// Bounded `codex exec resume <thread_id> --output-schema <schema_file> -o
/// <out_file>` spawn, mirroring `run_claude_bounded`'s resolve/spawn/drain/kill
/// shape: resolves the Codex binary via the same project/user config precedence
/// `claude_runtime` uses for Claude, enforces a hard wall-clock deadline killing
/// the child on timeout, and drains stdout/stderr on separate threads regardless
/// (Codex may still write progress/log lines to stdout even though the
/// structured result goes to `<out_file>`). Returns `(status_ok, stdout,
/// stderr)`; the caller is responsible for reading `<out_file>` from disk
/// afterward and feeding its bytes to [`parse_codex_proposals`] — NOT stdout
/// (`codex exec --json` is a per-event JSONL stream, not a single JSON value).
///
/// No production caller in this stage — see the module note above.
#[allow(dead_code)]
fn run_codex_bounded(
    thread_id: &str,
    schema_file: &str,
    out_file: &str,
) -> Result<(bool, Vec<u8>, Vec<u8>), String> {
    let user = keitora_tasks::UserConfig::load().unwrap_or_default();
    let project = keitora_tasks::resolve_active().ok();
    let codex = keitora_tasks::resolve_provider_path(CliKind::Codex, project.as_ref(), &user)
        .unwrap_or_else(|| "codex".to_string());

    let mut cmd = Command::new(&codex);
    cmd.arg("exec")
        .arg("resume")
        .arg(thread_id)
        .arg("--output-schema")
        .arg(schema_file)
        .arg("-o")
        .arg(out_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Windows: suppress the console window a GUI process would otherwise pop
    // (stdio is piped, so no console is needed). No-op off Windows.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn codex: {e} — is `codex` on PATH?"))?;

    // Drain both pipes on their own threads so a full pipe can never wedge the
    // child (which would otherwise deadlock our `try_wait` poll loop).
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

    // Poll for exit; kill past the deadline. Reuses `SUGGEST_TIMEOUT` — the same
    // bound the Claude advisory call enforces.
    let deadline = Instant::now() + SUGGEST_TIMEOUT;
    let status = loop {
        match child
            .try_wait()
            .map_err(|e| format!("failed to wait on codex: {e}"))?
        {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Reap the reader threads so their pipe handles close.
                    let _ = out_reader.join();
                    let _ = err_reader.join();
                    return Err(format!(
                        "codex timed out after {}s",
                        SUGGEST_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };

    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    Ok((status.success(), stdout, stderr))
}

/// Pure: parse a `codex exec --output-schema -o <outfile>` result into validated
/// proposals — the Codex analogue of [`parse_proposals`], minus the
/// `is_error`/`subtype`/`structured_output` envelope-unwrapping (that indirection
/// is a Claude `--output-format json` artifact; `-o` writes the model's final
/// JSON value straight to the file, no envelope). Takes the ALREADY-READ
/// `<outfile>` bytes (read from disk by the caller, never from stdout) so this
/// stays a pure, fast-testable function.
///
/// Empty bytes are treated as a distinct, actionable "missing/unreadable
/// outfile" error — e.g. `codex exec` exited before ever writing `-o`, or the
/// caller's `std::fs::read` failed — never conflated with "valid file, empty
/// proposals list" (that's the true-empty-array case, `"[]"`, which parses to
/// `Ok(vec![])` below). Applies the SAME post-parse filtering `parse_proposals`
/// does — dropping any proposal whose `task_id` isn't a currently open/unblocked
/// task or whose `worktree` isn't idle — so the two CLI paths share, rather than
/// reimplement, that filtering semantics.
///
/// No production caller in this stage — see the module note above.
#[allow(dead_code)]
fn parse_codex_proposals(
    outfile_contents: &[u8],
    valid_open_ids: &[i64],
    idle_worktrees: &[String],
) -> Result<Vec<Proposal>, String> {
    if outfile_contents.is_empty() {
        return Err(
            "codex `-o` output file is missing or empty — did `codex exec` exit before writing it?"
                .to_string(),
        );
    }

    // Deserialize the whole array strictly, directly against the `Proposal[]`
    // shape — no envelope to unwrap. A malformed entry (e.g. a string task_id) is
    // a schema violation, not "no matches" — surface it loudly rather than
    // silently dropping it.
    let parsed: Vec<Proposal> = serde_json::from_slice(outfile_contents)
        .map_err(|e| format!("codex `-o` output did not match the Proposal[] schema: {e}"))?;

    // Then drop hallucinated ids / non-idle worktrees — expected filtering, not an
    // error. Same rule `parse_proposals` applies (shared semantics, not shared
    // code, since the two paths' error/deserialize shapes differ upstream).
    let proposals = parsed
        .into_iter()
        .filter(|p| valid_open_ids.contains(&p.task_id) && idle_worktrees.contains(&p.worktree))
        .collect();
    Ok(proposals)
}

/// The recent task tags for a worktree (its "specialties"): tags of the tasks it
/// actually worked — only `active`/`done`, so a merely-reserved (open) task does
/// NOT feed its own tags back as a specialty. Derived from an already-fetched task
/// list to avoid an extra per-worktree scan; flattened and deduped.
fn recent_tags_for(tasks: &[keitora_tasks::TaskWithDeps], worktree: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for t in tasks {
        if t.task.assigned_to.as_deref() != Some(worktree) {
            continue;
        }
        if t.task.status != "active" && t.task.status != "done" {
            continue;
        }
        for tag in &t.task.tags {
            if seen.insert(tag.clone()) {
                out.push(tag.clone());
            }
        }
    }
    out
}

/// State-aware apply: reserve the task for `worktree` always, then inject it iff
/// the worktree is idle with a live PTY (else leave it reserved/queued). The
/// backend is authoritative on the idle decision so we never write into a busy PTY.
#[tauri::command]
pub async fn dispatch_apply(
    worktree: String,
    task_id: i64,
    client: State<'_, SharedBackend>,
    gate: State<'_, DispatchGate>,
) -> Result<DispatchOutcome, String> {
    // Hold the dispatch gate for the whole command so the guard-check → reserve →
    // inject → log sequence is atomic vs. concurrent dispatch_apply calls.
    // `tokio::sync::Mutex::lock` is cancel-safe and does not poison, so the guard is
    // always acquired cleanly.
    let _gate = gate.0.lock().await;

    let (backend, config) = crate::tasks::open_backend()?;

    // Idempotency guard + reserve in one intent-level call. If this task is
    // already dispatched and still reserved to this worktree, `dispatch_reserve`
    // returns `AlreadyDispatched` and performs NO write, so a re-click / re-drag /
    // keyboard re-apply no-ops. Otherwise it commits the reserve and returns the
    // post-assign `Task`. Errors (blocked / terminal / assigned elsewhere) bubble up
    // from the reserve as before — but only BEFORE the write (never-Err-after-reserve).
    let task = match backend
        .dispatch_reserve(task_id, "keitora", &worktree)
        .await
        .map_err(|e| e.to_string())?
    {
        ReserveOutcome::AlreadyDispatched => {
            return Ok(DispatchOutcome {
                dispatched: true,
                reason: Some("already dispatched".into()),
            });
        }
        ReserveOutcome::Reserved(task) => task,
    };

    // Live state via the shared reconciled view, so the idle rule matches
    // `sessions_list`/`dispatch_suggest`. `decide` is a pre-filter on the idle rule;
    // the AUTHORITATIVE liveness check is the daemon's `write_line` result below
    // (D6). A reconciled non-`offline` session is one the daemon holds live, so we
    // feed `state != "offline"` for `has_live_pty` — but the daemon is the final
    // word on whether the write actually landed.
    // Reuse the `config` already resolved by `open_backend` for both the project
    // name and the LocalStore open inside `reconciled_sessions` (FIX 6).
    let project = config.project.name.clone();
    let live = client.live_set_snapshot();
    let bg = background_set(client.inner().as_ref()); // on-demand, exact; empty on RPC error
    let sessions = crate::tasks::reconciled_sessions(&live, &bg, &project, &config)?;
    let state = sessions
        .iter()
        .find(|s| s.worktree == worktree)
        .map(|s| s.state.clone())
        .unwrap_or_else(|| "offline".to_string());
    let has_live_pty = state != "offline";

    match decide(&state, has_live_pty) {
        Decision::Dispatched => {
            let ctx = DispatchContext {
                task: &task,
                worktree: &worktree,
            };
            // Resolve the target worktree's CLI so the injected ritual matches it:
            // worktree entry `cli` → user `default_cli` →
            // `CliKind::Claude`. Dispatching INTO a Codex worktree is safe here
            // specifically because `decide` above already gated this branch on
            // idle + live state, and stage 030 landed Codex's own liveness signal
            // (`.codex/hooks.json`) — before 030, a Codex worktree's reconciled
            // state was always Claude-hook-derived (never meaningfully idle+live),
            // so this injection path was effectively unreachable for Codex until
            // that liveness signal landed.
            let user = keitora_tasks::UserConfig::load().unwrap_or_default();
            let entry = config
                .worktree_entries()
                .into_iter()
                .find(|e| e.name == worktree);
            let resolved_cli = keitora_tasks::resolve_cli(entry.as_ref(), &user);
            let steps = render_steps(select_ritual_steps(&config.dispatch, resolved_cli), &ctx);
            let key = SessionKey {
                project: project.clone(),
                worktree: worktree.clone(),
            };
            // Inject each step as its own submitted line via a single atomic daemon
            // `WriteLine` RPC: the daemon writes the text, pauses `SUBMIT_DELAY` ms,
            // then writes the `\r`. Folding the old two-step client-side
            // text→sleep→`\r` split into one server-side RPC removes the window where
            // a concurrent keystroke could land between a step's text and its Enter.
            // The `write_line` RESULT is AUTHORITATIVE for liveness (D6):
            //   * Ok            → Dispatched (text + `\r` reached the PTY).
            //   * Queued        → NoSuchSession / NotConnected: leave reserved, retry
            //                     later. NOT an Err (a retry would re-inject).
            //   * Error (other) → a real write failure; still NEVER an Err after
            //                     reserve — the partial/queued reporting preserves the
            //                     never-Err-after-reserve invariant.
            // The inter-step delay still runs on this thread so the TUI finishes one
            // submitted line before the next arrives — bounded and small for the
            // handful of steps a ritual holds.
            let (result, sent_any) = inject_steps(
                client.inner().as_ref(),
                &key,
                &steps,
                SUBMIT_DELAY.as_millis() as u32,
                INTER_STEP_DELAY,
            );
            // The task is already reserved. NEVER return Err after reserving (a retry
            // would re-inject). Map the authoritative final outcome to a (never-Err)
            // result + audit decision via the pure `injection_outcome` helper.
            let terminated = client.terminated_keys(&project).contains(&worktree);
            let detail = injection_detail(&result, terminated);
            let (decision, outcome) =
                injection_outcome(map_write_line_result(&result), sent_any, &worktree, &detail);
            // Best-effort audit: a failed log must not flip us to Err (which would
            // prompt a re-inject of an already-dispatched task).
            let _ = backend
                .dispatch_log_add(task_id, &worktree, decision, &detail)
                .await;
            Ok(outcome)
        }
        Decision::Queued => {
            // Best-effort audit, matching the Dispatched branch: the task is already
            // reserved, so a failed log must NOT flip us to Err (a retried
            // dispatch_apply would re-reserve / re-inject). never-Err-after-reserve.
            let _ = backend
                .dispatch_log_add(task_id, &worktree, "queued", "")
                .await;
            let terminated = client.terminated_keys(&project).contains(&worktree);
            Ok(DispatchOutcome {
                dispatched: false,
                reason: Some(queued_reason(&worktree, terminated)),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A captured-style `claude --output-format json` success envelope. The
    // validated payload is in the top-level `structured_output` field.
    fn success_envelope() -> &'static str {
        r#"{
          "type": "result",
          "subtype": "success",
          "is_error": false,
          "result": "Proposed 2 tasks.",
          "structured_output": {
            "proposals": [
              {"task_id": 42, "worktree": "sayo", "rationale": "sayo specializes in UI", "confidence": 0.9},
              {"task_id": 7,  "worktree": "feng", "rationale": "feng owns the parser", "confidence": 0.6},
              {"task_id": 99, "worktree": "sayo", "rationale": "unknown task id", "confidence": 0.5},
              {"task_id": 42, "worktree": "ghost", "rationale": "non-idle worktree", "confidence": 0.5}
            ]
          }
        }"#
    }

    #[test]
    fn parse_proposals_reads_valid_and_filters_unknown() {
        let valid_ids = vec![42, 7];
        let idle = vec!["sayo".to_string(), "feng".to_string()];
        let got = parse_proposals(success_envelope().as_bytes(), &valid_ids, &idle).unwrap();
        // Drops the unknown task id (99) and the non-idle worktree (ghost).
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].task_id, 42);
        assert_eq!(got[0].worktree, "sayo");
        assert_eq!(got[1].task_id, 7);
        assert_eq!(got[1].worktree, "feng");
    }

    #[test]
    fn parse_proposals_errors_on_is_error_true() {
        let env =
            r#"{"type":"result","subtype":"success","is_error":true,"result":"rate limited"}"#;
        let err = parse_proposals(env.as_bytes(), &[1], &["a".to_string()]).unwrap_err();
        assert!(err.contains("rate limited"), "got: {err}");
    }

    #[test]
    fn parse_proposals_errors_on_error_subtype() {
        let env = r#"{"type":"result","subtype":"error","is_error":false,"result":"bad request"}"#;
        let err = parse_proposals(env.as_bytes(), &[1], &["a".to_string()]).unwrap_err();
        assert!(err.contains("bad request"), "got: {err}");
    }

    #[test]
    fn parse_proposals_errors_on_garbage() {
        let err = parse_proposals(b"not json at all {{{", &[1], &["a".to_string()]).unwrap_err();
        assert!(err.contains("could not parse"), "got: {err}");
    }

    #[test]
    fn parse_proposals_errors_on_malformed_proposal() {
        // A string task_id violates the schema → Err, NOT an empty Ok (which would
        // be indistinguishable from "no matches").
        let env = r#"{
          "type": "result", "subtype": "success", "is_error": false,
          "structured_output": { "proposals": [
            {"task_id": "not-an-int", "worktree": "sayo", "rationale": "x", "confidence": 0.5}
          ]}
        }"#;
        let err = parse_proposals(env.as_bytes(), &[42], &["sayo".to_string()]).unwrap_err();
        assert!(
            err.contains("did not match the schema"),
            "expected a schema error, got: {err}"
        );
    }

    #[test]
    fn parse_proposals_accepts_valid_empty_array() {
        let env = r#"{
          "type": "result", "subtype": "success", "is_error": false,
          "structured_output": { "proposals": [] }
        }"#;
        let got = parse_proposals(env.as_bytes(), &[42], &["sayo".to_string()]).unwrap();
        assert!(got.is_empty(), "valid empty array → Ok(empty), got {got:?}");
    }

    #[test]
    fn parse_proposals_errors_on_missing_structured_output() {
        let env = r#"{"type":"result","subtype":"success","is_error":false,"result":"no payload"}"#;
        let err = parse_proposals(env.as_bytes(), &[1], &["a".to_string()]).unwrap_err();
        assert!(err.contains("structured_output"), "got: {err}");
    }

    #[test]
    fn decide_truth_table() {
        // Only idle + live PTY dispatches; everything else queues.
        assert_eq!(decide("idle", true), Decision::Dispatched);
        assert_eq!(decide("idle", false), Decision::Queued);
        assert_eq!(decide("busy", true), Decision::Queued);
        assert_eq!(decide("waiting", true), Decision::Queued);
        assert_eq!(decide("offline", true), Decision::Queued);
        assert_eq!(decide("offline", false), Decision::Queued);
    }

    /// Gate test: a session that is `idle` per the DB but present in the background
    /// set reconciles to `busy`, and `decide("busy", true)` returns `Queued` — so
    /// the dispatcher never injects into a session running background work.
    #[test]
    fn idle_with_background_task_gates_dispatch() {
        use crate::backend::{apply_reconcile, SessionKey};
        use keitora_tasks::WorktreeSession;
        use std::collections::HashSet;

        // Build a session that the DB reports as idle.
        let session = WorktreeSession {
            project: String::new(),
            worktree: "sayo".to_string(),
            state: "idle".to_string(),
            current_task_id: Some(5),
            pid: Some(9999),
            last_heartbeat: "2026-06-22T00:00:00Z".to_string(),
        };

        let key = SessionKey {
            project: "proj".to_string(),
            worktree: "sayo".to_string(),
        };

        // Session is live (present in the daemon live-set) …
        let mut live: HashSet<SessionKey> = HashSet::new();
        live.insert(key.clone());

        // … and the daemon reports it is running a background task.
        let mut bg: HashSet<SessionKey> = HashSet::new();
        bg.insert(key.clone());

        // Stage-003 override: idle + bg in live-set → busy.
        let reconciled = apply_reconcile(&[session], &live, &bg, "proj");
        assert_eq!(
            reconciled[0].state, "busy",
            "idle+bg session must reconcile to busy"
        );

        // Stage-004 gate: busy → Queued, so the dispatcher never injects.
        assert_eq!(
            decide("busy", true),
            Decision::Queued,
            "decide must queue a busy session even with a live PTY"
        );
    }

    fn task_fixture(id: i64, title: &str, desc: &str) -> keitora_tasks::Task {
        keitora_tasks::Task {
            id,
            title: title.to_string(),
            description: desc.to_string(),
            status: "active".to_string(),
            priority: "high".to_string(),
            assigned_to: Some("sol".to_string()),
            created_by: None,
            tags: vec![],
            branch: None,
            pr_url: None,
            created_at: "2026-06-21".to_string(),
            updated_at: "2026-06-21".to_string(),
        }
    }

    #[test]
    fn render_steps_empty_config_uses_default_line() {
        let task = task_fixture(42, "Fix\tthe   parser", "It crashes\non  empty\n\ninput");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let steps = render_steps(&DispatchConfig::default().claude_steps, &ctx);
        assert_eq!(
            steps.len(),
            1,
            "empty config yields exactly the default line"
        );
        let line = &steps[0];
        // Embedded newlines/tabs/multi-spaces collapse to single spaces.
        assert!(
            line.contains("Fix the parser"),
            "title not collapsed: {line}"
        );
        assert!(
            line.contains("It crashes on empty input"),
            "desc not collapsed: {line}"
        );
        // Built-in claim/done guidance preserved.
        assert!(line.contains("./keitora task claim 42"));
        assert!(line.contains("./keitora task done 42"));
        // The submit byte is NOT carried in-band any more — it is written separately.
        assert!(
            !line.ends_with('\r'),
            "submit byte must not be in the line: {line:?}"
        );
        assert!(
            !line.chars().any(|c| c.is_control()),
            "no control byte: {line:?}"
        );
    }

    #[test]
    fn render_steps_default_line_strips_control_bytes() {
        // ESC/ANSI in the title and a BEL in the desc must both be neutralised.
        let task = task_fixture(7, "a\u{1b}[31mb", "x\x07y");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let line = &render_steps(&DispatchConfig::default().claude_steps, &ctx)[0];
        assert!(
            line.contains("a [31mb") || line.contains("a[31mb"),
            "title: {line:?}"
        );
        assert!(line.contains("x y"), "desc: {line:?}");
        assert!(
            !line.chars().any(|c| c.is_control()),
            "no control byte may survive: {line:?}"
        );
        assert!(!line.contains('\u{1b}'), "no ESC may survive: {line:?}");
    }

    #[test]
    fn render_steps_interpolates_configured_steps_in_order() {
        let task = task_fixture(19, "custom dispatch", "wire it up");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec![
                "/clear".to_string(),
                "/park".to_string(),
                "/workflow pick up #{task_number}: {task_title}".to_string(),
            ],
            codex_steps: vec![],
        };
        let steps = render_steps(&cfg.claude_steps, &ctx);
        assert_eq!(
            steps,
            vec![
                "/clear".to_string(),
                "/park".to_string(),
                "/workflow pick up #0019: custom dispatch".to_string(),
            ]
        );
    }

    #[test]
    fn render_steps_drops_blank_steps() {
        let task = task_fixture(1, "t", "d");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec!["/clear".to_string(), "   ".to_string(), "/park".to_string()],
            codex_steps: vec![],
        };
        let steps = render_steps(&cfg.claude_steps, &ctx);
        assert_eq!(steps, vec!["/clear".to_string(), "/park".to_string()]);
    }

    #[test]
    fn render_steps_all_blank_config_falls_back_to_default() {
        // A misconfigured all-blank `steps` must NOT yield an empty injection (which
        // dispatch_apply would report as a successful no-op) — it falls back to the
        // built-in default line so a dispatch always sends something.
        let task = task_fixture(42, "the title", "the desc");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec!["   ".to_string(), "\t".to_string()],
            codex_steps: vec![],
        };
        let steps = render_steps(&cfg.claude_steps, &ctx);
        assert_eq!(steps.len(), 1, "must fall back to one default line");
        assert!(
            steps[0].contains("./keitora task claim 42"),
            "default line: {:?}",
            steps[0]
        );
    }

    #[test]
    fn render_steps_collapses_interpolated_control_chars() {
        // A title carrying a newline/ESC, injected via a custom step, must collapse so
        // it can't submit early or smuggle an escape sequence.
        let task = task_fixture(5, "line1\nline2\u{1b}[0m", "d");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec!["go {task_title}".to_string()],
            codex_steps: vec![],
        };
        let line = &render_steps(&cfg.claude_steps, &ctx)[0];
        assert!(
            !line.chars().any(|c| c.is_control()),
            "no control byte: {line:?}"
        );
        assert!(line.starts_with("go line1 line2"), "collapsed: {line:?}");
    }

    // -----------------------------------------------------------------------
    // select_ritual_steps
    // -----------------------------------------------------------------------

    #[test]
    fn select_ritual_steps_picks_claude_steps_for_claude_cli() {
        let cfg = DispatchConfig {
            claude_steps: vec!["/clear".to_string(), "/park".to_string()],
            codex_steps: vec!["/codex-step".to_string()],
        };
        assert_eq!(
            select_ritual_steps(&cfg, CliKind::Claude),
            &cfg.claude_steps[..]
        );
    }

    #[test]
    fn select_ritual_steps_picks_codex_steps_for_codex_cli() {
        let cfg = DispatchConfig {
            claude_steps: vec!["/clear".to_string()],
            codex_steps: vec!["/codex-step".to_string(), "/codex-step-2".to_string()],
        };
        assert_eq!(
            select_ritual_steps(&cfg, CliKind::Codex),
            &cfg.codex_steps[..]
        );
    }

    #[test]
    fn select_ritual_steps_empty_codex_list_falls_back_to_default_via_render_steps() {
        // `select_ritual_steps` itself has no fallback logic — it is a pure field
        // selector. The fallback lives in `render_steps`, exercised here through
        // the full pipeline for a Codex worktree with no configured `codex_steps`,
        // mirroring `render_steps_all_blank_config_falls_back_to_default` but for
        // the Codex list specifically.
        let task = task_fixture(42, "the title", "the desc");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec!["/clear".to_string()],
            codex_steps: vec![],
        };
        let steps = render_steps(select_ritual_steps(&cfg, CliKind::Codex), &ctx);
        assert_eq!(
            steps.len(),
            1,
            "empty codex_steps falls back to one default line"
        );
        assert!(
            steps[0].contains("./keitora task claim 42"),
            "default line: {:?}",
            steps[0]
        );
    }

    /// `select_ritual_steps` is exercised only from the injection path
    /// (`dispatch_apply`) — the advisory suggest pass never calls it. Paired with
    /// `run_claude_bounded_argv_not_parameterized_by_cli_kind` below, which
    /// confirms the suggest pass's own binary/model choice takes no `CliKind` at
    /// all.
    #[test]
    #[allow(clippy::type_complexity)]
    fn run_claude_bounded_argv_not_parameterized_by_cli_kind() {
        // Type-level guard, not a call: `run_claude_bounded` takes exactly one
        // argument, `&str` (the prompt) — no `CliKind`/worktree parameter.
        // Coercing it to this function-pointer type IS the assertion; a future
        // change that adds a `CliKind` param to special-case Codex would stop
        // this line from compiling, which is the regression this test exists to
        // catch. Never actually invoked (it spawns a real `claude` process).
        let _f: fn(&str) -> Result<(bool, Vec<u8>, Vec<u8>), String> = run_claude_bounded;
    }

    // -----------------------------------------------------------------------
    // parse_codex_proposals — standalone, unwired (see the
    // module note above the function).
    // -----------------------------------------------------------------------

    /// A bare `Proposal[]` array — the shape `codex exec --output-schema -o`
    /// writes directly to its output file, with the same fixture data as
    /// `success_envelope` above (minus the Claude envelope wrapper).
    fn codex_output_fixture() -> &'static str {
        r#"[
          {"task_id": 42, "worktree": "sayo", "rationale": "sayo specializes in UI", "confidence": 0.9},
          {"task_id": 7,  "worktree": "feng", "rationale": "feng owns the parser", "confidence": 0.6},
          {"task_id": 99, "worktree": "sayo", "rationale": "unknown task id", "confidence": 0.5},
          {"task_id": 42, "worktree": "ghost", "rationale": "non-idle worktree", "confidence": 0.5}
        ]"#
    }

    #[test]
    fn parse_codex_proposals_reads_valid_and_filters_unknown() {
        let valid_ids = vec![42, 7];
        let idle = vec!["sayo".to_string(), "feng".to_string()];
        let got =
            parse_codex_proposals(codex_output_fixture().as_bytes(), &valid_ids, &idle).unwrap();
        // Drops the unknown task id (99) and the non-idle worktree (ghost).
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].task_id, 42);
        assert_eq!(got[0].worktree, "sayo");
        assert_eq!(got[1].task_id, 7);
        assert_eq!(got[1].worktree, "feng");
    }

    #[test]
    fn parse_codex_proposals_errors_on_malformed_proposal() {
        // A string task_id violates the schema → Err, NOT an empty Ok (which would
        // be indistinguishable from "no matches").
        let raw = r#"[
          {"task_id": "not-an-int", "worktree": "sayo", "rationale": "x", "confidence": 0.5}
        ]"#;
        let err = parse_codex_proposals(raw.as_bytes(), &[42], &["sayo".to_string()]).unwrap_err();
        assert!(
            err.contains("did not match"),
            "expected a schema error, got: {err}"
        );
    }

    #[test]
    fn parse_codex_proposals_accepts_valid_empty_array() {
        let got = parse_codex_proposals(b"[]", &[42], &["sayo".to_string()]).unwrap();
        assert!(got.is_empty(), "valid empty array → Ok(empty), got {got:?}");
    }

    #[test]
    fn parse_codex_proposals_errors_on_missing_outfile() {
        // Empty bytes stand in for a missing/unreadable `<outfile>` (the caller
        // reads the file from disk; a missing file yields no bytes to pass in).
        // This must be a DISTINCT, actionable error — not conflated with "valid
        // file, empty proposals list" (see the `"[]"` case above, which is Ok).
        let err = parse_codex_proposals(b"", &[1], &["a".to_string()]).unwrap_err();
        assert!(
            err.contains("missing or empty"),
            "expected a distinct missing-outfile error, got: {err}"
        );
    }

    #[test]
    fn parse_codex_proposals_no_envelope_unwrapping() {
        // A bare `Proposal[]` array with no `is_error`/`subtype`/`structured_output`
        // envelope around it. `parse_proposals` (the Claude path) REQUIRES that
        // envelope and rejects this exact input outright; `parse_codex_proposals`
        // must accept it directly, confirming no envelope-unwrapping is attempted.
        let raw = r#"[{"task_id": 42, "worktree": "sayo", "rationale": "ok", "confidence": 0.9}]"#;
        let valid_ids = vec![42];
        let idle = vec!["sayo".to_string()];

        // Sanity check on the contrast: the Claude parser rejects this same bare
        // array (it expects an object envelope with `structured_output.proposals`).
        assert!(parse_proposals(raw.as_bytes(), &valid_ids, &idle).is_err());

        // The Codex parser reads it directly.
        let got = parse_codex_proposals(raw.as_bytes(), &valid_ids, &idle).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].task_id, 42);
        assert_eq!(got[0].worktree, "sayo");
    }

    // -----------------------------------------------------------------------
    // inject_steps — the injection loop against a FakeBackend.
    // -----------------------------------------------------------------------

    fn inject_key() -> SessionKey {
        SessionKey {
            project: "proj".to_string(),
            worktree: "sayo".to_string(),
        }
    }

    fn write_line_calls(
        fake: &crate::backend::fake::FakeBackend,
    ) -> Vec<(SessionKey, String, u32)> {
        use crate::backend::fake::Call;
        fake.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::WriteLine {
                    key,
                    text,
                    submit_delay_ms,
                } => Some((key, text, submit_delay_ms)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn inject_steps_all_ok_reports_sent() {
        use crate::backend::fake::FakeBackend;
        let fake = FakeBackend::new();
        fake.push_write_line(Ok(()));
        fake.push_write_line(Ok(()));
        let key = inject_key();
        let steps = vec!["/clear".to_string(), "/go".to_string()];
        let (result, sent_any) = inject_steps(&fake, &key, &steps, 60, Duration::ZERO);
        assert!(result.is_ok());
        assert!(sent_any);
        // Both steps attempted, with the right key + submit delay.
        let writes = write_line_calls(&fake);
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].0, key);
        assert_eq!(writes[0].1, "/clear");
        assert_eq!(writes[0].2, 60);
        assert_eq!(writes[1].1, "/go");
    }

    #[test]
    fn inject_steps_stops_on_no_such_session_but_sent_any() {
        use crate::backend::fake::FakeBackend;
        let fake = FakeBackend::new();
        fake.push_write_line(Ok(()));
        fake.push_write_line(Err(PtyBackendError::NoSuchSession));
        let key = inject_key();
        let steps = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (result, sent_any) = inject_steps(&fake, &key, &steps, 60, Duration::ZERO);
        // The failing step's result is authoritative and maps to Queued.
        assert!(matches!(result, Err(PtyBackendError::NoSuchSession)));
        assert_eq!(map_write_line_result(&result), WriteLineOutcome::Queued);
        assert!(sent_any, "step 1 reached the pane");
        // Stops after the failing step — step 3 never runs.
        assert_eq!(write_line_calls(&fake).len(), 2);
    }

    #[test]
    fn inject_steps_first_not_connected_sends_nothing() {
        use crate::backend::fake::FakeBackend;
        let fake = FakeBackend::new();
        fake.push_write_line(Err(PtyBackendError::NotConnected));
        let key = inject_key();
        let steps = vec!["a".to_string(), "b".to_string()];
        let (result, sent_any) = inject_steps(&fake, &key, &steps, 60, Duration::ZERO);
        assert!(matches!(result, Err(PtyBackendError::NotConnected)));
        assert_eq!(map_write_line_result(&result), WriteLineOutcome::Queued);
        assert!(!sent_any);
        // No step runs after the first failure.
        assert_eq!(write_line_calls(&fake).len(), 1);
    }

    #[test]
    fn a_terminated_worktree_is_named_in_the_queued_reason() {
        assert_eq!(queued_reason("sayo", false), "sayo busy — reserved");
        assert_eq!(queued_reason("sayo", true), "sayo is terminated — reserved");
    }

    #[test]
    fn a_missing_session_of_a_terminated_worktree_says_so() {
        use crate::backend::fake::FakeBackend;
        let fake = FakeBackend::new();
        let key = inject_key();
        fake.insert_terminated(key.clone());
        fake.push_write_line(Err(PtyBackendError::NoSuchSession));
        let (result, sent_any) = inject_steps(&fake, &key, &["a".to_string()], 60, Duration::ZERO);
        let terminated = fake.terminated_keys(&key.project).contains(&key.worktree);
        let detail = injection_detail(&result, terminated);
        assert_eq!(detail, "session is terminated; launch it to dispatch");
        let (decision, outcome) =
            injection_outcome(map_write_line_result(&result), sent_any, "sayo", &detail);
        assert_eq!(decision, "queued");
        assert!(!outcome.dispatched);
        assert!(outcome.reason.unwrap().contains("terminated"));
        // Not terminated, or another error: the error's own text.
        let missing = Err(PtyBackendError::NoSuchSession);
        assert_eq!(injection_detail(&missing, false), "no live session");
        let down = Err(PtyBackendError::NotConnected);
        assert_eq!(injection_detail(&down, true), "herdr unreachable");
        assert_eq!(injection_detail(&Ok(()), true), "");
    }
}
