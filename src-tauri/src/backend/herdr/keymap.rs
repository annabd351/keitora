//! `keymap` — the durable `SessionKey -> HerdrPane` map for `HerdrBackend`.
//!
//! herdr assigns its own `workspace_id`/`pane_id`/`terminal_id` (there is **no**
//! caller-chosen key), so
//! the backend keeps this map from the app's [`SessionKey`] to the herdr identity,
//! plus a monotonic `gen` that increments on every respawn (herdr has no
//! generation counter of its own).
//!
//! # Persistence
//! The map is persisted to a JSON file (`<KEITORA_HOME>/herdr/keymap.json`) with the
//! an atomic tmp+rename write, so a restart (of
//! the app, or of the herdr server) does not lose the `SessionKey -> pane` mapping.
//! Loading happens at construction ([`KeyMap::with_path`]); saving happens after
//! every mutation. Persistence is **best-effort**: a read or write failure logs and
//! continues — it never breaks a spawn.
//!
//! The file (version 2) also carries the server lineage: the [`ServerInstance`] the
//! entries were last reconciled against, the current [`Generation`] (bumped whenever
//! the server process is replaced; each entry is stamped with the generation it
//! belongs to), and the "should be running" recovery set — sessions that were live
//! when their server went away and are still owed a resume. Legacy files (a bare
//! entry array) still load: no instance, generation 0, empty recovery set.
//!
//! # Restore reconciliation (the duplicate-workspace fix)
//! herdr owns restore; after a server-restart restore it reassigns pane ids, so the
//! persisted ids are stale. [`reconcile_panes`] re-adopts restored panes by matching
//! each persisted entry to a live pane **by normalized cwd** (the worktree path) and
//! rewriting the entry to the live pane's current ids. `HerdrBackend::reconcile`
//! wires that to `pane_list()` after `ensure_server`, so `attach(key)` finds the
//! restored pane and the frontend never spawns a duplicate workspace.

use std::collections::{BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::backend::SessionKey;

use super::ops::{Generation, ServerInstance};

/// herdr's identity for one pane the backend owns, plus the backend-side `gen` and
/// the pane's `cwd` (the worktree path, used to re-adopt restored panes by path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HerdrPane {
    pub workspace_id: String,
    pub pane_id: String,
    pub terminal_id: String,
    /// Backend-side generation; bumped on each respawn for the same key.
    pub gen: u64,
    /// Pane working directory (the worktree path). The stable key across a herdr
    /// restore, which reassigns the id triple (see [`reconcile_panes`]).
    pub cwd: String,
    /// The server generation this pane belongs to. A pane from an older
    /// generation is treated as absent (restore reassigns ids, which can collide).
    pub server_generation: Generation,
}

/// A live herdr pane observed via `pane_list` during reconciliation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LivePane {
    pub workspace_id: String,
    pub pane_id: String,
    pub terminal_id: String,
    pub cwd: String,
}

/// One persisted keymap entry (the on-disk row). A flat record — `SessionKey` is a
/// struct, which cannot be a JSON object key.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedEntry {
    project: String,
    worktree: String,
    workspace_id: String,
    pane_id: String,
    terminal_id: String,
    gen: u64,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    server_generation: u64,
}

/// One key of the persisted recovery set.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedKey {
    project: String,
    worktree: String,
}

/// The current on-disk format version.
const FILE_VERSION: u32 = 2;

/// The whole keymap file (version 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedFile {
    version: u32,
    #[serde(default)]
    instance: Option<ServerInstance>,
    #[serde(default)]
    generation: Generation,
    #[serde(default)]
    should_be_running: Vec<PersistedKey>,
    #[serde(default)]
    entries: Vec<PersistedEntry>,
}

/// Server lineage and the recovery set, kept beside the entry map.
#[derive(Debug, Default)]
struct KeyMapMeta {
    instance: Option<ServerInstance>,
    generation: Generation,
    should_be_running: BTreeSet<(String, String)>,
    /// In memory only: a server death already bumped the generation and no
    /// reconcile has consumed that bump yet.
    death_bumped: bool,
    /// In memory only: the last `gen` of each key whose entry was dropped, so
    /// the key's next entry still gets a newer `gen` (a late kill of the old
    /// session must not match the new one).
    retired_gens: HashMap<SessionKey, u64>,
}

impl KeyMapMeta {
    /// Remember that `key`'s entry with `gen` left the map.
    fn retire(&mut self, key: &SessionKey, gen: u64) {
        let g = self.retired_gens.entry(key.clone()).or_default();
        *g = (*g).max(gen);
    }
}

/// `SessionKey -> HerdrPane` map, persisted to `path` (when set) after each mutation.
///
/// Lock order is always `inner` then `meta`: a persisting mutator builds its
/// snapshot under `inner` and briefly locks `meta` to copy it, so the file is one
/// consistent snapshot.
#[derive(Default)]
pub struct KeyMap {
    inner: Mutex<HashMap<SessionKey, HerdrPane>>,
    meta: Mutex<KeyMapMeta>,
    /// JSON file the map is mirrored to; `None` keeps the map purely in-memory
    /// (unit tests / the legacy [`KeyMap::new`] path).
    path: Option<PathBuf>,
    /// Serialises the on-disk write and tracks the last-written mutation `seq`, so the
    /// disk I/O happens WITHOUT holding `inner` (a slow volume must not stall concurrent
    /// `get`/`is_current_gen`/`entries` readers) while still landing on the newest
    /// snapshot (a slower write whose `seq` is stale is skipped, never overwriting a
    /// newer one). Also prevents two writers colliding on the shared `.tmp` path.
    io: Mutex<u64>,
    /// Monotonic mutation counter. Each mutation stamps its snapshot under `inner` (so
    /// the stamp order matches the mutation order) for the [`Self::io`] seq guard.
    seq: AtomicU64,
}

impl KeyMap {
    /// A purely in-memory map (no persistence). Used by unit tests; the live backend
    /// always constructs a persisted map via [`Self::with_path`].
    #[allow(dead_code)]
    pub fn new() -> Self {
        KeyMap::default()
    }

    /// A persisted map rooted at `path`. Loads any existing file immediately (a
    /// missing or corrupt file is treated as "empty").
    pub fn with_path(path: PathBuf) -> Self {
        let (entries, meta) = load_file(&path);
        KeyMap {
            inner: Mutex::new(entries),
            meta: Mutex::new(meta),
            path: Some(path),
            io: Mutex::new(0),
            seq: AtomicU64::new(0),
        }
    }

    /// Record (or replace) the herdr identity for `key`, returning the new `gen`.
    /// A fresh key starts at `gen = 1`; a respawn of the key increments the
    /// previous `gen`, also when its entry was dropped in between (so a stale
    /// `kill(key, old_gen)` can be rejected).
    pub fn record(
        &self,
        key: SessionKey,
        workspace_id: String,
        pane_id: String,
        terminal_id: String,
        cwd: String,
    ) -> u64 {
        let (gen, snapshot, seq) = {
            let mut map = self.inner.lock().unwrap();
            let (retired, server_generation) = {
                let meta = self.meta.lock().unwrap();
                (
                    meta.retired_gens.get(&key).copied().unwrap_or(0),
                    meta.generation,
                )
            };
            let gen = map.get(&key).map_or(0, |p| p.gen).max(retired) + 1;
            map.insert(
                key,
                HerdrPane {
                    workspace_id,
                    pane_id,
                    terminal_id,
                    gen,
                    cwd,
                    server_generation,
                },
            );
            (gen, self.snapshot(&map), self.next_seq())
        };
        self.write_snapshot(seq, snapshot);
        gen
    }

    /// The herdr identity currently mapped to `key`, if any.
    pub fn get(&self, key: &SessionKey) -> Option<HerdrPane> {
        self.inner.lock().unwrap().get(key).cloned()
    }

    /// Like [`Self::get`], but only a pane of the current server generation: one
    /// left over from a replaced server is treated as absent.
    pub fn get_current(&self, key: &SessionKey) -> Option<HerdrPane> {
        let map = self.inner.lock().unwrap();
        let generation = self.meta.lock().unwrap().generation;
        map.get(key)
            .filter(|p| p.server_generation == generation)
            .cloned()
    }

    /// `true` when `gen` matches the current mapping for `key`. A missing key or a
    /// mismatched (stale) generation is `false` — the gen-gated kill guard.
    pub fn is_current_gen(&self, key: &SessionKey, gen: u64) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(key)
            .is_some_and(|p| p.gen == gen)
    }

    /// Drop the mapping for `key` (after a confirmed close).
    pub fn remove(&self, key: &SessionKey) {
        self.mutate(|map, meta| {
            if let Some(p) = map.remove(key) {
                meta.retire(key, p.gen);
            }
        });
    }

    /// An explicit server stop: drop every mapping, the recovery set and the
    /// known instance (nothing is owed a resume after a requested stop).
    pub fn clear_for_shutdown(&self) {
        self.mutate(|map, meta| {
            for (k, p) in map.drain() {
                meta.retire(&k, p.gen);
            }
            meta.should_be_running.clear();
            meta.instance = None;
        });
    }

    /// The server instance the entries were last reconciled against.
    pub fn instance(&self) -> Option<ServerInstance> {
        self.meta.lock().unwrap().instance
    }

    pub fn set_instance(&self, instance: Option<ServerInstance>) {
        self.mutate(|_, meta| meta.instance = instance);
    }

    /// The current server generation.
    pub fn generation(&self) -> Generation {
        self.meta.lock().unwrap().generation
    }

    pub fn set_generation(&self, generation: Generation) {
        self.mutate(|_, meta| meta.generation = generation);
    }

    /// The sessions owed a resume, sorted by (project, worktree).
    pub fn should_be_running(&self) -> Vec<SessionKey> {
        self.meta
            .lock()
            .unwrap()
            .should_be_running
            .iter()
            .map(|(project, worktree)| SessionKey {
                project: project.clone(),
                worktree: worktree.clone(),
            })
            .collect()
    }

    /// Whether `key` is in the recovery set.
    pub fn is_pending_recovery(&self, key: &SessionKey) -> bool {
        self.meta
            .lock()
            .unwrap()
            .should_be_running
            .contains(&(key.project.clone(), key.worktree.clone()))
    }

    /// Add `keys` to the recovery set (a union) and persist.
    pub fn begin_recovery(&self, keys: &[SessionKey]) {
        self.mutate(|_, meta| {
            for k in keys {
                meta.should_be_running
                    .insert((k.project.clone(), k.worktree.clone()));
            }
        });
    }

    /// Remove `key` from the recovery set (resumed, killed or dropped) and persist.
    pub fn resolve_recovered(&self, key: &SessionKey) {
        self.mutate(|_, meta| {
            meta.should_be_running
                .remove(&(key.project.clone(), key.worktree.clone()));
        });
    }

    /// A server death: bump the generation once. A second call before
    /// [`Self::take_death_bumped`] consumes the bump is a no-op, which makes a
    /// repeated death report harmless. Returns the (possibly unchanged) generation.
    pub fn note_death_bump(&self) -> Generation {
        let mut out = Generation::default();
        self.mutate(|_, meta| {
            if !meta.death_bumped {
                meta.generation = meta.generation.next();
                meta.death_bumped = true;
            }
            out = meta.generation;
        });
        out
    }

    /// Whether a death already bumped the generation; reading clears the flag.
    pub fn take_death_bumped(&self) -> bool {
        std::mem::take(&mut self.meta.lock().unwrap().death_bumped)
    }

    /// Apply `f` to the map and meta under both locks, then persist.
    fn mutate(&self, f: impl FnOnce(&mut HashMap<SessionKey, HerdrPane>, &mut KeyMapMeta)) {
        let (snapshot, seq) = {
            let mut map = self.inner.lock().unwrap();
            {
                let mut meta = self.meta.lock().unwrap();
                f(&mut map, &mut meta);
            }
            (self.snapshot(&map), self.next_seq())
        };
        self.write_snapshot(seq, snapshot);
    }

    /// Build the whole on-disk file from `map` (held under `inner`) and the meta.
    fn snapshot(&self, map: &HashMap<SessionKey, HerdrPane>) -> PersistedFile {
        let meta = self.meta.lock().unwrap();
        PersistedFile {
            version: FILE_VERSION,
            instance: meta.instance,
            generation: meta.generation,
            should_be_running: meta
                .should_be_running
                .iter()
                .map(|(project, worktree)| PersistedKey {
                    project: project.clone(),
                    worktree: worktree.clone(),
                })
                .collect(),
            entries: snapshot_entries(map),
        }
    }

    /// Adopt a reconciled set into the map with snapshot-CAS semantics (restore
    /// reconciliation). `snapshot` is the `entries()` the reconcile was
    /// computed from; `reconciled` is the subset of those keys that matched a live pane,
    /// rewritten to the live ids (its keys are always a subset of `snapshot`'s).
    ///
    /// For each `snapshot` key, the change is applied ONLY if the current entry still
    /// equals the snapshot — i.e. nothing mutated it since (a concurrent `spawn`/
    /// `spawn_agent`/`kill` wins and is left untouched). An unchanged key that is in
    /// `reconciled` is rewritten to the live ids; an unchanged key absent from
    /// `reconciled` (its pane is gone) is dropped. Keys added since the snapshot are
    /// never touched. This replaces the old clear-and-rewrite, which clobbered a
    /// session created while the reconcile thread ran.
    pub fn adopt_reconciled(
        &self,
        snapshot: &[(SessionKey, HerdrPane)],
        reconciled: Vec<(SessionKey, HerdrPane)>,
    ) {
        let reconciled: HashMap<SessionKey, HerdrPane> = reconciled.into_iter().collect();
        self.mutate(|map, meta| {
            for (k, snap) in snapshot {
                match map.get(k) {
                    // Unchanged since the reconcile snapshot → safe to apply.
                    Some(cur) if cur == snap => {
                        if let Some(adopted) = reconciled.get(k) {
                            map.insert(k.clone(), adopted.clone());
                        } else if let Some(p) = map.remove(k) {
                            meta.retire(k, p.gen);
                        }
                    }
                    // Concurrently respawned/removed → leave the newer state in place.
                    _ => {}
                }
            }
        });
    }

    /// Every `(key, pane)` currently mapped (clone; for reconciliation/listing).
    pub fn entries(&self) -> Vec<(SessionKey, HerdrPane)> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Changes whenever the map or its lineage changes (a cache built from it
    /// is stale once this moves on).
    pub fn version(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    /// Next monotonic mutation `seq`. Called inside the `inner` critical section so the
    /// stamp order matches the mutation order (see [`Self::write_snapshot`]).
    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Best-effort persist of a pre-built `snapshot` to `self.path` (if set), WITHOUT
    /// holding `inner` — so the disk write never blocks concurrent map readers. The
    /// [`Self::io`] lock serialises writers (no shared `.tmp` collision) and the `seq`
    /// guard drops a write whose snapshot is older than one already on disk, so the file
    /// always lands on the newest state. A failure logs and is swallowed — persistence
    /// must never break a spawn.
    fn write_snapshot(&self, seq: u64, snapshot: PersistedFile) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let mut last = self.io.lock().unwrap();
        if seq <= *last {
            return; // a newer snapshot already landed
        }
        match write_keymap_atomic(path, &snapshot) {
            Ok(()) => *last = seq,
            Err(e) => log::warn!("herdr: failed to persist keymap to {}: {e}", path.display()),
        }
    }
}

/// Build the on-disk row set from the in-memory map. Called under the `inner` lock so
/// the snapshot is a consistent point-in-time copy, then written outside the lock.
fn snapshot_entries(map: &HashMap<SessionKey, HerdrPane>) -> Vec<PersistedEntry> {
    map.iter()
        .map(|(k, v)| PersistedEntry {
            project: k.project.clone(),
            worktree: k.worktree.clone(),
            workspace_id: v.workspace_id.clone(),
            pane_id: v.pane_id.clone(),
            terminal_id: v.terminal_id.clone(),
            gen: v.gen,
            cwd: v.cwd.clone(),
            server_generation: v.server_generation.0,
        })
        .collect()
}

/// Read + parse the persisted keymap: the v2 object first, then the legacy bare
/// entry array (no instance, generation 0, empty recovery set, every entry in
/// generation 0). A missing or unparseable file reads as empty (a corrupt file is
/// treated as "no prior state").
fn load_file(path: &Path) -> (HashMap<SessionKey, HerdrPane>, KeyMapMeta) {
    let Ok(bytes) = std::fs::read(path) else {
        return (HashMap::new(), KeyMapMeta::default());
    };
    let file = match serde_json::from_slice::<PersistedFile>(&bytes) {
        Ok(f) => f,
        Err(_) => match serde_json::from_slice::<Vec<PersistedEntry>>(&bytes) {
            Ok(entries) => PersistedFile {
                version: 1,
                instance: None,
                generation: Generation::default(),
                should_be_running: Vec::new(),
                entries: entries
                    .into_iter()
                    .map(|e| PersistedEntry {
                        server_generation: 0,
                        ..e
                    })
                    .collect(),
            },
            Err(_) => {
                log::warn!(
                    "herdr: keymap file {} is unparseable; starting empty",
                    path.display()
                );
                return (HashMap::new(), KeyMapMeta::default());
            }
        },
    };
    let meta = KeyMapMeta {
        instance: file.instance,
        generation: file.generation,
        should_be_running: file
            .should_be_running
            .into_iter()
            .map(|k| (k.project, k.worktree))
            .collect(),
        death_bumped: false,
        retired_gens: HashMap::new(),
    };
    let entries = file
        .entries
        .into_iter()
        .map(|e| {
            (
                SessionKey {
                    project: e.project,
                    worktree: e.worktree,
                },
                HerdrPane {
                    workspace_id: e.workspace_id,
                    pane_id: e.pane_id,
                    terminal_id: e.terminal_id,
                    gen: e.gen,
                    cwd: e.cwd,
                    server_generation: Generation(e.server_generation),
                },
            )
        })
        .collect();
    (entries, meta)
}

/// Atomically write the keymap: serialise to pretty JSON, write `<path>.tmp`, set
/// mode 0600 on unix, rename over the target.
fn write_keymap_atomic(path: &Path, file: &PersistedFile) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(file)?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&json)?;
        f.flush()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
    }
    // Atomic replace: `rename(2)` on Unix / `MoveFileExW(REPLACE_EXISTING)` on Windows
    // — both atomic on the local volumes `~/.keitora` lives on. A crash mid-write
    // leaves the old or new file, never a partial one; a torn write surfaces as
    // unparseable state, which `load_file` already treats as "empty".
    std::fs::rename(&tmp, path)
}

/// Normalise a cwd for cross-source comparison (herdr's backslash / `\\?\`-verbatim
/// form vs Keitora's worktree path). Pure: strip a `\\?\` Windows verbatim prefix,
/// unify separators to `/` (Windows only: a backslash is an ordinary filename
/// character on unix), trim trailing separators (except a bare root), and fold
/// case on case-insensitive platforms (Windows). So herdr's
/// `\\?\D:\repos\proj\wt\` and Keitora's `D:/repos/proj/wt` normalise equal.
pub fn normalize_cwd(cwd: &str) -> String {
    normalize_path(cwd, cfg!(windows))
}

/// [`normalize_cwd`] with `\` a separator on every host, for comparing against
/// paths a tool printed in Windows form (a build log read as text). Pure.
pub fn normalize_either_separator(path: &str) -> String {
    normalize_path(path, true)
}

fn normalize_path(path: &str, backslash_separates: bool) -> String {
    // Strip the Windows verbatim prefix in either slash form.
    let s = path
        .strip_prefix(r"\\?\")
        .or_else(|| path.strip_prefix("//?/"))
        .unwrap_or(path);
    // Unify separators.
    let mut s = if backslash_separates {
        s.replace('\\', "/")
    } else {
        s.to_string()
    };
    // Trim trailing separators, keeping a lone root ("/").
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    // Case-insensitive filesystems (Windows): fold case so a drive-letter or path
    // casing difference does not defeat the match.
    #[cfg(windows)]
    {
        s = s.to_lowercase();
    }
    s
}

/// A cwd Keitora holds (a worktree path as it was given), normalised into the
/// form herdr reports for a pane running there. On unix herdr reports the
/// directory with its symlinks resolved (`/tmp` comes back as `/private/tmp`),
/// so the links are resolved here too; a path that cannot be resolved (it no
/// longer exists) is compared as it is. On unix the first call for a cwd reads
/// the filesystem and a resolved path is remembered for the process (see
/// [`LinkMemo`]), so the status loop's recurring comparisons make no filesystem
/// call; elsewhere it is [`normalize_cwd`].
pub fn held_cwd(cwd: &str) -> String {
    normalize_cwd(&resolve_links(cwd))
}

/// Cwds whose symlinks were resolved, by the cwd as given. Only successes are
/// kept: a path that could not be resolved (not there yet, or gone) is tried
/// again next time. A worktree directory does not turn into a link to somewhere
/// else while Keitora runs, so an entry is never dropped; there is one per cwd
/// ever held.
#[cfg(unix)]
#[derive(Default)]
struct LinkMemo {
    resolved: std::sync::Mutex<HashMap<String, String>>,
}

#[cfg(unix)]
impl LinkMemo {
    /// `cwd` with its links resolved: remembered, else from `resolve` (kept when
    /// it answers), else `cwd` as given. `resolve` runs with the memo unlocked,
    /// so a slow filesystem holds up only its own caller.
    fn resolve(&self, cwd: &str, resolve: impl FnOnce(&str) -> Option<String>) -> String {
        if let Some(known) = self.resolved.lock().unwrap().get(cwd) {
            return known.clone();
        }
        match resolve(cwd) {
            Some(real) => {
                self.resolved
                    .lock()
                    .unwrap()
                    .insert(cwd.to_string(), real.clone());
                real
            }
            None => cwd.to_string(),
        }
    }
}

#[cfg(unix)]
fn resolve_links(cwd: &str) -> String {
    static MEMO: std::sync::OnceLock<LinkMemo> = std::sync::OnceLock::new();
    MEMO.get_or_init(LinkMemo::default).resolve(cwd, |cwd| {
        std::fs::canonicalize(cwd)
            .ok()
            .and_then(|p| p.into_os_string().into_string().ok())
    })
}

#[cfg(not(unix))]
fn resolve_links(cwd: &str) -> String {
    cwd.to_string()
}

/// Re-adopt restored panes by cwd (the duplicate-workspace fix).
///
/// For each persisted `(key, pane)`, find a live pane whose normalized cwd equals the
/// entry's cwd as herdr would report it ([`held_cwd`]) and return the entry rewritten to the live pane's **current**
/// ids (herdr reassigns the id triple on a server-restart restore). `gen` is preserved
/// when the ids are unchanged (the server survived; same pane) and bumped when they
/// differ (a restore produced a fresh pane) so a stale `kill(key, old_gen)` is rejected.
/// Persisted entries with no live match are dropped; unmatched live panes are ignored
/// (they belong to no known session). A live pane is matched at most once.
pub fn reconcile_panes(
    persisted: &[(SessionKey, HerdrPane)],
    live: &[LivePane],
) -> Vec<(SessionKey, HerdrPane)> {
    let mut used = vec![false; live.len()];
    let mut out = Vec::new();
    for (key, pane) in persisted {
        let want = held_cwd(&pane.cwd);
        let found = live
            .iter()
            .enumerate()
            .find(|(i, lp)| !used[*i] && normalize_cwd(&lp.cwd) == want);
        if let Some((i, lp)) = found {
            used[i] = true;
            let ids_changed = lp.pane_id != pane.pane_id || lp.workspace_id != pane.workspace_id;
            let gen = if ids_changed { pane.gen + 1 } else { pane.gen };
            out.push((
                key.clone(),
                HerdrPane {
                    workspace_id: lp.workspace_id.clone(),
                    pane_id: lp.pane_id.clone(),
                    terminal_id: lp.terminal_id.clone(),
                    gen,
                    // Keep the persisted (canonical Keitora) cwd so future reconciles
                    // keep matching against the same stored form.
                    cwd: pane.cwd.clone(),
                    server_generation: pane.server_generation,
                },
            ));
        }
        // no live match → the pane is gone; drop the entry.
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(p: &str, w: &str) -> SessionKey {
        SessionKey {
            project: p.to_string(),
            worktree: w.to_string(),
        }
    }

    fn pane(ws: &str, p: &str, t: &str, gen: u64, cwd: &str) -> HerdrPane {
        HerdrPane {
            workspace_id: ws.into(),
            pane_id: p.into(),
            terminal_id: t.into(),
            gen,
            cwd: cwd.into(),
            server_generation: Generation::default(),
        }
    }

    fn live(ws: &str, p: &str, t: &str, cwd: &str) -> LivePane {
        LivePane {
            workspace_id: ws.into(),
            pane_id: p.into(),
            terminal_id: t.into(),
            cwd: cwd.into(),
        }
    }

    #[test]
    fn store_and_lookup_roundtrips() {
        let m = KeyMap::new();
        let k = key("alpha", "sayo");
        let gen = m.record(
            k.clone(),
            "w2".into(),
            "w2:p1".into(),
            "term_abc".into(),
            "/wt/sayo".into(),
        );
        assert_eq!(gen, 1);
        let p = m.get(&k).expect("mapped");
        assert_eq!(p.workspace_id, "w2");
        assert_eq!(p.pane_id, "w2:p1");
        assert_eq!(p.terminal_id, "term_abc");
        assert_eq!(p.gen, 1);
        assert_eq!(p.cwd, "/wt/sayo");
        assert!(m.get(&key("alpha", "other")).is_none());
    }

    #[test]
    fn respawn_bumps_gen() {
        let m = KeyMap::new();
        let k = key("alpha", "sayo");
        assert_eq!(
            m.record(
                k.clone(),
                "w2".into(),
                "w2:p1".into(),
                "t1".into(),
                "/c".into()
            ),
            1
        );
        assert_eq!(
            m.record(
                k.clone(),
                "w3".into(),
                "w3:p1".into(),
                "t2".into(),
                "/c".into()
            ),
            2
        );
        assert_eq!(
            m.record(
                k.clone(),
                "w4".into(),
                "w4:p1".into(),
                "t3".into(),
                "/c".into()
            ),
            3
        );
        assert_eq!(m.get(&k).unwrap().terminal_id, "t3");
        assert_eq!(m.get(&k).unwrap().gen, 3);
    }

    #[test]
    fn a_key_recorded_again_after_its_entry_left_gets_a_newer_gen() {
        let m = KeyMap::new();
        let k = key("alpha", "sayo");
        let rec =
            |pane: &str| m.record(k.clone(), "w".into(), pane.into(), "t".into(), "/c".into());
        assert_eq!(rec("w1:p1"), 1);
        assert_eq!(rec("w1:p2"), 2);
        // Removed (a respawn replaces the pane; a kill; a stale attach).
        m.remove(&k);
        assert_eq!(rec("w1:p3"), 3);
        assert!(!m.is_current_gen(&k, 2), "the old session's gen is stale");
        // Dropped by a reconcile against a replaced server.
        m.adopt_reconciled(&m.entries(), Vec::new());
        assert!(m.get(&k).is_none());
        assert_eq!(rec("w1:p4"), 4);
        // Cleared by an explicit stop.
        m.clear_for_shutdown();
        assert_eq!(rec("w1:p5"), 5);
    }

    #[test]
    fn stale_gen_is_rejected_current_gen_accepted() {
        let m = KeyMap::new();
        let k = key("alpha", "sayo");
        m.record(
            k.clone(),
            "w2".into(),
            "w2:p1".into(),
            "t1".into(),
            "/c".into(),
        );
        m.record(
            k.clone(),
            "w3".into(),
            "w3:p1".into(),
            "t2".into(),
            "/c".into(),
        ); // gen 2
        assert!(!m.is_current_gen(&k, 1), "old gen must be stale");
        assert!(m.is_current_gen(&k, 2), "current gen is accepted");
        assert!(!m.is_current_gen(&key("alpha", "missing"), 1));
    }

    #[test]
    fn remove_drops_mapping() {
        let m = KeyMap::new();
        let k = key("alpha", "sayo");
        m.record(
            k.clone(),
            "w2".into(),
            "w2:p1".into(),
            "t1".into(),
            "/c".into(),
        );
        m.remove(&k);
        assert!(m.get(&k).is_none());
        // The key's gen keeps counting after the removal.
        assert_eq!(
            m.record(
                k.clone(),
                "w9".into(),
                "w9:p1".into(),
                "t9".into(),
                "/c".into()
            ),
            2
        );
    }

    #[test]
    fn entries_lists_all_mappings() {
        let m = KeyMap::new();
        m.record(
            key("a", "x"),
            "w1".into(),
            "w1:p1".into(),
            "t1".into(),
            "/x".into(),
        );
        m.record(
            key("b", "y"),
            "w2".into(),
            "w2:p1".into(),
            "t2".into(),
            "/y".into(),
        );
        let mut got: Vec<String> = m
            .entries()
            .into_iter()
            .map(|(k, _)| k.to_string())
            .collect();
        got.sort();
        assert_eq!(got, vec!["a/x".to_string(), "b/y".to_string()]);
    }

    // ---- persistence -------------------------------------------------------

    fn temp_root() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "keitora-keymap-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn persist_roundtrips_through_a_fresh_load() {
        let root = temp_root();
        let path = root.join("keymap.json");
        {
            let m = KeyMap::with_path(path.clone());
            m.record(
                key("proj", "beta"),
                "w2".into(),
                "w2:p1".into(),
                "term_1".into(),
                r"D:\repos\proj\beta".into(),
            );
            m.record(
                key("proj", "gamma"),
                "w3".into(),
                "w3:p1".into(),
                "term_2".into(),
                r"D:\repos\proj\gamma".into(),
            );
            // respawn beta → gen 2, must survive the roundtrip.
            m.record(
                key("proj", "beta"),
                "w4".into(),
                "w4:p1".into(),
                "term_3".into(),
                r"D:\repos\proj\beta".into(),
            );
        }
        assert!(path.exists(), "atomic write must leave the real file");

        // A fresh KeyMap over the same path loads the persisted entries verbatim.
        let loaded = KeyMap::with_path(path.clone());
        let beta = loaded.get(&key("proj", "beta")).expect("beta persisted");
        assert_eq!(beta.gen, 2, "respawned gen survives persistence");
        assert_eq!(beta.terminal_id, "term_3");
        assert_eq!(beta.cwd, r"D:\repos\proj\beta");
        let gamma = loaded.get(&key("proj", "gamma")).expect("gamma persisted");
        assert_eq!(gamma.pane_id, "w3:p1");
        assert_eq!(loaded.entries().len(), 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_and_clear_are_persisted() {
        let root = temp_root();
        let path = root.join("keymap.json");
        let m = KeyMap::with_path(path.clone());
        m.record(
            key("a", "x"),
            "w1".into(),
            "w1:p1".into(),
            "t1".into(),
            "/x".into(),
        );
        m.record(
            key("b", "y"),
            "w2".into(),
            "w2:p1".into(),
            "t2".into(),
            "/y".into(),
        );
        m.remove(&key("a", "x"));
        assert_eq!(KeyMap::with_path(path.clone()).entries().len(), 1);
        m.clear_for_shutdown();
        assert!(KeyMap::with_path(path.clone()).entries().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_or_corrupt_file_loads_empty() {
        let root = temp_root();
        let missing = root.join("nope.json");
        assert!(KeyMap::with_path(missing).entries().is_empty());
        let corrupt = root.join("keymap.json");
        std::fs::write(&corrupt, b"{ not valid json").unwrap();
        assert!(
            KeyMap::with_path(corrupt).entries().is_empty(),
            "a corrupt file must read as empty, not panic"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_is_atomic_leaving_no_tmp() {
        let root = temp_root();
        let path = root.join("keymap.json");
        let m = KeyMap::with_path(path.clone());
        m.record(
            key("a", "x"),
            "w1".into(),
            "w1:p1".into(),
            "t1".into(),
            "/x".into(),
        );
        assert!(path.exists());
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the tmp file must be renamed away, not left behind"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- normalize_cwd -----------------------------------------------------

    #[test]
    #[cfg(windows)]
    fn normalize_cwd_unifies_separators_and_verbatim_prefix() {
        // herdr's backslash + \\?\ verbatim form vs Keitora's forward-slash form.
        let herdr = normalize_cwd(r"\\?\D:\repos\proj\wt");
        let keitora = normalize_cwd("D:/repos/proj/wt");
        assert_eq!(
            herdr, keitora,
            "verbatim+backslash must equal forward-slash"
        );
    }

    #[test]
    fn normalize_cwd_trims_trailing_slash() {
        assert_eq!(normalize_cwd("D:/repos/wt/"), normalize_cwd("D:/repos/wt"));
        assert_eq!(normalize_cwd("/repos/wt//"), "/repos/wt");
        // A bare root is preserved (not trimmed to empty).
        assert_eq!(normalize_cwd("/"), "/");
    }

    #[test]
    #[cfg(windows)]
    fn normalize_cwd_trims_a_trailing_backslash_on_windows() {
        assert_eq!(normalize_cwd(r"D:\repos\wt\"), normalize_cwd("D:/repos/wt"));
    }

    #[test]
    fn normalize_either_separator_unifies_backslashes_on_every_host() {
        let want = normalize_either_separator("D:/repos/wt");
        assert_eq!(normalize_either_separator(r"D:\repos\wt\"), want);
        assert_eq!(normalize_either_separator(r"\\?\D:\repos\wt"), want);
    }

    /// A backslash is an ordinary filename character on unix: it is neither a
    /// separator nor trimmed.
    #[test]
    #[cfg(unix)]
    fn normalize_cwd_keeps_backslashes_on_unix() {
        assert_eq!(normalize_cwd(r"/repos/a\b"), r"/repos/a\b");
        assert_ne!(normalize_cwd(r"/repos/a\b"), normalize_cwd("/repos/a/b"));
        assert_eq!(normalize_cwd(r"/repos/wt\"), r"/repos/wt\");
    }

    // ---- held_cwd ----------------------------------------------------------

    #[test]
    #[cfg(unix)]
    fn held_cwd_resolves_a_symlink_to_the_directory_herdr_reports() {
        let tmp = tempfile::tempdir().unwrap();
        // The temp dir may itself sit behind a link (`/var` on macOS).
        let real = std::fs::canonicalize(tmp.path()).unwrap().join("wt");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("ln");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let reported = normalize_cwd(&real.to_string_lossy());
        assert_eq!(held_cwd(&link.to_string_lossy()), reported);
        assert_eq!(
            held_cwd(&format!("{}/", link.display())),
            reported,
            "and is normalised like any cwd"
        );
        assert_eq!(held_cwd(&real.to_string_lossy()), reported);
    }

    #[cfg(unix)]
    #[test]
    fn link_memo_asks_the_filesystem_once_per_resolved_cwd() {
        let memo = LinkMemo::default();
        let calls = std::cell::Cell::new(0);
        let real = |cwd: &str| {
            calls.set(calls.get() + 1);
            Some(format!("/private{cwd}"))
        };
        assert_eq!(memo.resolve("/tmp/wt", real), "/private/tmp/wt");
        assert_eq!(calls.get(), 1);
        // Again: answered from the memo, the filesystem is not touched.
        let again = memo.resolve("/tmp/wt", |_| {
            panic!("a resolved cwd is not resolved again")
        });
        assert_eq!(again, "/private/tmp/wt");
        // Another cwd is its own entry.
        assert_eq!(memo.resolve("/tmp/other", real), "/private/tmp/other");
        assert_eq!(calls.get(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn link_memo_does_not_remember_a_failed_resolution() {
        let memo = LinkMemo::default();
        let calls = std::cell::Cell::new(0);
        let missing = |_: &str| {
            calls.set(calls.get() + 1);
            None
        };
        // Not there (yet): compared as given, and asked again each time.
        assert_eq!(memo.resolve("/tmp/wt", missing), "/tmp/wt");
        assert_eq!(memo.resolve("/tmp/wt", missing), "/tmp/wt");
        assert_eq!(calls.get(), 2);
        // Once it resolves, that is what is remembered.
        assert_eq!(
            memo.resolve("/tmp/wt", |_| Some("/private/tmp/wt".into())),
            "/private/tmp/wt"
        );
        assert_eq!(
            memo.resolve("/tmp/wt", |_| panic!("remembered")),
            "/private/tmp/wt"
        );
    }

    #[test]
    fn held_cwd_of_a_missing_path_is_the_path_as_given() {
        assert_eq!(held_cwd("/no/such/dir/"), "/no/such/dir");
        assert_eq!(held_cwd("D:/repos/wt"), normalize_cwd("D:/repos/wt"));
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_panes_matches_a_pane_reached_through_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(tmp.path()).unwrap().join("wt");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("ln");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let held = link.to_string_lossy().into_owned();

        let persisted = vec![(key("p", "w"), pane("ws", "pane", "term", 3, &held))];
        let listed = vec![live("ws", "pane", "term", &real.to_string_lossy())];
        let out = reconcile_panes(&persisted, &listed);

        assert_eq!(out.len(), 1, "the pane is adopted");
        assert_eq!(out[0].1.gen, 3);
        assert_eq!(out[0].1.cwd, held, "the cwd is kept as Keitora holds it");
    }

    #[test]
    #[cfg(windows)]
    fn normalize_cwd_is_case_insensitive_on_windows() {
        assert_eq!(
            normalize_cwd(r"C:\Repos\Proj"),
            normalize_cwd("c:/repos/proj")
        );
    }

    #[test]
    #[cfg(unix)]
    fn normalize_cwd_is_case_sensitive_on_unix() {
        assert_ne!(normalize_cwd("/Repos/Proj"), normalize_cwd("/repos/proj"));
    }

    // ---- reconcile_panes ---------------------------------------------------

    /// `path` (forward slashes) spelled another way that must still match it:
    /// herdr's verbatim backslash form on Windows, a trailing separator elsewhere.
    fn reported(path: &str) -> String {
        if cfg!(windows) {
            format!(r"\\?\{}", path.replace('/', r"\"))
        } else {
            format!("{path}/")
        }
    }

    #[test]
    fn reconcile_adopts_restored_pane_new_ids_by_cwd() {
        // Persisted entry has stale ids; the restored live pane has NEW ids but the
        // same worktree cwd (spelled differently). Must adopt the new
        // ids and bump gen.
        let persisted = vec![(
            key("proj", "beta"),
            pane("w2", "w2:p1", "old_term", 3, "D:/repos/proj/beta"),
        )];
        let live_panes = vec![live(
            "w7",
            "w7:p1",
            "new_term",
            &reported("D:/repos/proj/beta"),
        )];
        let out = reconcile_panes(&persisted, &live_panes);
        assert_eq!(out.len(), 1);
        let (k, p) = &out[0];
        assert_eq!(k, &key("proj", "beta"));
        assert_eq!(p.workspace_id, "w7");
        assert_eq!(p.pane_id, "w7:p1");
        assert_eq!(p.terminal_id, "new_term");
        assert_eq!(p.gen, 4, "new ids → gen bumped");
    }

    #[test]
    fn reconcile_preserves_gen_when_ids_unchanged() {
        // Server survived the app restart: same ids → gen preserved.
        let persisted = vec![(
            key("proj", "beta"),
            pane("w2", "w2:p1", "term", 5, "D:/repos/proj/beta"),
        )];
        let live_panes = vec![live("w2", "w2:p1", "term", "D:/repos/proj/beta")];
        let out = reconcile_panes(&persisted, &live_panes);
        assert_eq!(out[0].1.gen, 5, "unchanged ids → gen preserved");
    }

    #[test]
    fn reconcile_drops_persisted_without_live_match() {
        let persisted = vec![
            (
                key("proj", "beta"),
                pane("w2", "w2:p1", "t", 1, "/repos/beta"),
            ),
            (
                key("proj", "gone"),
                pane("w3", "w3:p1", "t", 1, "/repos/gone"),
            ),
        ];
        let live_panes = vec![live("w7", "w7:p1", "t2", "/repos/beta")];
        let out = reconcile_panes(&persisted, &live_panes);
        assert_eq!(out.len(), 1, "the entry with no live pane is dropped");
        assert_eq!(out[0].0, key("proj", "beta"));
    }

    #[test]
    fn reconcile_ignores_extra_live_panes() {
        let persisted = vec![(
            key("proj", "beta"),
            pane("w2", "w2:p1", "t", 1, "/repos/beta"),
        )];
        let live_panes = vec![
            live("w7", "w7:p1", "t2", "/repos/beta"),
            live("w8", "w8:p1", "t3", "/repos/unmanaged"),
        ];
        let out = reconcile_panes(&persisted, &live_panes);
        assert_eq!(out.len(), 1, "an unmatched live pane is ignored");
        assert_eq!(out[0].1.pane_id, "w7:p1");
    }

    #[test]
    fn reconcile_matches_multiple_worktrees_independently() {
        let persisted = vec![
            (
                key("proj", "beta"),
                pane("w2", "w2:p1", "t", 1, &reported("D:/repos/proj/beta")),
            ),
            (
                key("proj", "gamma"),
                pane("w3", "w3:p1", "t", 2, &reported("D:/repos/proj/gamma")),
            ),
        ];
        // Live panes arrive in a different order and with new ids.
        let live_panes = vec![
            live("w9", "w9:p1", "t9", "D:/repos/proj/gamma"),
            live("w8", "w8:p1", "t8", "D:/repos/proj/beta"),
        ];
        let mut out = reconcile_panes(&persisted, &live_panes);
        out.sort_by(|a, b| a.0.worktree.cmp(&b.0.worktree));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0.worktree, "beta");
        assert_eq!(out[0].1.pane_id, "w8:p1");
        assert_eq!(out[1].0.worktree, "gamma");
        assert_eq!(out[1].1.pane_id, "w9:p1");
    }

    #[test]
    fn adopt_reconciled_rewrites_matched_drops_gone_and_persists() {
        let root = temp_root();
        let path = root.join("keymap.json");
        let m = KeyMap::with_path(path.clone());
        // Two persisted sessions; "beta" will be re-adopted with new ids, "gone" has no
        // live pane so it must be dropped.
        m.record(
            key("proj", "beta"),
            "w2".into(),
            "w2:p1".into(),
            "old".into(),
            "/repos/beta".into(),
        );
        m.record(
            key("proj", "gone"),
            "w3".into(),
            "w3:p1".into(),
            "old".into(),
            "/repos/gone".into(),
        );
        let snapshot = m.entries();
        // Only "beta" matched a live pane (rewritten to w7 ids, gen bumped by reconcile).
        m.adopt_reconciled(
            &snapshot,
            vec![(
                key("proj", "beta"),
                pane("w7", "w7:p1", "term", 2, "/repos/beta"),
            )],
        );
        assert_eq!(m.get(&key("proj", "beta")).unwrap().pane_id, "w7:p1");
        assert_eq!(m.get(&key("proj", "beta")).unwrap().gen, 2);
        assert!(m.get(&key("proj", "gone")).is_none(), "gone entry dropped");
        // Persisted to disk.
        let loaded = KeyMap::with_path(path);
        assert_eq!(loaded.get(&key("proj", "beta")).unwrap().pane_id, "w7:p1");
        assert_eq!(loaded.entries().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn adopt_reconciled_preserves_a_session_changed_since_the_snapshot() {
        // The reconcile thread snapshots the map, then a concurrent spawn respawns the
        // SAME key (bumping gen / ids) before adopt runs. adopt must NOT clobber the
        // newer entry with the stale reconciled one.
        let m = KeyMap::new();
        m.record(
            key("proj", "beta"),
            "w2".into(),
            "w2:p1".into(),
            "old".into(),
            "/repos/beta".into(),
        );
        let snapshot = m.entries(); // captured BEFORE the concurrent respawn
                                    // Concurrent respawn: new ids, gen 2.
        m.record(
            key("proj", "beta"),
            "w9".into(),
            "w9:p1".into(),
            "fresh".into(),
            "/repos/beta".into(),
        );
        // adopt tries to rewrite "beta" from the stale snapshot — must be ignored.
        m.adopt_reconciled(
            &snapshot,
            vec![(
                key("proj", "beta"),
                pane("w7", "w7:p1", "stale", 2, "/repos/beta"),
            )],
        );
        let cur = m.get(&key("proj", "beta")).unwrap();
        assert_eq!(
            cur.pane_id, "w9:p1",
            "concurrent respawn must survive adopt"
        );
        assert_eq!(cur.gen, 2);
    }

    #[test]
    fn adopt_reconciled_leaves_a_session_added_since_the_snapshot() {
        // A key created after the snapshot (not in it) must be untouched by adopt.
        let m = KeyMap::new();
        m.record(
            key("proj", "beta"),
            "w2".into(),
            "w2:p1".into(),
            "t".into(),
            "/repos/beta".into(),
        );
        let snapshot = m.entries();
        // New session appears mid-reconcile.
        m.record(
            key("proj", "new"),
            "w5".into(),
            "w5:p1".into(),
            "t".into(),
            "/repos/new".into(),
        );
        // Reconcile found no live pane for "beta" → drops it; "new" is outside the snapshot.
        m.adopt_reconciled(&snapshot, vec![]);
        assert!(
            m.get(&key("proj", "beta")).is_none(),
            "snapshot entry dropped"
        );
        assert!(
            m.get(&key("proj", "new")).is_some(),
            "post-snapshot entry preserved"
        );
    }

    // ---- v2 file: lineage + recovery set ----------------------------------

    const INSTANCE: ServerInstance = ServerInstance {
        pid: 4242,
        start_time: 1_759_700_000,
    };

    #[test]
    fn legacy_bare_array_file_loads() {
        let root = temp_root();
        let path = root.join("keymap.json");
        std::fs::write(
            &path,
            br#"[{"project":"proj","worktree":"beta","workspace_id":"w2","pane_id":"w2:p1","terminal_id":"t1","gen":3,"cwd":"/repos/beta"}]"#,
        )
        .unwrap();
        let m = KeyMap::with_path(path);
        assert_eq!(m.instance(), None);
        assert_eq!(m.generation(), Generation(0));
        assert!(m.should_be_running().is_empty());
        let p = m.get(&key("proj", "beta")).expect("legacy entry loads");
        assert_eq!(p.gen, 3);
        assert_eq!(p.server_generation, Generation(0));
        assert!(
            m.get_current(&key("proj", "beta")).is_some(),
            "a legacy entry belongs to the legacy generation"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn v2_file_round_trips_lineage_recovery_set_and_entry_generation() {
        let root = temp_root();
        let path = root.join("keymap.json");
        {
            let m = KeyMap::with_path(path.clone());
            m.set_instance(Some(INSTANCE));
            m.set_generation(Generation(3));
            m.record(
                key("proj", "beta"),
                "w2".into(),
                "w2:p1".into(),
                "t1".into(),
                "/repos/beta".into(),
            );
            m.begin_recovery(&[key("proj", "gamma")]);
        }
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["version"], 2);
        let m = KeyMap::with_path(path);
        assert_eq!(m.instance(), Some(INSTANCE));
        assert_eq!(m.generation(), Generation(3));
        assert_eq!(m.should_be_running(), vec![key("proj", "gamma")]);
        assert_eq!(
            m.get(&key("proj", "beta")).unwrap().server_generation,
            Generation(3)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_v2_file_loads_empty() {
        let root = temp_root();
        let path = root.join("keymap.json");
        std::fs::write(&path, br#"{"version":2,"entries":[{"project":1}]}"#).unwrap();
        let m = KeyMap::with_path(path);
        assert!(m.entries().is_empty());
        assert_eq!(m.generation(), Generation(0));
        assert!(m.should_be_running().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn begin_recovery_and_resolve_recovered_persist() {
        let root = temp_root();
        let path = root.join("keymap.json");
        let m = KeyMap::with_path(path.clone());
        m.begin_recovery(&[key("b", "y"), key("a", "x")]);
        m.begin_recovery(&[key("a", "x")]);
        assert_eq!(
            KeyMap::with_path(path.clone()).should_be_running(),
            vec![key("a", "x"), key("b", "y")],
            "a sorted union, on disk"
        );
        assert!(m.is_pending_recovery(&key("a", "x")));
        m.resolve_recovered(&key("a", "x"));
        assert!(!m.is_pending_recovery(&key("a", "x")));
        assert_eq!(
            KeyMap::with_path(path).should_be_running(),
            vec![key("b", "y")]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn get_current_hides_an_old_generation_pane() {
        let m = KeyMap::new();
        m.record(
            key("proj", "beta"),
            "w2".into(),
            "w2:p1".into(),
            "t".into(),
            "/repos/beta".into(),
        );
        assert!(m.get_current(&key("proj", "beta")).is_some());
        m.set_generation(Generation(1));
        assert!(
            m.get_current(&key("proj", "beta")).is_none(),
            "a pane of the replaced server is absent"
        );
        assert!(m.get(&key("proj", "beta")).is_some(), "the raw entry stays");
        m.record(
            key("proj", "beta"),
            "w5".into(),
            "w5:p1".into(),
            "t".into(),
            "/repos/beta".into(),
        );
        assert_eq!(
            m.get_current(&key("proj", "beta")).unwrap().pane_id,
            "w5:p1",
            "a record stamps the current generation"
        );
    }

    #[test]
    fn clear_for_shutdown_clears_entries_recovery_set_and_instance() {
        let root = temp_root();
        let path = root.join("keymap.json");
        let m = KeyMap::with_path(path.clone());
        m.set_instance(Some(INSTANCE));
        m.set_generation(Generation(2));
        m.record(
            key("a", "x"),
            "w1".into(),
            "w1:p1".into(),
            "t".into(),
            "/x".into(),
        );
        m.begin_recovery(&[key("a", "x")]);
        m.clear_for_shutdown();
        let loaded = KeyMap::with_path(path);
        assert!(loaded.entries().is_empty());
        assert!(loaded.should_be_running().is_empty());
        assert_eq!(loaded.instance(), None);
        assert_eq!(loaded.generation(), Generation(2), "the generation is kept");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn note_death_bump_bumps_once_until_consumed() {
        let m = KeyMap::new();
        assert!(!m.take_death_bumped());
        assert_eq!(m.note_death_bump(), Generation(1));
        assert_eq!(
            m.note_death_bump(),
            Generation(1),
            "second report is a no-op"
        );
        assert_eq!(m.generation(), Generation(1));
        assert!(m.take_death_bumped());
        assert!(!m.take_death_bumped(), "reading clears the flag");
        assert_eq!(
            m.note_death_bump(),
            Generation(2),
            "a later death bumps again"
        );
    }
}
