//! `headroom` — how much memory the machine can still give.
//!
//! A [`HeadroomSnapshot`] carries the two resources that matter before starting
//! more sessions: memory the system could still *commit* (exhausting it is what
//! kills the herdr server) and *physical* memory immediately available (with
//! plenty of commit but no free RAM the machine is thrashing). Values are
//! absolute bytes, never percentages: a growable pagefile moves the commit limit.
//!
//! [`SystemHeadroom`] samples the OS (Windows `GlobalMemoryStatusEx`, Linux
//! `/proc/meminfo`, macOS `sysinfo` + the `kern.memorystatus_vm_pressure_level`
//! sysctl). Consumers hold an `Arc<dyn Headroom>` so tests inject
//! [`FakeHeadroom`]. This module is the single home of the headroom types.

#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::Mutex;

pub const MIB: u64 = 1024 * 1024;

/// Physical-memory margin that must stay free on top of the per-session cost.
/// Fixed (the commit margin is the configurable one).
pub const PHYS_MARGIN_BYTES: u64 = 1024 * MIB;

/// Bytes. Absolute, never a percentage: a growable pagefile moves the commit limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadroomSnapshot {
    /// Memory the system could still commit (Windows: ullAvailPageFile; Linux:
    /// MemAvailable + SwapFree; macOS: available + free swap).
    pub commit_avail: u64,
    /// Physical memory immediately available (Windows: ullAvailPhys; Linux:
    /// MemAvailable; macOS: sysinfo available).
    pub phys_avail: u64,
    /// OS-reported pressure level. Only macOS sets it; `None` elsewhere.
    pub pressure: Option<MemoryPressure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPressure {
    Normal,
    Warn,
    Critical,
}

impl MemoryPressure {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryPressure::Normal => "normal",
            MemoryPressure::Warn => "warn",
            MemoryPressure::Critical => "critical",
        }
    }
}

pub trait Headroom: Send + Sync {
    /// Sample now. `None` when the OS query fails (never panics, never blocks on
    /// I/O beyond one syscall / one small file read / one `sysctl` process on macOS).
    fn sample(&self) -> Option<HeadroomSnapshot>;
}

/// Real implementation over the per-OS probe below. Zero-sized.
pub struct SystemHeadroom;

impl Headroom for SystemHeadroom {
    fn sample(&self) -> Option<HeadroomSnapshot> {
        sample_os()
    }
}

#[cfg(windows)]
fn sample_os() -> Option<HeadroomSnapshot> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut st = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `st` is a live, correctly sized MEMORYSTATUSEX with dwLength set, as
    // the API requires.
    unsafe { GlobalMemoryStatusEx(&mut st) }.ok()?;
    // `ullAvailPageFile` is the smaller of the system and per-process commit limits,
    // minus the commit charge. Keitora runs outside a commit-limited job, so this is
    // the system value; if it ever reads as a small per-process number, switch to
    // `GetPerformanceInfo` (CommitLimit - CommitTotal, times PageSize).
    Some(HeadroomSnapshot {
        commit_avail: st.ullAvailPageFile,
        phys_avail: st.ullAvailPhys,
        pressure: None,
    })
}

#[cfg(target_os = "macos")]
fn sample_os() -> Option<HeadroomSnapshot> {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let phys_avail = sys.available_memory();
    let commit_avail = phys_avail.saturating_add(sys.free_swap());
    // A failed sysctl only loses the pressure level; the byte counts still stand.
    let pressure = std::process::Command::new("sysctl")
        .args(["-n", "kern.memorystatus_vm_pressure_level"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| parse_pressure_level(&String::from_utf8_lossy(&o.stdout)));
    Some(HeadroomSnapshot {
        commit_avail,
        phys_avail,
        pressure,
    })
}

#[cfg(target_os = "linux")]
fn sample_os() -> Option<HeadroomSnapshot> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo(&text)
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn sample_os() -> Option<HeadroomSnapshot> {
    None
}

/// Parse `/proc/meminfo`: `MemAvailable` (kB) is `phys_avail`; `commit_avail` is
/// `MemAvailable + SwapFree`. Lines look like `MemAvailable:    8123456 kB`.
/// Unknown/malformed lines are skipped; missing `MemAvailable` is `None`; missing
/// `SwapFree` counts as 0.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_meminfo(text: &str) -> Option<HeadroomSnapshot> {
    let mut avail_kb: Option<u64> = None;
    let mut swap_free_kb: u64 = 0;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(value) = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        match key.trim() {
            "MemAvailable" => avail_kb = Some(value),
            "SwapFree" => swap_free_kb = value,
            _ => {}
        }
    }
    let avail_kb = avail_kb?;
    Some(HeadroomSnapshot {
        commit_avail: avail_kb.saturating_add(swap_free_kb).saturating_mul(1024),
        phys_avail: avail_kb.saturating_mul(1024),
        pressure: None,
    })
}

/// `kern.memorystatus_vm_pressure_level` output: `1` Normal, `2` Warn, `4`
/// Critical (trimmed); anything else is `None`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn parse_pressure_level(s: &str) -> Option<MemoryPressure> {
    match s.trim() {
        "1" => Some(MemoryPressure::Normal),
        "2" => Some(MemoryPressure::Warn),
        "4" => Some(MemoryPressure::Critical),
        _ => None,
    }
}

/// Commit requirement: `base + per_session * sessions`, saturating.
pub fn required_bytes(base: u64, per_session: u64, sessions: u32) -> u64 {
    base.saturating_add(per_session.saturating_mul(u64::from(sessions)))
}

/// Physical requirement: `PHYS_MARGIN_BYTES + per_session * sessions`, saturating.
/// With a 600 MiB per-session cost and one session this is 1624 MiB.
pub fn required_phys_bytes(per_session: u64, sessions: u32) -> u64 {
    required_bytes(PHYS_MARGIN_BYTES, per_session, sessions)
}

/// True iff both resources meet their requirement and the OS pressure level is not
/// Warn/Critical. Commit is what ran out in the observed server deaths, but free
/// commit with almost no free physical memory is thrashing, so both are checked.
pub fn meets(snap: &HeadroomSnapshot, required_commit: u64, required_phys: u64) -> bool {
    let pressured = matches!(
        snap.pressure,
        Some(MemoryPressure::Warn | MemoryPressure::Critical)
    );
    snap.commit_avail >= required_commit && snap.phys_avail >= required_phys && !pressured
}

/// `commit_avail_mib=… phys_avail_mib=…[ pressure=…]`, or `headroom=unknown`.
pub fn format_fields(snap: Option<&HeadroomSnapshot>) -> String {
    match snap {
        None => "headroom=unknown".to_string(),
        Some(s) => {
            let mut out = format!(
                "commit_avail_mib={} phys_avail_mib={}",
                s.commit_avail / MIB,
                s.phys_avail / MIB
            );
            if let Some(p) = s.pressure {
                out.push_str(" pressure=");
                out.push_str(p.as_str());
            }
            out
        }
    }
}

/// Scripted [`Headroom`] for tests.
#[cfg(test)]
pub struct FakeHeadroom {
    script: Mutex<VecDeque<Option<HeadroomSnapshot>>>,
    last: Mutex<Option<HeadroomSnapshot>>,
    calls: AtomicUsize,
}

#[cfg(test)]
impl FakeHeadroom {
    /// Always returns `Some(s)`.
    pub fn constant(s: HeadroomSnapshot) -> Self {
        Self::scripted(vec![Some(s)])
    }

    /// Always returns `None`.
    pub fn unavailable() -> Self {
        Self::scripted(vec![None])
    }

    /// Pops one value per call; once exhausted, repeats the last popped value.
    pub fn scripted(v: Vec<Option<HeadroomSnapshot>>) -> Self {
        FakeHeadroom {
            script: Mutex::new(v.into()),
            last: Mutex::new(None),
            calls: AtomicUsize::new(0),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
impl Headroom for FakeHeadroom {
    fn sample(&self) -> Option<HeadroomSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut last = self.last.lock().unwrap();
        if let Some(next) = self.script.lock().unwrap().pop_front() {
            *last = next;
        }
        *last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * MIB;

    fn snap(commit: u64, phys: u64) -> HeadroomSnapshot {
        HeadroomSnapshot {
            commit_avail: commit,
            phys_avail: phys,
            pressure: None,
        }
    }

    #[test]
    fn parse_meminfo_full_fixture() {
        let text = "MemTotal:       16303428 kB\n\
                    MemFree:         1234567 kB\n\
                    MemAvailable:    8123456 kB\n\
                    Buffers:          123456 kB\n\
                    SwapTotal:       2097148 kB\n\
                    SwapFree:        1048576 kB\n";
        let s = parse_meminfo(text).expect("parses");
        assert_eq!(s.phys_avail, 8123456 * 1024);
        assert_eq!(s.commit_avail, (8123456 + 1048576) * 1024);
        assert_eq!(s.pressure, None);
    }

    #[test]
    fn parse_meminfo_missing_swap_free_counts_zero() {
        let s = parse_meminfo("MemAvailable: 1000 kB\n").expect("parses");
        assert_eq!(s.commit_avail, s.phys_avail);
        assert_eq!(s.phys_avail, 1000 * 1024);
    }

    #[test]
    fn parse_meminfo_missing_mem_available_is_none() {
        assert_eq!(parse_meminfo("MemTotal: 1000 kB\nSwapFree: 5 kB\n"), None);
    }

    #[test]
    fn parse_meminfo_garbage_or_empty_is_none() {
        assert_eq!(parse_meminfo(""), None);
        assert_eq!(parse_meminfo("not meminfo at all\n:::\n"), None);
    }

    #[test]
    fn parse_meminfo_skips_non_numeric_value() {
        let text = "MemAvailable: lots kB\nMemAvailable: 2000 kB\nSwapFree: x kB\n";
        let s = parse_meminfo(text).expect("parses");
        assert_eq!(s.phys_avail, 2000 * 1024);
        assert_eq!(s.commit_avail, 2000 * 1024);
    }

    #[test]
    fn parse_pressure_level_known_and_unknown() {
        assert_eq!(parse_pressure_level("1\n"), Some(MemoryPressure::Normal));
        assert_eq!(parse_pressure_level("2"), Some(MemoryPressure::Warn));
        assert_eq!(parse_pressure_level("4"), Some(MemoryPressure::Critical));
        assert_eq!(parse_pressure_level("3"), None);
        assert_eq!(parse_pressure_level(""), None);
        assert_eq!(parse_pressure_level("x"), None);
    }

    #[test]
    fn required_bytes_adds_per_session_cost() {
        assert_eq!(required_bytes(4 * GIB, 600 * MIB, 0), 4 * GIB);
        assert_eq!(required_bytes(4 * GIB, 600 * MIB, 3), 4 * GIB + 1800 * MIB);
    }

    #[test]
    fn required_bytes_saturates() {
        assert_eq!(required_bytes(u64::MAX, 1, 1), u64::MAX);
        assert_eq!(required_bytes(1, u64::MAX, u32::MAX), u64::MAX);
    }

    #[test]
    fn required_phys_bytes_uses_fixed_margin() {
        assert_eq!(required_phys_bytes(600 * MIB, 1), 1624 * MIB);
        assert_eq!(required_phys_bytes(600 * MIB, 0), GIB);
        assert_eq!(required_phys_bytes(u64::MAX, 2), u64::MAX);
    }

    #[test]
    fn meets_exact_equality_passes() {
        assert!(meets(&snap(100, 50), 100, 50));
    }

    #[test]
    fn meets_one_byte_short_on_commit_fails() {
        assert!(!meets(&snap(99, 50), 100, 50));
    }

    #[test]
    fn meets_one_byte_short_on_physical_fails_with_ample_commit() {
        assert!(!meets(&snap(u64::MAX, 49), 100, 50));
    }

    #[test]
    fn meets_fails_under_warn_or_critical_pressure() {
        for p in [MemoryPressure::Warn, MemoryPressure::Critical] {
            let s = HeadroomSnapshot {
                pressure: Some(p),
                ..snap(u64::MAX, u64::MAX)
            };
            assert!(!meets(&s, 1, 1), "{p:?} must fail the gate");
        }
        let normal = HeadroomSnapshot {
            pressure: Some(MemoryPressure::Normal),
            ..snap(u64::MAX, u64::MAX)
        };
        assert!(meets(&normal, 1, 1));
    }

    #[test]
    fn format_fields_some_none_and_pressure() {
        assert_eq!(
            format_fields(Some(&snap(5120 * MIB, 3200 * MIB))),
            "commit_avail_mib=5120 phys_avail_mib=3200"
        );
        let warn = HeadroomSnapshot {
            pressure: Some(MemoryPressure::Warn),
            ..snap(5120 * MIB, 3200 * MIB)
        };
        assert_eq!(
            format_fields(Some(&warn)),
            "commit_avail_mib=5120 phys_avail_mib=3200 pressure=warn"
        );
        assert_eq!(format_fields(None), "headroom=unknown");
    }

    #[test]
    fn fake_headroom_constant_always_returns() {
        let f = FakeHeadroom::constant(snap(1, 2));
        for _ in 0..3 {
            assert_eq!(f.sample(), Some(snap(1, 2)));
        }
        assert_eq!(f.calls(), 3);
    }

    #[test]
    fn fake_headroom_scripted_pops_then_repeats_last() {
        let f = FakeHeadroom::scripted(vec![Some(snap(1, 1)), None, Some(snap(3, 3))]);
        assert_eq!(f.sample(), Some(snap(1, 1)));
        assert_eq!(f.sample(), None);
        assert_eq!(f.sample(), Some(snap(3, 3)));
        assert_eq!(f.sample(), Some(snap(3, 3)));
        assert_eq!(f.calls(), 4);
    }

    #[test]
    fn fake_headroom_unavailable_is_none() {
        let f = FakeHeadroom::unavailable();
        assert_eq!(f.sample(), None);
        assert_eq!(f.sample(), None);
        assert_eq!(f.calls(), 2);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn system_headroom_samples_this_machine() {
        let s = SystemHeadroom.sample().expect("OS memory query succeeds");
        assert!(s.phys_avail > 0);
    }
}
