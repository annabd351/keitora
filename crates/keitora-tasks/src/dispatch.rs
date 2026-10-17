//! Pure rendering for custom dispatch steps.
//!
//! A dispatched task's prompt lines (configured in `[dispatch].steps`, see
//! [`crate::DispatchConfig`]) are templates with a fixed placeholder set. This
//! module turns a template + a [`DispatchContext`] into the final text. It is
//! deliberately Tauri-free and fully unit-tested; the orchestration (writing the
//! lines into a PTY, the submit byte, inter-step delays) lives in the app crate.

use crate::Task;

/// Everything a dispatch step can interpolate: the reserved task plus the
/// worktree it is being dispatched into (authoritative — equals `assigned_to`
/// once the task is reserved, but passed explicitly so rendering never depends on
/// read-after-write ordering).
pub struct DispatchContext<'a> {
    pub task: &'a Task,
    pub worktree: &'a str,
}

/// The 4-digit zero-padded work number for a task id: `19` → `"0019"`. Ids with
/// more than four digits render at full width (`12345` → `"12345"`).
pub fn work_number(id: i64) -> String {
    format!("{id:04}")
}

/// Substitute the fixed placeholder set into `template`:
///
/// | placeholder          | source                                |
/// |----------------------|---------------------------------------|
/// | `{task_number}`      | [`work_number`] of the task id        |
/// | `{task_id}`          | the raw task id                       |
/// | `{task_title}`       | the task title                        |
/// | `{task_description}` | the task description                  |
/// | `{branch}`           | the task branch, or `""` when unset   |
/// | `{worktree}`         | the dispatch target worktree          |
///
/// Unknown `{...}` tokens are left literal so templates can contain stray braces
/// without surprises. Substitution is SINGLE-PASS: a substituted value is appended
/// to the output and never re-scanned, so a task title/description that happens to
/// contain a placeholder token (e.g. a title literally `"{worktree}"`) is emitted
/// verbatim rather than being recursively expanded.
pub fn interpolate(template: &str, ctx: &DispatchContext) -> String {
    let resolve = |key: &str| -> Option<String> {
        Some(match key {
            "task_number" => work_number(ctx.task.id),
            "task_id" => ctx.task.id.to_string(),
            "task_title" => ctx.task.title.clone(),
            "task_description" => ctx.task.description.clone(),
            "branch" => ctx.task.branch.clone().unwrap_or_default(),
            "worktree" => ctx.worktree.to_string(),
            _ => return None,
        })
    };

    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        // Look for the matching close brace for this token.
        match rest[open + 1..].find('}') {
            Some(close_off) => {
                let close = open + 1 + close_off;
                let key = &rest[open + 1..close];
                match resolve(key) {
                    Some(val) => out.push_str(&val),
                    None => out.push_str(&rest[open..=close]), // unknown → literal
                }
                rest = &rest[close + 1..];
            }
            // Unbalanced `{` with no closing brace: emit the remainder verbatim.
            None => {
                out.push_str(&rest[open..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Pure: a dispatch is a redundant re-apply (skip the inject) iff the task is still
/// reserved to THIS worktree (open + assigned here) and its most recent dispatch
/// decision into this worktree was already `dispatched`.
pub fn already_dispatched(
    status: &str,
    assigned_to: Option<&str>,
    worktree: &str,
    latest_decision: Option<&str>,
) -> bool {
    status == "open" && assigned_to == Some(worktree) && latest_decision == Some("dispatched")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> Task {
        Task {
            id: 19,
            title: "custom dispatch".to_string(),
            description: "wire it up".to_string(),
            status: "active".to_string(),
            priority: "high".to_string(),
            assigned_to: Some("sol".to_string()),
            created_by: None,
            tags: vec![],
            branch: Some("llm/user/keitora/0019".to_string()),
            pr_url: None,
            created_at: "2026-06-21".to_string(),
            updated_at: "2026-06-21".to_string(),
        }
    }

    #[test]
    fn work_number_pads_to_four_then_overflows() {
        assert_eq!(work_number(1), "0001");
        assert_eq!(work_number(19), "0019");
        assert_eq!(work_number(1234), "1234");
        assert_eq!(work_number(12345), "12345");
    }

    #[test]
    fn each_placeholder_substitutes() {
        let t = task();
        let ctx = DispatchContext {
            task: &t,
            worktree: "sol",
        };
        assert_eq!(interpolate("{task_number}", &ctx), "0019");
        assert_eq!(interpolate("{task_id}", &ctx), "19");
        assert_eq!(interpolate("{task_title}", &ctx), "custom dispatch");
        assert_eq!(interpolate("{task_description}", &ctx), "wire it up");
        assert_eq!(interpolate("{branch}", &ctx), "llm/user/keitora/0019");
        assert_eq!(interpolate("{worktree}", &ctx), "sol");
    }

    #[test]
    fn all_placeholders_in_one_template() {
        let t = task();
        let ctx = DispatchContext {
            task: &t,
            worktree: "sol",
        };
        assert_eq!(
            interpolate(
                "/workflow #{task_number} ({task_id}) {task_title} -> {worktree}",
                &ctx
            ),
            "/workflow #0019 (19) custom dispatch -> sol"
        );
    }

    #[test]
    fn branch_is_empty_when_none() {
        let mut t = task();
        t.branch = None;
        let ctx = DispatchContext {
            task: &t,
            worktree: "sol",
        };
        assert_eq!(interpolate("b=[{branch}]", &ctx), "b=[]");
    }

    #[test]
    fn unknown_token_left_literal_and_plain_text_unchanged() {
        let t = task();
        let ctx = DispatchContext {
            task: &t,
            worktree: "sol",
        };
        assert_eq!(interpolate("{nope} stays", &ctx), "{nope} stays");
        assert_eq!(interpolate("/clear", &ctx), "/clear");
    }

    #[test]
    fn substituted_values_are_not_re_expanded() {
        // A title that literally contains a placeholder token must be emitted
        // verbatim, NOT recursively expanded into the worktree/branch value.
        let mut t = task();
        t.title = "{worktree} and {branch}".to_string();
        t.branch = Some("BR".to_string());
        let ctx = DispatchContext {
            task: &t,
            worktree: "sol",
        };
        assert_eq!(
            interpolate("title={task_title}; wt={worktree}", &ctx),
            "title={worktree} and {branch}; wt=sol"
        );
    }

    #[test]
    fn unbalanced_open_brace_is_literal() {
        let t = task();
        let ctx = DispatchContext {
            task: &t,
            worktree: "sol",
        };
        assert_eq!(interpolate("a {task_id} b {oops", &ctx), "a 19 b {oops");
    }

    #[test]
    fn already_dispatched_truth_table() {
        // True ONLY when open + assigned to this worktree + latest decision == dispatched.
        assert!(already_dispatched(
            "open",
            Some("feng"),
            "feng",
            Some("dispatched")
        ));

        // Non-open status → false.
        assert!(!already_dispatched(
            "active",
            Some("feng"),
            "feng",
            Some("dispatched")
        ));
        assert!(!already_dispatched(
            "done",
            Some("feng"),
            "feng",
            Some("dispatched")
        ));

        // Wrong assignee → false.
        assert!(!already_dispatched(
            "open",
            Some("sayo"),
            "feng",
            Some("dispatched")
        ));

        // None assignee → false.
        assert!(!already_dispatched(
            "open",
            None,
            "feng",
            Some("dispatched")
        ));

        // Latest decision is not dispatched → false.
        assert!(!already_dispatched(
            "open",
            Some("feng"),
            "feng",
            Some("queued")
        ));
        assert!(!already_dispatched(
            "open",
            Some("feng"),
            "feng",
            Some("suggested")
        ));
        assert!(!already_dispatched("open", Some("feng"), "feng", None));
    }
}
