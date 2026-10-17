// Pure transforms from parsed hunks to flat, virtualization-friendly row lists. Kept out
// of the component (and framework-free) so the fiddly side-by-side pairing is unit-tested.
//
// Both views flatten to one array of fixed-height rows so a virtualizer can index them
// directly: a hunk-header row, then one row per line (inline) or per aligned old/new pair
// (side-by-side).

import type { DiffHunk, DiffLine, DiffSpan } from "./ipc";

/** Token class names, indexed by the backend's class id (`DiffSpan.cls`). Must stay in sync with
 *  `diff::highlight::CLASSES` in the Rust backend. Rendered as `tok-<name>` CSS classes. */
export const TOKEN_CLASSES = [
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
] as const;

/** One rendered chunk of a diff line: its text and the token class to color it (if any). */
export interface TextChunk {
  text: string;
  /** CSS class suffix (`tok-<cls>`), or undefined for plain text. */
  cls?: string;
}

/** Split a line's text into styled chunks using its highlight spans. Slices by **code points**
 *  (not UTF-16 units) so multi-byte characters line up with the backend's span lengths. Returns a
 *  single plain chunk when there are no spans. Any uncovered remainder is appended unstyled. */
export function splitBySpans(text: string, spans: DiffSpan[] | undefined): TextChunk[] {
  if (!spans || spans.length === 0) return [{ text }];
  const cps = Array.from(text);
  const out: TextChunk[] = [];
  let i = 0;
  for (const s of spans) {
    if (i >= cps.length) break;
    const chunk = cps.slice(i, i + s.len).join("");
    i += s.len;
    const cls = s.cls != null ? TOKEN_CLASSES[s.cls] : undefined;
    out.push(cls ? { text: chunk, cls } : { text: chunk });
  }
  if (i < cps.length) out.push({ text: cps.slice(i).join("") });
  return out;
}

export type InlineRow = { kind: "hunk"; header: string } | { kind: "line"; line: DiffLine };

/** Flatten hunks for the inline (unified) view: header row + one row per line. */
export function inlineRows(hunks: DiffHunk[]): InlineRow[] {
  const rows: InlineRow[] = [];
  for (const h of hunks) {
    rows.push({ kind: "hunk", header: h.header });
    for (const line of h.lines) rows.push({ kind: "line", line });
  }
  return rows;
}

export type SbsRow =
  | { kind: "hunk"; header: string }
  | { kind: "pair"; left: DiffLine | null; right: DiffLine | null };

/**
 * Flatten hunks for the side-by-side view. Context lines map to a pair with the same line on
 * both sides; a run of deletions followed by a run of additions is zipped index-wise (a
 * modified line shows old-left / new-right, and uneven runs leave one side empty).
 */
export function sideBySideRows(hunks: DiffHunk[]): SbsRow[] {
  const rows: SbsRow[] = [];
  for (const h of hunks) {
    rows.push({ kind: "hunk", header: h.header });
    const lines = h.lines;
    let i = 0;
    while (i < lines.length) {
      const l = lines[i];
      if (l.kind === "context") {
        rows.push({ kind: "pair", left: l, right: l });
        i += 1;
        continue;
      }
      const dels: DiffLine[] = [];
      const adds: DiffLine[] = [];
      while (i < lines.length && lines[i].kind === "del") dels.push(lines[i++]);
      while (i < lines.length && lines[i].kind === "add") adds.push(lines[i++]);
      const n = Math.max(dels.length, adds.length);
      for (let j = 0; j < n; j++) {
        rows.push({ kind: "pair", left: dels[j] ?? null, right: adds[j] ?? null });
      }
    }
  }
  return rows;
}

export interface DiffTotals {
  files: number;
  added: number;
  deleted: number;
}

/** The minimum a changed file needs to be placed in the tree and counted. */
export interface TreeFile {
  path: string;
  added: number;
  deleted: number;
}

/** A folder in the changed-file tree. For a compacted chain, `name` is the joined chain
 *  (`docs/old/x`), `path` is its *deepest* directory (for tooltips), and `key` is its
 *  *topmost* directory — the stable collapse key, unchanged when a refresh lengthens or shortens
 *  the merged chain at its deep end. Aggregates cover the whole subtree. */
export interface DirNode<T extends TreeFile> {
  kind: "dir";
  name: string;
  path: string;
  key: string;
  children: TreeNode<T>[];
  fileCount: number;
  added: number;
  deleted: number;
}

export interface FileNode<T extends TreeFile> {
  kind: "file";
  /** Basename, used for sorting. */
  name: string;
  file: T;
}

export type TreeNode<T extends TreeFile> = DirNode<T> | FileNode<T>;

/** A visible row of the flattened tree; `depth` 0 is the root's direct children. */
export type TreeRow<T extends TreeFile> =
  | { kind: "dir"; node: DirNode<T>; depth: number }
  | { kind: "file"; file: T; depth: number };

// Case-insensitive, natural ("file2" < "file10") ordering, like a file explorer.
const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

/** Natural, case-insensitive compare with a deterministic tie-break, so names differing only by
 *  case/accent ("Foo" vs "foo") keep a stable order regardless of the input order. */
function compareNames(a: string, b: string): number {
  const c = collator.compare(a, b);
  if (c !== 0) return c;
  return a < b ? -1 : a > b ? 1 : 0;
}

function newDir<T extends TreeFile>(name: string, path: string): DirNode<T> {
  return { kind: "dir", name, path, key: path, children: [], fileCount: 0, added: 0, deleted: 0 };
}

/**
 * Build the changed-file tree: nest files under their directories, then (bottom-up) sum each
 * folder's file count and ± lines, compact single-folder chains with no files of their own into
 * one row (the root is never compacted), and sort each folder's children folders-first, A→Z.
 * The returned root has `path === ""`; its children are the top-level rows.
 */
export function buildFileTree<T extends TreeFile>(files: readonly T[]): DirNode<T> {
  const root = newDir<T>("", "");
  // Directory path → node, so inserting a file is O(depth), not a scan of siblings.
  const dirs = new Map<string, DirNode<T>>([["", root]]);
  for (const file of files) {
    const segs = file.path.split("/").filter((s) => s !== "");
    const name = segs.pop() ?? file.path;
    let parent = root;
    for (const seg of segs) {
      const path = parent.path ? `${parent.path}/${seg}` : seg;
      let dir = dirs.get(path);
      if (!dir) {
        dir = newDir<T>(seg, path);
        dirs.set(path, dir);
        parent.children.push(dir);
      }
      parent = dir;
    }
    parent.children.push({ kind: "file", name, file });
  }
  finalize(root);
  return root;
}

/** Aggregate, compact, and sort `dir`'s subtree in place. */
function finalize<T extends TreeFile>(dir: DirNode<T>): void {
  dir.fileCount = 0;
  dir.added = 0;
  dir.deleted = 0;
  dir.children = dir.children.map((child) => {
    if (child.kind === "file") {
      dir.fileCount += 1;
      dir.added += child.file.added;
      dir.deleted += child.file.deleted;
      return child;
    }
    finalize(child);
    // Compact: a folder whose only child is a folder merges with it (its aggregates are equal).
    // `finalize(child)` already compacted the grandchildren, so the merged child can't itself have
    // a lone folder child — this fires at most once (hence `if`, not a loop). The merged node keeps
    // `child`'s `key` (the topmost dir) so collapse state survives the chain changing length.
    let merged = child;
    if (child.children.length === 1 && child.children[0].kind === "dir") {
      const only: DirNode<T> = child.children[0];
      merged = { ...only, name: `${child.name}/${only.name}`, key: child.key };
    }
    dir.fileCount += merged.fileCount;
    dir.added += merged.added;
    dir.deleted += merged.deleted;
    return merged;
  });
  dir.children.sort((a, b) =>
    a.kind !== b.kind ? (a.kind === "dir" ? -1 : 1) : compareNames(a.name, b.name),
  );
}

/** The visible rows of `root`, skipping the descendants of any folder whose `key` is in
 *  `collapsed` (the collapsed folder row itself stays visible). */
export function flattenTree<T extends TreeFile>(
  root: DirNode<T>,
  collapsed: ReadonlySet<string>,
): TreeRow<T>[] {
  const out: TreeRow<T>[] = [];
  const walk = (dir: DirNode<T>, depth: number) => {
    for (const c of dir.children) {
      if (c.kind === "dir") {
        out.push({ kind: "dir", node: c, depth });
        if (!collapsed.has(c.key)) walk(c, depth + 1);
      } else {
        out.push({ kind: "file", file: c.file, depth });
      }
    }
  };
  walk(root, 0);
  return out;
}

/** Every file in tree display order, regardless of collapse state. */
export function treeFiles<T extends TreeFile>(root: DirNode<T>): T[] {
  const out: T[] = [];
  const walk = (dir: DirNode<T>) => {
    for (const c of dir.children) {
      if (c.kind === "dir") walk(c);
      else out.push(c.file);
    }
  };
  walk(root);
  return out;
}

/** Every folder's collapse key, in display order, for "collapse all". */
export function treeDirPaths<T extends TreeFile>(root: DirNode<T>): string[] {
  const out: string[] = [];
  const walk = (dir: DirNode<T>) => {
    for (const c of dir.children) {
      if (c.kind === "dir") {
        out.push(c.key);
        walk(c);
      }
    }
  };
  walk(root);
  return out;
}

/** The collapse keys of the folders on the path from `root` to the file at `filePath` (its visible
 *  ancestor folders), so the UI can reveal a file by expanding them. Empty if the file is at root
 *  or absent. */
export function treeAncestorKeys<T extends TreeFile>(root: DirNode<T>, filePath: string): string[] {
  const keys: string[] = [];
  const walk = (dir: DirNode<T>): boolean => {
    for (const c of dir.children) {
      if (c.kind === "file") {
        if (c.file.path === filePath) return true;
      } else if (filePath.startsWith(`${c.path}/`) && walk(c)) {
        keys.push(c.key);
        return true;
      }
    }
    return false;
  };
  walk(root);
  return keys;
}
