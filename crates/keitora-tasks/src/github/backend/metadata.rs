//! The Keitora metadata block embedded in an issue body.
//!
//! GitHub Issues + Projects v2 cannot model everything Keitora tracks per task
//! (dependency edges, the originating actor, branch/PR, an idempotency marker,
//! and a small activity log). We persist these as an HTML-comment block appended
//! to the issue body, so the *human-authored* description is preserved verbatim
//! and the block is invisible in the rendered GitHub UI.
//!
//! Format (a single HTML comment, line-oriented `key = value` pairs):
//!
//! ```text
//! <the human description, preserved verbatim>
//!
//! <!-- keitora:meta
//! created_by = alice
//! idempotency_key = abc-123
//! branch = feature/x
//! pr_url = https://github.com/o/r/pull/9
//! blocked_by = [3, 7]
//! log = 2026-01-01T00:00:00Z|alice|created|
//! log = 2026-01-02T00:00:00Z|alice|claimed|
//! -->
//! ```
//!
//! Parsing tolerates a missing block (older / externally-created issues): then
//! every field is empty and the whole body is the human description.

use crate::model::LogEntry;

const BLOCK_OPEN: &str = "<!-- keitora:meta";
const BLOCK_CLOSE: &str = "-->";

/// The parsed contents of the Keitora metadata block.
#[derive(Debug, Clone, Default)]
pub(crate) struct Metadata {
    pub created_by: Option<String>,
    pub idempotency_key: Option<String>,
    pub branch: Option<String>,
    pub pr_url: Option<String>,
    /// Dependency edges: ids this task is blocked by.
    pub blocked_by: Vec<i64>,
    /// Oldest-first activity log.
    pub logs: Vec<LogEntry>,
}

impl Metadata {
    /// Parse the metadata block out of an issue body. Absent block → defaults.
    pub(crate) fn parse(body: &str) -> Metadata {
        let mut meta = Metadata::default();
        let Some(block) = extract_block(body) else {
            return meta;
        };
        for line in block.lines() {
            let line = line.trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            match key {
                "created_by" => meta.created_by = non_empty(value),
                "idempotency_key" => meta.idempotency_key = non_empty(value),
                "branch" => meta.branch = non_empty(value),
                "pr_url" => meta.pr_url = non_empty(value),
                "blocked_by" => meta.blocked_by = parse_id_list(value),
                "log" => {
                    if let Some(entry) = parse_log(value) {
                        meta.logs.push(entry);
                    }
                }
                _ => {}
            }
        }
        meta
    }

    /// The human-authored description (the body with the metadata block removed).
    pub(crate) fn human_body(&self, body: &str) -> String {
        strip_block(body)
    }
}

/// Render a full issue body = human description + the metadata block.
pub(crate) fn render_body(human: &str, meta: &Metadata) -> String {
    let mut block = String::new();
    block.push_str(BLOCK_OPEN);
    block.push('\n');
    if let Some(v) = &meta.created_by {
        block.push_str(&format!("created_by = {}\n", sanitize(v)));
    }
    if let Some(v) = &meta.idempotency_key {
        block.push_str(&format!("idempotency_key = {}\n", sanitize(v)));
    }
    if let Some(v) = &meta.branch {
        block.push_str(&format!("branch = {}\n", sanitize(v)));
    }
    if let Some(v) = &meta.pr_url {
        block.push_str(&format!("pr_url = {}\n", sanitize(v)));
    }
    if !meta.blocked_by.is_empty() {
        let ids: Vec<String> = meta.blocked_by.iter().map(|d| d.to_string()).collect();
        block.push_str(&format!("blocked_by = [{}]\n", ids.join(", ")));
    }
    for log in &meta.logs {
        block.push_str(&format!(
            "log = {}|{}|{}|{}\n",
            sanitize(&log.created_at),
            sanitize(&log.worktree),
            sanitize(&log.action),
            sanitize(&log.detail)
        ));
    }
    block.push_str(BLOCK_CLOSE);

    let human = human.trim_end();
    if human.is_empty() {
        block
    } else {
        format!("{human}\n\n{block}")
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// The sentinel a literal `|` is escaped to so it can't be mistaken for the
/// log-field separator (`parse_log` splits on `|`). U+2223 DIVIDES is visually
/// close but never appears unescaped in our output. `unescape_field` reverses it.
const PIPE_SENTINEL: char = '\u{2223}';

/// Strip newlines and the comment terminator so a value can't break the block,
/// and escape the `|` log-field separator so a value containing it round-trips
/// without shifting columns (see [`parse_log`]).
fn sanitize(s: &str) -> String {
    s.replace('\n', " ")
        .replace("-->", "—>")
        .replace('|', &PIPE_SENTINEL.to_string())
}

/// Reverse [`sanitize`]'s `|` escaping for a parsed log field.
fn unescape_field(s: &str) -> String {
    s.replace(PIPE_SENTINEL, "|")
}

fn parse_id_list(value: &str) -> Vec<i64> {
    let inner = value.trim().trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .filter_map(|p| p.trim().parse::<i64>().ok())
        .collect()
}

fn parse_log(value: &str) -> Option<LogEntry> {
    let mut parts = value.splitn(4, '|');
    let created_at = unescape_field(parts.next()?);
    let worktree = unescape_field(parts.next().unwrap_or(""));
    let action = unescape_field(parts.next().unwrap_or(""));
    let detail = unescape_field(parts.next().unwrap_or(""));
    Some(LogEntry {
        created_at,
        worktree,
        action,
        detail,
    })
}

/// Return the inner text of the metadata block (between the markers), if present.
///
/// Keitora always appends its block at the **end** of the body, so we anchor on the
/// **last** `BLOCK_OPEN` (`rfind`): user-authored prose that happens to contain
/// the literal `<!-- keitora:meta` can't hijack parsing or truncate the real block.
fn extract_block(body: &str) -> Option<&str> {
    let start = body.rfind(BLOCK_OPEN)?;
    let after = &body[start + BLOCK_OPEN.len()..];
    let end = after.find(BLOCK_CLOSE)?;
    Some(&after[..end])
}

/// Return the body with the metadata block (and any trailing blank lines that
/// preceded it) removed. Anchored on the **last** `BLOCK_OPEN` (see
/// [`extract_block`]) so human prose containing the literal marker is preserved.
fn strip_block(body: &str) -> String {
    match body.rfind(BLOCK_OPEN) {
        Some(idx) => body[..idx].trim_end().to_string(),
        None => body.trim_end().to_string(),
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_all_fields() {
        let meta = Metadata {
            created_by: Some("alice".into()),
            idempotency_key: Some("k-1".into()),
            branch: Some("feat/x".into()),
            pr_url: Some("https://example/pr/1".into()),
            blocked_by: vec![3, 7],
            logs: vec![LogEntry {
                created_at: "2026-01-01T00:00:00Z".into(),
                worktree: "alice".into(),
                action: "created".into(),
                detail: "".into(),
            }],
        };
        let body = render_body("human text", &meta);
        let parsed = Metadata::parse(&body);
        assert_eq!(parsed.created_by.as_deref(), Some("alice"));
        assert_eq!(parsed.idempotency_key.as_deref(), Some("k-1"));
        assert_eq!(parsed.branch.as_deref(), Some("feat/x"));
        assert_eq!(parsed.pr_url.as_deref(), Some("https://example/pr/1"));
        assert_eq!(parsed.blocked_by, vec![3, 7]);
        assert_eq!(parsed.logs.len(), 1);
        assert_eq!(parsed.logs[0].action, "created");
        assert_eq!(parsed.human_body(&body), "human text");
    }

    #[test]
    fn absent_block_is_all_human() {
        let body = "just a plain issue body";
        let parsed = Metadata::parse(body);
        assert!(parsed.created_by.is_none());
        assert!(parsed.blocked_by.is_empty());
        assert_eq!(parsed.human_body(body), body);
    }

    #[test]
    fn empty_human_yields_block_only() {
        let meta = Metadata {
            created_by: Some("bob".into()),
            ..Default::default()
        };
        let body = render_body("", &meta);
        assert!(body.starts_with(BLOCK_OPEN));
        assert_eq!(Metadata::parse(&body).human_body(&body), "");
    }

    #[test]
    fn human_prose_containing_block_marker_round_trips() {
        // A user-authored description that literally contains "<!-- keitora:meta"
        // must not hijack parsing: anchoring on the LAST marker (rfind) keeps the
        // real (appended) block authoritative and the prose intact.
        let human = "Investigate why the parser chokes on `<!-- keitora:meta` in user text.";
        let meta = Metadata {
            created_by: Some("alice".into()),
            blocked_by: vec![4],
            logs: vec![LogEntry {
                created_at: "2026-01-01T00:00:00Z".into(),
                worktree: "alice".into(),
                action: "created".into(),
                detail: "".into(),
            }],
            ..Default::default()
        };
        let body = render_body(human, &meta);
        let parsed = Metadata::parse(&body);
        // The real block still parses (not the decoy in the prose).
        assert_eq!(parsed.created_by.as_deref(), Some("alice"));
        assert_eq!(parsed.blocked_by, vec![4]);
        assert_eq!(parsed.logs.len(), 1);
        // The human body is preserved verbatim, decoy marker and all.
        assert_eq!(parsed.human_body(&body), human);
    }

    #[test]
    fn log_detail_containing_pipe_round_trips() {
        // A log field containing the `|` separator must survive without shifting
        // columns (sanitize escapes it; parse_log unescapes).
        let meta = Metadata {
            logs: vec![LogEntry {
                created_at: "2026-01-01T00:00:00Z".into(),
                worktree: "wt|a".into(),
                action: "updated".into(),
                detail: "title,priority | branch".into(),
            }],
            ..Default::default()
        };
        let body = render_body("desc", &meta);
        let parsed = Metadata::parse(&body);
        assert_eq!(parsed.logs.len(), 1);
        assert_eq!(parsed.logs[0].created_at, "2026-01-01T00:00:00Z");
        assert_eq!(parsed.logs[0].worktree, "wt|a");
        assert_eq!(parsed.logs[0].action, "updated");
        assert_eq!(parsed.logs[0].detail, "title,priority | branch");
    }

    #[test]
    fn rewrite_preserves_human_body() {
        let body = render_body(
            "original description",
            &Metadata {
                blocked_by: vec![1],
                ..Default::default()
            },
        );
        let mut meta = Metadata::parse(&body);
        let human = meta.human_body(&body);
        meta.blocked_by.push(2);
        let rewritten = render_body(&human, &meta);
        let reparsed = Metadata::parse(&rewritten);
        assert_eq!(reparsed.blocked_by, vec![1, 2]);
        assert_eq!(reparsed.human_body(&rewritten), "original description");
    }
}
