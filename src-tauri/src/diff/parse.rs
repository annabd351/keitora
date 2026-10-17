//! Pure parsers for git's porcelain output and unified diffs. No I/O, no subprocess —
//! every function here is a byte/string transform so the correctness-critical logic is
//! unit-testable without a repo. The diff commands in [`super`] run git and feed the raw
//! bytes through these.
//!
//! All the `*_z` parsers consume **NUL-delimited** output (`git ... -z`): paths are then
//! emitted verbatim with no `"`-quoting or octal escaping, so a path with a space, tab or
//! newline survives intact.

use serde::Serialize;

/// Per-file change kind, as surfaced to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
}

impl DiffStatus {
    /// Map a `--name-status` status token (`A`, `M`, `D`, `R100`, `C75`, `T`, …) to a kind.
    fn from_name_status(tok: &str) -> Self {
        match tok.as_bytes().first() {
            Some(b'A') => DiffStatus::Added,
            Some(b'D') => DiffStatus::Deleted,
            Some(b'R') => DiffStatus::Renamed,
            // Copy: shown like a rename (keeps the source path), close enough for a viewer.
            Some(b'C') => DiffStatus::Renamed,
            // M (modified), T (typechange), and anything unexpected read as "modified".
            _ => DiffStatus::Modified,
        }
    }
}

/// One file in the changed-file summary (the cheap, tree-populating payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffFileSummary {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: DiffStatus,
    pub added: u32,
    pub deleted: u32,
    pub binary: bool,
}

/// A `--name-status -z` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameStatusEntry {
    pub status: DiffStatus,
    pub path: String,
    pub old_path: Option<String>,
}

/// A `--numstat -z` record (keyed by the new/current path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumStatEntry {
    pub path: String,
    pub added: u32,
    pub deleted: u32,
    pub binary: bool,
}

/// An untracked file with its line count, computed by the command (file read lives there,
/// so this stays a plain value the pure merge can consume).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrackedEntry {
    pub path: String,
    pub added: u32,
    pub binary: bool,
}

/// A single diff line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffLine {
    pub kind: DiffLineKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_no: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_no: Option<u32>,
    pub text: String,
    /// Syntax-highlight spans covering `text` in order (code-point lengths). `None` when the
    /// file isn't a highlighted language or highlighting was skipped; attached after parsing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spans: Option<Vec<super::highlight::Span>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffLineKind {
    Context,
    Add,
    Del,
}

/// One `@@ … @@` hunk with its lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// The full per-file diff payload returned by `worktree_file_diff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    pub binary: bool,
    /// The diff exceeded the size cap and was not fully rendered.
    pub truncated: bool,
    pub hunks: Vec<DiffHunk>,
}

/// Split a NUL-delimited buffer into `&str` tokens, dropping a trailing empty token.
/// Lossily decodes non-UTF-8 bytes (git paths are usually UTF-8; a stray byte becomes
/// U+FFFD rather than dropping the record).
fn nul_tokens(buf: &[u8]) -> Vec<String> {
    buf.split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| String::from_utf8_lossy(r).into_owned())
        .collect()
}

/// Parse `git diff --name-status -z`.
///
/// Status and path are **separate** NUL tokens (`M\0path\0`); a rename/copy is three
/// tokens (`R100\0old\0new\0`), so this walks tokens with a tiny state machine rather than
/// zipping pairs.
pub fn parse_name_status_z(buf: &[u8]) -> Vec<NameStatusEntry> {
    let toks = nul_tokens(buf);
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let status_tok = &toks[i];
        i += 1;
        let status = DiffStatus::from_name_status(status_tok);
        let is_move = matches!(status_tok.as_bytes().first(), Some(b'R') | Some(b'C'));
        if is_move {
            let old = toks.get(i).cloned();
            let new = toks.get(i + 1).cloned();
            i += 2;
            if let (Some(old), Some(new)) = (old, new) {
                out.push(NameStatusEntry {
                    status,
                    path: new,
                    old_path: Some(old),
                });
            }
        } else if let Some(path) = toks.get(i).cloned() {
            i += 1;
            out.push(NameStatusEntry {
                status,
                path,
                old_path: None,
            });
        }
    }
    out
}

/// Parse `git diff --numstat -z`.
///
/// A record is `added\tdeleted\t<path>` as one NUL token for an ordinary change; for a
/// rename the counts token ends in a trailing tab (`added\tdeleted\t`) and the old/new
/// paths follow as two more NUL tokens. `-\t-` in the count columns marks a binary file.
pub fn parse_numstat_z(buf: &[u8]) -> Vec<NumStatEntry> {
    let toks = nul_tokens(buf);
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let tok = &toks[i];
        i += 1;
        // Only the first two TABs are field separators; a path may itself contain a TAB.
        let mut parts = tok.splitn(3, '\t');
        let a = parts.next().unwrap_or("");
        let d = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        let binary = a == "-" && d == "-";
        let added = a.parse::<u32>().unwrap_or(0);
        let deleted = d.parse::<u32>().unwrap_or(0);
        if rest.is_empty() {
            // Rename: the new path is the SECOND of the two following tokens.
            let new = toks.get(i + 1).cloned();
            i += 2;
            if let Some(new) = new {
                out.push(NumStatEntry {
                    path: new,
                    added,
                    deleted,
                    binary,
                });
            }
        } else {
            out.push(NumStatEntry {
                path: rest.to_string(),
                added,
                deleted,
                binary,
            });
        }
    }
    out
}

/// Join name-status (authoritative for status + rename) with numstat (counts + binary) and
/// the separately-discovered untracked files into the final summary. Pure.
pub fn merge_summary(
    name_status: Vec<NameStatusEntry>,
    numstat: Vec<NumStatEntry>,
    untracked: Vec<UntrackedEntry>,
) -> Vec<DiffFileSummary> {
    use std::collections::HashMap;
    let counts: HashMap<&str, &NumStatEntry> =
        numstat.iter().map(|n| (n.path.as_str(), n)).collect();
    let mut out: Vec<DiffFileSummary> = name_status
        .iter()
        .map(|ns| {
            let c = counts.get(ns.path.as_str());
            DiffFileSummary {
                path: ns.path.clone(),
                old_path: ns.old_path.clone(),
                status: ns.status,
                added: c.map(|c| c.added).unwrap_or(0),
                deleted: c.map(|c| c.deleted).unwrap_or(0),
                binary: c.map(|c| c.binary).unwrap_or(false),
            }
        })
        .collect();
    out.extend(untracked.into_iter().map(|u| DiffFileSummary {
        path: u.path,
        old_path: None,
        status: DiffStatus::Untracked,
        added: u.added,
        deleted: 0,
        binary: u.binary,
    }));
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Classified paths from `git status --porcelain=v2 -z --untracked-files=all`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PorcelainV2 {
    /// Tracked paths with staged and/or unstaged changes (`1`, `2`, and `u` records).
    pub dirty_tracked: Vec<String>,
    /// Original paths of renames/copies (`2` records). The committed diff may pair a
    /// rename's new path to one of these, so the overlay must stat the origin too.
    pub rename_origins: Vec<String>,
    /// Untracked paths (`?` records).
    pub untracked: Vec<String>,
}

/// Parse porcelain **v2** NUL-delimited status. Record shapes (space-separated fields, then
/// the path as the final field so embedded spaces survive):
/// - `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`                       — ordinary change
/// - `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>` + **next token** `<origPath>`
/// - `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`            — unmerged
/// - `? <path>` — untracked · `! <path>` — ignored (skipped)
///
/// A `2` record's original path is the *following* NUL token (the `-z` form splits it out),
/// so this walks tokens with an index rather than filtering, consuming that extra token.
pub fn parse_porcelain_v2_z(buf: &[u8]) -> PorcelainV2 {
    let toks = nul_tokens(buf);
    let mut out = PorcelainV2::default();
    let mut i = 0;
    while i < toks.len() {
        let tok = &toks[i];
        i += 1;
        let mut bytes = tok.bytes();
        match (bytes.next(), bytes.next()) {
            (Some(b'1'), Some(b' ')) => {
                if let Some(path) = nth_field(tok, 8) {
                    out.dirty_tracked.push(path.to_string());
                }
            }
            (Some(b'2'), Some(b' ')) => {
                if let Some(path) = nth_field(tok, 9) {
                    out.dirty_tracked.push(path.to_string());
                }
                // The original path is the next NUL token.
                if let Some(orig) = toks.get(i) {
                    out.rename_origins.push(orig.clone());
                    i += 1;
                }
            }
            (Some(b'u'), Some(b' ')) => {
                if let Some(path) = nth_field(tok, 10) {
                    out.dirty_tracked.push(path.to_string());
                }
            }
            (Some(b'?'), Some(b' ')) => {
                out.untracked.push(tok[2..].to_string());
            }
            // `!` ignored, blank, or anything unexpected: skip.
            _ => {}
        }
    }
    out
}

/// Return the field at index `n` (0-based) of a space-split record **as the remainder** —
/// i.e. everything from field `n` onward, so a path containing spaces is returned whole.
/// `None` if the record has fewer than `n+1` fields.
fn nth_field(record: &str, n: usize) -> Option<&str> {
    let mut rest = record;
    for _ in 0..n {
        let sp = rest.find(' ')?;
        rest = &rest[sp + 1..];
    }
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

/// Merge the committed (tree↔tree) rows with the working-tree overlay and untracked files.
///
/// `dirty` is the set of paths the overlay is authoritative for (dirty tracked paths, rename
/// origins, and the committed-side counterparts pulled in by the caller). A committed row is
/// kept only when neither its `path` nor its `old_path` is in `dirty` — otherwise the working
/// tree, not the commit, decides that path's current state (an edit on top of the commit, or
/// a revert back to base that drops the row entirely). All `overlay` rows and `untracked`
/// entries are then appended. Pure.
///
/// Kept committed rows and overlay rows are disjoint by construction: the overlay is computed
/// from the `dirty` pathspec, so every overlay path is in `dirty`, while every kept committed
/// row's `path` is *not* in `dirty` — so appending needs no extra dedup pass.
pub fn merge_snapshot(
    committed: Vec<DiffFileSummary>,
    overlay: Vec<DiffFileSummary>,
    dirty: &std::collections::HashSet<String>,
    untracked: Vec<UntrackedEntry>,
) -> Vec<DiffFileSummary> {
    let mut out: Vec<DiffFileSummary> = committed
        .into_iter()
        .filter(|r| {
            !dirty.contains(&r.path)
                && !r
                    .old_path
                    .as_ref()
                    .map(|o| dirty.contains(o))
                    .unwrap_or(false)
        })
        .collect();
    out.extend(overlay);
    out.extend(untracked.into_iter().map(|u| DiffFileSummary {
        path: u.path,
        old_path: None,
        status: DiffStatus::Untracked,
        added: u.added,
        deleted: 0,
        binary: u.binary,
    }));
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn parse_range(s: &str) -> (u32, u32) {
    match s.split_once(',') {
        Some((start, len)) => (start.parse().unwrap_or(0), len.parse().unwrap_or(0)),
        None => (s.parse().unwrap_or(0), 1),
    }
}

/// Parse a hunk header line `@@ -oldStart,oldLines +newStart,newLines @@ heading`.
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32, String)> {
    let rest = line.strip_prefix("@@ ")?;
    let end = rest.find(" @@")?;
    let ranges = &rest[..end];
    let heading = rest[end + 3..].trim().to_string();
    let mut it = ranges.split(' ');
    let old = it.next()?.strip_prefix('-')?;
    let new = it.next()?.strip_prefix('+')?;
    let (os, ol) = parse_range(old);
    let (ns, nl) = parse_range(new);
    Some((os, ol, ns, nl, heading))
}

/// Parse a single-file unified diff (`git diff` output for one path) into typed hunks.
/// Lines before the first `@@` (the `diff --git`/`index`/`---`/`+++` headers) are ignored;
/// a `Binary files … differ` line sets `binary`.
pub fn parse_unified_diff(text: &str) -> FileDiff {
    let mut out = FileDiff::default();
    let mut cur: Option<DiffHunk> = None;
    let (mut old_no, mut new_no) = (0u32, 0u32);

    for line in text.split('\n') {
        if line.starts_with("@@ ") {
            if let Some(h) = cur.take() {
                out.hunks.push(h);
            }
            if let Some((os, ol, ns, nl, header)) = parse_hunk_header(line) {
                old_no = os;
                new_no = ns;
                cur = Some(DiffHunk {
                    old_start: os,
                    old_lines: ol,
                    new_start: ns,
                    new_lines: nl,
                    header,
                    lines: Vec::new(),
                });
            }
            continue;
        }
        if cur.is_none() {
            if line.starts_with("Binary files ") && line.ends_with(" differ") {
                out.binary = true;
            }
            continue; // pre-hunk header noise
        }
        let hunk = cur.as_mut().unwrap();
        match line.as_bytes().first() {
            Some(b' ') => {
                hunk.lines.push(DiffLine {
                    kind: DiffLineKind::Context,
                    old_no: Some(old_no),
                    new_no: Some(new_no),
                    text: line[1..].to_string(),
                    spans: None,
                });
                old_no += 1;
                new_no += 1;
            }
            Some(b'+') => {
                hunk.lines.push(DiffLine {
                    kind: DiffLineKind::Add,
                    old_no: None,
                    new_no: Some(new_no),
                    text: line[1..].to_string(),
                    spans: None,
                });
                new_no += 1;
            }
            Some(b'-') => {
                hunk.lines.push(DiffLine {
                    kind: DiffLineKind::Del,
                    old_no: Some(old_no),
                    new_no: None,
                    text: line[1..].to_string(),
                    spans: None,
                });
                old_no += 1;
            }
            // "\ No newline at end of file" and stray blank trailing split — ignore.
            _ => {}
        }
    }
    if let Some(h) = cur.take() {
        out.hunks.push(h);
    }
    out
}

/// Build an all-additions `FileDiff` from an untracked file's contents (used by the
/// command, which reads the file; kept here so the synthesis is tested alongside parsing).
pub fn untracked_file_diff(contents: &str) -> FileDiff {
    let lines: Vec<&str> = if contents.is_empty() {
        Vec::new()
    } else {
        contents
            .strip_suffix('\n')
            .unwrap_or(contents)
            .split('\n')
            .collect()
    };
    let n = lines.len() as u32;
    let hunk = DiffHunk {
        old_start: 0,
        old_lines: 0,
        new_start: 1,
        new_lines: n,
        header: String::new(),
        lines: lines
            .into_iter()
            .enumerate()
            .map(|(i, t)| DiffLine {
                kind: DiffLineKind::Add,
                old_no: None,
                new_no: Some(i as u32 + 1),
                text: t.to_string(),
                spans: None,
            })
            .collect(),
    };
    FileDiff {
        binary: false,
        truncated: false,
        hunks: if n == 0 { Vec::new() } else { vec![hunk] },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(parts: &[&str]) -> Vec<u8> {
        let mut b = Vec::new();
        for p in parts {
            b.extend_from_slice(p.as_bytes());
            b.push(0);
        }
        b
    }

    #[test]
    fn name_status_modify_add_delete() {
        let buf = z(&["M", "src/a.rs", "A", "src/b.rs", "D", "src/c.rs"]);
        let got = parse_name_status_z(&buf);
        assert_eq!(got.len(), 3);
        assert_eq!(
            got[0],
            NameStatusEntry {
                status: DiffStatus::Modified,
                path: "src/a.rs".into(),
                old_path: None
            }
        );
        assert_eq!(got[1].status, DiffStatus::Added);
        assert_eq!(got[2].status, DiffStatus::Deleted);
    }

    #[test]
    fn name_status_rename_has_two_paths() {
        let buf = z(&["R100", "old/name.rs", "new/name.rs", "M", "x.rs"]);
        let got = parse_name_status_z(&buf);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].status, DiffStatus::Renamed);
        assert_eq!(got[0].path, "new/name.rs");
        assert_eq!(got[0].old_path.as_deref(), Some("old/name.rs"));
        assert_eq!(got[1].path, "x.rs");
    }

    #[test]
    fn name_status_preserves_paths_with_spaces() {
        let buf = z(&["M", "dir with space/f a.rs"]);
        let got = parse_name_status_z(&buf);
        assert_eq!(got[0].path, "dir with space/f a.rs");
    }

    #[test]
    fn numstat_normal_binary_and_rename() {
        // normal, binary (-\t-), rename (trailing tab + two path tokens)
        let buf = z(&[
            "8\t3\tsrc/a.rs",
            "-\t-\tassets/logo.png",
            "4\t2\t",
            "old.rs",
            "new.rs",
        ]);
        let got = parse_numstat_z(&buf);
        assert_eq!(got.len(), 3);
        assert_eq!(
            got[0],
            NumStatEntry {
                path: "src/a.rs".into(),
                added: 8,
                deleted: 3,
                binary: false
            }
        );
        assert_eq!(
            got[1],
            NumStatEntry {
                path: "assets/logo.png".into(),
                added: 0,
                deleted: 0,
                binary: true
            }
        );
        assert_eq!(
            got[2],
            NumStatEntry {
                path: "new.rs".into(),
                added: 4,
                deleted: 2,
                binary: false
            }
        );
    }

    #[test]
    fn merge_joins_status_counts_and_untracked() {
        let ns = vec![
            NameStatusEntry {
                status: DiffStatus::Modified,
                path: "b.rs".into(),
                old_path: None,
            },
            NameStatusEntry {
                status: DiffStatus::Renamed,
                path: "a_new.rs".into(),
                old_path: Some("a_old.rs".into()),
            },
        ];
        let numstat = vec![
            NumStatEntry {
                path: "b.rs".into(),
                added: 5,
                deleted: 1,
                binary: false,
            },
            NumStatEntry {
                path: "a_new.rs".into(),
                added: 2,
                deleted: 2,
                binary: false,
            },
        ];
        let untracked = vec![UntrackedEntry {
            path: "z.rs".into(),
            added: 10,
            binary: false,
        }];
        let got = merge_summary(ns, numstat, untracked);
        // sorted by path: a_new.rs, b.rs, z.rs
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].path, "a_new.rs");
        assert_eq!(got[0].status, DiffStatus::Renamed);
        assert_eq!(got[0].old_path.as_deref(), Some("a_old.rs"));
        assert_eq!(got[0].added, 2);
        assert_eq!(got[1].path, "b.rs");
        assert_eq!(got[1].added, 5);
        assert_eq!(got[2].path, "z.rs");
        assert_eq!(got[2].status, DiffStatus::Untracked);
        assert_eq!(got[2].added, 10);
    }

    #[test]
    fn unified_diff_tracks_line_numbers() {
        let text = "diff --git a/f.rs b/f.rs\nindex 111..222 100644\n--- a/f.rs\n+++ b/f.rs\n@@ -10,3 +10,4 @@ fn demo()\n ctx\n-removed\n+added one\n+added two\n ctx2\n";
        let d = parse_unified_diff(text);
        assert!(!d.binary);
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!(
            (h.old_start, h.old_lines, h.new_start, h.new_lines),
            (10, 3, 10, 4)
        );
        assert_eq!(h.header, "fn demo()");
        assert_eq!(h.lines.len(), 5);
        // context line keeps both numbers
        assert_eq!(
            h.lines[0],
            DiffLine {
                kind: DiffLineKind::Context,
                old_no: Some(10),
                new_no: Some(10),
                text: "ctx".into(),
                spans: None,
            }
        );
        // removed: old number only, advances old counter
        assert_eq!(
            h.lines[1],
            DiffLine {
                kind: DiffLineKind::Del,
                old_no: Some(11),
                new_no: None,
                text: "removed".into(),
                spans: None,
            }
        );
        // added lines: new numbers 11, 12
        assert_eq!(
            h.lines[2],
            DiffLine {
                kind: DiffLineKind::Add,
                old_no: None,
                new_no: Some(11),
                text: "added one".into(),
                spans: None,
            }
        );
        assert_eq!(h.lines[3].new_no, Some(12));
        // trailing context: old 12 (10 ctx +1, 11 removed +1), new 13 (10 ctx, 11+12 added)
        assert_eq!(
            h.lines[4],
            DiffLine {
                kind: DiffLineKind::Context,
                old_no: Some(12),
                new_no: Some(13),
                text: "ctx2".into(),
                spans: None,
            }
        );
    }

    #[test]
    fn unified_diff_default_range_len_is_one() {
        let text = "@@ -5 +5 @@\n-a\n+b\n";
        let d = parse_unified_diff(text);
        let h = &d.hunks[0];
        assert_eq!((h.old_lines, h.new_lines), (1, 1));
    }

    #[test]
    fn unified_diff_multi_hunk() {
        let text = "@@ -1,1 +1,1 @@\n-a\n+b\n@@ -10,1 +10,1 @@\n-c\n+d\n";
        let d = parse_unified_diff(text);
        assert_eq!(d.hunks.len(), 2);
        assert_eq!(d.hunks[1].old_start, 10);
    }

    #[test]
    fn unified_diff_binary_marker() {
        let text = "diff --git a/x.png b/x.png\nBinary files a/x.png and b/x.png differ\n";
        let d = parse_unified_diff(text);
        assert!(d.binary);
        assert!(d.hunks.is_empty());
    }

    #[test]
    fn unified_diff_ignores_no_newline_marker() {
        let text = "@@ -1,1 +1,1 @@\n-old\n+new\n\\ No newline at end of file\n";
        let d = parse_unified_diff(text);
        assert_eq!(d.hunks[0].lines.len(), 2);
    }

    #[test]
    fn untracked_synthesizes_all_add_hunk() {
        let d = untracked_file_diff("line1\nline2\nline3\n");
        assert_eq!(d.hunks.len(), 1);
        assert_eq!(d.hunks[0].lines.len(), 3);
        assert!(d.hunks[0].lines.iter().all(|l| l.kind == DiffLineKind::Add));
        assert_eq!(d.hunks[0].lines[2].new_no, Some(3));
    }

    #[test]
    fn untracked_empty_file_has_no_hunks() {
        assert!(untracked_file_diff("").hunks.is_empty());
    }

    fn row(path: &str, status: DiffStatus) -> DiffFileSummary {
        DiffFileSummary {
            path: path.into(),
            old_path: None,
            status,
            added: 1,
            deleted: 0,
            binary: false,
        }
    }

    #[test]
    fn porcelain_v2_classifies_record_kinds() {
        let buf = z(&[
            "1 .M N... 100644 100644 100644 aaaa bbbb src/a.rs",
            "1 M. N... 100644 100644 100644 cccc dddd src/staged.rs",
            "2 R. N... 100644 100644 100644 eeee ffff R100 src/new.rs",
            "old/orig.rs",
            "u UU N... 100644 100644 100644 000000 g1 g2 g3 src/conflict.rs",
            "? build/output.bin",
            "! ignored/thing.txt",
        ]);
        let v2 = parse_porcelain_v2_z(&buf);
        assert_eq!(
            v2.dirty_tracked,
            vec!["src/a.rs", "src/staged.rs", "src/new.rs", "src/conflict.rs"]
        );
        assert_eq!(v2.rename_origins, vec!["old/orig.rs"]);
        assert_eq!(v2.untracked, vec!["build/output.bin"]);
    }

    #[test]
    fn porcelain_v2_preserves_paths_with_spaces() {
        let buf = z(&["1 .M N... 100644 100644 100644 aaaa bbbb dir with space/f a.rs"]);
        let v2 = parse_porcelain_v2_z(&buf);
        assert_eq!(v2.dirty_tracked, vec!["dir with space/f a.rs"]);
    }

    #[test]
    fn porcelain_v2_empty_and_truncated_are_safe() {
        assert_eq!(parse_porcelain_v2_z(b""), PorcelainV2::default());
        // A `2` record with no following origin token must not panic.
        let buf = z(&["2 R. N... 100644 100644 100644 eeee ffff R100 src/new.rs"]);
        let v2 = parse_porcelain_v2_z(&buf);
        assert_eq!(v2.dirty_tracked, vec!["src/new.rs"]);
        assert!(v2.rename_origins.is_empty());
        // A short `1` record (no path) is skipped, not panicked on.
        assert_eq!(parse_porcelain_v2_z(&z(&["1 .M"])), PorcelainV2::default());
    }

    #[test]
    fn merge_snapshot_committed_only() {
        let committed = vec![row("a.rs", DiffStatus::Modified)];
        let got = merge_snapshot(committed, vec![], &Default::default(), vec![]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "a.rs");
    }

    #[test]
    fn merge_snapshot_overlay_wins_over_committed() {
        let committed = vec![row("a.rs", DiffStatus::Modified)];
        let overlay = vec![DiffFileSummary {
            added: 9,
            ..row("a.rs", DiffStatus::Modified)
        }];
        let dirty: std::collections::HashSet<String> = ["a.rs".to_string()].into_iter().collect();
        let got = merge_snapshot(committed, overlay, &dirty, vec![]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].added, 9, "overlay row replaces the committed one");
    }

    #[test]
    fn merge_snapshot_reverted_committed_path_drops() {
        // Committed change to a.rs, but a.rs is dirty and the overlay has NO row for it
        // (reverted back to base in the working tree) → the row disappears.
        let committed = vec![row("a.rs", DiffStatus::Modified)];
        let dirty: std::collections::HashSet<String> = ["a.rs".to_string()].into_iter().collect();
        let got = merge_snapshot(committed, vec![], &dirty, vec![]);
        assert!(got.is_empty());
    }

    #[test]
    fn merge_snapshot_committed_rename_suppressed_by_dirty_origin() {
        // Committed rename old.rs -> new.rs; old.rs is in dirty (rename origin re-touched),
        // so the committed rename row is dropped and the overlay re-describes it.
        let committed = vec![DiffFileSummary {
            old_path: Some("old.rs".into()),
            ..row("new.rs", DiffStatus::Renamed)
        }];
        let overlay = vec![row("new.rs", DiffStatus::Modified)];
        let dirty: std::collections::HashSet<String> = ["old.rs".to_string(), "new.rs".to_string()]
            .into_iter()
            .collect();
        let got = merge_snapshot(committed, overlay, &dirty, vec![]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].status, DiffStatus::Modified);
    }

    #[test]
    fn merge_snapshot_appends_untracked_sorted() {
        let committed = vec![row("z.rs", DiffStatus::Modified)];
        let untracked = vec![UntrackedEntry {
            path: "a_new.rs".into(),
            added: 3,
            binary: false,
        }];
        let got = merge_snapshot(committed, vec![], &Default::default(), untracked);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].path, "a_new.rs", "sorted by path");
        assert_eq!(got[0].status, DiffStatus::Untracked);
        assert_eq!(got[1].path, "z.rs");
    }
}
