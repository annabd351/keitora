import { describe, expect, it } from "vitest";
import {
  buildFileTree,
  flattenTree,
  inlineRows,
  sideBySideRows,
  splitBySpans,
  TOKEN_CLASSES,
  type TreeRow,
  treeAncestorKeys,
  treeDirPaths,
  treeFiles,
} from "./diffRows";
import type { DiffHunk, DiffLine } from "./ipc";

const ctx = (oldNo: number, newNo: number, text: string): DiffLine => ({
  kind: "context",
  oldNo,
  newNo,
  text,
});
const del = (oldNo: number, text: string): DiffLine => ({ kind: "del", oldNo, text });
const add = (newNo: number, text: string): DiffLine => ({ kind: "add", newNo, text });

const hunk = (lines: DiffLine[]): DiffHunk => ({
  oldStart: 1,
  oldLines: 1,
  newStart: 1,
  newLines: 1,
  header: "@@ -1 +1 @@",
  lines,
});

describe("inlineRows", () => {
  it("emits a header row then one row per line", () => {
    const rows = inlineRows([hunk([ctx(1, 1, "a"), del(2, "b"), add(2, "c")])]);
    expect(rows.map((r) => r.kind)).toEqual(["hunk", "line", "line", "line"]);
  });
});

describe("sideBySideRows", () => {
  it("pairs context on both sides", () => {
    const rows = sideBySideRows([hunk([ctx(1, 1, "a")])]);
    expect(rows[1]).toEqual({ kind: "pair", left: ctx(1, 1, "a"), right: ctx(1, 1, "a") });
  });

  it("zips a del run against an add run (modification)", () => {
    const rows = sideBySideRows([
      hunk([del(1, "old1"), del(2, "old2"), add(1, "new1"), add(2, "new2")]),
    ]);
    const pairs = rows.filter((r) => r.kind === "pair");
    expect(pairs).toEqual([
      { kind: "pair", left: del(1, "old1"), right: add(1, "new1") },
      { kind: "pair", left: del(2, "old2"), right: add(2, "new2") },
    ]);
  });

  it("leaves the shorter side empty on uneven runs", () => {
    const rows = sideBySideRows([hunk([del(1, "only old"), add(1, "new1"), add(2, "new2")])]);
    const pairs = rows.filter((r) => r.kind === "pair");
    expect(pairs).toEqual([
      { kind: "pair", left: del(1, "only old"), right: add(1, "new1") },
      { kind: "pair", left: null, right: add(2, "new2") },
    ]);
  });

  it("a pure addition has only right side", () => {
    const rows = sideBySideRows([hunk([ctx(1, 1, "a"), add(2, "added")])]);
    expect(rows.filter((r) => r.kind === "pair")).toEqual([
      { kind: "pair", left: ctx(1, 1, "a"), right: ctx(1, 1, "a") },
      { kind: "pair", left: null, right: add(2, "added") },
    ]);
  });
});

const f = (path: string, added = 0, deleted = 0) => ({ path, added, deleted });

/** Compact text rendering of the visible rows: indent per depth, folders end in "/". */
const render = (rows: TreeRow<ReturnType<typeof f>>[]) =>
  rows.map((r) =>
    r.kind === "dir"
      ? `${"  ".repeat(r.depth)}${r.node.name}/`
      : `${"  ".repeat(r.depth)}${r.file.path.slice(r.file.path.lastIndexOf("/") + 1)}`,
  );

describe("buildFileTree / flattenTree", () => {
  it("nests folders under their parents regardless of git's path order", () => {
    // The order git emitted in the bug report: a deep docs/ path before docs/, src/analysis/ before src/.
    const root = buildFileTree([
      f("README.md"),
      f("docs/plans/old/frame-phase-analysis/plan.md"),
      f("docs/unreal-frame-lifecycle.md"),
      f("src/analysis/capture.rs"),
      f("src/cli.rs"),
      f("src/report/mod.rs"),
    ]);
    expect(render(flattenTree(root, new Set()))).toEqual([
      "docs/",
      "  plans/old/frame-phase-analysis/",
      "    plan.md",
      "  unreal-frame-lifecycle.md",
      "src/",
      "  analysis/",
      "    capture.rs",
      "  report/",
      "    mod.rs",
      "  cli.rs",
      "README.md",
    ]);
  });

  it("compacts single-folder chains but stops at a folder with files or 2+ children", () => {
    const root = buildFileTree([f("a/b/c/x.ts"), f("a/b/c/y.ts"), f("a/b/d/z.ts")]);
    // a/b has two child folders (c, d) → the chain stops at a/b.
    expect(render(flattenTree(root, new Set()))).toEqual([
      "a/b/",
      "  c/",
      "    x.ts",
      "    y.ts",
      "  d/",
      "    z.ts",
    ]);
    // The merged node keeps its topmost dir as the stable collapse key; path is the deepest.
    expect(root.children[0].kind === "dir" && root.children[0].key).toBe("a");
  });

  it("never compacts the root, even with a single top-level folder", () => {
    const root = buildFileTree([f("only/deep/file.rs")]);
    expect(root.path).toBe("");
    expect(render(flattenTree(root, new Set()))).toEqual(["only/deep/", "  file.rs"]);
  });

  it("sorts folders first, then files, case-insensitive with natural numbers", () => {
    const root = buildFileTree([
      f("b.ts"),
      f("Zeta/x.ts"),
      f("alpha/x.ts"),
      f("file10.ts"),
      f("File2.ts"),
      f("a.ts"),
    ]);
    expect(render(flattenTree(root, new Set()))).toEqual([
      "alpha/",
      "  x.ts",
      "Zeta/",
      "  x.ts",
      "a.ts",
      "b.ts",
      "File2.ts",
      "file10.ts",
    ]);
  });

  it("aggregates file count and ± lines across the whole subtree", () => {
    const root = buildFileTree([f("src/a.rs", 10, 2), f("src/x/b.rs", 5, 1), f("top.md", 1, 0)]);
    const src = root.children.find((c) => c.kind === "dir");
    expect(src).toMatchObject({ name: "src", fileCount: 2, added: 15, deleted: 3 });
    expect(root).toMatchObject({ fileCount: 3, added: 16, deleted: 3 });
  });

  it("collapsing a folder hides only its descendants, not the folder row", () => {
    const root = buildFileTree([f("src/a.rs"), f("src/x/b.rs"), f("top.md")]);
    expect(render(flattenTree(root, new Set(["src"])))).toEqual(["src/", "top.md"]);
    expect(render(flattenTree(root, new Set(["src/x"])))).toEqual([
      "src/",
      "  x/",
      "  a.rs",
      "top.md",
    ]);
  });

  it("places a renamed file under its new path", () => {
    const root = buildFileTree([
      { path: "new/dir/r.ts", oldPath: "old/r.ts", added: 0, deleted: 0 },
    ]);
    expect(treeFiles(root).map((x) => x.path)).toEqual(["new/dir/r.ts"]);
    expect(treeDirPaths(root)).toEqual(["new"]);
  });

  it("treeFiles lists every file in display order, ignoring collapse; treeDirPaths every folder", () => {
    const root = buildFileTree([f("z.md"), f("src/b.rs"), f("src/a.rs"), f("docs/x/y.md")]);
    expect(treeFiles(root).map((x) => x.path)).toEqual([
      "docs/x/y.md",
      "src/a.rs",
      "src/b.rs",
      "z.md",
    ]);
    expect(treeDirPaths(root)).toEqual(["docs", "src"]);
  });

  it("keeps a folder's collapse key stable when a merged chain lengthens", () => {
    // 'a' holds a.ts and subfolder 'b/x.ts' → 'a' is its own row (has a file), key 'a'.
    const before = buildFileTree([f("a/a.ts"), f("a/b/x.ts")]);
    expect(before.children[0].kind === "dir" && before.children[0].key).toBe("a");
    // Revert a.ts → 'a' now has only child 'b' and compacts to 'a/b'. Key must stay 'a' so a
    // collapsed 'a' does not pop open on the refresh.
    const after = buildFileTree([f("a/b/x.ts")]);
    const row = after.children[0];
    expect(row.kind === "dir" && [row.name, row.key, row.path]).toEqual(["a/b", "a", "a/b"]);
    expect(flattenTree(after, new Set(["a"]))).toEqual([{ kind: "dir", node: row, depth: 0 }]);
  });

  it("treeAncestorKeys returns the collapse keys to expand to reveal a file", () => {
    const root = buildFileTree([f("docs/deep/nested/y.md"), f("top.md")]);
    // The chain docs/deep/nested merges to one row keyed 'docs'.
    expect(treeAncestorKeys(root, "docs/deep/nested/y.md")).toEqual(["docs"]);
    expect(treeAncestorKeys(root, "top.md")).toEqual([]);
    expect(treeAncestorKeys(root, "nope.md")).toEqual([]);
  });

  it("handles an empty change set", () => {
    const root = buildFileTree([]);
    expect(flattenTree(root, new Set())).toEqual([]);
    expect(root.fileCount).toBe(0);
  });
});

describe("splitBySpans", () => {
  it("returns a single plain chunk when there are no spans", () => {
    expect(splitBySpans("hello", undefined)).toEqual([{ text: "hello" }]);
    expect(splitBySpans("hello", [])).toEqual([{ text: "hello" }]);
  });

  it("maps class ids to tok-class names and slices in order", () => {
    // "fn x" → keyword(cls 0) "fn", plain " ", variable(cls 10) "x"
    const chunks = splitBySpans("fn x", [{ len: 2, cls: 0 }, { len: 1 }, { len: 1, cls: 10 }]);
    expect(chunks).toEqual([
      { text: "fn", cls: "keyword" },
      { text: " " },
      { text: "x", cls: "variable" },
    ]);
  });

  it("slices by code points, not UTF-16 units (surrogate pairs)", () => {
    // "😀x" — the emoji is 1 code point but 2 UTF-16 units; a span of len 1 must take the whole
    // emoji, leaving "x" for the next span.
    const chunks = splitBySpans("😀x", [
      { len: 1, cls: 1 },
      { len: 1, cls: 10 },
    ]);
    expect(chunks).toEqual([
      { text: "😀", cls: "string" },
      { text: "x", cls: "variable" },
    ]);
  });

  it("appends any uncovered remainder as plain text", () => {
    expect(splitBySpans("abcd", [{ len: 2, cls: 0 }])).toEqual([
      { text: "ab", cls: "keyword" },
      { text: "cd" },
    ]);
  });
});

describe("TOKEN_CLASSES", () => {
  it("stays index-aligned with diff::highlight::CLASSES in the Rust backend", () => {
    // The wire carries a bare class index (DiffSpan.cls); this order must match the backend's
    // CLASSES array and the .tok-* rules in src/index.css, or tokens mis-color silently.
    expect([...TOKEN_CLASSES]).toEqual([
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
    ]);
  });
});
