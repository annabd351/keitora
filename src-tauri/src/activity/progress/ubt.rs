//! `ubt.rs` — Unreal Build Tool log parsing.
//!
//! Installed-engine UBT writes one shared log at `%LOCALAPPDATA%/UnrealBuildTool/Log.txt`
//! (source engines log to `<engine>/Engine/Programs/UnrealBuildTool/Log.txt` instead — a
//! documented secondary we don't resolve here). Builds hold UBT's single-instance mutex, so
//! the log holds one run at a time. [`parse_line`] turns a line into a [`ProgressDelta`]:
//! `Resolved project argument` / `Command line: … -Project=` attribution, `Using … run N
//! action(s)` totals, `[done/total] Verb [x64] file` steps, and `Result: Succeeded|Failed`.
//! Retry/UBA noise (`[NoUba]`, `Exited with error code`) is skipped so counts never regress.

use std::path::PathBuf;

use crate::backend::herdr::keymap::normalize_either_separator;

use super::ProgressDelta;

/// Accepted step verbs (→ `phase`).
const STEP_VERBS: &[&str] = &["Compile", "Link", "Resource", "Copy", "Generate", "Build"];

/// The shared installed-engine UBT log: `%LOCALAPPDATA%/UnrealBuildTool/Log.txt`.
/// (Source-engine builds log under `<engine>/Engine/Programs/UnrealBuildTool/Log.txt`; not
/// resolved here — the installed location is the supported case.)
pub fn ubt_log_path() -> Option<PathBuf> {
    Some(
        dirs::data_local_dir()?
            .join("UnrealBuildTool")
            .join("Log.txt"),
    )
}

/// Parse one UBT log line into a progress delta, accumulating the first error line into
/// `message` for the eventual `Result: Failed` summary.
pub fn parse_line(line: &str, message: &mut Option<String>) -> Option<ProgressDelta> {
    // Retry/UBA noise must not move the count.
    if line.contains("[NoUba]") || line.contains("Exited with error code") {
        return None;
    }
    if let Some(path) = parse_resolved(line) {
        return Some(ProgressDelta::Attribute(path));
    }
    // The `Command line: … -Project=<path>` line also identifies the run's project.
    if line.contains("Command line:") {
        if let Some(path) = extract_project_from_cmdline(line) {
            return Some(ProgressDelta::Attribute(path));
        }
    }
    if line.contains("Result: Succeeded") {
        return Some(ProgressDelta::Succeeded);
    }
    if let Some(idx) = line.find("Result: Failed") {
        let suffix = line[idx + "Result: ".len()..].trim().to_string();
        let summary = message.clone().unwrap_or(suffix);
        return Some(ProgressDelta::Failed(Some(summary)));
    }
    if let Some((done, total, phase)) = parse_step(line) {
        return Some(ProgressDelta::Step {
            done,
            total: Some(total),
            phase: Some(phase),
        });
    }
    if let Some(n) = parse_total(line) {
        return Some(ProgressDelta::Total(n));
    }
    // Error lines only set the failure summary (first one), never a delta.
    if message.is_none() {
        if let Some(msg) = parse_error(line) {
            *message = Some(truncate(msg, 200));
        }
    }
    None
}

/// Extract and normalize the `-Project=<path>` token from a `Command line:` log line
/// (quotes stripped). `None` when absent.
pub fn extract_project_from_cmdline(line: &str) -> Option<String> {
    const PREFIX: &str = "-Project=";
    // The path may contain spaces only when quoted; unquoted it runs to the next space.
    let idx = find_ascii_ci(line, PREFIX)?;
    let rest = &line[idx + PREFIX.len()..];
    let path = if let Some(stripped) = rest.strip_prefix('"') {
        stripped.split('"').next().unwrap_or("")
    } else if let Some(stripped) = rest.strip_prefix('\'') {
        stripped.split('\'').next().unwrap_or("")
    } else {
        rest.split_whitespace().next().unwrap_or("")
    };
    let trimmed = path.trim();
    (!trimmed.is_empty()).then(|| normalize_either_separator(trimmed))
}

/// Extract and normalize the `-Project=<path>` value from an already-split argument list
/// (a live process's argv). Works per argument, so a path with spaces survives; one
/// surrounding pair of `"` or `'` is stripped. `None` when absent or empty.
pub fn extract_project_from_args(args: &[String]) -> Option<String> {
    let value = args
        .iter()
        .find_map(|a| strip_prefix_ascii_ci(a, "-Project="))?;
    let trimmed = strip_quote_pair(value.trim()).trim();
    (!trimmed.is_empty()).then(|| normalize_either_separator(trimmed))
}

/// Whether a UBT argv is a real build: no `-Mode=` argument, or `-Mode=Build`
/// (case-insensitive). Any other mode (`QueryTargets`, `JsonExport`,
/// `GenerateProjectFiles`, … which IDEs run in the background) is not a build.
pub fn is_build_mode(args: &[String]) -> bool {
    match args.iter().find_map(|a| strip_prefix_ascii_ci(a, "-Mode=")) {
        None => true,
        Some(mode) => strip_quote_pair(mode.trim())
            .trim()
            .eq_ignore_ascii_case("Build"),
    }
}

/// `s` with an ASCII `prefix` removed, compared case-insensitively.
fn strip_prefix_ascii_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.as_bytes().get(..prefix.len())?;
    // An ASCII match guarantees `prefix.len()` is a char boundary.
    head.eq_ignore_ascii_case(prefix.as_bytes())
        .then(|| &s[prefix.len()..])
}

/// Strip one surrounding pair of `"` or `'`, if present.
fn strip_quote_pair(s: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

/// Case-insensitive ASCII substring search → byte index of the first match.
fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    if nb.is_empty() || hb.len() < nb.len() {
        return None;
    }
    (0..=hb.len() - nb.len()).find(|&i| hb[i..i + nb.len()].eq_ignore_ascii_case(nb))
}

/// Map a normalized project path to the worktree whose normalized path is its longest
/// component-prefix.
pub fn project_to_worktree(project: &str, worktrees: &[(String, String)]) -> Option<String> {
    // Tolerate either slash form and any casing on both sides; a relative or short-form
    // path simply fails to match.
    let project = normalize_either_separator(project);
    let mut best: Option<&str> = None;
    let mut best_len = 0;
    for (name, path) in worktrees {
        let path = normalize_either_separator(path);
        if is_component_prefix(&project, &path) && path.len() > best_len {
            best_len = path.len();
            best = Some(name);
        }
    }
    best.map(str::to_string)
}

/// `true` when `wt`'s components are a leading prefix of `path` (`path == wt` or starts
/// with `wt/`). A plain string prefix does not match (`…/wt2` is not under `…/wt`).
/// Both are compared case-insensitively (Windows paths; drive-letter casing varies).
pub fn is_component_prefix(path: &str, wt: &str) -> bool {
    if wt.is_empty() {
        return false;
    }
    let (pl, wl) = (path.to_ascii_lowercase(), wt.to_ascii_lowercase());
    pl == wl || (pl.len() > wl.len() && pl.starts_with(&wl) && pl.as_bytes()[wl.len()] == b'/')
}

/// Parse a `Resolved project argument` line → the normalized project path.
fn parse_resolved(line: &str) -> Option<String> {
    let idx = line.find("Resolved project argument")?;
    let rest = &line[idx + "Resolved project argument".len()..];
    // Formats: `… argument <path> to <path>` and `… argument: <path>`.
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':').unwrap_or(rest).trim();
    let path = match rest.split_once(" to ") {
        Some((before, _)) => before.trim(),
        None => rest,
    };
    if path.is_empty() {
        return None;
    }
    Some(normalize_either_separator(path))
}

/// Parse a `^\s*[done/total] Verb` step line. Rejects `done > total` / `total == 0`.
fn parse_step(line: &str) -> Option<(u32, u32, String)> {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= b.len() || b[i] != b'[' {
        return None;
    }
    i += 1;
    let (done, after_done) = take_u32(b, i)?;
    i = after_done;
    if i >= b.len() || b[i] != b'/' {
        return None;
    }
    i += 1;
    let (total, after_total) = take_u32(b, i)?;
    i = after_total;
    if i >= b.len() || b[i] != b']' {
        return None;
    }
    i += 1;
    let mut saw_space = false;
    while i < b.len() && b[i] == b' ' {
        i += 1;
        saw_space = true;
    }
    if !saw_space {
        return None;
    }
    let verb_start = i;
    while i < b.len() && b[i].is_ascii_alphanumeric() {
        i += 1;
    }
    let verb = &line[verb_start..i];
    if !STEP_VERBS.contains(&verb) {
        return None;
    }
    if total == 0 || done > total {
        return None;
    }
    Some((done, total, verb.to_string()))
}

/// Parse a `Using … executor to run N action(s)` line → `N`.
fn parse_total(line: &str) -> Option<u32> {
    let idx = line.find("executor to run")?;
    let rest = line[idx + "executor to run".len()..].trim_start();
    let b = rest.as_bytes();
    let (n, _) = take_u32(b, 0)?;
    if n == 0 {
        return None;
    }
    Some(n)
}

/// An `error C<digits>` / `error:` line → the trimmed line (failure summary source).
fn parse_error(line: &str) -> Option<&str> {
    if line.contains("error:") || has_error_code(line) {
        let t = line.trim();
        if !t.is_empty() {
            return Some(t);
        }
    }
    None
}

/// `true` when `line` contains `error C` immediately followed by a digit.
fn has_error_code(line: &str) -> bool {
    let mut from = 0;
    while let Some(rel) = line[from..].find("error C") {
        let i = from + rel + "error C".len();
        if line.as_bytes().get(i).is_some_and(u8::is_ascii_digit) {
            return true;
        }
        from = from + rel + 1;
    }
    false
}

/// Parse a run of ASCII digits at `b[i..]` → `(value, next_index)` (saturating `None` on
/// overflow / no digit).
fn take_u32(b: &[u8], mut i: usize) -> Option<(u32, usize)> {
    let start = i;
    let mut v: u64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        v = v * 10 + u64::from(b[i] - b'0');
        if v > u64::from(u32::MAX) {
            return None;
        }
        i += 1;
    }
    if i == start {
        return None;
    }
    Some((v as u32, i))
}

/// Trim `s` to at most `max` chars (char-boundary safe).
fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Option<ProgressDelta> {
        parse_line(line, &mut None)
    }

    // ── attribution ───────────────────────────────────────────────────────────────

    #[test]
    fn project_to_worktree_by_component_prefix() {
        let worktrees = vec![
            (
                "alpha".to_string(),
                normalize_either_separator(r"D:\repos\proj-alpha"),
            ),
            (
                "alpha2".to_string(),
                normalize_either_separator(r"D:\repos\proj-alpha2"),
            ),
        ];
        let proj = normalize_either_separator(r"D:\repos\proj-alpha\X.uproject");
        assert_eq!(
            project_to_worktree(&proj, &worktrees).as_deref(),
            Some("alpha"),
            "proj-alpha2 must not match"
        );
    }

    #[test]
    fn project_to_worktree_tolerates_slash_form_and_case() {
        let worktrees = vec![("alpha".to_string(), r"D:\Repos\Proj-Alpha".to_string())];
        for proj in [
            r"d:\repos\proj-alpha\X.uproject",
            "D:/REPOS/proj-alpha/X.uproject",
        ] {
            assert_eq!(
                project_to_worktree(proj, &worktrees).as_deref(),
                Some("alpha")
            );
        }
        assert_eq!(project_to_worktree("X.uproject", &worktrees), None);
    }

    #[test]
    fn is_component_prefix_rejects_sibling_and_ignores_case() {
        assert!(is_component_prefix("d:/r/wt/x.uproject", "D:/r/wt"));
        assert!(is_component_prefix("D:/r/wt", "d:/r/wt"));
        assert!(!is_component_prefix("D:/r/wt2/x", "D:/r/wt"));
        assert!(!is_component_prefix("D:/r/wt", ""));
    }

    #[test]
    fn extract_project_from_cmdline_quoted_and_unquoted() {
        let unq =
            "Command line: UBT.dll Target Win64 Development -Project=D:\\wt\\X.uproject -Progress";
        assert_eq!(
            extract_project_from_cmdline(unq).as_deref(),
            Some(normalize_either_separator(r"D:\wt\X.uproject").as_str())
        );
        let q = "Command line: UBT.dll -Project=\"D:\\A B\\X.uproject\" -Progress";
        assert_eq!(
            extract_project_from_cmdline(q).as_deref(),
            Some(normalize_either_separator(r"D:\A B\X.uproject").as_str())
        );
        assert_eq!(extract_project_from_cmdline("Command line: UBT.dll"), None);
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn extract_project_from_args_keeps_spaces_and_strips_quotes() {
        let spaced = argv(&[
            r"C:\UE\UnrealBuildTool.exe",
            "MyGameEditor",
            r"-Project=C:\work\my alpha\MyGame.uproject",
            "-Progress",
        ]);
        assert_eq!(
            extract_project_from_args(&spaced).as_deref(),
            Some(normalize_either_separator(r"C:\work\my alpha\MyGame.uproject").as_str())
        );
        let quoted = argv(&[
            "UnrealBuildTool",
            r#"-project="C:\work\alpha\MyGame.uproject""#,
        ]);
        assert_eq!(
            extract_project_from_args(&quoted).as_deref(),
            Some(normalize_either_separator(r"C:\work\alpha\MyGame.uproject").as_str()),
            "case-insensitive prefix, one quote pair stripped"
        );
        let single = argv(&[r"-PROJECT='C:\work\beta\MyGame.uproject'"]);
        assert_eq!(
            extract_project_from_args(&single).as_deref(),
            Some(normalize_either_separator(r"C:\work\beta\MyGame.uproject").as_str())
        );
        assert_eq!(
            extract_project_from_args(&argv(&["UnrealBuildTool", "-Progress"])),
            None
        );
        assert_eq!(extract_project_from_args(&argv(&["-Project=\"\""])), None);
    }

    #[test]
    fn is_build_mode_only_for_no_mode_or_build() {
        assert!(is_build_mode(&argv(&[
            "UnrealBuildTool",
            "MyGameEditor",
            "Win64"
        ])));
        assert!(is_build_mode(&argv(&["UnrealBuildTool", "-Mode=Build"])));
        assert!(is_build_mode(&argv(&["UnrealBuildTool", "-mode=build"])));
        assert!(!is_build_mode(&argv(&[
            "UnrealBuildTool",
            "-Mode=QueryTargets"
        ])));
        assert!(!is_build_mode(&argv(&[
            "UnrealBuildTool",
            "-Mode=JsonExport"
        ])));
        assert!(!is_build_mode(&argv(&[
            "UnrealBuildTool",
            "-MODE=GenerateProjectFiles"
        ])));
    }

    #[test]
    fn parse_resolved_project_argument_both_forms() {
        let want = ProgressDelta::Attribute(normalize_either_separator(r"D:\wt\X.uproject"));
        assert_eq!(
            parse(r"Resolved project argument D:\wt\X.uproject to D:\wt\X.uproject"),
            Some(want.clone())
        );
        assert_eq!(
            parse(r"Resolved project argument: D:\wt\X.uproject"),
            Some(want)
        );
    }

    #[test]
    fn command_line_project_is_attribution() {
        match parse(
            "Command line: UBT.dll T Win64 Development -Project=D:\\wt\\X.uproject -Progress",
        ) {
            Some(ProgressDelta::Attribute(p)) => {
                assert_eq!(p, normalize_either_separator(r"D:\wt\X.uproject"))
            }
            other => panic!("expected Attribute, got {other:?}"),
        }
    }

    // ── steps / total ─────────────────────────────────────────────────────────────

    #[test]
    fn parse_compile_link_resource_with_x64_token() {
        for (line, done, total, phase) in [
            ("[12/340] Compile [x64] Foo.cpp", 12, 340, "Compile"),
            (
                "[340/340] Link [x64] UnrealEditor-Foo.dll",
                340,
                340,
                "Link",
            ),
            ("[5/9] Resource [x64] Foo.rc", 5, 9, "Resource"),
        ] {
            match parse(line) {
                Some(ProgressDelta::Step {
                    done: d,
                    total: t,
                    phase: ph,
                }) => assert_eq!((d, t, ph.as_deref()), (done, Some(total), Some(phase))),
                other => panic!("expected Step for {line:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_rejects_done_gt_total() {
        assert!(parse("[9/5] Compile x").is_none());
    }

    #[test]
    fn parse_skips_nouba_and_exited_lines() {
        assert!(parse("[130/130] WriteMetadata MyGameEditor.target [NoUba]").is_none());
        assert!(parse("[3/5] Compile a.cpp -- Exited with error code 2").is_none());
    }

    #[test]
    fn parse_total_from_executor_line() {
        let line = "Using Unreal Build Accelerator local executor to run 1234 action(s)";
        assert_eq!(parse(line), Some(ProgressDelta::Total(1234)));
    }

    // ── result + message ──────────────────────────────────────────────────────────

    #[test]
    fn parse_result_succeeded() {
        assert_eq!(parse("Result: Succeeded"), Some(ProgressDelta::Succeeded));
    }

    #[test]
    fn parse_result_failed_uses_preceding_error_message() {
        let mut msg = None;
        assert!(parse_line(
            r"D:\wt\Foo.cpp(12): error C2065: 'Bar': undeclared identifier",
            &mut msg
        )
        .is_none());
        assert!(msg.as_deref().unwrap().contains("error C2065"));
        match parse_line("Result: Failed (OtherCompilationError)", &mut msg) {
            Some(ProgressDelta::Failed(Some(m))) => assert!(m.contains("error C2065")),
            other => panic!("expected Failed with message, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_failed_without_error_uses_suffix() {
        match parse("Result: Failed (OtherCompilationError)") {
            Some(ProgressDelta::Failed(Some(m))) => assert_eq!(m, "Failed (OtherCompilationError)"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn error_message_truncated_to_200_chars() {
        let long = format!("error C9999: {}", "x".repeat(400));
        let mut msg = None;
        let _ = parse_line(&long, &mut msg);
        assert_eq!(msg.as_deref().unwrap().chars().count(), 200);
    }
}
