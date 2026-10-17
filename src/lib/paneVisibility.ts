import type { View } from "../components/TopBar";

/**
 * Whether a worktree's terminal pane is currently on-screen — drives the
 * TerminalPane `active` prop in App.tsx. (Notification suppression is a separate,
 * focus-based concern; see isWorktreeFocused in attention.ts.)
 *
 * - `grid`     → all panes are on-screen.
 * - `focus`    → only the selected worktree's pane is on-screen.
 * - `board`    → terminals are hidden (display:none).
 *
 * Only the terminal views (`grid`/`focus`) make a pane visible — `board` hides
 * the terminal stage, so a pane there must NOT be marked active (else it
 * fits/resizes a 0-size container).
 */
export function isPaneVisible(view: View, selected: string | null, worktree: string): boolean {
  return view === "grid" || (view === "focus" && selected === worktree);
}
