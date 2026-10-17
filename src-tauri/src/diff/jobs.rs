//! Per-worktree cancellation for the expensive per-file `git diff`. When the user clicks
//! through files quickly, each new request for a worktree supersedes the previous one: the
//! prior child is killed so we don't burn CPU diffing a file the user already navigated away
//! from. The frontend additionally ignores out-of-order responses by request id, so this is
//! purely a resource optimization — correctness does not depend on it.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

/// Hard cap on captured stdout, so a pathological single file can't balloon memory. The
/// command marks the result truncated when the diff is this large.
const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;

type ChildHandle = Arc<Mutex<Option<Child>>>;

#[derive(Default)]
struct Slot {
    generation: u64,
    child: Option<ChildHandle>,
}

/// Managed-state registry of in-flight per-file diff jobs, keyed by an opaque job key
/// (worktree + path). Clone shares the registry.
#[derive(Clone, Default)]
pub struct DiffJobs {
    inner: Arc<Mutex<HashMap<String, Slot>>>,
}

impl DiffJobs {
    /// Begin a job for `key`: advance the generation and kill+drop any previous child for
    /// this key. Returns this job's generation.
    fn begin(&self, key: &str) -> u64 {
        let mut map = self.inner.lock().expect("diff jobs mutex");
        let slot = map.entry(key.to_string()).or_default();
        slot.generation += 1;
        if let Some(child) = slot.child.take() {
            kill(&child);
        }
        slot.generation
    }

    /// Attach the spawned child so a later request can kill it. If we were already
    /// superseded between spawn and attach, kill immediately.
    fn attach(&self, key: &str, generation: u64, child: ChildHandle) {
        let mut map = self.inner.lock().expect("diff jobs mutex");
        let slot = map.entry(key.to_string()).or_default();
        if slot.generation == generation {
            slot.child = Some(child);
        } else {
            drop(map);
            kill(&child);
        }
    }

    /// True if this job is still the current one for `key`.
    fn is_current(&self, key: &str, generation: u64) -> bool {
        self.inner
            .lock()
            .ok()
            .and_then(|m| m.get(key).map(|s| s.generation == generation))
            .unwrap_or(false)
    }

    /// Clear our stored child handle if we're still current.
    fn finish(&self, key: &str, generation: u64) {
        if let Ok(mut map) = self.inner.lock() {
            if let Some(slot) = map.get_mut(key) {
                if slot.generation == generation {
                    slot.child = None;
                }
            }
        }
    }
}

fn kill(child: &ChildHandle) {
    if let Ok(mut guard) = child.lock() {
        if let Some(mut ch) = guard.take() {
            let _ = ch.kill();
            let _ = ch.wait();
        }
    }
}

/// Spawn `cmd` as a superseding job for `key`, drain stdout/stderr on separate threads to
/// avoid a pipe-buffer deadlock, and return captured stdout. `Err("superseded")` if a newer
/// request for the same key replaced this one while it ran.
///
/// `.3` of the tuple is whether the capture hit [`MAX_CAPTURE_BYTES`] (caller marks the diff
/// truncated).
pub fn run_cancellable(
    jobs: &DiffJobs,
    key: &str,
    mut cmd: Command,
) -> Result<(Vec<u8>, bool), String> {
    let generation = jobs.begin(key);

    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("spawn git: {e}"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let handle: ChildHandle = Arc::new(Mutex::new(Some(child)));
    jobs.attach(key, generation, handle.clone());

    let out_t = thread::spawn(move || drain_capped(stdout, MAX_CAPTURE_BYTES));
    let err_t = thread::spawn(move || drain_capped(stderr, 64 * 1024));

    let (stdout_bytes, truncated) = out_t.join().unwrap_or((Vec::new(), false));
    let (stderr_bytes, _) = err_t.join().unwrap_or((Vec::new(), false));

    let status = {
        let mut guard = handle.lock().expect("child mutex");
        match guard.as_mut() {
            Some(ch) => ch.wait().ok(),
            None => None, // killed by a superseding request
        }
    };
    jobs.finish(key, generation);

    if !jobs.is_current(key, generation) {
        return Err("superseded".to_string());
    }
    match status {
        Some(s) if s.success() => Ok((stdout_bytes, truncated)),
        Some(_) => Err(String::from_utf8_lossy(&stderr_bytes).trim().to_string()),
        None => Err("superseded".to_string()),
    }
}

fn drain_capped<R: Read>(reader: Option<R>, cap: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut truncated = false;
    if let Some(mut r) = reader {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match r.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() < cap {
                        let take = n.min(cap - buf.len());
                        buf.extend_from_slice(&chunk[..take]);
                        if take < n {
                            truncated = true;
                        }
                    } else {
                        truncated = true;
                    }
                }
                Err(_) => break,
            }
        }
    }
    (buf, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_advances_generation_per_key() {
        let jobs = DiffJobs::default();
        assert_eq!(jobs.begin("a"), 1);
        assert_eq!(jobs.begin("a"), 2);
        // independent key has its own counter
        assert_eq!(jobs.begin("b"), 1);
        assert_eq!(jobs.begin("a"), 3);
    }

    #[test]
    fn newer_generation_supersedes_older() {
        let jobs = DiffJobs::default();
        let g1 = jobs.begin("a");
        // a second request for the same key begins, advancing the generation
        let g2 = jobs.begin("a");
        assert!(g2 > g1);
        assert!(!jobs.is_current("a", g1));
        assert!(jobs.is_current("a", g2));
    }

    #[test]
    fn same_worktree_new_file_supersedes_previous() {
        // Per-worktree keying: selecting a second file (same worktree key) supersedes the
        // first file's in-flight diff so we don't keep diffing a file the user left.
        let jobs = DiffJobs::default();
        let wt = "alpha";
        let g_file1 = jobs.begin(wt);
        assert!(jobs.is_current(wt, g_file1));
        let g_file2 = jobs.begin(wt);
        assert!(g_file2 > g_file1);
        assert!(
            !jobs.is_current(wt, g_file1),
            "first file's job is superseded"
        );
        assert!(jobs.is_current(wt, g_file2));
    }

    #[test]
    fn finish_only_clears_current_job() {
        let jobs = DiffJobs::default();
        let g1 = jobs.begin("a");
        let handle: ChildHandle = Arc::new(Mutex::new(None));
        jobs.attach("a", g1, handle);
        // a newer job arrives
        let _g2 = jobs.begin("a");
        // the old job finishing must not clobber the newer job's slot
        jobs.finish("a", g1);
        assert!(jobs.is_current("a", _g2));
    }
}
