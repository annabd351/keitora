//! `branch` — a worktree's current git branch, read straight from the `.git` metadata
//! with no subprocess.
//!
//! A linked worktree's `.git` is a *file* containing `gitdir: <path>` (absolute or relative
//! to the worktree); the branch is in that dir's `HEAD`. A main checkout's `.git` is a
//! *directory*, so `HEAD` is `<wt>/.git/HEAD`. `HEAD` is either `ref: refs/heads/<branch>`
//! or, when detached, a raw commit SHA (rendered as a short sha). Results are TTL-cached so
//! a per-tick sweep over many worktrees does negligible I/O.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Reuse a worktree's resolved branch for at most this long. Short enough that a real
/// branch switch shows up promptly; long enough to coalesce repeated ticks.
const BRANCH_TTL: Duration = Duration::from_secs(30);

/// Parse the `gitdir: <path>` line of a linked worktree's `.git` *file*. Returns the raw
/// path string (may be relative to the worktree). Pure.
fn parse_gitdir(contents: &str) -> Option<String> {
    for line in contents.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("gitdir:") {
            let p = rest.trim();
            if !p.is_empty() {
                return Some(p.to_string());
            }
        }
    }
    None
}

/// Parse a git `HEAD` file's contents into a display branch. `ref: refs/heads/<b>` → `<b>`;
/// a bare 40-hex SHA → `<short7> (detached)`; anything else / empty → `None`. Pure.
fn parse_head(contents: &str) -> Option<String> {
    let head = contents.trim();
    if head.is_empty() {
        return None;
    }
    if let Some(rest) = head.strip_prefix("ref:") {
        let r = rest.trim();
        let branch = r.strip_prefix("refs/heads/").unwrap_or(r);
        return (!branch.is_empty()).then(|| branch.to_string());
    }
    // Detached HEAD: a raw commit object id.
    let is_sha = head.len() >= 7 && head.len() <= 64 && head.chars().all(|c| c.is_ascii_hexdigit());
    is_sha.then(|| format!("{}{DETACHED_SUFFIX}", &head[..7]))
}

const DETACHED_SUFFIX: &str = " (detached)";

/// `true` for a detached-HEAD display branch as produced by [`parse_head`].
pub fn is_detached(branch: &str) -> bool {
    branch.ends_with(DETACHED_SUFFIX)
}

/// Resolve the directory that holds `HEAD` for a worktree: the `gitdir:` target for a linked
/// worktree (`.git` is a file), or `<wt>/.git` for a main checkout (`.git` is a directory).
fn head_dir(worktree: &Path) -> Option<PathBuf> {
    let dot_git = worktree.join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    // `.git` is a file: follow its `gitdir:` pointer (absolute, or relative to the worktree).
    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let raw = parse_gitdir(&contents)?;
    let p = PathBuf::from(&raw);
    Some(if p.is_absolute() { p } else { worktree.join(p) })
}

/// Read a worktree's current branch from disk (no subprocess). `None` when there is no git
/// metadata, `HEAD` is missing/empty, or it is an unrecognised form.
pub fn read_branch(worktree: &Path) -> Option<String> {
    let head_path = head_dir(worktree)?.join("HEAD");
    let contents = std::fs::read_to_string(head_path).ok()?;
    parse_head(&contents)
}

/// TTL cache of per-worktree branches, so an all-worktrees sweep re-reads `HEAD` at most
/// once per [`BRANCH_TTL`] per worktree.
#[derive(Default)]
pub struct BranchCache {
    entries: HashMap<PathBuf, (Option<String>, Instant)>,
}

impl BranchCache {
    /// The worktree's branch, re-reading `HEAD` only when the cached value is older than the
    /// TTL. A failed read is cached as `None` (and retried after the TTL).
    pub fn get(&mut self, worktree: &Path) -> Option<String> {
        if let Some((val, at)) = self.entries.get(worktree) {
            if at.elapsed() < BRANCH_TTL {
                return val.clone();
            }
        }
        let val = read_branch(worktree);
        self.entries
            .insert(worktree.to_path_buf(), (val.clone(), Instant::now()));
        val
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_head_ref_strips_prefix_and_keeps_slashes() {
        assert_eq!(
            parse_head("ref: refs/heads/llm/user/keitora/0037-x\n").as_deref(),
            Some("llm/user/keitora/0037-x")
        );
        assert_eq!(parse_head("ref: refs/heads/main").as_deref(), Some("main"));
        // CRLF + trailing space tolerated.
        assert_eq!(
            parse_head("ref: refs/heads/dev \r\n").as_deref(),
            Some("dev")
        );
    }

    #[test]
    fn parse_head_detached_sha_is_short() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(parse_head(sha).as_deref(), Some("0123456 (detached)"));
        assert!(is_detached(&parse_head(sha).unwrap()));
        assert!(!is_detached("main"));
    }

    #[test]
    fn parse_head_rejects_empty_and_garbage() {
        assert_eq!(parse_head(""), None);
        assert_eq!(parse_head("   \n"), None);
        assert_eq!(parse_head("not a ref or sha"), None);
        assert_eq!(parse_head("ref: "), None);
    }

    #[test]
    fn parse_gitdir_extracts_pointer() {
        assert_eq!(
            parse_gitdir("gitdir: D:/repos/proj/.git/worktrees/alpha\n").as_deref(),
            Some("D:/repos/proj/.git/worktrees/alpha")
        );
        assert_eq!(
            parse_gitdir("gitdir: ../../.git/worktrees/wt\n").as_deref(),
            Some("../../.git/worktrees/wt")
        );
        assert_eq!(parse_gitdir("nope"), None);
    }

    #[test]
    fn read_branch_from_dot_git_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();
        let git = wt.join(".git");
        std::fs::create_dir(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(read_branch(wt).as_deref(), Some("feature/x"));
    }

    #[test]
    fn read_branch_from_linked_worktree_absolute_gitdir() {
        let tmp = tempfile::tempdir().unwrap();
        // A separate "main repo" directory holding the worktree's git dir.
        let gitdir = tmp.path().join("main/.git/worktrees/wtA");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/topic\n").unwrap();

        let wt = tmp.path().join("wtA");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();

        assert_eq!(read_branch(&wt).as_deref(), Some("topic"));
    }

    #[test]
    fn read_branch_from_linked_worktree_relative_gitdir() {
        let tmp = tempfile::tempdir().unwrap();
        let gitdir = tmp.path().join(".bare/worktrees/wtB");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/rel\n").unwrap();

        let wt = tmp.path().join("wtB");
        std::fs::create_dir_all(&wt).unwrap();
        // Relative pointer, resolved against the worktree dir.
        std::fs::write(wt.join(".git"), "gitdir: ../.bare/worktrees/wtB\n").unwrap();

        assert_eq!(read_branch(&wt).as_deref(), Some("rel"));
    }

    #[test]
    fn read_branch_missing_or_empty_head_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();
        // No .git at all.
        assert_eq!(read_branch(wt), None);
        // .git dir but no HEAD.
        std::fs::create_dir(wt.join(".git")).unwrap();
        assert_eq!(read_branch(wt), None);
        // Empty HEAD.
        std::fs::write(wt.join(".git").join("HEAD"), "\n").unwrap();
        assert_eq!(read_branch(wt), None);
    }

    #[test]
    fn cache_serves_within_ttl_and_refreshes_after() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();
        let git = wt.join(".git");
        std::fs::create_dir(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/one\n").unwrap();

        let mut cache = BranchCache::default();
        assert_eq!(cache.get(wt).as_deref(), Some("one"));

        // Change on disk, but within the TTL the cached value is served.
        std::fs::write(git.join("HEAD"), "ref: refs/heads/two\n").unwrap();
        assert_eq!(cache.get(wt).as_deref(), Some("one"), "cached within TTL");

        // Expire the entry → re-reads the new value.
        cache.entries.insert(
            wt.to_path_buf(),
            (
                Some("one".into()),
                Instant::now() - BRANCH_TTL - Duration::from_secs(1),
            ),
        );
        assert_eq!(cache.get(wt).as_deref(), Some("two"), "refreshed after TTL");
    }
}
