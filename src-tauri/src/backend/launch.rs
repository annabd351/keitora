//! `launch` — spawn a long-lived helper process (the herdr server) so it
//! outlives the GUI.
//!
//! On Windows the GUI (WebView2/Tauri) may run inside a Job Object configured
//! `KILL_ON_JOB_CLOSE`, which would take a merely-detached child down when the app
//! exits. The helper therefore launches with `CREATE_BREAKAWAY_FROM_JOB` (escapes
//! job teardown) and `CREATE_NEW_PROCESS_GROUP` (escapes console-group signals such
//! as Ctrl+C / Ctrl+Break). `taskkill /T` on Keitora still walks the parent-pid
//! tree: breakaway guards job teardown, the new group guards console-group signals.
//! The helper also gets no console at all (`DETACHED_PROCESS`, so no hidden conhost
//! is created alongside it).
//! On unix the child gets its own process group so app exit does not signal it.

use std::io;
use std::process::{Child, Command};

/// How to launch one detached helper. Ignored on unix (a new process group is
/// always used there).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LaunchProfile {
    /// Request `CREATE_BREAKAWAY_FROM_JOB` (retried without it if the job forbids it).
    pub breakaway: bool,
}

pub(crate) const NEW_PROCESS_GROUP: u32 = 0x0000_0200;
pub(crate) const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub(crate) const DETACHED_PROCESS: u32 = 0x0000_0008;

/// The Windows creation flags for a helper launch. Pure; compiled on every OS so it is
/// unit-testable everywhere.
// Only the Windows spawn path reads it outside tests.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn launch_flags(breakaway: bool) -> u32 {
    let mut f = NEW_PROCESS_GROUP | CREATE_NO_WINDOW | DETACHED_PROCESS;
    if breakaway {
        f |= CREATE_BREAKAWAY_FROM_JOB;
    }
    f
}

/// Whether a failed spawn should be retried without breakaway. A parent job
/// without `JOB_OBJECT_LIMIT_BREAKAWAY_OK` makes `CreateProcessW` fail (typically
/// `ERROR_ACCESS_DENIED`) rather than silently ignore the flag, so any failure of a
/// breakaway spawn is retried; the error itself is not inspected.
// Only the Windows spawn path reads it outside tests.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn should_retry_without_breakaway(profile: LaunchProfile, _err: &io::Error) -> bool {
    profile.breakaway
}

/// Spawn `cmd` so it outlives the GUI and its job; returns the [`Child`]. The
/// caller sets up stdio before calling.
#[cfg(windows)]
pub(crate) fn spawn_detached(cmd: &mut Command, profile: LaunchProfile) -> io::Result<Child> {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS alone is not enough to outlive the GUI: the app's Job
    // Object may be KILL_ON_JOB_CLOSE. CREATE_BREAKAWAY_FROM_JOB escapes that job;
    // CREATE_NEW_PROCESS_GROUP detaches console signals. CREATE_NO_WINDOW keeps a
    // console-subsystem child from flashing a window (its stdio is redirected by
    // the caller, so it needs no visible console).
    cmd.creation_flags(launch_flags(profile.breakaway));
    match cmd.spawn() {
        Ok(child) => Ok(child),
        Err(e) if should_retry_without_breakaway(profile, &e) => {
            // Breakaway is not a silent best-effort: retry without it so the child
            // at least starts (it then shares the GUI's job lifetime). Otherwise the
            // app would hang waiting for a helper that never launched.
            log::warn!(
                "detached spawn with CREATE_BREAKAWAY_FROM_JOB failed ({e}); retrying without breakaway"
            );
            cmd.creation_flags(launch_flags(false));
            cmd.spawn()
        }
        Err(e) => Err(e),
    }
}

/// Spawn `cmd` in its own process group so app exit does not signal it; the
/// profile is ignored on unix.
#[cfg(unix)]
pub(crate) fn spawn_detached(cmd: &mut Command, profile: LaunchProfile) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;
    let _ = profile;
    cmd.process_group(0);
    cmd.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_flags_with_breakaway_is_group_breakaway_detached_no_window() {
        assert_eq!(
            launch_flags(true),
            NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW | DETACHED_PROCESS
        );
    }

    #[test]
    fn launch_flags_without_breakaway_drops_only_breakaway() {
        let f = launch_flags(false);
        assert_eq!(f, NEW_PROCESS_GROUP | CREATE_NO_WINDOW | DETACHED_PROCESS);
        assert_eq!(f & CREATE_BREAKAWAY_FROM_JOB, 0);
    }

    #[test]
    fn retry_without_breakaway_only_when_breakaway_was_requested() {
        let denied = io::Error::from_raw_os_error(5);
        let other = io::Error::other("boom");
        let with = LaunchProfile { breakaway: true };
        let without = LaunchProfile { breakaway: false };
        assert!(should_retry_without_breakaway(with, &denied));
        assert!(should_retry_without_breakaway(with, &other));
        assert!(!should_retry_without_breakaway(without, &denied));
        assert!(!should_retry_without_breakaway(without, &other));
    }
}
