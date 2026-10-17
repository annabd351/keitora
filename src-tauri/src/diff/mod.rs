//! `diff` — read-only, off-thread worktree diff subsystem for the diff view.
//!
//! Three Tauri commands, all resolving the active project server-side and running git on the
//! blocking pool so the UI never stalls:
//! - [`worktree_diff_summary`] — the cheap changed-file list (names + counts), tree-first.
//! - [`worktree_file_diff`] — one file's structured diff, lazy and cancellable.
//! - [`worktree_diff_revision`] — a cheap change fingerprint for liveness polling.
//!
//! Diff base is `--merge-base <main>` → working tree (committed + staged + unstaged), with
//! untracked files discovered separately. Pure parsing lives in [`parse`]; the LRU in
//! [`cache`]; per-file cancellation in [`jobs`].

pub mod cache;
pub mod highlight;
pub mod jobs;
pub mod parse;
pub mod snapshot;
pub mod watch;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use tauri::State;

use cache::{DiffCache, FileKey};
use highlight::{LineTokens, Span};
use jobs::DiffJobs;
use keitora_tasks::git::git_in;
use parse::{DiffFileSummary, DiffLineKind, FileDiff, UntrackedEntry};

/// Cap for reading an untracked file's contents into an all-add diff.
const MAX_UNTRACKED_BYTES: usize = 1024 * 1024;
/// Parse at most this many bytes of a single file's diff; beyond it, mark `truncated`.
const MAX_DIFF_BYTES: usize = 4 * 1024 * 1024;
/// Max per-side bytes to syntax-highlight. Highlighting the *whole* file (not just the hunk) is
/// costlier than diffing, so cap below [`MAX_DIFF_BYTES`]; larger files render without spans.
const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
/// Bytes sniffed for a NUL to classify a file as binary.
const BINARY_SNIFF: usize = 8000;
/// Max untracked files to stat for line counts per summary. Beyond this (e.g. an un-ignored
/// build/output directory yielding thousands of `??` entries) the extras are listed without a
/// count, so a summary refresh can't trigger thousands of file reads.
pub(super) const MAX_UNTRACKED_COUNTED: usize = 300;
/// How long a resolved base branch stays cached per worktree. The base (`main`) effectively
/// never changes, and resolving it costs 1–2 `git rev-parse` spawns, so caching removes them
/// from the hot path (summary, per-file diff, and the 2s liveness probe all need it).
const BASE_TTL: Duration = Duration::from_secs(60);
/// Short TTL for cached HEAD + merge-base oids (used in the per-file cache key). Brief because
/// HEAD moves on commit; it only needs to span a burst of file-to-file clicks.
const OIDS_TTL: Duration = Duration::from_secs(2);

/// Per-worktree cache of the resolved diff base (TTL [`BASE_TTL`]).
#[allow(clippy::type_complexity)]
static BASE_CACHE: OnceLock<Mutex<HashMap<PathBuf, (ResolvedBase, Instant)>>> = OnceLock::new();
/// Per-worktree cache of `(head_oid, base_oid)` (TTL [`OIDS_TTL`]).
#[allow(clippy::type_complexity)]
static OIDS_CACHE: OnceLock<Mutex<HashMap<PathBuf, (OidPair, Instant)>>> = OnceLock::new();

/// Resolve a bare worktree name to its absolute path within the active project (same
/// convention as the transfer/worktree commands).
fn worktree_dir(worktree: &str) -> Result<std::path::PathBuf, String> {
    let config = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    Ok(config.worktree_path(worktree))
}

/// Run a git command to completion, returning stdout on success.
pub(super) fn run(mut cmd: Command) -> Result<Vec<u8>, String> {
    let out = cmd.output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// True if the command exits 0 (used for `rev-parse --verify`).
fn ok(mut cmd: Command) -> bool {
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

/// How `HEAD` is compared against the resolved base ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Compare {
    /// Diff `merge-base(ref, HEAD)` → working tree (the historical behavior).
    MergeBase,
    /// Diff the ref's tip → working tree (every difference from the ref as it is now).
    Tip,
}

/// A [`DiffBase`] resolved against a worktree: the concrete ref to compare against, how to
/// compare, and an error message when a configured ref could not be resolved (the diff is then
/// short-circuited to empty and the UI shows the error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedBase {
    pub ref_name: String,
    pub compare: Compare,
    pub error: Option<String>,
}

/// `origin/main` → local `main` → `origin/HEAD` → `"main"`. `origin/main` comes first so the
/// diff reflects the real upstream mainline even when local `main` lags; the merge-base with
/// `HEAD` still resolves the fork point. Backs [`DiffBase::Auto`].
fn resolve_auto_ref(dir: &Path) -> String {
    for cand in ["origin/main", "main"] {
        let mut c = git_in(dir);
        c.args(["rev-parse", "--verify", "--quiet", cand]);
        if ok(c) {
            return cand.to_string();
        }
    }
    let mut c = git_in(dir);
    c.args([
        "symbolic-ref",
        "--quiet",
        "--short",
        "refs/remotes/origin/HEAD",
    ]);
    if let Ok(out) = run(c) {
        let s = String::from_utf8_lossy(&out).trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    "main".to_string()
}

/// True if `rev` resolves to a commit in `dir`.
fn ref_exists(dir: &Path, rev: &str) -> bool {
    let mut c = git_in(dir);
    c.args([
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{rev}^{{commit}}"),
    ]);
    ok(c)
}

/// The configured diff-base spec for the worktree at `dir`: the per-worktree override, else the
/// project default, else [`DiffBase::Auto`]. Any resolution failure (no active project, dir not
/// a known worktree) falls back to `Auto` so the diff view always works.
fn effective_base_spec(dir: &Path) -> keitora_tasks::DiffBase {
    let Ok(cfg) = keitora_tasks::resolve_active() else {
        return keitora_tasks::DiffBase::Auto;
    };
    let name = cfg
        .worktree_entries()
        .into_iter()
        .map(|e| e.name)
        .find(|n| same_path(&cfg.worktree_path(n), dir));
    cfg.diff_base(name.as_deref())
}

/// Path equality tolerant of symlinks / `..` — canonicalize both, falling back to the raw paths
/// when canonicalization fails (e.g. a path that no longer exists).
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Resolve the configured spec to a concrete ref + comparison mode, validating that configured
/// refs exist. A missing ref yields `error: Some(..)` rather than a silent fallback.
fn resolve_spec(dir: &Path, spec: &keitora_tasks::DiffBase) -> ResolvedBase {
    use keitora_tasks::DiffBase;
    let found = |ref_name: String, compare: Compare| ResolvedBase {
        ref_name,
        compare,
        error: None,
    };
    match spec {
        DiffBase::Auto => found(resolve_auto_ref(dir), Compare::MergeBase),
        DiffBase::Ref { name } => {
            if ref_exists(dir, name) {
                found(name.clone(), Compare::Tip)
            } else {
                ResolvedBase {
                    ref_name: name.clone(),
                    compare: Compare::Tip,
                    error: Some(format!("base ref '{name}' not found")),
                }
            }
        }
        DiffBase::ForkPoint { parent } => {
            if ref_exists(dir, parent) {
                found(parent.clone(), Compare::MergeBase)
            } else {
                ResolvedBase {
                    ref_name: parent.clone(),
                    compare: Compare::MergeBase,
                    error: Some(format!("fork-point parent '{parent}' not found")),
                }
            }
        }
        DiffBase::Upstream => {
            let mut c = git_in(dir);
            c.args([
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ]);
            match run(c) {
                Ok(out) => {
                    let name = String::from_utf8_lossy(&out).trim().to_string();
                    if name.is_empty() {
                        ResolvedBase {
                            ref_name: String::new(),
                            compare: Compare::MergeBase,
                            error: Some("no upstream is configured for this branch".to_string()),
                        }
                    } else {
                        found(name, Compare::MergeBase)
                    }
                }
                Err(_) => ResolvedBase {
                    ref_name: String::new(),
                    compare: Compare::MergeBase,
                    error: Some("no upstream is configured for this branch".to_string()),
                },
            }
        }
    }
}

/// Per-worktree cache of the resolved base (TTL [`BASE_TTL`]), so resolution spawns don't recur
/// on every summary / file-diff / probe call. Invalidated eagerly by [`invalidate_base`] when
/// the base setting changes, so a picker change reflects immediately rather than after the TTL.
pub(super) fn resolved_base(dir: &Path) -> ResolvedBase {
    let cache = BASE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    if let Ok(map) = cache.lock() {
        if let Some((base, at)) = map.get(dir) {
            if now.duration_since(*at) < BASE_TTL {
                return base.clone();
            }
        }
    }
    let base = resolve_spec(dir, &effective_base_spec(dir));
    if let Ok(mut map) = cache.lock() {
        map.insert(dir.to_path_buf(), (base.clone(), now));
    }
    base
}

/// Clear the cached resolved base (and dependent oids) for `dir` so the next read re-resolves.
/// Called when the diff-base setting changes, so a picker change reflects immediately.
pub(crate) fn invalidate_base(dir: &Path) {
    if let Some(cache) = BASE_CACHE.get() {
        if let Ok(mut map) = cache.lock() {
            map.remove(dir);
        }
    }
    if let Some(cache) = OIDS_CACHE.get() {
        if let Ok(mut map) = cache.lock() {
            map.remove(dir);
        }
    }
}

/// Clear every worktree's cached base + oids. Used when a change affects all worktrees of a
/// project at once — e.g. the project-default diff base changed in settings.
pub(crate) fn invalidate_all_bases() {
    if let Some(cache) = BASE_CACHE.get() {
        if let Ok(mut map) = cache.lock() {
            map.clear();
        }
    }
    if let Some(cache) = OIDS_CACHE.get() {
        if let Ok(mut map) = cache.lock() {
            map.clear();
        }
    }
}

/// The resolved base's ref name — kept for the snapshot's `--merge-base <ref>` fallback and the
/// committed range. Prefer [`worktree_oids`] (which already encodes the comparison mode) for the
/// actual diff.
pub(super) fn base_ref(dir: &Path) -> String {
    resolved_base(dir).ref_name
}

/// `(head_oid, merge_base_oid)` for a worktree, cached with a short TTL so back-to-back
/// (cache-hit) file selections don't each spawn `git`. The TTL is brief because HEAD moves on
/// commit; the liveness probe/summary still catch a change within one poll, and the file-diff
/// cache key carries both oids so a refreshed value invalidates stale per-file diffs.
/// `(head_oid, merge_base_oid)`.
type OidPair = (String, String);

pub(super) fn worktree_oids(dir: &Path) -> OidPair {
    let cache = OIDS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    if let Ok(map) = cache.lock() {
        if let Some((oids, at)) = map.get(dir) {
            if now.duration_since(*at) < OIDS_TTL {
                return oids.clone();
            }
        }
    }
    let base = resolved_base(dir);
    // A configured ref that could not be resolved ⇒ no base oid; callers short-circuit to an
    // empty diff and the UI shows `base.error` instead of a confusing git failure.
    if base.error.is_some() {
        let oids = (String::new(), String::new());
        if let Ok(mut map) = cache.lock() {
            map.insert(dir.to_path_buf(), (oids.clone(), now));
        }
        return oids;
    }
    let (head_r, base_r) = std::thread::scope(|s| {
        let head = s.spawn(|| {
            let mut c = git_in(dir);
            c.args(["rev-parse", "HEAD"]);
            run(c)
        });
        // Tip ⇒ the ref's own oid; MergeBase ⇒ the fork point with HEAD.
        let base_oid = s.spawn(|| {
            let mut c = git_in(dir);
            match base.compare {
                Compare::Tip => c.args(["rev-parse", &base.ref_name]),
                Compare::MergeBase => c.args(["merge-base", &base.ref_name, "HEAD"]),
            };
            run(c)
        });
        (head.join(), base_oid.join())
    });
    let head = trimmed(head_r.ok().and_then(Result::ok).unwrap_or_default());
    let base_oid = trimmed(base_r.ok().and_then(Result::ok).unwrap_or_default());
    let oids = (head, base_oid);
    if let Ok(mut map) = cache.lock() {
        map.insert(dir.to_path_buf(), (oids.clone(), now));
    }
    oids
}

fn trimmed(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// `size:mtime_nanos` for a worktree file; moves on any edit (including unstaged).
fn file_sig(dir: &Path, rel: &str) -> String {
    match std::fs::metadata(dir.join(rel)) {
        Ok(m) => {
            let size = m.len();
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{size}:{mtime}")
        }
        Err(_) => "absent".to_string(),
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF).any(|b| *b == 0)
}

/// Read at most `max` bytes of a file (plus a probe byte to detect truncation), so a huge
/// untracked file is never slurped whole just to count lines or render a capped diff. Returns
/// the capped bytes and whether the file had more.
fn read_capped(path: &Path, max: usize) -> std::io::Result<(Vec<u8>, bool)> {
    use std::io::Read;
    let mut handle = std::fs::File::open(path)?.take(max as u64 + 1);
    let mut buf = Vec::new();
    handle.read_to_end(&mut buf)?;
    let truncated = buf.len() > max;
    buf.truncate(max);
    Ok((buf, truncated))
}

/// Read an untracked file's line count + binary flag for the summary (capped read).
pub(super) fn untracked_entry(dir: &Path, rel: &str) -> UntrackedEntry {
    match read_capped(&dir.join(rel), MAX_UNTRACKED_BYTES) {
        Ok((bytes, _truncated)) => {
            let binary = is_binary(&bytes);
            let added = if binary {
                0
            } else {
                String::from_utf8_lossy(&bytes).lines().count() as u32
            };
            UntrackedEntry {
                path: rel.to_string(),
                added,
                binary,
            }
        }
        Err(_) => UntrackedEntry {
            path: rel.to_string(),
            added: 0,
            binary: false,
        },
    }
}

/// Synthesize a diff for an untracked file (all additions) from its capped contents.
fn untracked_file_diff(dir: &Path, rel: &str) -> FileDiff {
    match read_capped(&dir.join(rel), MAX_DIFF_BYTES) {
        Ok((bytes, truncated)) => {
            if is_binary(&bytes) {
                return FileDiff {
                    binary: true,
                    ..FileDiff::default()
                };
            }
            let mut d = parse::untracked_file_diff(&String::from_utf8_lossy(&bytes));
            d.truncated = truncated;
            d
        }
        Err(_) => FileDiff::default(),
    }
}

/// Read a file as UTF-8 (lossy) up to `max` bytes, or `None` if it's missing, binary, or over the
/// cap (a partial highlight would mis-classify, so skip rather than truncate).
fn read_text_for_highlight(path: &Path, max: usize) -> Option<String> {
    let (bytes, truncated) = read_capped(path, max).ok()?;
    if truncated || is_binary(&bytes) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// `git show <oid>:<repo_path>` as UTF-8 (lossy), or `None` when absent/binary/over the cap.
/// `repo_path` uses forward slashes (git's own path form), as it comes from the diff.
fn show_blob_for_highlight(dir: &Path, oid: &str, repo_path: &str) -> Option<String> {
    let mut c = git_in(dir);
    c.args(["show", &format!("{oid}:{repo_path}")]);
    let out = run(c).ok()?;
    if out.len() > MAX_HIGHLIGHT_BYTES || is_binary(&out) {
        return None;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// The highlight spans for 1-based source line `n`, cloned from a highlighted side.
fn line_spans(side: &Option<Vec<LineTokens>>, n: u32) -> Option<Vec<Span>> {
    side.as_ref()
        .and_then(|lines| lines.get(n as usize - 1))
        .cloned()
}

/// Attach syntax-highlight spans to a file's diff lines. Both sides are highlighted from their
/// *full* contents (old via `git show <base_oid>:<old_path>`, new from the working tree) so a
/// construct that opens above a hunk — a block comment, a raw/template string — is still
/// classified correctly. Best-effort: an unknown language, missing content, or an over-cap/binary
/// file simply leaves lines unhighlighted (no error, no regression).
fn apply_highlight(
    diff: &mut FileDiff,
    dir: &Path,
    path: &str,
    old_path: Option<&str>,
    base_oid: &str,
) {
    if diff.binary || diff.truncated || diff.hunks.is_empty() {
        return;
    }
    let Some(lang) = highlight::lang_for_path(path) else {
        return;
    };

    // New side is read from the working tree, separately from the `git diff` that produced the
    // hunk text. If the file is edited in the tiny window between the two reads, spans can briefly
    // misalign with `text` — `splitBySpans` stays safe (no crash), and the fs-watcher recompute
    // triggered by that same edit corrects it on the next fetch.
    let new_side = read_text_for_highlight(&dir.join(path), MAX_HIGHLIGHT_BYTES)
        .and_then(|src| highlight::highlight(lang, &src));
    let old_side = if base_oid.is_empty() {
        None
    } else {
        show_blob_for_highlight(dir, base_oid, old_path.unwrap_or(path))
            .and_then(|src| highlight::highlight(lang, &src))
    };
    if new_side.is_none() && old_side.is_none() {
        return;
    }

    for hunk in &mut diff.hunks {
        for line in &mut hunk.lines {
            line.spans = match line.kind {
                DiffLineKind::Add => line.new_no.and_then(|n| line_spans(&new_side, n)),
                DiffLineKind::Del => line.old_no.and_then(|n| line_spans(&old_side, n)),
                // Context lines exist on both sides; prefer the new side, fall back to the old.
                DiffLineKind::Context => line
                    .new_no
                    .and_then(|n| line_spans(&new_side, n))
                    .or_else(|| line.old_no.and_then(|n| line_spans(&old_side, n))),
            };
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_file_diff(
    worktree: &str,
    dir: &Path,
    path: &str,
    old_path: Option<&str>,
    ignore_ws: bool,
    untracked: bool,
    cache: &DiffCache,
    jobs: &DiffJobs,
) -> Result<FileDiff, String> {
    // HEAD + merge-base come from a short-TTL cache so rapid file-to-file clicks (all cache
    // hits) don't each spawn `git rev-parse`/`merge-base`. base_oid is part of the key so an
    // advancing `main` invalidates the per-file diff, matching the summary fingerprint.
    let (head_oid, base_oid) = worktree_oids(dir);

    let key = FileKey {
        worktree: worktree.to_string(),
        path: path.to_string(),
        file_sig: file_sig(dir, path),
        head_oid,
        base_oid,
        ignore_ws,
    };
    if let Some(hit) = cache.get_file(&key) {
        return Ok(hit);
    }

    // An untracked file has no git-tracked diff — synthesize the all-additions view directly
    // and skip the subprocess entirely (the caller already classified it from the summary).
    if untracked {
        let mut diff = untracked_file_diff(dir, path);
        // All-additions: only the working-tree (new) side is highlighted; no base blob.
        apply_highlight(&mut diff, dir, path, None, "");
        cache.put_file(key, diff.clone());
        return Ok(diff);
    }

    // A configured base ref that couldn't be resolved ⇒ no diff (the base-info command carries
    // the error for the UI). base_oid is empty in that case; distinguish it from the genuine
    // "no merge-base" (unrelated history) case below.
    if resolved_base(dir).error.is_some() {
        let diff = FileDiff::default();
        cache.put_file(key, diff.clone());
        return Ok(diff);
    }

    let mut cmd = git_in(dir);
    cmd.arg("diff");
    if key.base_oid.is_empty() {
        // No base oid despite no error ⇒ unrelated history; let git recompute the merge-base.
        cmd.args(["--merge-base", &base_ref(dir)]);
    } else {
        // Compare against the resolved oid (merge-base or tip) — consistent with the summary,
        // which also diffs against the cached oid.
        cmd.arg(&key.base_oid);
    }
    cmd.arg("-M");
    if ignore_ws {
        cmd.arg("-w");
    }
    cmd.arg("--").arg(path);
    if let Some(op) = old_path {
        cmd.arg(op);
    }

    // Supersede per worktree: navigating to a new file cancels the previous file's diff.
    let (stdout, capture_truncated) = jobs::run_cancellable(jobs, worktree, cmd)?;

    let mut diff = if stdout.is_empty() {
        // Empty `git diff` output for a *tracked* file: a reverted or whitespace-only (`-w`)
        // change. The caller told us it is not untracked, so this is "no textual change",
        // not a whole-file addition. (The `is_untracked` probe is gone — a stale summary that
        // mislabels a brand-new file as tracked simply shows an empty diff until the next
        // refresh, far cheaper than a pathspec `status` on every empty result.)
        FileDiff::default()
    } else {
        let parse_truncated = stdout.len() > MAX_DIFF_BYTES;
        let end = stdout.len().min(MAX_DIFF_BYTES);
        let text = String::from_utf8_lossy(&stdout[..end]);
        let mut d = parse::parse_unified_diff(&text);
        d.truncated = parse_truncated || capture_truncated;
        d
    };
    diff.truncated |= capture_truncated;

    apply_highlight(&mut diff, dir, path, old_path, &key.base_oid);

    cache.put_file(key, diff.clone());
    Ok(diff)
}

/// Changed-file summary for a worktree (names + ± counts). One working-tree scan via the
/// [`snapshot::Snapshotter`], reusing the cached rows when the watcher generation is
/// unchanged, so an idle open view costs nothing.
///
/// `async fn` + `spawn_blocking`: the git subprocesses are blocking, so they run on the
/// blocking pool rather than occupying an async runtime worker. The `Clone` state handles are
/// moved into the closure (a borrowed `State` can't cross the `'static` boundary).
#[tauri::command]
pub async fn worktree_diff_summary(
    worktree: String,
    ignore_ws: bool,
    snap: State<'_, snapshot::Snapshotter>,
    watchers: State<'_, watch::DiffWatchers>,
) -> Result<Vec<DiffFileSummary>, String> {
    let dir = worktree_dir(&worktree)?;
    let snap = snap.inner().clone();
    let gen = watchers.generation(&dir);
    tauri::async_runtime::spawn_blocking(move || snap.summary(&dir, ignore_ws, gen))
        .await
        .map_err(|e| format!("diff summary task panicked: {e}"))?
}

/// Committed changes only (`merge-base..HEAD`) — no working-tree scan, so the list paints
/// instantly on open. The full summary (with uncommitted + untracked files) follows.
#[tauri::command]
pub async fn worktree_diff_committed(
    worktree: String,
    ignore_ws: bool,
) -> Result<Vec<DiffFileSummary>, String> {
    let dir = worktree_dir(&worktree)?;
    tauri::async_runtime::spawn_blocking(move || snapshot::committed_rows(&dir, ignore_ws))
        .await
        .map_err(|e| format!("diff committed task panicked: {e}"))?
}

/// Structured diff for one file, computed lazily and cancellably. `untracked` lets the
/// caller (which already classified the row) skip the probe that distinguishes an untracked
/// file from a no-op tracked diff.
#[tauri::command]
pub async fn worktree_file_diff(
    worktree: String,
    path: String,
    old_path: Option<String>,
    ignore_ws: bool,
    untracked: bool,
    cache: State<'_, DiffCache>,
    jobs: State<'_, DiffJobs>,
) -> Result<FileDiff, String> {
    let dir = worktree_dir(&worktree)?;
    let cache = cache.inner().clone();
    let jobs = jobs.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        compute_file_diff(
            &worktree,
            &dir,
            &path,
            old_path.as_deref(),
            ignore_ws,
            untracked,
            &cache,
            &jobs,
        )
    })
    .await
    .map_err(|e| format!("diff file task panicked: {e}"))?
}

/// Scan-free liveness token for the open view: `"{effective_gen}:{head}:{merge_base}"`. The
/// generation moves on a relevant filesystem event; the oids catch HEAD/`main` moves even in
/// the unwatched fallback. The frontend refetches the summary only when this changes.
///
/// The gen is run through [`snapshot::Snapshotter::effective_gen`], not the raw watcher gen,
/// so that a change arriving during a snapshot cooldown isn't reported as already-loaded: the
/// token stays pinned to the cached snapshot's gen until the cooldown lifts, then advances so
/// the frontend re-requests and sees the change.
#[tauri::command]
pub async fn worktree_diff_revision(
    worktree: String,
    snap: State<'_, snapshot::Snapshotter>,
    watchers: State<'_, watch::DiffWatchers>,
) -> Result<String, String> {
    let dir = worktree_dir(&worktree)?;
    let gen = snap.effective_gen(&dir, watchers.generation(&dir));
    tauri::async_runtime::spawn_blocking(move || {
        let (head, mb) = worktree_oids(&dir);
        Ok(format!("{gen}:{head}:{mb}"))
    })
    .await
    .map_err(|e| format!("diff revision task panicked: {e}"))?
}

/// The effective diff base for a worktree: the resolved ref name, how it's compared, and an
/// error when a configured ref couldn't be resolved. Drives the diff header label + error banner.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffBaseInfo {
    pub ref_name: String,
    pub compare: Compare,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Report the resolved diff base for a worktree (for the header label + error banner). Cheap:
/// served from the resolved-base cache, resolving 1–2 `git` spawns only on a cold/expired entry.
#[tauri::command]
pub async fn worktree_diff_base(worktree: String) -> Result<DiffBaseInfo, String> {
    let dir = worktree_dir(&worktree)?;
    tauri::async_runtime::spawn_blocking(move || {
        let rb = resolved_base(&dir);
        Ok(DiffBaseInfo {
            ref_name: rb.ref_name,
            compare: rb.compare,
            error: rb.error,
        })
    })
    .await
    .map_err(|e| format!("diff base task panicked: {e}"))?
}

/// Start (ref++) the filesystem watcher for an open diff view on `worktree`.
#[tauri::command]
pub async fn worktree_diff_watch_start(
    worktree: String,
    watchers: State<'_, watch::DiffWatchers>,
) -> Result<(), String> {
    let dir = worktree_dir(&worktree)?;
    watchers.acquire(&dir);
    Ok(())
}

/// Stop (ref--) the filesystem watcher when a diff view closes.
#[tauri::command]
pub async fn worktree_diff_watch_stop(
    worktree: String,
    watchers: State<'_, watch::DiffWatchers>,
) -> Result<(), String> {
    let dir = worktree_dir(&worktree)?;
    watchers.release(&dir);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_capped_truncates_and_flags() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("keitora-diff-capped-{}.txt", std::process::id()));
        std::fs::write(&path, b"abcdefghij").unwrap(); // 10 bytes

        let (small, truncated) = read_capped(&path, 4).unwrap();
        assert_eq!(small, b"abcd");
        assert!(truncated);

        let (whole, truncated) = read_capped(&path, 10).unwrap();
        assert_eq!(whole, b"abcdefghij");
        assert!(!truncated, "exact-fit read must not report truncation");

        let (over, truncated) = read_capped(&path, 100).unwrap();
        assert_eq!(over.len(), 10);
        assert!(!truncated);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_capped_missing_file_errors() {
        let missing = std::env::temp_dir().join("keitora-diff-does-not-exist-xyz.txt");
        assert!(read_capped(&missing, 16).is_err());
    }

    use keitora_tasks::DiffBase;
    use std::process::Command as Cmd;

    /// Run a git command in `dir`, asserting success (hermetic identity + default branch `main`).
    fn git(dir: &Path, args: &[&str]) {
        let out = Cmd::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with `main` (one commit) and `feature` (branched from main, one extra commit).
    fn repo_with_branch() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "base"]);
        git(dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        git(dir, &["commit", "-qam", "feature change"]);
        tmp
    }

    #[test]
    fn ref_exists_distinguishes_known_and_unknown_refs() {
        let tmp = repo_with_branch();
        let dir = tmp.path();
        assert!(ref_exists(dir, "main"));
        assert!(ref_exists(dir, "feature"));
        assert!(!ref_exists(dir, "no-such-ref"));
    }

    #[test]
    fn resolve_spec_auto_uses_merge_base_against_local_main() {
        let tmp = repo_with_branch();
        let rb = resolve_spec(tmp.path(), &DiffBase::Auto);
        assert_eq!(rb.compare, Compare::MergeBase);
        assert_eq!(rb.error, None);
        // No remote ⇒ origin/main absent ⇒ local main.
        assert_eq!(rb.ref_name, "main");
    }

    #[test]
    fn resolve_spec_ref_compares_against_tip() {
        let tmp = repo_with_branch();
        let rb = resolve_spec(
            tmp.path(),
            &DiffBase::Ref {
                name: "main".into(),
            },
        );
        assert_eq!(rb.compare, Compare::Tip);
        assert_eq!(rb.ref_name, "main");
        assert_eq!(rb.error, None);
    }

    #[test]
    fn resolve_spec_missing_ref_reports_error_not_fallback() {
        let tmp = repo_with_branch();
        let rb = resolve_spec(
            tmp.path(),
            &DiffBase::Ref {
                name: "origin/does-not-exist".into(),
            },
        );
        assert!(rb.error.is_some(), "missing ref must surface an error");
        assert_eq!(rb.compare, Compare::Tip);

        let fp = resolve_spec(
            tmp.path(),
            &DiffBase::ForkPoint {
                parent: "nope".into(),
            },
        );
        assert!(fp.error.is_some());
        assert_eq!(fp.compare, Compare::MergeBase);
    }

    #[test]
    fn resolve_spec_fork_point_uses_merge_base() {
        let tmp = repo_with_branch();
        let rb = resolve_spec(
            tmp.path(),
            &DiffBase::ForkPoint {
                parent: "main".into(),
            },
        );
        assert_eq!(rb.compare, Compare::MergeBase);
        assert_eq!(rb.ref_name, "main");
        assert_eq!(rb.error, None);
    }

    #[test]
    fn resolve_spec_upstream_errors_when_none_configured() {
        let tmp = repo_with_branch();
        // `feature` has no upstream configured.
        let rb = resolve_spec(tmp.path(), &DiffBase::Upstream);
        assert!(rb.error.is_some(), "no upstream ⇒ error");
    }

    #[test]
    fn worktree_oids_tip_and_merge_base_differ() {
        // On a branch ahead of main, the merge-base (fork point) and main's tip are the same
        // commit here, but tip-of-feature vs merge-base-with-feature differ — verify the oid
        // resolution path picks the right commit per compare mode.
        let tmp = repo_with_branch();
        let dir = tmp.path();
        // Advance main beyond the fork point so merge-base(main,HEAD) != tip(main).
        git(dir, &["checkout", "-q", "main"]);
        std::fs::write(dir.join("b.txt"), "x\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "main advances"]);
        git(dir, &["checkout", "-q", "feature"]);

        let tip = resolve_oid(
            dir,
            &resolve_spec(
                dir,
                &DiffBase::Ref {
                    name: "main".into(),
                },
            ),
        );
        let mb = resolve_oid(
            dir,
            &resolve_spec(
                dir,
                &DiffBase::ForkPoint {
                    parent: "main".into(),
                },
            ),
        );
        assert_ne!(
            tip, mb,
            "tip of advanced main must differ from the fork point"
        );
        assert!(!tip.is_empty() && !mb.is_empty());
    }

    /// Test helper mirroring [`worktree_oids`]'s base-oid resolution for a given [`ResolvedBase`]
    /// (without the config lookup, so it is hermetic).
    fn resolve_oid(dir: &Path, base: &ResolvedBase) -> String {
        let mut c = git_in(dir);
        match base.compare {
            Compare::Tip => c.args(["rev-parse", &base.ref_name]),
            Compare::MergeBase => c.args(["merge-base", &base.ref_name, "HEAD"]),
        };
        trimmed(run(c).unwrap_or_default())
    }

    #[test]
    fn apply_highlight_classifies_added_and_context_lines() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        // v1: a block comment opening on line 1 and continuing on line 2, then code.
        std::fs::write(dir.join("f.rs"), "/* header\n   more */\nfn a() {}\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "v1"]);
        let base_oid = {
            let mut c = git_in(dir);
            c.args(["rev-parse", "HEAD"]);
            trimmed(run(c).unwrap())
        };
        // Working change: append a new line.
        std::fs::write(
            dir.join("f.rs"),
            "/* header\n   more */\nfn a() {}\nconst B: i32 = 2;\n",
        )
        .unwrap();

        let mut diff = {
            let mut c = git_in(dir);
            c.args(["diff", &base_oid, "--", "f.rs"]);
            parse::parse_unified_diff(&String::from_utf8_lossy(&run(c).unwrap()))
        };
        apply_highlight(&mut diff, dir, "f.rs", None, &base_oid);

        let lines: Vec<_> = diff.hunks.iter().flat_map(|h| &h.lines).collect();
        // The added `const …` line (new side) is highlighted; its first span is `keyword`.
        let added = lines
            .iter()
            .find(|l| l.kind == DiffLineKind::Add && l.text.contains("const"))
            .expect("added const line present");
        let spans = added.spans.as_ref().expect("added line highlighted");
        assert_eq!(
            spans
                .first()
                .and_then(|s| s.cls)
                .map(|i| highlight::CLASSES[i as usize]),
            Some("keyword"),
            "`const` classifies as a keyword"
        );
        // A context line that sits inside the block comment is classed as a comment, proving the
        // whole-file highlight (the comment opened on line 1, above any hunk context shown).
        if let Some(ctx) = lines
            .iter()
            .find(|l| l.kind == DiffLineKind::Context && l.text.contains("more"))
        {
            let spans = ctx.spans.as_ref().expect("context line highlighted");
            assert_eq!(
                spans
                    .first()
                    .and_then(|s| s.cls)
                    .map(|i| highlight::CLASSES[i as usize]),
                Some("comment"),
            );
        }
    }

    #[test]
    fn apply_highlight_skips_unknown_language_binary_and_truncated() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();

        // Unknown extension → no spans.
        let mut md = parse::untracked_file_diff("# hello\nworld\n");
        apply_highlight(&mut md, dir, "notes.md", None, "");
        assert!(md
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .all(|l| l.spans.is_none()));

        // Truncated → skipped even for a known language.
        let mut trunc = parse::untracked_file_diff("fn a() {}\n");
        trunc.truncated = true;
        apply_highlight(&mut trunc, dir, "a.rs", None, "");
        assert!(trunc
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .all(|l| l.spans.is_none()));

        // Binary → skipped.
        let mut bin = parse::untracked_file_diff("fn a() {}\n");
        bin.binary = true;
        apply_highlight(&mut bin, dir, "a.rs", None, "");
        assert!(bin
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .all(|l| l.spans.is_none()));
    }
}
