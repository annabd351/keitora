//! Opt-in keystroke latency probe. Off unless `KEITORA_LATENCY_PROBE=1` is set at app
//! start; when off every entry point returns immediately without locking or allocating.
//!
//! Hops recorded on the Rust side (the frontend logs its own in `latencyProbe.ts`):
//! - `ipc_queue`: frontend stamp to `pty_write` command entry (IPC + main-thread queue)
//! - `t0_to_stdin`: frontend stamp to the bridge stdin write
//! - `stdin_to_first_frame`: bridge stdin write to the next `terminal.frame` read
//!
//! Every [`BATCH`] samples of a hop log `latency-probe hop=… n= p50= p95= p99=` at info
//! and reset that hop. Correlation is "first frame after a write", which is good enough
//! for typing. The per-key side slots avoid threading probe data through `PtyBackend`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::backend::SessionKey;

/// Samples per hop between log lines.
const BATCH: usize = 200;

/// Whether the probe is on, read once from the environment.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("KEITORA_LATENCY_PROBE").is_ok_and(|v| v == "1"))
}

/// Nearest-rank percentile (`p` in 0..=100) of an ascending-sorted slice.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// p50/p95/p99 of `samples`, sorting them in place.
pub fn summarize(samples: &mut [f64]) -> (f64, f64, f64) {
    samples.sort_by(|a, b| a.total_cmp(b));
    (
        percentile(samples, 50.0),
        percentile(samples, 95.0),
        percentile(samples, 99.0),
    )
}

/// Per-hop sample buffers; hands back a full batch's log line when one completes.
#[derive(Default)]
struct Hops {
    samples: HashMap<&'static str, Vec<f64>>,
}

impl Hops {
    fn record(&mut self, hop: &'static str, ms: f64) -> Option<String> {
        let buf = self.samples.entry(hop).or_default();
        buf.push(ms);
        if buf.len() < BATCH {
            return None;
        }
        let n = buf.len();
        let (p50, p95, p99) = summarize(buf);
        buf.clear();
        Some(format!(
            "latency-probe hop={hop} n={n} p50={p50:.2} p95={p95:.2} p99={p99:.2}"
        ))
    }
}

#[derive(Default)]
struct State {
    hops: Hops,
    /// Frontend stamp (epoch ms) from the latest `pty_write`, per session.
    pending_t0: HashMap<String, f64>,
    /// When the latest bridge stdin write happened, per session.
    last_write: HashMap<String, Instant>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}

fn slot(project: &str, worktree: &str) -> String {
    format!("{project}\u{0}{worktree}")
}

fn now_epoch_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// Records one sample for `hop`, logging when a batch completes.
pub fn record(hop: &'static str, dur_ms: f64) {
    if !enabled() {
        return;
    }
    let line = state().lock().unwrap().hops.record(hop, dur_ms);
    if let Some(line) = line {
        log::info!("{line}");
    }
}

/// `pty_write` entry: records `ipc_queue` and remembers `t0` for the bridge write.
pub fn on_command_entry(key: &SessionKey, t0: Option<f64>) {
    if !enabled() {
        return;
    }
    let Some(t0) = t0 else { return };
    record("ipc_queue", (now_epoch_ms() - t0).max(0.0));
    state()
        .lock()
        .unwrap()
        .pending_t0
        .insert(slot(&key.project, &key.worktree), t0);
}

/// A bridge stdin write for `key` just completed: records `t0_to_stdin` (when a stamp is
/// pending) and arms the first-frame measurement.
pub fn on_stdin_written(key: &SessionKey) {
    if !enabled() {
        return;
    }
    let s = slot(&key.project, &key.worktree);
    let t0 = {
        let mut st = state().lock().unwrap();
        st.last_write.insert(s.clone(), Instant::now());
        st.pending_t0.remove(&s)
    };
    if let Some(t0) = t0 {
        record("t0_to_stdin", (now_epoch_ms() - t0).max(0.0));
    }
}

/// A `terminal.frame` was read for this session: records `stdin_to_first_frame` if a
/// write is outstanding, then disarms it.
pub fn on_frame(project: &str, worktree: &str) {
    if !enabled() {
        return;
    }
    let at = state()
        .lock()
        .unwrap()
        .last_write
        .remove(&slot(project, worktree));
    if let Some(at) = at {
        record("stdin_to_first_frame", at.elapsed().as_secs_f64() * 1000.0);
    }
}

/// Lets the frontend learn once whether to stamp writes.
#[tauri::command]
pub async fn latency_probe_enabled() -> bool {
    enabled()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 50.0), 50.0);
        assert_eq!(percentile(&v, 95.0), 95.0);
        assert_eq!(percentile(&v, 99.0), 99.0);
        assert_eq!(percentile(&v, 100.0), 100.0);
        assert_eq!(percentile(&v, 0.0), 1.0);
    }

    #[test]
    fn percentile_edge_cases() {
        assert_eq!(percentile(&[], 50.0), 0.0);
        assert_eq!(percentile(&[7.0], 99.0), 7.0);
    }

    #[test]
    fn summarize_sorts_unordered_input() {
        let mut v = vec![5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(summarize(&mut v), (3.0, 5.0, 5.0));
    }

    #[test]
    fn hops_log_once_per_batch_and_reset() {
        let mut hops = Hops::default();
        for i in 0..BATCH - 1 {
            assert!(hops.record("a", i as f64).is_none());
        }
        // A different hop does not share the batch counter.
        assert!(hops.record("b", 1.0).is_none());
        let line = hops
            .record("a", (BATCH - 1) as f64)
            .expect("batch complete");
        assert!(line.starts_with("latency-probe hop=a n=200 p50="), "{line}");
        assert!(line.contains("p95=") && line.contains("p99="));
        assert!(
            hops.record("a", 0.0).is_none(),
            "buffer reset after logging"
        );
    }

    #[test]
    fn disabled_entry_points_are_noops() {
        // The env var is unset under test, so nothing may be recorded or stored.
        assert!(!enabled());
        let key = SessionKey {
            project: "p".into(),
            worktree: "w".into(),
        };
        on_command_entry(&key, Some(1.0));
        on_stdin_written(&key);
        on_frame("p", "w");
        record("x", 1.0);
        let st = state().lock().unwrap();
        assert!(st.pending_t0.is_empty() && st.last_write.is_empty() && st.hops.samples.is_empty());
    }
}
