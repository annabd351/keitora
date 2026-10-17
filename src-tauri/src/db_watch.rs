//! Shared filesystem-watcher scaffolding for a single directory + filename family.
//!
//! Three watchers share this plumbing: `host::start_tasks_watcher` (emits
//! `tasks://changed` / `sessions://changed`) and `bg_notifier::spawn_project_watcher`
//! (fires background OS notifications) both watch a `tasks.db` / `-wal` / `-shm`
//! family, and `host::start_config_watcher` watches the active project's `keitora.toml`.
//! All need the exact same plumbing: a `notify` watcher over a
//! directory, filtered to a filename prefix, draining bursts behind a debounce, and a
//! poll loop that exits promptly when its stop channel is dropped. Only the
//! *per-event* work, the filename prefix, and the debounce differ. [`watch_dir`] owns
//! the plumbing; each caller passes its `file_prefix`, `debounce`, and an `on_event`
//! closure, so the three cannot drift.

use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

/// Watch `dir` for writes to the `file_prefix` family (any filename starting with
/// `file_prefix`, e.g. `tasks.db` / `tasks.db-wal` / `tasks.db-shm`, or `keitora.toml`)
/// and invoke `on_event` once per `debounce`d burst.
///
/// Runs **synchronously** â€” it *is* the watcher-loop body, so the caller owns the
/// thread and whatever DB connection / mutable state `on_event` captures (no
/// `Send`/`'static` bound is imposed here). Returns when the watcher fails to
/// start, when `stop_rx` is signalled (`()` sent) or its sender is dropped, or
/// when the internal event channel disconnects.
///
/// The loop polls with `recv_timeout` rather than a blocking `recv` so the stop
/// check still runs when the watched file is idle (an idle file fires no `notify`
/// events): a blocking recv would never observe the stop signal and the thread
/// would leak.
pub(crate) fn watch_dir(
    dir: PathBuf,
    file_prefix: String,
    debounce: Duration,
    stop_rx: mpsc::Receiver<()>,
    mut on_event: impl FnMut(),
) {
    use notify::{RecursiveMode, Watcher};

    let (tx, rx) = mpsc::channel::<()>();
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if ev.paths.iter().any(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with(&file_prefix))
                        .unwrap_or(false)
                }) {
                    let _ = tx.send(());
                }
            }
        }) {
            Ok(w) => w,
            Err(_) => return,
        };
    if watcher.watch(&dir, RecursiveMode::NonRecursive).is_err() {
        return;
    }

    loop {
        // Stop check: exit if this watcher was superseded (its stop sender was
        // dropped, or `()` was sent). Checked first each iteration so a stop
        // signal is honoured even right after a DB event.
        match stop_rx.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => return,
            Err(mpsc::TryRecvError::Empty) => {} // still the live watcher
        }
        // Poll with a timeout so the stop check above runs even when the DB is
        // idle. A timeout just loops back to re-check the stop flag.
        match rx.recv_timeout(Duration::from_millis(500)) {
            Err(mpsc::RecvTimeoutError::Timeout) => continue, // re-check stop
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // watcher dropped
            Ok(()) => {} // DB event â€” fall through to debounce + process
        }
        // Debounce: drain further events for `debounce` before processing once.
        let deadline = Instant::now() + debounce;
        while rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .is_ok()
        {}
        on_event();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    /// Dropping the stop sender must wind the loop down promptly even when the
    /// watched directory never changes â€” the `recv_timeout(500ms)` poll is what
    /// guarantees the disconnect is observed (a blocking recv would leak the
    /// thread). We give a generous 3 s join budget (one poll cycle is 500 ms).
    #[test]
    fn watch_dir_exits_when_stop_sender_dropped() {
        let dir = std::env::temp_dir();
        let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(0);
        let (done_tx, done_rx) = mpsc::channel::<()>();

        let handle = thread::spawn(move || {
            watch_dir(
                dir,
                "tasks.db".to_string(),
                Duration::from_millis(300),
                stop_rx,
                || {},
            );
            let _ = done_tx.send(());
        });

        // Drop the stop sender â†’ the watcher's `stop_rx` disconnects, so the loop
        // returns on its next poll.
        drop(stop_tx);

        assert!(
            done_rx.recv_timeout(Duration::from_secs(3)).is_ok(),
            "watch_dir must exit promptly after its stop sender is dropped",
        );
        handle.join().expect("watcher thread should join cleanly");
    }
}
