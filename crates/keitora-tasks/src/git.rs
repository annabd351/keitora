//! Reusable `git` command construction for callers outside this crate (notably the
//! Tauri app's diff subsystem). The low-level builder lives in [`crate::config`]
//! (`git_command`) with the Windows no-window flag and the unix PATH augmentation that
//! lets the LFS filter resolve `git-lfs`; this module re-exports it and adds a
//! worktree-scoped variant with optional-locks disabled.

use std::path::Path;
use std::process::Command;

/// A bare `git` [`Command`] with `.no_window()` applied and (on unix) an augmented PATH.
/// Prefer [`git_in`] for anything that reads or writes a specific worktree.
pub fn git_command() -> Command {
    crate::config::git_command()
}

/// A `git` [`Command`] scoped to `dir` with **optional locks disabled**.
///
/// These commands run against worktrees where an interactive agent may be running git
/// concurrently. `git status`/`git diff` otherwise opportunistically refresh and *write*
/// the index, which can collide with the agent's own git. `GIT_OPTIONAL_LOCKS=0` (the
/// env equivalent of the `--no-optional-locks` flag, and order-independent across
/// subcommands) makes these reads non-mutating.
pub fn git_in(dir: &Path) -> Command {
    let mut cmd = git_command();
    cmd.current_dir(dir);
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    #[test]
    fn git_command_invokes_git() {
        let cmd = git_command();
        let prog = PathBuf::from(cmd.get_program());
        assert_eq!(prog.file_stem().and_then(OsStr::to_str), Some("git"));
    }

    #[test]
    fn git_in_scopes_dir_and_disables_optional_locks() {
        let dir = PathBuf::from(if cfg!(windows) {
            "C:\\tmp\\wt"
        } else {
            "/tmp/wt"
        });
        let cmd = git_in(&dir);
        assert_eq!(cmd.get_current_dir(), Some(dir.as_path()));
        let locks = cmd
            .get_envs()
            .find(|(k, _)| *k == OsStr::new("GIT_OPTIONAL_LOCKS"))
            .and_then(|(_, v)| v)
            .and_then(OsStr::to_str);
        assert_eq!(locks, Some("0"));
    }
}
