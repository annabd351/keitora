//! Filesystem-watcher-backed change signal for the diff view.
//!
//! While a diff view is open on a worktree, a recursive `notify` watcher increments a
//! per-worktree **generation** counter whenever something relevant changes on disk. The
//! revision probe reads that counter instead of running a full `git status`, so an idle open
//! view costs nothing, and [`super::snapshot::Snapshotter`] reuses its cached rows whenever
//! the generation it computed for is still current.
//!
//! Watchers are refcounted per worktree (a view may be opened, closed and reopened; the app
//! keys the component on the worktree): the first `acquire` spawns the watcher, the last
//! `release` stops it. The registry mirrors the N-sender stop pattern in
//! [`crate::bg_notifier`], specialised to one keyed, refcounted entry per worktree.
//!
//! If a watcher cannot be created (unsupported FS, permissions), the worktree is tracked as
//! *unwatched*: [`DiffWatchers::generation`] then returns a fresh value every call, so the
//! caller always recomputes — degrading to the old scan-on-probe behaviour, but still
//! single-flighted and cooldowned by the snapshotter, never a pile-up.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use keitora_tasks::git::git_in;

use super::run;

/// Managed-state registry of per-worktree diff watchers. Clone shares it.
#[derive(Clone, Default)]
pub struct DiffWatchers {
    inner: Arc<Mutex<HashMap<PathBuf, Entry>>>,
}

struct Entry {
    refs: usize,
    gen: Arc<AtomicU64>,
    /// Liveness handle: held so the watcher thread keeps running, and **dropped** (when the
    /// entry is removed at zero refs) to disconnect its stop receiver and end it. Never read
    /// directly — its value is its existence. `None` when the worktree is unwatched (watcher
    /// creation failed) — the fallback path.
    #[allow(dead_code)]
    stop: Option<mpsc::SyncSender<()>>,
    watched: bool,
}

impl DiffWatchers {
    /// Register interest in `dir`. On the first reference, spawn the watcher thread.
    ///
    /// The git-dir resolution (`git rev-parse`) and watcher spawn run **outside** the registry
    /// lock, so opening a diff view never stalls the 2 s `generation()` probes of other open
    /// worktrees behind a subprocess. A concurrent `acquire` for the same new worktree is
    /// resolved on re-check: the loser drops its freshly-spawned watcher (whose thread exits
    /// when its stop sender is dropped).
    pub fn acquire(&self, dir: &Path) {
        {
            let mut map = self.inner.lock().expect("diff watchers mutex");
            if let Some(entry) = map.get_mut(dir) {
                entry.refs += 1;
                return;
            }
        }
        // First reference: build the watcher without holding the lock.
        let gen = Arc::new(AtomicU64::new(1));
        let git_dir = resolve_git_dir(dir);
        let (stop, watched) = match spawn_watcher(dir.to_path_buf(), git_dir, gen.clone()) {
            Some(stop) => (Some(stop), true),
            None => (None, false),
        };
        let mut map = self.inner.lock().expect("diff watchers mutex");
        match map.get_mut(dir) {
            // Another thread won the race while we spawned — adopt its entry, drop ours.
            Some(entry) => {
                entry.refs += 1;
                drop(stop); // ends our redundant watcher thread
            }
            None => {
                map.insert(
                    dir.to_path_buf(),
                    Entry {
                        refs: 1,
                        gen,
                        stop,
                        watched,
                    },
                );
            }
        }
    }

    /// Drop interest in `dir`. On the last reference, stop and remove the watcher.
    pub fn release(&self, dir: &Path) {
        let mut map = self.inner.lock().expect("diff watchers mutex");
        if let Some(entry) = map.get_mut(dir) {
            entry.refs = entry.refs.saturating_sub(1);
            if entry.refs == 0 {
                map.remove(dir); // dropping `stop` disconnects the thread's receiver
            }
        }
    }

    /// Current change generation for `dir`.
    ///
    /// - Watched: the live counter (stable until a relevant filesystem event bumps it).
    /// - Unwatched or not-open: a fresh value every call, so the caller always recomputes.
    pub fn generation(&self, dir: &Path) -> u64 {
        static FALLBACK: AtomicU64 = AtomicU64::new(1);
        let map = self.inner.lock().expect("diff watchers mutex");
        match map.get(dir) {
            Some(e) if e.watched => e.gen.load(Ordering::SeqCst),
            // Unwatched entry: bump its own counter so successive probes differ.
            Some(e) => e.gen.fetch_add(1, Ordering::SeqCst) + 1,
            None => FALLBACK.fetch_add(1, Ordering::SeqCst) + 1,
        }
    }
}

/// Resolve a worktree's git directory (`git rev-parse --git-dir`), absolute. `None` if it
/// can't be resolved (the watcher then simply doesn't special-case git paths).
fn resolve_git_dir(dir: &Path) -> Option<PathBuf> {
    let mut c = git_in(dir);
    c.args(["rev-parse", "--absolute-git-dir"]);
    let out = run(c).ok()?;
    let s = String::from_utf8_lossy(&out).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Spawn the watcher thread. Returns its stop sender, or `None` if the recursive watch could
/// not be established (caller falls back to unwatched mode).
fn spawn_watcher(
    dir: PathBuf,
    git_dir: Option<PathBuf>,
    gen: Arc<AtomicU64>,
) -> Option<mpsc::SyncSender<()>> {
    use notify::{RecursiveMode, Watcher};

    let (evt_tx, evt_rx) = mpsc::channel::<bool>();
    let filter_git = git_dir.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        // A read error (e.g. buffer overflow / rescan) is treated as "changed".
        let relevant = match res {
            Ok(ev) => is_relevant(&ev.paths, filter_git.as_deref()),
            Err(_) => true,
        };
        let _ = evt_tx.send(relevant);
    })
    .ok()?;

    watcher.watch(&dir, RecursiveMode::Recursive).ok()?;
    // Also watch the gitdir non-recursively so HEAD/index/refs moves (commit, stage, branch
    // switch) bump the generation even when it lives outside the worktree (linked worktree).
    if let Some(gd) = &git_dir {
        if gd != &dir {
            let _ = watcher.watch(gd, RecursiveMode::NonRecursive);
        }
    }

    let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(0);
    thread::spawn(move || {
        // Keep the watcher alive for the thread's lifetime.
        let _watcher = watcher;
        loop {
            match stop_rx.try_recv() {
                Ok(()) | Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => {}
            }
            match evt_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(true) => {
                    gen.fetch_add(1, Ordering::SeqCst);
                }
                Ok(false) => {} // irrelevant event (git internals) — ignore
                Err(mpsc::RecvTimeoutError::Timeout) => {} // re-check stop
                Err(mpsc::RecvTimeoutError::Disconnected) => return, // watcher dropped
            }
        }
    });
    Some(stop_tx)
}

/// Decide whether a batch of changed paths is relevant to a worktree's diff.
///
/// A path outside the gitdir is always relevant (a source edit). A path **inside** the
/// gitdir is relevant only when it reflects real repository state — `HEAD`, `index`, or
/// anything under `refs/` — so routine git churn (`objects/`, lock files, `logs/`,
/// `COMMIT_EDITMSG`) doesn't trigger a rescan. With no known gitdir, everything is relevant.
fn is_relevant(paths: &[PathBuf], git_dir: Option<&Path>) -> bool {
    let Some(git_dir) = git_dir else {
        return !paths.is_empty();
    };
    paths.iter().any(|p| match p.strip_prefix(git_dir) {
        // Outside the gitdir → a worktree file changed.
        Err(_) => true,
        // Inside the gitdir → only HEAD / index / refs/* count as real state changes.
        Ok(rel) => {
            let first = rel.components().next().and_then(|c| c.as_os_str().to_str());
            matches!(first, Some("HEAD") | Some("index") | Some("refs"))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_file_is_relevant() {
        let git = PathBuf::from("/repo/.git");
        assert!(is_relevant(
            &[PathBuf::from("/repo/src/main.rs")],
            Some(&git)
        ));
    }

    #[test]
    fn git_object_churn_is_irrelevant() {
        let git = PathBuf::from("/repo/.git");
        assert!(!is_relevant(
            &[PathBuf::from("/repo/.git/objects/ab/cdef")],
            Some(&git)
        ));
        assert!(!is_relevant(
            &[PathBuf::from("/repo/.git/COMMIT_EDITMSG")],
            Some(&git)
        ));
        assert!(!is_relevant(
            &[PathBuf::from("/repo/.git/index.lock")],
            Some(&git)
        ));
    }

    #[test]
    fn head_index_refs_are_relevant() {
        let git = PathBuf::from("/repo/.git");
        assert!(is_relevant(&[PathBuf::from("/repo/.git/HEAD")], Some(&git)));
        assert!(is_relevant(
            &[PathBuf::from("/repo/.git/index")],
            Some(&git)
        ));
        assert!(is_relevant(
            &[PathBuf::from("/repo/.git/refs/heads/main")],
            Some(&git)
        ));
    }

    #[test]
    fn mixed_batch_is_relevant_if_any_source_path() {
        let git = PathBuf::from("/repo/.git");
        assert!(is_relevant(
            &[
                PathBuf::from("/repo/.git/objects/x"),
                PathBuf::from("/repo/src/lib.rs"),
            ],
            Some(&git)
        ));
    }

    #[test]
    fn no_gitdir_treats_any_path_as_relevant() {
        assert!(is_relevant(&[PathBuf::from("/repo/.git/objects/x")], None));
        assert!(!is_relevant(&[], None));
    }

    #[test]
    fn refcount_spawns_once_and_removes_at_zero() {
        let w = DiffWatchers::default();
        let dir = std::env::temp_dir();
        w.acquire(&dir);
        w.acquire(&dir);
        {
            let map = w.inner.lock().unwrap();
            assert_eq!(map.get(&dir).map(|e| e.refs), Some(2));
        }
        w.release(&dir);
        {
            let map = w.inner.lock().unwrap();
            assert_eq!(map.get(&dir).map(|e| e.refs), Some(1));
        }
        w.release(&dir);
        {
            let map = w.inner.lock().unwrap();
            assert!(map.get(&dir).is_none(), "entry removed at zero refs");
        }
    }

    #[test]
    fn generation_without_entry_is_monotonic() {
        let w = DiffWatchers::default();
        let dir = std::env::temp_dir().join("keitora-diff-watch-nonexistent-xyz");
        let a = w.generation(&dir);
        let b = w.generation(&dir);
        assert!(
            b > a,
            "no-entry probe must always advance so callers recompute"
        );
    }
}
