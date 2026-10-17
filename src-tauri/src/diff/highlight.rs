//! Tree-sitter syntax highlighting for the diff view: turns full file contents into per-line
//! token spans for Rust, TypeScript/TSX, and C/C++. Performance-first:
//! - one `HighlightConfiguration` per language, built lazily per worker thread (configs are not
//!   required to be `Sync`, and highlighting runs on the blocking pool);
//! - highlighting operates on **whole files** so multi-line constructs (block comments, raw /
//!   template strings) are classified correctly even when they open outside a diff hunk;
//! - output is compact per-line spans (code-point lengths), attached to diff lines upstream.
//!
//! Unknown extensions return `None` and render as plain text — no regression.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// Languages we highlight. `.ts`/`.tsx` both use the TSX grammar (a superset that parses plain
/// TypeScript), so one config covers both — the only cost is the rare legacy `<T>` cast form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    TypeScript,
    C,
    Cpp,
}

/// The fixed, grammar-independent token classes. The index into this array is the class id
/// carried on the wire (`Span.cls`); the frontend maps the same index to a CSS class. Tree-sitter
/// capture names match by longest prefix, so `type.builtin` → `type`, `function.method` →
/// `function`, `punctuation.bracket` → `punctuation`, etc.
pub const CLASSES: [&str; 12] = [
    "keyword",
    "string",
    "comment",
    "number",
    "type",
    "function",
    "constant",
    "operator",
    "punctuation",
    "property",
    "variable",
    "namespace",
];

/// One highlighted span of a single line: `len` code points carrying class `cls` (`None` = no
/// class → default text color). Code points (not bytes/UTF-16 units) so the frontend can slice
/// the line text consistently. Serializes compactly (`{len}` for a gap, `{len, cls}` for a
/// classed run) as part of a diff line's payload.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Span {
    pub len: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cls: Option<u8>,
}

/// Per-line token runs covering the whole line in order (classed and unclassed), one entry per
/// source line.
pub type LineTokens = Vec<Span>;

/// Map a file path's extension to a language, or `None` for "don't highlight".
pub fn lang_for_path(path: &str) -> Option<Lang> {
    let ext = path.rsplit('.').next()?;
    // Guard against a dotless path (`rsplit` yields the whole string): only treat it as an
    // extension when there actually was a '.'.
    if !path.contains('.') {
        return None;
    }
    match ext.to_ascii_lowercase().as_str() {
        "rs" => Some(Lang::Rust),
        "ts" | "mts" | "cts" | "tsx" => Some(Lang::TypeScript),
        "c" | "h" => Some(Lang::C),
        "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" => Some(Lang::Cpp),
        _ => None,
    }
}

thread_local! {
    /// Per-thread cache of built configs (`None` = construction failed; don't retry).
    static CONFIGS: RefCell<HashMap<Lang, Option<Arc<HighlightConfiguration>>>> =
        RefCell::new(HashMap::new());
}

/// Build the `HighlightConfiguration` for a language, trying the richest query set first and
/// falling back to a simpler one so a query incompatibility degrades gracefully to sparser (or,
/// worst case, no) highlighting instead of failing. `None` ⇒ this language renders plain.
fn build_config(lang: Lang) -> Option<HighlightConfiguration> {
    // Each candidate: (tree_sitter Language, name, highlights_query, injections_query).
    let (language, name, candidates, injections): (tree_sitter::Language, &str, Vec<String>, &str) =
        match lang {
            Lang::Rust => (
                tree_sitter::Language::new(tree_sitter_rust::LANGUAGE),
                "rust",
                vec![tree_sitter_rust::HIGHLIGHTS_QUERY.to_string()],
                tree_sitter_rust::INJECTIONS_QUERY,
            ),
            Lang::C => (
                tree_sitter::Language::new(tree_sitter_c::LANGUAGE),
                "c",
                vec![tree_sitter_c::HIGHLIGHT_QUERY.to_string()],
                "",
            ),
            Lang::Cpp => (
                tree_sitter::Language::new(tree_sitter_cpp::LANGUAGE),
                "cpp",
                vec![
                    // C++ highlights layer over the C base; fall back to C++-only if that fails.
                    format!(
                        "{}\n{}",
                        tree_sitter_c::HIGHLIGHT_QUERY,
                        tree_sitter_cpp::HIGHLIGHT_QUERY
                    ),
                    tree_sitter_cpp::HIGHLIGHT_QUERY.to_string(),
                ],
                "",
            ),
            Lang::TypeScript => (
                tree_sitter::Language::new(tree_sitter_typescript::LANGUAGE_TSX),
                "tsx",
                vec![
                    // JS base + JSX + TS-specific, applied to the TSX grammar. Fall back to JS-only.
                    format!(
                        "{}\n{}\n{}",
                        tree_sitter_javascript::HIGHLIGHT_QUERY,
                        tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                        tree_sitter_typescript::HIGHLIGHTS_QUERY
                    ),
                    tree_sitter_javascript::HIGHLIGHT_QUERY.to_string(),
                ],
                tree_sitter_javascript::INJECTIONS_QUERY,
            ),
        };

    for query in &candidates {
        match HighlightConfiguration::new(language.clone(), name, query, injections, "") {
            Ok(mut config) => {
                config.configure(&CLASSES);
                return Some(config);
            }
            Err(_) => continue,
        }
    }
    None
}

/// Highlight whole-file `source` for `lang`, returning one [`LineTokens`] per source line (spans
/// cover every code point in order). Returns `None` if the language config could not be built.
pub fn highlight(lang: Lang, source: &str) -> Option<Vec<LineTokens>> {
    let config = CONFIGS.with(|c| {
        c.borrow_mut()
            .entry(lang)
            .or_insert_with(|| build_config(lang).map(Arc::new))
            .clone()
    })?;

    let mut hl = Highlighter::new();
    let events = hl
        .highlight(&config, source.as_bytes(), None, |_| None)
        .ok()?;

    let mut lines: Vec<LineTokens> = vec![Vec::new()];
    let mut stack: Vec<usize> = Vec::new();
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(h) => stack.push(h.0),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                let cls = stack.last().map(|&i| i as u8);
                emit_source(&mut lines, &source[start..end], cls);
            }
        }
    }
    Some(lines)
}

/// Append a byte range's text to `lines`, splitting at newlines into per-line spans and merging
/// runs of the same class. Span lengths are code points.
fn emit_source(lines: &mut Vec<LineTokens>, text: &str, cls: Option<u8>) {
    let mut rest = text;
    loop {
        match rest.find('\n') {
            Some(nl) => {
                push_span(lines.last_mut().expect("current line"), &rest[..nl], cls);
                lines.push(Vec::new());
                rest = &rest[nl + 1..];
            }
            None => {
                push_span(lines.last_mut().expect("current line"), rest, cls);
                break;
            }
        }
    }
}

/// Append `text` (code-point length) as a span of class `cls`, merging into the previous span
/// when the class matches. Empty text is ignored.
fn push_span(line: &mut LineTokens, text: &str, cls: Option<u8>) {
    let len = text.chars().count();
    if len == 0 {
        return;
    }
    match line.last_mut() {
        Some(prev) if prev.cls == cls => prev.len += len,
        _ => line.push(Span { len, cls }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class_name(cls: Option<u8>) -> Option<&'static str> {
        cls.map(|i| CLASSES[i as usize])
    }

    /// The class id carried by the span covering 1-based code-point column `col` on `line`.
    fn class_at(lines: &[LineTokens], line: usize, col: usize) -> Option<&'static str> {
        let mut c = 0;
        for span in &lines[line] {
            c += span.len;
            if col <= c {
                return class_name(span.cls);
            }
        }
        None
    }

    #[test]
    fn class_order_is_stable() {
        // The wire carries a bare index into CLASSES (DiffSpan.cls); the frontend resolves it by
        // position against TOKEN_CLASSES (src/lib/diffRows.ts) and the .tok-* rules (src/index.css).
        // Reordering/inserting here without updating both of those silently mis-colors every token,
        // so pin the order: a change that breaks this test must also update the frontend.
        assert_eq!(
            CLASSES,
            [
                "keyword",
                "string",
                "comment",
                "number",
                "type",
                "function",
                "constant",
                "operator",
                "punctuation",
                "property",
                "variable",
                "namespace",
            ]
        );
    }

    #[test]
    fn lang_for_path_maps_known_extensions() {
        assert_eq!(lang_for_path("src/main.rs"), Some(Lang::Rust));
        assert_eq!(lang_for_path("a/b.ts"), Some(Lang::TypeScript));
        assert_eq!(lang_for_path("a/b.tsx"), Some(Lang::TypeScript));
        assert_eq!(lang_for_path("x.MTS"), Some(Lang::TypeScript));
        assert_eq!(lang_for_path("foo.c"), Some(Lang::C));
        assert_eq!(lang_for_path("foo.hpp"), Some(Lang::Cpp));
        assert_eq!(lang_for_path("foo.cpp"), Some(Lang::Cpp));
        assert_eq!(lang_for_path("README.md"), None);
        assert_eq!(lang_for_path("Makefile"), None);
        assert_eq!(lang_for_path("noext"), None);
    }

    #[test]
    fn spans_cover_every_code_point_per_line() {
        let src = "fn main() {\n    let x = 1;\n}\n";
        let lines = highlight(Lang::Rust, src).expect("rust config builds");
        for (i, text) in src.split('\n').enumerate() {
            let covered: usize = lines[i].iter().map(|s| s.len).sum();
            assert_eq!(
                covered,
                text.chars().count(),
                "line {i} span coverage must equal its code-point length ({text:?})"
            );
        }
    }

    #[test]
    fn rust_keyword_and_comment_classified() {
        let src = "// hi\nfn main() {}\n";
        let lines = highlight(Lang::Rust, src).unwrap();
        assert_eq!(
            class_at(&lines, 0, 1),
            Some("comment"),
            "// line is a comment"
        );
        assert_eq!(class_at(&lines, 1, 1), Some("keyword"), "fn is a keyword");
    }

    #[test]
    fn block_comment_opened_above_is_classified_across_lines() {
        // The comment opens on line 0; line 1's text must still be classified as a comment,
        // which only works because we highlight the whole file, not a single hunk.
        let src = "/* multi\n   line */\nlet y = 2;\n";
        let lines = highlight(Lang::Cpp, src).unwrap();
        assert_eq!(class_at(&lines, 0, 1), Some("comment"));
        assert_eq!(
            class_at(&lines, 1, 4),
            Some("comment"),
            "continuation line stays a comment"
        );
    }

    #[test]
    fn typescript_string_and_keyword_classified() {
        let src = "const s = \"hello\";\n";
        let lines = highlight(Lang::TypeScript, src).unwrap();
        assert_eq!(
            class_at(&lines, 0, 1),
            Some("keyword"),
            "const is a keyword"
        );
        // The quote sits at column 11 ("const s = ").
        assert_eq!(
            class_at(&lines, 0, 12),
            Some("string"),
            "the literal is a string"
        );
    }

    #[test]
    fn non_ascii_lengths_are_code_points_not_bytes() {
        // "café" is 4 code points but 5 bytes; "π" is 1 code point, 2 bytes.
        let src = "let s = \"café π\";\n";
        let lines = highlight(Lang::Rust, src).unwrap();
        let covered: usize = lines[0].iter().map(|s| s.len).sum();
        assert_eq!(covered, src.split('\n').next().unwrap().chars().count());
    }

    #[test]
    fn multiline_source_yields_one_entry_per_line() {
        let src = "a\nb\nc";
        let lines = highlight(Lang::Rust, src).unwrap();
        // 2 newlines → 3 lines.
        assert_eq!(lines.len(), 3);
    }
}
