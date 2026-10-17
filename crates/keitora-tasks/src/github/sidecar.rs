//! Local SQLite sidecar for the GitHub backend.
//!
//! The GitHub backend is authoritative for the task record itself; the sidecar
//! owns **only** the machine-local state that GitHub cannot model:
//!
//! - `dispatch_log` — the dispatcher audit trail ([`DispatchLogEntry`]).
//! - `revision_counter` — a monotonically-incrementing integer bumped on every
//!   [`Sidecar::dispatch_log_add`] call; composes with the GitHub GraphQL cursor
//!   in `GitHubBackend::revision()` to satisfy `test_revision_advances_on_mutations`.
//!
//! The sidecar reuses the same rusqlite stack and `dispatch_log` schema as
//! [`SqliteBackend`](crate::SqliteBackend) — the DDL strings are factored into
//! `pub(crate) const`s ([`DISPATCH_LOG_DDL`]) so neither side copy-pastes SQL.
//! `SqliteBackend`'s behaviour is unchanged: it continues to call its own
//! full-schema `init_schema` on its own DB file.
//!
//! Reservation filtering (cross-checking GitHub task state) happens later
//! inside `GitHubBackend`; the sidecar's `dispatched_reservations` returns raw
//! dispatch-derived candidates, mirroring what `SqliteBackend` provides locally.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection};

use crate::backend::{BackendError, BackendResult, Reservation};
use crate::model::DispatchLogEntry;
use crate::row_to_dispatch_log_entry;
use crate::sqlite_backend::{map_sqlite, open_conn};

// ---------------------------------------------------------------------------
// Shared DDL constants — imported by sidecar; sqlite_backend keeps its own
// full schema string but the dispatch_log portion must match exactly.
// ---------------------------------------------------------------------------

/// DDL for the `dispatch_log` table and its index.
/// Shared so the sidecar's schema is byte-for-byte identical to the main DB.
pub(crate) const DISPATCH_LOG_DDL: &str = r#"
    CREATE TABLE IF NOT EXISTS dispatch_log (
      id         INTEGER PRIMARY KEY AUTOINCREMENT,
      task_id    INTEGER NOT NULL,
      worktree   TEXT NOT NULL,
      decision   TEXT NOT NULL,
      rationale  TEXT NOT NULL DEFAULT '',
      created_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_dispatch_log_task ON dispatch_log(task_id);
"#;

/// DDL for the `revision_counter` table: a single-row monotonic counter.
const REVISION_COUNTER_DDL: &str = r#"
    CREATE TABLE IF NOT EXISTS revision_counter (
      id      INTEGER PRIMARY KEY CHECK(id = 1),
      counter INTEGER NOT NULL DEFAULT 0
    );
    INSERT OR IGNORE INTO revision_counter(id, counter) VALUES (1, 0);
"#;

// ---------------------------------------------------------------------------
// Sidecar
// ---------------------------------------------------------------------------

/// Local SQLite sidecar owning the `dispatch_log` and a `revision_counter`.
///
/// The connection is guarded by a `Mutex` for the same reason as
/// [`SqliteBackend`](crate::SqliteBackend): `rusqlite::Connection` is `!Sync`.
pub(crate) struct Sidecar {
    conn: Mutex<Connection>,
}

impl Sidecar {
    /// Open (creating if needed) the sidecar DB at `path`. Initialises the
    /// `dispatch_log` and `revision_counter` tables if absent (idempotent).
    pub(crate) fn open(path: &Path) -> BackendResult<Self> {
        let path_str = path
            .to_str()
            .ok_or_else(|| BackendError::Config("sidecar path is not valid UTF-8".to_string()))?;
        let conn = open_conn(path_str).map_err(|e| BackendError::Permanent(e.to_string()))?;
        conn.execute_batch(&format!("{DISPATCH_LOG_DDL}{REVISION_COUNTER_DDL}"))
            .map_err(|e| BackendError::Permanent(e.to_string()))?;
        Ok(Sidecar {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> BackendResult<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| BackendError::Permanent("sidecar connection mutex poisoned".into()))
    }

    // ---- dispatch_log -------------------------------------------------------

    /// Append a dispatch-log entry and bump the revision counter atomically.
    pub(crate) fn dispatch_log_add(
        &self,
        task_id: i64,
        worktree: &str,
        decision: &str,
        rationale: &str,
    ) -> BackendResult<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO dispatch_log (task_id, worktree, decision, rationale) \
             VALUES (?1, ?2, ?3, ?4)",
            params![task_id, worktree, decision, rationale],
        )
        .map_err(map_sqlite)?;
        // Bump the revision counter in the same lock hold so the counter always
        // advances in step with the dispatch_log row.
        conn.execute(
            "UPDATE revision_counter SET counter = counter + 1 WHERE id = 1",
            [],
        )
        .map_err(map_sqlite)?;
        Ok(())
    }

    /// List the most recent `limit` dispatch-log entries, newest first.
    pub(crate) fn dispatch_log_list(&self, limit: i64) -> BackendResult<Vec<DispatchLogEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, task_id, worktree, decision, rationale, created_at \
                 FROM dispatch_log ORDER BY created_at DESC, id DESC LIMIT ?1",
            )
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map(params![limit], row_to_dispatch_log_entry)
            .map_err(map_sqlite)?
            .collect::<rusqlite::Result<Vec<DispatchLogEntry>>>()
            .map_err(map_sqlite)?;
        Ok(rows)
    }

    /// All dispatch-log entries for a given task, newest first.
    pub(crate) fn dispatch_log_for(&self, task_id: i64) -> BackendResult<Vec<DispatchLogEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, task_id, worktree, decision, rationale, created_at \
                 FROM dispatch_log WHERE task_id = ?1 ORDER BY created_at DESC, id DESC",
            )
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map(params![task_id], row_to_dispatch_log_entry)
            .map_err(map_sqlite)?
            .collect::<rusqlite::Result<Vec<DispatchLogEntry>>>()
            .map_err(map_sqlite)?;
        Ok(rows)
    }

    /// Drop every dispatch-log row for a task. Called when the task is deleted so
    /// its dispatch history doesn't outlive it (the sidecar `dispatch_log` has no
    /// FK to GitHub issues, so nothing else cleans these up). Returns the row count
    /// removed; deleting a task that was never dispatched is a no-op.
    pub(crate) fn dispatch_log_purge(&self, task_id: i64) -> BackendResult<usize> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM dispatch_log WHERE task_id = ?1",
            params![task_id],
        )
        .map_err(map_sqlite)
    }

    /// The most recent dispatch-log entry for (task_id, worktree), or `None`.
    ///
    /// Mirrors the `TaskBackend::dispatch_log_latest_for` signature so `GitHubBackend`
    /// can forward 1:1 later.
    pub(crate) fn dispatch_log_latest_for(
        &self,
        task_id: i64,
        worktree: &str,
    ) -> BackendResult<Option<DispatchLogEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, task_id, worktree, decision, rationale, created_at \
                 FROM dispatch_log WHERE task_id = ?1 AND worktree = ?2 \
                 ORDER BY created_at DESC, id DESC LIMIT 1",
            )
            .map_err(map_sqlite)?;
        let mut rows = stmt
            .query_map(params![task_id, worktree], row_to_dispatch_log_entry)
            .map_err(map_sqlite)?;
        rows.next().transpose().map_err(map_sqlite)
    }

    /// Candidate (task_id, worktree) pairs derived from the dispatch_log.
    ///
    /// Returns all rows where the most recent decision for a (task_id, worktree)
    /// pair is `'dispatched'`. The GitHub-state cross-filter (checking that the
    /// GitHub task is still open and assigned) happens later inside
    /// `GitHubBackend`; the sidecar returns raw candidates only.
    pub(crate) fn dispatched_reservations(&self) -> BackendResult<Vec<Reservation>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT dl.task_id, dl.worktree \
                 FROM dispatch_log dl \
                 WHERE dl.decision = 'dispatched' \
                   AND dl.id = ( \
                     SELECT MAX(dl2.id) FROM dispatch_log dl2 \
                     WHERE dl2.task_id = dl.task_id AND dl2.worktree = dl.worktree \
                   ) \
                 GROUP BY dl.task_id, dl.worktree",
            )
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Reservation {
                    task_id: r.get::<_, i64>(0)?,
                    worktree: r.get::<_, String>(1)?,
                })
            })
            .map_err(map_sqlite)?
            .collect::<rusqlite::Result<Vec<Reservation>>>()
            .map_err(map_sqlite)?;
        Ok(rows)
    }

    // ---- revision counter ---------------------------------------------------

    /// Return the current revision counter value (the sidecar half of the
    /// composed `"<gh-cursor>:<sidecar-seq>"` revision string).
    pub(crate) fn revision_counter(&self) -> BackendResult<u64> {
        let conn = self.lock()?;
        let counter: i64 = conn
            .query_row(
                "SELECT counter FROM revision_counter WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .map_err(map_sqlite)?;
        Ok(counter as u64)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TempDb {
        path: PathBuf,
    }
    impl TempDb {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let mut path = std::env::temp_dir();
            path.push(format!(
                "keitora-sidecar-test-{}-{}.db",
                std::process::id(),
                n
            ));
            let td = TempDb { path };
            td.cleanup();
            td
        }
        fn open(&self) -> Sidecar {
            Sidecar::open(&self.path).unwrap()
        }
        fn cleanup(&self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{}", self.path.display(), suffix));
            }
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            self.cleanup();
        }
    }

    #[test]
    fn open_creates_schema() {
        let db = TempDb::new();
        let s = db.open();
        // After open the counter is at 0 and the log is empty.
        assert_eq!(s.revision_counter().unwrap(), 0);
        assert!(s.dispatch_log_list(100).unwrap().is_empty());
    }

    #[test]
    fn open_is_idempotent() {
        // Opening the same file twice must not error.
        let db = TempDb::new();
        let _s1 = db.open();
        let _s2 = db.open(); // would fail if CREATE TABLE were not IF NOT EXISTS
    }

    #[test]
    fn dispatch_log_add_list_round_trip() {
        let db = TempDb::new();
        let s = db.open();

        s.dispatch_log_add(1, "wt-a", "dispatched", "reason A")
            .unwrap();
        s.dispatch_log_add(2, "wt-b", "queued", "reason B").unwrap();
        s.dispatch_log_add(1, "wt-a", "released", "reason C")
            .unwrap();

        let all = s.dispatch_log_list(100).unwrap();
        assert_eq!(all.len(), 3);
        // Newest first.
        assert_eq!(all[0].decision, "released");
        assert_eq!(all[1].decision, "queued");
        assert_eq!(all[2].decision, "dispatched");
    }

    #[test]
    fn dispatch_log_list_respects_limit() {
        let db = TempDb::new();
        let s = db.open();

        for i in 0..10i64 {
            s.dispatch_log_add(i, "wt", "dispatched", "").unwrap();
        }
        let limited = s.dispatch_log_list(3).unwrap();
        assert_eq!(limited.len(), 3);
    }

    #[test]
    fn dispatch_log_for_filters_by_task() {
        let db = TempDb::new();
        let s = db.open();

        s.dispatch_log_add(1, "wt-a", "dispatched", "").unwrap();
        s.dispatch_log_add(2, "wt-b", "dispatched", "").unwrap();
        s.dispatch_log_add(1, "wt-a", "released", "").unwrap();

        let for_task1 = s.dispatch_log_for(1).unwrap();
        assert_eq!(for_task1.len(), 2);
        assert!(for_task1.iter().all(|e| e.task_id == 1));

        let for_task2 = s.dispatch_log_for(2).unwrap();
        assert_eq!(for_task2.len(), 1);
        assert_eq!(for_task2[0].task_id, 2);
    }

    #[test]
    fn dispatch_log_latest_for_returns_newest() {
        let db = TempDb::new();
        let s = db.open();

        s.dispatch_log_add(1, "wt-a", "dispatched", "first")
            .unwrap();
        s.dispatch_log_add(1, "wt-a", "released", "second").unwrap();

        let entry = s.dispatch_log_latest_for(1, "wt-a").unwrap().unwrap();
        assert_eq!(entry.decision, "released");
        assert_eq!(entry.rationale, "second");
    }

    #[test]
    fn dispatch_log_latest_for_none_when_missing() {
        let db = TempDb::new();
        let s = db.open();
        // No entries at all → None.
        assert!(s.dispatch_log_latest_for(99, "wt").unwrap().is_none());
    }

    #[test]
    fn dispatched_reservations_returns_candidates() {
        let db = TempDb::new();
        let s = db.open();

        // task 1 dispatched to wt-a, then released → not a reservation.
        s.dispatch_log_add(1, "wt-a", "dispatched", "").unwrap();
        s.dispatch_log_add(1, "wt-a", "released", "").unwrap();

        // task 2 dispatched to wt-b and not superseded → IS a reservation.
        s.dispatch_log_add(2, "wt-b", "dispatched", "").unwrap();

        // task 3 queued (not dispatched) → not a reservation.
        s.dispatch_log_add(3, "wt-c", "queued", "").unwrap();

        let res = s.dispatched_reservations().unwrap();
        assert_eq!(
            res.len(),
            1,
            "expected exactly one reservation, got {res:?}"
        );
        assert_eq!(res[0].task_id, 2);
        assert_eq!(res[0].worktree, "wt-b");
    }

    #[test]
    fn dispatched_reservations_empty_when_no_log() {
        let db = TempDb::new();
        let s = db.open();
        assert!(s.dispatched_reservations().unwrap().is_empty());
    }

    #[test]
    fn revision_counter_advances_on_each_add() {
        let db = TempDb::new();
        let s = db.open();

        assert_eq!(s.revision_counter().unwrap(), 0);
        s.dispatch_log_add(1, "wt", "dispatched", "").unwrap();
        assert_eq!(s.revision_counter().unwrap(), 1);
        s.dispatch_log_add(1, "wt", "released", "").unwrap();
        assert_eq!(s.revision_counter().unwrap(), 2);
        s.dispatch_log_add(2, "wt", "dispatched", "").unwrap();
        assert_eq!(s.revision_counter().unwrap(), 3);
    }

    #[test]
    fn revision_counter_persists_across_reopen() {
        let db = TempDb::new();
        {
            let s = db.open();
            s.dispatch_log_add(1, "wt", "dispatched", "").unwrap();
            s.dispatch_log_add(1, "wt", "released", "").unwrap();
            assert_eq!(s.revision_counter().unwrap(), 2);
        }
        // Reopen the same file — counter must survive.
        let s2 = db.open();
        assert_eq!(s2.revision_counter().unwrap(), 2);
    }

    #[test]
    fn dispatch_log_fields_round_trip() {
        let db = TempDb::new();
        let s = db.open();

        s.dispatch_log_add(42, "my-worktree", "suggested", "detailed rationale here")
            .unwrap();

        let entries = s.dispatch_log_for(42).unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.task_id, 42);
        assert_eq!(e.worktree, "my-worktree");
        assert_eq!(e.decision, "suggested");
        assert_eq!(e.rationale, "detailed rationale here");
        assert!(!e.created_at.is_empty());
        assert!(e.id > 0);
    }
}
