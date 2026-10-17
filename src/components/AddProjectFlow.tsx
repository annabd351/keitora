// Presentation-agnostic add-project flow: folder picker → onboard_project →
// worktree picker → onComplete. Used by both Onboarding (first-run full-screen)
// and AddProjectDialog (on-demand modal). Extracted from Onboarding.tsx so the
// step machine and handlers are shared.

import { open } from "@tauri-apps/plugin-dialog";
import { useEffect, useState } from "react";
import { detectLegacy, migrateLegacy, onboardProject, type ProjectSummary } from "../lib/ipc";
import { WorktreePicker } from "./WorktreePicker";

export interface AddProjectFlowProps {
  /** Called after a project is successfully registered and active. */
  onComplete: () => void;
  /** Called when the user cancels (only wired in dialog context; absent → no cancel affordance). */
  onCancel?: () => void;
}

type Step = "entry" | "checking" | "error" | "pick-worktrees";

export function AddProjectFlow({ onComplete, onCancel }: AddProjectFlowProps) {
  const [step, setStep] = useState<Step>("entry");
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const [projectSummary, setProjectSummary] = useState<ProjectSummary | null>(null);
  // Path of a detected legacy task-config.json, if any — drives an explicit
  // "import" offer. We detect (read-only) on mount; migration only happens when
  // the user clicks Import, so we never write a keitora.toml without consent.
  const [legacyPath, setLegacyPath] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    void detectLegacy()
      .then((p) => {
        if (!cancelled) setLegacyPath(p);
      })
      .catch(() => {
        /* detection is best-effort; the folder picker is always available */
      });
    return () => {
      cancelled = true;
    };
  }, []);

  async function handleImportLegacy() {
    setStep("checking");
    setErrorMsg(null);
    try {
      const summary = await migrateLegacy();
      if (summary !== null) {
        onComplete();
      } else {
        // Raced away between detect and import — fall back to the entry screen.
        setLegacyPath(null);
        setStep("entry");
      }
    } catch (e: unknown) {
      setErrorMsg(String(e));
      setStep("error");
    }
  }

  async function handleChooseFolder() {
    const path: string | null = await open({
      directory: true,
      multiple: false,
      title: "Choose project folder",
    });
    // User cancelled the picker — stay on entry.
    if (path === null) return;

    setStep("checking");
    setErrorMsg(null);
    try {
      const summary = await onboardProject(path);
      // Success: advance to worktree picker step.
      setProjectSummary(summary);
      setStep("pick-worktrees");
    } catch (e: unknown) {
      setErrorMsg(String(e));
      setStep("error");
    }
  }

  if (step === "checking") {
    return (
      <div className="msg-card">
        <p>Checking folder…</p>
      </div>
    );
  }

  // Step 2: worktree picker after a project was onboarded.
  if (step === "pick-worktrees" && projectSummary !== null) {
    return (
      <WorktreePicker projectRoot={projectSummary.root} onDone={onComplete} onSkip={onComplete} />
    );
  }

  // entry | error
  return (
    <div className="msg-card">
      <b>No project configured</b>
      {errorMsg && (
        <p className="mono" style={{ color: "var(--urgent)" }}>
          {errorMsg}
        </p>
      )}
      <div style={{ display: "flex", flexDirection: "column", gap: "8px", marginTop: "12px" }}>
        <button className="pane-btn" onClick={() => void handleChooseFolder()}>
          Choose project folder…
        </button>
        {legacyPath !== null && (
          <button className="pane-btn" onClick={() => void handleImportLegacy()}>
            Import existing task-config.json
          </button>
        )}
        {onCancel && (
          <button
            className="pane-btn"
            style={{
              background: "transparent",
              color: "var(--tx-2)",
              border: "1px solid var(--line)",
            }}
            onClick={onCancel}
          >
            Cancel
          </button>
        )}
      </div>
      {legacyPath !== null && (
        <p className="mono" style={{ marginTop: "6px", fontSize: "11px", opacity: 0.7 }}>
          Found a legacy config at {legacyPath}
        </p>
      )}
      {step === "error" && (
        <button
          className="pane-btn"
          style={{ marginTop: "8px" }}
          onClick={() => {
            setStep("entry");
            setErrorMsg(null);
          }}
        >
          Try again
        </button>
      )}
    </div>
  );
}
