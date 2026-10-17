// Multiselect for discovered git worktrees. Shown after onboarding (step 2)
// and in the empty-worktrees branch of App.tsx.

import { useEffect, useState } from "react";
import {
  discoverWorktrees,
  setWorktrees,
  type WorktreeCandidate,
  type WorktreeEntry,
} from "../lib/ipc";

interface WorktreePickerProps {
  projectRoot: string;
  onDone: () => void;
  onSkip?: () => void;
  /**
   * Presentation. `"onboarding"` (default) is the transparent full-page card used
   * during first-run setup. `"modal"` gives an opaque dialog surface and a
   * "Cancel" affordance, for overlaying existing UI (e.g. the Settings drawer).
   */
  variant?: "onboarding" | "modal";
}

type LoadState = "loading" | "loaded" | "error";

export function WorktreePicker({
  projectRoot,
  onDone,
  onSkip,
  variant = "onboarding",
}: WorktreePickerProps) {
  const cardClass = variant === "modal" ? "msg-card is-modal" : "msg-card";
  const dismissLabel = variant === "modal" ? "Cancel" : "Skip for now";
  const [loadState, setLoadState] = useState<LoadState>("loading");
  const [candidates, setCandidates] = useState<WorktreeCandidate[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [loadError, setLoadError] = useState<string | null>(null);

  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  function load() {
    setLoadState("loading");
    setLoadError(null);
    discoverWorktrees(projectRoot)
      .then((list) => {
        setCandidates(list);
        // Default: check all candidates (product decision: pre-check everything).
        setSelected(new Set(list.map((c) => c.name)));
        setLoadState("loaded");
      })
      .catch((e: unknown) => {
        setLoadError(String(e));
        setLoadState("error");
      });
  }

  // Load on mount.
  useEffect(load, [projectRoot]);

  function toggle(name: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });
  }

  async function handleTrack() {
    setSaving(true);
    setSaveError(null);
    try {
      const entries: WorktreeEntry[] = Array.from(selected)
        .map((name) => {
          const candidate = candidates.find((c) => c.name === name);
          if (!candidate) return null;
          return { name, path: candidate.path };
        })
        .filter((e): e is WorktreeEntry => e !== null);
      await setWorktrees(entries);
      onDone();
    } catch (e: unknown) {
      setSaveError(String(e));
    } finally {
      setSaving(false);
    }
  }

  // --- Loading state ---
  if (loadState === "loading") {
    return (
      <div className={cardClass}>
        <p>Discovering worktrees…</p>
      </div>
    );
  }

  // --- Error state ---
  if (loadState === "error") {
    return (
      <div className={cardClass}>
        <b>Could not discover worktrees</b>
        <p className="mono" style={{ color: "var(--urgent)" }}>
          {loadError}
        </p>
        <button className="pane-btn" onClick={load}>
          Retry
        </button>
        {onSkip && (
          <button
            className="pane-btn"
            style={{
              background: "transparent",
              color: "var(--tx-2)",
              border: "1px solid var(--line)",
            }}
            onClick={onSkip}
          >
            {dismissLabel}
          </button>
        )}
      </div>
    );
  }

  // --- Loaded (empty) ---
  if (candidates.length === 0) {
    return (
      <div className={cardClass}>
        <b>No worktrees found</b>
        <p>No additional git worktrees were discovered under this project root.</p>
        {onSkip && (
          <button className="pane-btn" onClick={onSkip}>
            {variant === "modal" ? "Close" : "Continue"}
          </button>
        )}
      </div>
    );
  }

  // --- Loaded with candidates ---
  const count = selected.size;

  return (
    <div className={cardClass} style={{ alignItems: "stretch", maxWidth: 520, textAlign: "left" }}>
      <b style={{ textAlign: "center" }}>Pick worktrees to track</b>
      <p style={{ color: "var(--tx-2)", textAlign: "center", marginBottom: 4 }}>
        Select the worktrees you want Keitora to manage.
      </p>

      <div className="worktree-picker-list">
        {candidates.map((c) => (
          <label key={c.name} className="worktree-picker-row">
            <input
              type="checkbox"
              checked={selected.has(c.name)}
              onChange={() => toggle(c.name)}
              disabled={saving}
            />
            <div className="worktree-picker-info">
              <span className="worktree-picker-name">{c.name}</span>
              {c.branch && <span className="worktree-picker-branch">{c.branch}</span>}
              <span className="worktree-picker-path mono">{c.path}</span>
            </div>
          </label>
        ))}
      </div>

      {saveError && (
        <p className="mono" style={{ color: "var(--urgent)", fontSize: 11 }}>
          {saveError}
        </p>
      )}

      <div style={{ display: "flex", flexDirection: "column", gap: 8, marginTop: 8 }}>
        <button
          className="pane-btn"
          disabled={saving || count === 0}
          onClick={() => void handleTrack()}
        >
          {saving ? "Saving…" : `Track ${count} worktree${count === 1 ? "" : "s"}`}
        </button>
        {onSkip && (
          <button
            className="pane-btn"
            style={{
              background: "transparent",
              color: "var(--tx-2)",
              border: "1px solid var(--line)",
            }}
            disabled={saving}
            onClick={onSkip}
          >
            {dismissLabel}
          </button>
        )}
      </div>
    </div>
  );
}
