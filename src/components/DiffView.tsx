// Full-window, read-only diff view for one worktree (merge-base of main → working tree).
// GitHub Desktop shape: changed-file tree on the left, the selected file's diff on the
// right, with inline/side-by-side and hide-whitespace toggles. Performance: the cheap
// summary paints the tree immediately; each file's diff is fetched lazily on selection and
// older in-flight requests are ignored by request id (the backend also cancels them). Long
// diffs are virtualized so a big file stays responsive.

import { useVirtualizer } from "@tanstack/react-virtual";
import { listen } from "@tauri-apps/api/event";
import { ChevronDown, ChevronRight, ChevronsDownUp, ChevronsUpDown, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  buildFileTree,
  type DirNode,
  flattenTree,
  inlineRows,
  type SbsRow,
  sideBySideRows,
  splitBySpans,
  treeAncestorKeys,
  treeDirPaths,
  treeFiles,
} from "../lib/diffRows";
import {
  type DiffBase,
  type DiffBaseInfo,
  type DiffFileSummary,
  type DiffLine,
  type DiffStatus,
  type FileDiff,
  setWorktreeDiffBase,
  worktreeDiffBase,
  worktreeDiffCommitted,
  worktreeDiffRevision,
  worktreeDiffSummary,
  worktreeDiffWatchStart,
  worktreeDiffWatchStop,
  worktreeFileDiff,
} from "../lib/ipc";
import { useLiveResource } from "../lib/useLiveResource";
import { useLocalStorageState } from "../lib/useLocalStorageState";

interface Props {
  worktree: string;
  onClose: () => void;
}

const STATUS_BADGE: Record<DiffStatus, { letter: string; cls: string }> = {
  added: { letter: "A", cls: "bg-idle text-bg-0" },
  modified: { letter: "M", cls: "bg-busy text-bg-0" },
  deleted: { letter: "D", cls: "bg-urgent text-bg-0" },
  renamed: { letter: "R", cls: "bg-waiting text-bg-0" },
  untracked: { letter: "U", cls: "bg-accent-2 text-bg-0" },
};

/** Fixed row-height estimate for the virtualizer; actual rows self-measure (lines wrap). */
const ROW_ESTIMATE = 20;

export function DiffView({ worktree, onClose }: Props) {
  const [sbs, setSbs] = useLocalStorageState("keitora:diffSbs", true);
  const [hideWs, setHideWs] = useLocalStorageState("keitora:diffHideWs", true);
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [collapsed, setCollapsed] = useState<Set<string>>(() => new Set());
  const [fileDiff, setFileDiff] = useState<FileDiff | null>(null);
  const [fileLoading, setFileLoading] = useState(false);
  const [fileError, setFileError] = useState<string | null>(null);
  // Committed-only rows (merge-base..HEAD), fetched without a working-tree scan so the tree
  // paints instantly on open while the full summary (incl. uncommitted/untracked) computes.
  const [committed, setCommitted] = useState<DiffFileSummary[] | null>(null);

  const containerRef = useRef<HTMLDivElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const reqId = useRef(0);

  // Grab keyboard focus so Esc and scrolling don't leak to the mounted-but-hidden PTY.
  useEffect(() => {
    containerRef.current?.focus();
  }, []);

  // Per-worktree state (selection, collapsed dirs, loaded diff) resets by remounting: the
  // App mount site keys this component on the worktree name, so switching targets gives a
  // fresh instance rather than carrying the previous worktree's selection/tree state.

  // Refcount the worktree's filesystem watcher for the lifetime of this open view, so the
  // liveness probe can read a cheap change-generation instead of running a full status scan.
  useEffect(() => {
    void worktreeDiffWatchStart(worktree);
    return () => {
      void worktreeDiffWatchStop(worktree);
    };
  }, [worktree]);

  // Instant first paint: committed changes need no working-tree scan.
  useEffect(() => {
    let alive = true;
    worktreeDiffCommitted(worktree, hideWs)
      .then((rows) => {
        if (alive) setCommitted(rows);
      })
      .catch(() => {
        // Non-fatal: the full summary below is the source of truth; ignore.
      });
    return () => {
      alive = false;
    };
  }, [worktree, hideWs]);

  const load = useCallback(() => worktreeDiffSummary(worktree, hideWs), [worktree, hideWs]);
  const revision = useCallback(() => worktreeDiffRevision(worktree), [worktree]);
  const { data, error, reload } = useLiveResource(load, "worktrees://changed", {
    intervalMs: 2000,
    revision,
  });

  // The resolved diff base (ref name + comparison mode + any error) for the header label,
  // picker, and error banner. Refetched on worktree change, after the picker writes a new
  // override, and on any `project://changed` (e.g. the Settings drawer edits the project default).
  const [baseInfo, setBaseInfo] = useState<DiffBaseInfo | null>(null);
  // Bumped whenever the base changes so effects keyed on the base (the open file diff) re-run even
  // when the file's summary row (status/±counts) is unchanged.
  const [baseNonce, setBaseNonce] = useState(0);

  const refreshAfterBaseChange = useCallback(() => {
    worktreeDiffBase(worktree)
      .then(setBaseInfo)
      .catch(() => setBaseInfo(null));
    // Re-pull the summary + committed list against the new base now rather than waiting for the
    // next liveness tick, and force the open file diff to refetch.
    void reload();
    worktreeDiffCommitted(worktree, hideWs)
      .then(setCommitted)
      .catch(() => {});
    setBaseNonce((n) => n + 1);
  }, [worktree, hideWs, reload]);

  useEffect(() => {
    let alive = true;
    worktreeDiffBase(worktree)
      .then((b) => {
        if (alive) setBaseInfo(b);
      })
      .catch(() => {
        if (alive) setBaseInfo(null);
      });
    return () => {
      alive = false;
    };
  }, [worktree]);

  // A project settings change (e.g. the project-default diff base) can change this worktree's
  // effective base; refetch the base + diffs so an already-open view doesn't show a stale label.
  useEffect(() => {
    const un = listen("project://changed", () => refreshAfterBaseChange());
    return () => {
      void un.then((f) => f());
    };
  }, [refreshAfterBaseChange]);

  const applyBase = useCallback(
    async (base: DiffBase | null) => {
      await setWorktreeDiffBase(worktree, base);
      refreshAfterBaseChange();
    },
    [worktree, refreshAfterBaseChange],
  );

  // Short label for the resolved base, e.g. "origin/main" or "main (tip)".
  const baseLabel = baseInfo
    ? baseInfo.compare === "tip"
      ? `${baseInfo.refName} (tip)`
      : baseInfo.refName
    : "main";
  // Until the full summary resolves, show committed rows; afterwards `data` is authoritative
  // and stays visible across refreshes (no flash back to committed-only).
  const files: DiffFileSummary[] = data ?? committed ?? [];
  // Truly nothing yet (neither committed nor full) — distinct from a loaded-but-empty result.
  const summaryLoading = data === null && committed === null && !error;
  // The full working-tree scan is still running behind the committed-only list.
  const scanningWorkingTree = data === null && committed !== null;

  // Nested, sorted, compact-folder tree of the changed files; rows are its visible flattening.
  const tree = useMemo(() => buildFileTree(files), [files]);
  const treeRows = useMemo(() => flattenTree(tree, collapsed), [tree, collapsed]);
  // Every file in tree display order — the default selection follows what the user sees first.
  const orderedFiles = useMemo(() => treeFiles(tree), [tree]);
  const totals = useMemo(
    () => ({
      files: files.length,
      added: files.reduce((a, f) => a + f.added, 0),
      deleted: files.reduce((a, f) => a + f.deleted, 0),
    }),
    [files],
  );

  const selectedFile = useMemo(
    () => files.find((f) => f.path === selectedPath) ?? null,
    [files, selectedPath],
  );

  // Keep a valid selection: pick the first file when nothing is selected, and re-select when
  // the current file leaves the summary (reverted/committed) so the pane never gets stuck blank.
  useEffect(() => {
    if (orderedFiles.length === 0) return;
    const stillPresent = selectedPath !== null && orderedFiles.some((f) => f.path === selectedPath);
    if (!stillPresent) {
      const next = orderedFiles[0].path;
      setSelectedPath(next);
      // Reveal the auto-selected file: expand its ancestor folders so its row is visible even
      // after "Collapse all" (otherwise the pane would show a file with no highlighted row).
      const ancestors = treeAncestorKeys(tree, next);
      if (ancestors.length > 0) {
        setCollapsed((prev) => {
          if (!ancestors.some((k) => prev.has(k))) return prev;
          const n = new Set(prev);
          for (const k of ancestors) n.delete(k);
          return n;
        });
      }
    }
  }, [orderedFiles, selectedPath, tree]);

  // Drop collapse keys for folders that no longer exist (reverted files, merged chains), so the
  // set can't grow unbounded over a long session or resurrect a stale collapse for a reused path.
  useEffect(() => {
    const live = new Set(treeDirPaths(tree));
    setCollapsed((prev) => {
      let changed = false;
      const next = new Set<string>();
      for (const k of prev) {
        if (live.has(k)) next.add(k);
        else changed = true;
      }
      return changed ? next : prev;
    });
  }, [tree]);

  // Signature of the selected row: refetch the diff only when the selection or the row's own
  // change shape moves, not on every summary identity change (a no-op poll keeps `data`'s
  // identity stable, but a real refresh replaces it even when this file is untouched).
  const selOldPath = selectedFile?.oldPath;
  const selUntracked = selectedFile?.status === "untracked";
  const selShape = selectedFile
    ? `${selectedFile.status}:${selectedFile.added}:${selectedFile.deleted}`
    : null;

  // Fetch the selected file's diff. Tagged with a monotonic id so a slow earlier response
  // can't overwrite a newer selection. baseNonce isn't read in the body but is a dep so the diff
  // refetches when the base changes (same file, new base).
  // biome-ignore lint/correctness/useExhaustiveDependencies: baseNonce intentionally forces refetch
  useEffect(() => {
    if (!selectedPath) {
      setFileDiff(null);
      return;
    }
    // The row isn't in the current list yet (e.g. selected before the full summary loaded) —
    // wait for it so we pass the correct untracked/oldPath rather than guessing.
    if (selShape === null) return;
    const my = ++reqId.current;
    setFileLoading(true);
    setFileError(null);
    worktreeFileDiff(worktree, selectedPath, selOldPath, hideWs, selUntracked)
      .then((d) => {
        if (my === reqId.current) {
          setFileDiff(d);
          setFileLoading(false);
        }
      })
      .catch((e) => {
        // A superseded backend job rejects; ignore unless it's still the current request.
        if (my === reqId.current) {
          setFileError(String(e));
          setFileLoading(false);
        }
      });
  }, [worktree, selectedPath, hideWs, selShape, selOldPath, selUntracked, baseNonce]);

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      onClose();
    }
  };

  const toggleDir = (dir: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(dir)) next.delete(dir);
      else next.add(dir);
      return next;
    });
  };

  return (
    <div
      ref={containerRef}
      tabIndex={-1}
      onKeyDown={onKeyDown}
      className="flex h-full w-full flex-col bg-bg-1 text-tx-1 outline-none"
    >
      {/* header */}
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-line bg-bg-2 px-3 py-2">
        <div className="flex min-w-0 items-baseline gap-2">
          <span className="truncate text-sm font-semibold">{worktree}</span>
          <DiffBasePicker base={baseInfo} label={baseLabel} onApply={applyBase} />
        </div>
        <span className="text-xs text-tx-2 tabular-nums">
          <b className="text-tx-1">{totals.files}</b> {totals.files === 1 ? "file" : "files"}
          {" · "}
          <span className="font-mono" style={{ color: "var(--diff-add-edge)" }}>
            +{totals.added}
          </span>{" "}
          <span className="font-mono" style={{ color: "var(--diff-del-edge)" }}>
            −{totals.deleted}
          </span>
        </span>
        <div className="ml-auto flex items-center gap-2">
          <div className="inline-flex overflow-hidden rounded-chip border border-line">
            <button
              type="button"
              aria-pressed={!sbs}
              onClick={() => setSbs(false)}
              className={`px-2.5 py-1 text-xs ${!sbs ? "bg-accent-dim text-accent-text" : "text-tx-2"}`}
            >
              Inline
            </button>
            <button
              type="button"
              aria-pressed={sbs}
              onClick={() => setSbs(true)}
              className={`border-l border-line px-2.5 py-1 text-xs ${sbs ? "bg-accent-dim text-accent-text" : "text-tx-2"}`}
            >
              Side-by-side
            </button>
          </div>
          <label className="inline-flex cursor-pointer select-none items-center gap-1.5 rounded-chip border border-line px-2.5 py-1 text-xs text-tx-2">
            <input
              type="checkbox"
              checked={hideWs}
              onChange={(e) => setHideWs(e.target.checked)}
              className="accent-accent"
            />
            Hide whitespace
          </label>
          <button
            type="button"
            aria-label="Close diff view"
            title="Close (Esc)"
            onClick={onClose}
            className="flex h-7 w-7 items-center justify-center rounded-chip border border-line text-tx-2 hover:bg-bg-3 hover:text-tx-1"
          >
            <X size={15} />
          </button>
        </div>
      </div>

      {/* indeterminate activity bar — visible whenever a summary or file diff is in flight,
          so loads/refreshes always show progress even while prior content stays on screen */}
      <div className="h-0.5 w-full overflow-hidden" aria-hidden="true">
        {(summaryLoading || fileLoading) && (
          <div className="h-full w-2/5 bg-accent animate-indeterminate" />
        )}
      </div>

      {/* base-resolution error (e.g. a configured ref that no longer exists) */}
      {baseInfo?.error && (
        <div className="border-b border-line bg-bg-2 px-3 py-1.5 text-xs text-urgent">
          Diff base unavailable: {baseInfo.error}. Pick another base above.
        </div>
      )}

      {/* body */}
      <div className="flex min-h-0 flex-1">
        {/* file tree */}
        <aside className="flex w-72 flex-none flex-col border-r border-line">
          <div className="flex items-center justify-between border-b border-line px-3 py-2 text-[11px] uppercase tracking-wider text-tx-3">
            <span>Changed files</span>
            <span className="flex items-center gap-1.5 tabular-nums">
              {scanningWorkingTree && (
                <span
                  className="normal-case tracking-normal text-tx-3"
                  title="Including uncommitted changes…"
                >
                  scanning…
                </span>
              )}
              {totals.files}
              {tree.children[0]?.kind === "dir" && (
                <span className="ml-0.5 flex items-center normal-case tracking-normal">
                  <button
                    type="button"
                    title="Collapse all folders"
                    aria-label="Collapse all folders"
                    onClick={() => setCollapsed(new Set(treeDirPaths(tree)))}
                    className="flex h-5 w-5 items-center justify-center rounded text-tx-3 hover:bg-bg-3 hover:text-tx-1"
                  >
                    <ChevronsDownUp size={12} />
                  </button>
                  <button
                    type="button"
                    title="Expand all folders"
                    aria-label="Expand all folders"
                    onClick={() => setCollapsed(new Set())}
                    className="flex h-5 w-5 items-center justify-center rounded text-tx-3 hover:bg-bg-3 hover:text-tx-1"
                  >
                    <ChevronsUpDown size={12} />
                  </button>
                </span>
              )}
            </span>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto px-1.5 py-1.5">
            {error && <div className="px-2 py-3 text-xs text-urgent">{error}</div>}
            {summaryLoading && <div className="px-2 py-3 text-xs text-tx-3">Loading changes…</div>}
            {!error && !summaryLoading && files.length === 0 && (
              <div className="px-2 py-3 text-xs text-tx-3">No changes vs {baseLabel}.</div>
            )}
            {treeRows.map((r) =>
              r.kind === "dir" ? (
                <DirRow
                  key={`d:${r.node.key}`}
                  node={r.node}
                  depth={r.depth}
                  collapsed={collapsed.has(r.node.key)}
                  onToggle={() => toggleDir(r.node.key)}
                />
              ) : (
                <FileRow
                  key={`f:${r.file.path}`}
                  file={r.file}
                  depth={r.depth}
                  selected={r.file.path === selectedPath}
                  onSelect={() => setSelectedPath(r.file.path)}
                />
              ),
            )}
          </div>
        </aside>

        {/* diff pane */}
        <section className="flex min-w-0 flex-1 flex-col bg-bg-1">
          <DiffHeader file={selectedFile} />
          {/* Banner lives OUTSIDE the scroll container so it never offsets the virtualizer's
              row positions (which assume row 0 at scrollTop 0). */}
          {fileDiff?.truncated && (
            <div className="border-b border-line bg-bg-2 px-3 py-1 text-[11px] text-busy">
              Large file — diff truncated for performance.
            </div>
          )}
          <div ref={scrollRef} className="min-h-0 flex-1 overflow-auto">
            <DiffBody
              file={selectedFile}
              fileDiff={fileDiff}
              loading={fileLoading}
              error={fileError}
              sbs={sbs}
              scrollRef={scrollRef}
            />
          </div>
        </section>
      </div>
    </div>
  );
}

/** Tree indentation: a fixed inset plus one step per nesting level. Files sit one chevron-width
 *  further in than folders at the same depth, so they line up under their folder's name. */
const TREE_INSET_PX = 6;
const TREE_STEP_PX = 12;
const CHEVRON_PX = 15;

function DirRow({
  node,
  depth,
  collapsed,
  onToggle,
}: {
  node: DirNode<DiffFileSummary>;
  depth: number;
  collapsed: boolean;
  onToggle: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onToggle}
      aria-expanded={!collapsed}
      title={`${node.path}/ — ${node.fileCount} ${node.fileCount === 1 ? "file" : "files"}`}
      style={{ paddingLeft: TREE_INSET_PX + depth * TREE_STEP_PX }}
      className="mt-0.5 flex w-full items-center gap-1 rounded-md py-1 pr-2 text-left font-mono text-[11px] text-tx-2 hover:bg-bg-2"
    >
      {collapsed ? (
        <ChevronRight size={11} className="flex-none text-tx-3" />
      ) : (
        <ChevronDown size={11} className="flex-none text-tx-3" />
      )}
      <span className="min-w-0 flex-1 truncate">{node.name}/</span>
      <span className="flex flex-none items-center gap-1.5 text-[10.5px] tabular-nums">
        <span className="text-tx-3">{node.fileCount}</span>
        {node.added > 0 && <span style={{ color: "var(--diff-add-edge)" }}>+{node.added}</span>}
        {node.deleted > 0 && <span style={{ color: "var(--diff-del-edge)" }}>−{node.deleted}</span>}
      </span>
    </button>
  );
}

function FileRow({
  file,
  depth,
  selected,
  onSelect,
}: {
  file: DiffFileSummary;
  depth: number;
  selected: boolean;
  onSelect: () => void;
}) {
  const badge = STATUS_BADGE[file.status];
  const dir = (p: string) => p.slice(0, p.lastIndexOf("/") + 1);
  const name = file.path.slice(file.path.lastIndexOf("/") + 1);
  // For a rename, show just the old basename when it moved within the same folder, but the full
  // old path when the folder changed — otherwise a move between same-named files reads "r.ts → r.ts".
  const oldLabel = file.oldPath
    ? dir(file.oldPath) === dir(file.path)
      ? file.oldPath.slice(file.oldPath.lastIndexOf("/") + 1)
      : file.oldPath
    : undefined;
  return (
    <button
      type="button"
      onClick={onSelect}
      title={file.oldPath ? `${file.oldPath} → ${file.path}` : file.path}
      style={{ paddingLeft: TREE_INSET_PX + depth * TREE_STEP_PX + CHEVRON_PX }}
      className={`flex w-full items-center gap-2 rounded-md py-1 pr-2 text-left text-[12.5px] ${
        selected ? "bg-accent-dim" : "hover:bg-bg-2"
      }`}
    >
      <span
        className={`flex h-4 w-4 flex-none items-center justify-center rounded font-mono text-[10px] font-bold ${badge.cls}`}
      >
        {badge.letter}
      </span>
      <span className={`min-w-0 flex-1 truncate font-mono ${selected ? "text-accent-text" : ""}`}>
        {oldLabel && <span className="text-tx-3 line-through">{oldLabel} → </span>}
        {name}
      </span>
      <span className="flex flex-none gap-1.5 font-mono text-[10.5px] tabular-nums">
        {file.added > 0 && <span style={{ color: "var(--diff-add-edge)" }}>+{file.added}</span>}
        {file.deleted > 0 && <span style={{ color: "var(--diff-del-edge)" }}>−{file.deleted}</span>}
      </span>
    </button>
  );
}

function DiffHeader({ file }: { file: DiffFileSummary | null }) {
  return (
    <div className="flex items-center gap-2 border-b border-line bg-bg-2 px-3 py-2">
      <span className="min-w-0 flex-1 truncate font-mono text-[12.5px]">
        {file ? (
          <>
            {file.oldPath && <span className="text-tx-3">{file.oldPath} → </span>}
            {file.path}
          </>
        ) : (
          "—"
        )}
      </span>
      {file && (
        <span className="flex flex-none gap-2 font-mono text-[11.5px] tabular-nums">
          <span style={{ color: "var(--diff-add-edge)" }}>+{file.added}</span>
          <span style={{ color: "var(--diff-del-edge)" }}>−{file.deleted}</span>
        </span>
      )}
    </div>
  );
}

function DiffBody({
  file,
  fileDiff,
  loading,
  error,
  sbs,
  scrollRef,
}: {
  file: DiffFileSummary | null;
  fileDiff: FileDiff | null;
  loading: boolean;
  error: string | null;
  sbs: boolean;
  scrollRef: React.RefObject<HTMLDivElement | null>;
}) {
  const rows = useMemo(() => {
    if (!fileDiff) return [];
    return sbs ? sideBySideRows(fileDiff.hunks) : inlineRows(fileDiff.hunks);
  }, [fileDiff, sbs]);

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_ESTIMATE,
    overscan: 24,
    // Round measured heights to whole pixels. Rows are absolutely positioned at
    // the cumulative sum of their heights; fractional line heights (12.5px text at
    // 1.5 line-height = 18.75px/line) otherwise land adjacent rows on sub-pixel
    // boundaries, leaving a thin seam of the body background between them.
    measureElement: (el) => Math.round(el.getBoundingClientRect().height),
  });

  if (!file) {
    return (
      <div className="px-6 py-10 text-center text-sm text-tx-3">
        Select a file to view its diff.
      </div>
    );
  }
  if (error) return <div className="px-6 py-10 text-center text-sm text-urgent">{error}</div>;
  if (loading && !fileDiff) {
    return <div className="px-6 py-10 text-center text-sm text-tx-3">Loading diff…</div>;
  }
  if (fileDiff?.binary) {
    return <div className="px-6 py-10 text-center text-sm text-tx-3">Binary file — not shown.</div>;
  }
  if (fileDiff && fileDiff.hunks.length === 0) {
    return <div className="px-6 py-10 text-center text-sm text-tx-3">No textual changes.</div>;
  }

  const items = virtualizer.getVirtualItems();
  return (
    <div
      className="relative font-mono text-[12.5px] leading-[1.5]"
      style={{ height: virtualizer.getTotalSize() }}
    >
      {items.map((vi) => {
        const row = rows[vi.index];
        return (
          <div
            key={vi.key}
            data-index={vi.index}
            ref={virtualizer.measureElement}
            className="absolute left-0 top-0 w-full"
            style={{ transform: `translateY(${vi.start}px)` }}
          >
            {sbs ? (
              <SbsRowView row={row as SbsRow} />
            ) : (
              <InlineRowView row={row as ReturnType<typeof inlineRows>[number]} />
            )}
          </div>
        );
      })}
    </div>
  );
}

function HunkHeader({ text }: { text: string }) {
  return (
    <div className="bg-bg-2 px-3 py-0.5 text-[11.5px] text-tx-3 border-y border-line">
      {text || "@@"}
    </div>
  );
}

const GUTTER = "w-12 flex-none select-none px-2 text-right text-tx-3 tabular-nums";
const CODE = "min-w-0 flex-1 whitespace-pre-wrap break-words px-3";

function lineClasses(kind: DiffLine["kind"]): string {
  if (kind === "add") return "dln-add";
  if (kind === "del") return "dln-del";
  return "";
}
function gutterClasses(kind: DiffLine["kind"]): string {
  if (kind === "add") return "dgut-add";
  if (kind === "del") return "dgut-del";
  return "";
}
function sign(kind: DiffLine["kind"]): string {
  return kind === "add" ? "+" : kind === "del" ? "−" : " ";
}

/** A line's text, syntax-highlighted via its spans (plain text when there are none). */
function HlText({ line }: { line: DiffLine }) {
  if (!line.spans || line.spans.length === 0) return <>{line.text}</>;
  let off = 0;
  return (
    <>
      {splitBySpans(line.text, line.spans).map((c) => {
        const key = off;
        off += c.text.length;
        return c.cls ? (
          <span key={key} className={`tok-${c.cls}`}>
            {c.text}
          </span>
        ) : (
          <span key={key}>{c.text}</span>
        );
      })}
    </>
  );
}

function InlineRowView({ row }: { row: ReturnType<typeof inlineRows>[number] }) {
  if (row.kind === "hunk") return <HunkHeader text={row.header} />;
  const l = row.line;
  return (
    <div className={`flex ${lineClasses(l.kind)}`}>
      <span className={`${GUTTER} ${gutterClasses(l.kind)}`}>{l.oldNo ?? ""}</span>
      <span className={`${GUTTER} ${gutterClasses(l.kind)}`}>{l.newNo ?? ""}</span>
      <span className={CODE}>
        <span className="inline-block w-[1ch] select-none">{sign(l.kind)}</span>
        <HlText line={l} />
      </span>
    </div>
  );
}

function SbsRowView({ row }: { row: SbsRow }) {
  if (row.kind === "hunk") return <HunkHeader text={row.header} />;
  const { left, right } = row;
  return (
    <div className="flex">
      <div
        className={`flex w-1/2 min-w-0 border-r border-line ${left ? lineClasses(left.kind) : ""}`}
      >
        <span className={`${GUTTER} ${left ? gutterClasses(left.kind) : ""}`}>
          {left?.oldNo ?? ""}
        </span>
        <span className={CODE}>
          {left && <span className="inline-block w-[1ch] select-none">{sign(left.kind)}</span>}
          {left && <HlText line={left} />}
        </span>
      </div>
      <div className={`flex w-1/2 min-w-0 ${right ? lineClasses(right.kind) : ""}`}>
        <span className={`${GUTTER} ${right ? gutterClasses(right.kind) : ""}`}>
          {right?.newNo ?? ""}
        </span>
        <span className={CODE}>
          {right && <span className="inline-block w-[1ch] select-none">{sign(right.kind)}</span>}
          {right && <HlText line={right} />}
        </span>
      </div>
    </div>
  );
}

/** Header control: shows the resolved diff base and lets the user set a per-worktree override
 *  (Default / Auto / Upstream / a ref's tip / a fork-point parent). Writes via `onApply`. */
function DiffBasePicker({
  base,
  label,
  onApply,
}: {
  base: DiffBaseInfo | null;
  label: string;
  onApply: (base: DiffBase | null) => void | Promise<void>;
}) {
  const [open, setOpen] = useState(false);
  // `null` = the option menu; "ref"/"forkPoint" = the text-entry sub-step for that kind.
  const [entry, setEntry] = useState<null | "ref" | "forkPoint">(null);
  const [text, setText] = useState("");

  const close = () => {
    setOpen(false);
    setEntry(null);
    setText("");
  };
  const choose = (b: DiffBase | null) => {
    void onApply(b);
    close();
  };
  const submitText = () => {
    const v = text.trim();
    if (!v) return;
    choose(entry === "ref" ? { kind: "ref", name: v } : { kind: "forkPoint", parent: v });
  };

  return (
    <div className="relative">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className={`inline-flex items-center gap-0.5 rounded-chip px-1 text-xs hover:bg-bg-3 ${
          base?.error ? "text-urgent" : "text-tx-3"
        }`}
        title="Change the diff base"
      >
        vs {label}
        <ChevronDown size={11} className="flex-none opacity-70" />
      </button>
      {open && (
        <>
          {/* click-away backdrop */}
          <button
            type="button"
            aria-label="Dismiss"
            className="fixed inset-0 z-40 cursor-default"
            onClick={close}
          />
          <div className="absolute left-0 top-full z-50 mt-1 w-56 rounded-md border border-line bg-bg-2 p-1 text-xs shadow-lg">
            {entry === null ? (
              <>
                <PickerItem label="Project default" onClick={() => choose(null)} />
                <PickerItem
                  label="Auto (origin/main → main)"
                  onClick={() => choose({ kind: "auto" })}
                />
                <PickerItem
                  label="Upstream (@{upstream})"
                  onClick={() => choose({ kind: "upstream" })}
                />
                <PickerItem
                  label="Compare to a ref's tip…"
                  onClick={() => {
                    setEntry("ref");
                    setText("");
                  }}
                />
                <PickerItem
                  label="Fork point of…"
                  onClick={() => {
                    setEntry("forkPoint");
                    setText("");
                  }}
                />
              </>
            ) : (
              <div className="p-1">
                <div className="mb-1 text-tx-3">
                  {entry === "ref" ? "Ref to diff against (tip)" : "Parent branch (fork point)"}
                </div>
                <input
                  autoFocus
                  value={text}
                  onChange={(e) => setText(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") submitText();
                    else if (e.key === "Escape") setEntry(null);
                  }}
                  placeholder={entry === "ref" ? "origin/release-5.2" : "develop"}
                  className="w-full rounded border border-line bg-bg-1 px-1.5 py-1 font-mono text-tx-1 outline-none focus:border-accent"
                />
                <div className="mt-1 flex justify-end gap-1">
                  <button
                    type="button"
                    onClick={() => setEntry(null)}
                    className="rounded px-2 py-0.5 text-tx-2 hover:bg-bg-3"
                  >
                    Back
                  </button>
                  <button
                    type="button"
                    onClick={submitText}
                    className="rounded bg-accent-dim px-2 py-0.5 text-accent-text"
                  >
                    Apply
                  </button>
                </div>
              </div>
            )}
          </div>
        </>
      )}
    </div>
  );
}

function PickerItem({ label, onClick }: { label: string; onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="block w-full rounded px-2 py-1 text-left text-tx-2 hover:bg-bg-3 hover:text-tx-1"
    >
      {label}
    </button>
  );
}
