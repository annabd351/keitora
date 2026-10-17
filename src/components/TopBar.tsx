// Top bar: brand, a read-only `project / worktree` breadcrumb, and the
// Board/Focus/Grid view switch. Project switching lives in the rail's project groups.

import keitoraMark from "../assets/keitora-mark.png";

export type View = "board" | "focus" | "grid";

const VIEWS: { id: View; label: string }[] = [
  { id: "board", label: "Board" },
  { id: "focus", label: "Focus" },
  { id: "grid", label: "Grid" },
];

export function TopBar({
  view,
  onView,
  projectLabel,
  worktreeLabel,
}: {
  view: View;
  onView: (v: View) => void;
  /** Active project's registry key; the breadcrumb is hidden without it. */
  projectLabel?: string | null;
  /** Selected worktree name; the separator and worktree segment show only when set. */
  worktreeLabel?: string | null;
}) {
  const crumb = projectLabel
    ? worktreeLabel
      ? `${projectLabel} / ${worktreeLabel}`
      : projectLabel
    : undefined;
  return (
    <header className="topbar">
      <div className="brand">
        <img className="glyph" src={keitoraMark} alt="Keitora" />
        <span className="name">Keitora</span>
      </div>

      {projectLabel ? (
        <div className="topbar-crumb" title={crumb}>
          <span className="crumb-proj">{projectLabel}</span>
          {worktreeLabel ? (
            <>
              <span className="crumb-sep">/</span>
              <span className="crumb-wt">{worktreeLabel}</span>
            </>
          ) : null}
        </div>
      ) : null}

      <div className="views">
        {VIEWS.map((v) => (
          <button key={v.id} className={view === v.id ? "active" : ""} onClick={() => onView(v.id)}>
            {v.label}
          </button>
        ))}
      </div>
    </header>
  );
}
