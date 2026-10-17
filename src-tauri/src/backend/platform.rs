//! App-owned process primitives: the herdr supervisor's force-kill, and a
//! liveness probe the tests use to check a kill landed.
//!
//! - `process_is_alive(pid)` (tests only) — `pid == 0 => false`.
//! - `kill_group_kill(pgid)` — unix: `SIGKILL` to `-pgid`; windows: `TerminateProcess(pid)`.

#[cfg(unix)]
mod imp {
    /// `process_is_alive(pid)` via `kill(pid, 0)`: `Ok(())` -> alive, `Err(ESRCH)`
    /// -> gone. PID 0 is never alive.
    #[cfg(test)]
    pub fn process_is_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        kill(Pid::from_raw(pid as i32), None).is_ok()
    }

    /// Signal the process **group** of `pid` (`kill(-pgid, signal)`). The herdr
    /// server is launched as a group leader (see `launch`), so `pid == pgid` and
    /// the whole group is targeted.
    fn kill_group(pid: u32, signal: nix::sys::signal::Signal) {
        if pid == 0 {
            return;
        }
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        // Negative pid -> process group.
        let _ = kill(Pid::from_raw(-(pid as i32)), signal);
    }

    /// Kill the process group (forceful; `SIGKILL`).
    pub fn kill_group_kill(pgid: u32) {
        kill_group(pgid, nix::sys::signal::Signal::SIGKILL);
    }
}

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    /// Liveness: `OpenProcess(PROCESS_SYNCHRONIZE)` + `WaitForSingleObject(0)`.
    /// `WAIT_TIMEOUT` means the process object is not signaled -> still running. A
    /// failed `OpenProcess` (PID gone) reads as dead. PID 0 is never alive.
    #[cfg(test)]
    pub fn process_is_alive(pid: u32) -> bool {
        use windows::Win32::Foundation::WAIT_TIMEOUT;
        use windows::Win32::System::Threading::{WaitForSingleObject, PROCESS_SYNCHRONIZE};
        if pid == 0 {
            return false;
        }
        // SAFETY: open with synchronize rights, poll the object with a 0 timeout,
        // close the handle before returning regardless of outcome.
        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
                return false;
            };
            let result = WaitForSingleObject(handle, 0);
            let _ = CloseHandle(handle);
            result == WAIT_TIMEOUT
        }
    }

    /// Terminate a single process by PID (`OpenProcess(PROCESS_TERMINATE)` +
    /// `TerminateProcess`). Windows has no signals or process groups, so both kill
    /// variants collapse to the same forceful single-process terminate.
    fn terminate_pid(pid: u32) {
        if pid == 0 {
            return;
        }
        // SAFETY: open with terminate rights, terminate, close. Best-effort.
        unsafe {
            if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                let _ = TerminateProcess(handle, 1);
                let _ = CloseHandle(handle);
            }
        }
    }

    /// Forceful group kill. No Windows signal equivalent -> forceful terminate.
    pub fn kill_group_kill(pgid: u32) {
        terminate_pid(pgid);
    }
}

pub use imp::kill_group_kill;
#[cfg(test)]
pub use imp::process_is_alive;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    const UNUSED_HIGH_PID: u32 = 2_000_000_000;
    #[cfg(windows)]
    const UNUSED_HIGH_PID: u32 = 0xFFFF_FFF0;

    #[test]
    fn process_is_alive_pid_zero_is_dead() {
        assert!(!process_is_alive(0), "pid 0 must be treated as not alive");
    }

    #[test]
    fn process_is_alive_self_is_alive() {
        assert!(process_is_alive(std::process::id()));
    }

    #[test]
    fn process_is_alive_unused_high_pid_is_dead() {
        assert!(!process_is_alive(UNUSED_HIGH_PID));
    }
}
