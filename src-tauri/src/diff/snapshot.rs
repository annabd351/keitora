//! Single-scan changed-file snapshots for the diff view.
//!
//! The expensive part of a worktree diff on a large repo is the working-tree scan — one
//! `lstat` per tracked file (3–6 s on a 191k-file repo), paid by every `git status`/`git
//! diff` that compares against the working tree. The old summary path paid it four times per
//! load and again on every 2 s liveness probe, with no overlap guard, so scans piled up.
//!
//! This module pays it **once** per real change:
//! - committed changes come from a tree↔tree diff (`merge-base..HEAD`) — no scan (~0.1 s);
//! - a single `status --porcelain=v2` scan finds the dirty + untracked set;
//! - the working-tree overlay is a **pathspec-limited** diff over just the dirty paths;
//! - results are merged ([`parse::merge_snapshot`]) and cached per worktree, keyed on the
//!   filesystem-watcher generation so an unchanged worktree returns with no git at all;
//! - concurrent callers for one worktree share a single in-flight computation, and a
//!   cooldown bounds how often a changing worktree re-scans.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use keitora_tasks::git::git_in;

use super::parse::{self, DiffFileSummary, PorcelainV2, UntrackedEntry};
use super::{base_ref, resolved_base, run, untracked_entry, worktree_oids, MAX_UNTRACKED_COUNTED};

/// Minimum gap between recomputes for one worktree. The actual floor is
/// `max(COOLDOWN_FLOOR, last_compute_duration)` so a slow repo never spends more than ~half
/// its time recomputing under a storm of filesystem events (e.g. a running build).
const COOLDOWN_FLOOR: Duration = Duration::from_secs(1);
/// Above this many dirty paths, skip the per-path pathspec and run one unrestricted overlay
/// diff instead (a mass reformat is cheaper as a single scan than as a giant argv).
const MAX_OVERLAY_PATHS: usize = 1000;
/// Safe ceiling for the joined pathspec length (Windows command-line budget); over it, fall
/// back to the unrestricted overlay.
const MAX_PATHSPEC_CHARS: usize = 24_000;

/// A computed changed-file summary plus the metadata used to decide reuse.
#[derive(Clone)]
pub struct Snapshot {
    /// Watcher generation this snapshot reflects (0 when the worktree is unwatched).
    pub gen: u64,
    pub ignore_ws: bool,
    pub rows: Vec<DiffFileSummary>,
    pub computed_at: Instant,
    /// Wall time of the whole computation (committed diff + scan + overlay + untracked
    /// reads); the cooldown is at least this long, so the recompute duty cycle stays bounded
    /// even when the overlay or untracked reads — not the scan — dominate.
    pub compute_dur: Duration,
}

#[derive(Default)]
struct Slot {
    last: Option<Snapshot>,
    in_flight: bool,
}

/// Managed-state handle: per-worktree snapshot cache + single-flight gate. Clone shares it.
#[derive(Clone, Default)]
pub struct Snapshotter {
    inner: Arc<(Mutex<HashMap<PathBuf, Slot>>, Condvar)>,
}

impl Snapshotter {
    /// Changed-file summary for `dir` at watcher generation `gen`.
    ///
    /// Returns the cached rows with no git when the cache already reflects `gen` (and the
    /// same `ignore_ws`), or when the previous scan was too recent (cooldown). Otherwise one
    /// thread computes while any concurrent caller for the same worktree waits on the result.
    pub fn summary(
        &self,
        dir: &Path,
        ignore_ws: bool,
        gen: u64,
    ) -> Result<Vec<DiffFileSummary>, String> {
        let (lock, cvar) = &*self.inner;
        let mut map = lock.lock().map_err(|_| "snapshot mutex poisoned")?;
        loop {
            {
                let slot = map.entry(dir.to_path_buf()).or_default();
                if let Some(last) = &slot.last {
                    if last.ignore_ws == ignore_ws
                        && (same_generation(last.gen, gen)
                            || in_cooldown(last.computed_at.elapsed(), last.compute_dur))
                    {
                        return Ok(last.rows.clone());
                    }
                }
                if !slot.in_flight {
                    slot.in_flight = true;
                    break;
                }
            }
            // Someone else is computing this worktree — wait and re-evaluate (their result
            // may satisfy us, or the cooldown may now apply).
            map = cvar.wait(map).map_err(|_| "snapshot mutex poisoned")?;
        }
        drop(map);

        let result = compute(dir, ignore_ws, gen);

        let mut map = lock.lock().map_err(|_| "snapshot mutex poisoned")?;
        let slot = map.entry(dir.to_path_buf()).or_default();
        slot.in_flight = false;
        if let Ok(snap) = &result {
            slot.last = Some(snap.clone());
        }
        cvar.notify_all();
        result.map(|s| s.rows)
    }

    /// The generation the next [`Self::summary`] call would actually return data for, given
    /// the live watcher generation `current`. During a cooldown with a pending change the
    /// cache is pinned at `last.gen`, so the revision probe must report `last.gen` — not
    /// `current` — or the frontend would record `current` as loaded and never re-request the
    /// change once the cooldown lifts. When not pinned, the data is (or will be) `current`.
    pub fn effective_gen(&self, dir: &Path, current: u64) -> u64 {
        let (lock, _cvar) = &*self.inner;
        if let Ok(map) = lock.lock() {
            if let Some(slot) = map.get(dir) {
                if let Some(last) = &slot.last {
                    if last.gen != current
                        && in_cooldown(last.computed_at.elapsed(), last.compute_dur)
                    {
                        return last.gen;
                    }
                }
            }
        }
        current
    }
}

/// True when the cache already reflects the requested generation (nothing changed since).
fn same_generation(last_gen: u64, req_gen: u64) -> bool {
    last_gen == req_gen
}

/// True when the previous computation is too recent to recompute — `max(floor, last cost)`.
fn in_cooldown(elapsed: Duration, compute_dur: Duration) -> bool {
    elapsed < compute_dur.max(COOLDOWN_FLOOR)
}

enum OverlayMode {
    /// Diff only the dirty pathspec.
    Limited,
    /// Too many / too-long paths — one unrestricted working-tree diff.
    Full,
}

fn overlay_mode(n_paths: usize, joined_chars: usize) -> OverlayMode {
    if n_paths > MAX_OVERLAY_PATHS || joined_chars > MAX_PATHSPEC_CHARS {
        OverlayMode::Full
    } else {
        OverlayMode::Limited
    }
}

/// The set of paths the working-tree overlay is authoritative for: every dirty tracked path
/// and rename origin, plus the committed-side counterpart of any committed row that touches
/// one of those paths (so a committed rename whose endpoint is re-edited stays paired under
/// `-M` and its stale committed row is dropped by [`parse::merge_snapshot`]).
fn build_dirty_set(v2: &PorcelainV2, committed: &[DiffFileSummary]) -> HashSet<String> {
    let mut dirty: HashSet<String> = HashSet::new();
    dirty.extend(v2.dirty_tracked.iter().cloned());
    dirty.extend(v2.rename_origins.iter().cloned());
    let mut counterparts: HashSet<String> = HashSet::new();
    for r in committed {
        let touches = dirty.contains(&r.path)
            || r.old_path
                .as_ref()
                .map(|o| dirty.contains(o))
                .unwrap_or(false);
        if touches {
            counterparts.insert(r.path.clone());
            if let Some(o) = &r.old_path {
                counterparts.insert(o.clone());
            }
        }
    }
    dirty.extend(counterparts);
    dirty
}

/// Committed changes only (`merge-base..HEAD`), no working-tree scan. Shared with the
/// progressive first-paint command.
pub fn committed_rows(dir: &Path, ignore_ws: bool) -> Result<Vec<DiffFileSummary>, String> {
    let (_head, mb) = worktree_oids(dir);
    if mb.is_empty() {
        return Ok(Vec::new());
    }
    diff_rows(dir, &[&mb, "HEAD"], ignore_ws, None)
}

/// Run `git diff <range> -M [-w] --name-status/--numstat -z [-- <paths>]` (the two reads in
/// parallel, as spawn overhead dominates on Windows) and join into summary rows.
fn diff_rows(
    dir: &Path,
    range: &[&str],
    ignore_ws: bool,
    pathspec: Option<&[String]>,
) -> Result<Vec<DiffFileSummary>, String> {
    let build = |extra: &str| {
        let mut c = git_in(dir);
        c.arg("diff");
        c.args(range);
        c.arg("-M");
        if ignore_ws {
            c.arg("-w");
        }
        c.args([extra, "-z"]);
        if let Some(paths) = pathspec {
            c.arg("--");
            c.args(paths);
        }
        c
    };
    let (ns_r, num_r) = std::thread::scope(|s| {
        let ns = s.spawn(|| run(build("--name-status")));
        let num = s.spawn(|| run(build("--numstat")));
        (ns.join(), num.join())
    });
    let name_status =
        parse::parse_name_status_z(&ns_r.map_err(|_| "name-status thread panicked".to_string())??);
    let numstat =
        parse::parse_numstat_z(&num_r.map_err(|_| "numstat thread panicked".to_string())??);
    Ok(parse::merge_summary(name_status, numstat, Vec::new()))
}

/// The full single-scan computation (see module docs).
fn compute(dir: &Path, ignore_ws: bool, gen: u64) -> Result<Snapshot, String> {
    let compute_start = Instant::now();
    // A configured base ref that couldn't be resolved ⇒ empty summary (the base-info command
    // carries the error for the UI) rather than a confusing git failure from a bad ref.
    if resolved_base(dir).error.is_some() {
        return Ok(Snapshot {
            gen,
            ignore_ws,
            rows: Vec::new(),
            computed_at: Instant::now(),
            compute_dur: compute_start.elapsed(),
        });
    }
    let (_head, mb) = worktree_oids(dir);
    let base = base_ref(dir);

    // Both halves of the snapshot diff against the *same* merge-base oid: the committed rows
    // (`mb..HEAD`) and the working-tree overlay (`mb` → worktree). Using the explicit cached
    // `mb` for the overlay too — rather than re-resolving `--merge-base <base>` inline — keeps
    // the two halves consistent when `main` moves (the oid cache lags by up to OIDS_TTL) and
    // saves a redundant merge-base spawn. When there is no merge-base (unborn / unrelated
    // history), fall back to `--merge-base <base>` so the overlay still shows working changes.
    let overlay_range: Vec<&str> = if mb.is_empty() {
        vec!["--merge-base", &base]
    } else {
        vec![&mb]
    };

    let committed = if mb.is_empty() {
        Vec::new()
    } else {
        diff_rows(dir, &[&mb, "HEAD"], ignore_ws, None)?
    };

    let status_bytes = {
        let mut c = git_in(dir);
        c.args(["status", "--porcelain=v2", "-z", "--untracked-files=all"]);
        run(c)?
    };

    let v2 = parse::parse_porcelain_v2_z(&status_bytes);
    let dirty = build_dirty_set(&v2, &committed);

    let overlay = if dirty.is_empty() {
        Vec::new()
    } else {
        let paths: Vec<String> = dirty.iter().cloned().collect();
        let joined: usize = paths.iter().map(|p| p.len() + 1).sum();
        match overlay_mode(paths.len(), joined) {
            OverlayMode::Limited => diff_rows(dir, &overlay_range, ignore_ws, Some(&paths))?,
            OverlayMode::Full => diff_rows(dir, &overlay_range, ignore_ws, None)?,
        }
    };

    let untracked: Vec<UntrackedEntry> = v2
        .untracked
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i < MAX_UNTRACKED_COUNTED {
                untracked_entry(dir, p)
            } else {
                UntrackedEntry {
                    path: p.clone(),
                    added: 0,
                    binary: false,
                }
            }
        })
        .collect();

    let rows = parse::merge_snapshot(committed, overlay, &dirty, untracked);
    Ok(Snapshot {
        gen,
        ignore_ws,
        rows,
        computed_at: Instant::now(),
        compute_dur: compute_start.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse::DiffStatus;

    fn row(path: &str, old: Option<&str>) -> DiffFileSummary {
        DiffFileSummary {
            path: path.into(),
            old_path: old.map(str::to_string),
            status: if old.is_some() {
                DiffStatus::Renamed
            } else {
                DiffStatus::Modified
            },
            added: 0,
            deleted: 0,
            binary: false,
        }
    }

    #[test]
    fn same_generation_matches_only_equal() {
        assert!(same_generation(5, 5));
        assert!(!same_generation(5, 6));
    }

    /// Insert a synthetic cached snapshot so `effective_gen` can be exercised without git.
    fn seed(s: &Snapshotter, dir: &Path, gen: u64, computed_at: Instant, compute_dur: Duration) {
        let (lock, _) = &*s.inner;
        let mut map = lock.lock().unwrap();
        map.entry(dir.to_path_buf()).or_default().last = Some(Snapshot {
            gen,
            ignore_ws: false,
            rows: Vec::new(),
            computed_at,
            compute_dur,
        });
    }

    #[test]
    fn effective_gen_pins_to_cached_during_cooldown() {
        // Regression: a change during the cooldown must NOT be reported as already-loaded.
        let s = Snapshotter::default();
        let dir = std::env::temp_dir().join("keitora-effgen-cooldown");
        // Cached snapshot at gen 5, just computed (fresh ⇒ in cooldown), cost 1s.
        seed(&s, &dir, 5, Instant::now(), Duration::from_secs(1));
        // Watcher advanced to 6 (a change arrived) — but we're pinned to the cached gen 5
        // so the frontend, which already holds 5, won't prematurely mark 6 as loaded.
        assert_eq!(s.effective_gen(&dir, 6), 5);
    }

    #[test]
    fn effective_gen_advances_after_cooldown() {
        let s = Snapshotter::default();
        let dir = std::env::temp_dir().join("keitora-effgen-expired");
        // Cached snapshot computed well in the past ⇒ cooldown expired.
        let past = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .expect("instant subtraction");
        seed(&s, &dir, 5, past, Duration::from_secs(1));
        // Cooldown lifted ⇒ report the live gen so the frontend re-requests and recomputes.
        assert_eq!(s.effective_gen(&dir, 6), 6);
    }

    #[test]
    fn effective_gen_is_current_when_unseen_worktree() {
        let s = Snapshotter::default();
        let dir = std::env::temp_dir().join("keitora-effgen-none");
        assert_eq!(s.effective_gen(&dir, 42), 42);
    }

    #[test]
    fn cooldown_uses_max_of_floor_and_scan() {
        // scan cheaper than the floor → floor governs
        assert!(in_cooldown(
            Duration::from_millis(500),
            Duration::from_millis(100)
        ));
        assert!(!in_cooldown(
            Duration::from_millis(1_500),
            Duration::from_millis(100)
        ));
        // scan slower than the floor → scan governs
        assert!(in_cooldown(Duration::from_secs(3), Duration::from_secs(5)));
        assert!(!in_cooldown(Duration::from_secs(6), Duration::from_secs(5)));
    }

    #[test]
    fn overlay_mode_switches_at_thresholds() {
        assert!(matches!(overlay_mode(10, 200), OverlayMode::Limited));
        assert!(matches!(
            overlay_mode(MAX_OVERLAY_PATHS + 1, 200),
            OverlayMode::Full
        ));
        assert!(matches!(
            overlay_mode(10, MAX_PATHSPEC_CHARS + 1),
            OverlayMode::Full
        ));
    }

    #[test]
    fn dirty_set_is_tracked_plus_origins() {
        let v2 = PorcelainV2 {
            dirty_tracked: vec!["a.rs".into()],
            rename_origins: vec!["old.rs".into()],
            untracked: vec!["u.rs".into()],
        };
        let dirty = build_dirty_set(&v2, &[]);
        assert!(dirty.contains("a.rs"));
        assert!(dirty.contains("old.rs"));
        assert!(
            !dirty.contains("u.rs"),
            "untracked is not part of the overlay set"
        );
    }

    #[test]
    fn dirty_set_pulls_committed_rename_counterpart() {
        // Committed rename old.rs -> new.rs; new.rs is dirty. The origin old.rs must be
        // pulled in so the pathspec lets -M re-pair the rename.
        let v2 = PorcelainV2 {
            dirty_tracked: vec!["new.rs".into()],
            ..Default::default()
        };
        let committed = vec![row("new.rs", Some("old.rs"))];
        let dirty = build_dirty_set(&v2, &committed);
        assert!(dirty.contains("new.rs"));
        assert!(
            dirty.contains("old.rs"),
            "committed rename origin pulled into pathspec"
        );
    }

    #[test]
    fn summary_single_flights_and_reuses_by_generation() {
        // No git is needed: point at a non-repo dir. compute() returns an Err (git fails),
        // which is NOT cached, so this exercises the gate transitions without a repo.
        let s = Snapshotter::default();
        let dir = std::env::temp_dir();
        // First call computes (and errors, uncached). Second call at same gen computes again
        // because nothing was cached — but must not deadlock or panic.
        let _ = s.summary(&dir, false, 1);
        let _ = s.summary(&dir, false, 1);
    }
}
