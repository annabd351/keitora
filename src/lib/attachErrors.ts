// What a terminal pane does with a failed `pty_attach`. The backend answers with
// a stable leading token (host.rs `attach_err_kind`): "NotConnected" |
// "NoLiveSession" | "EndedWhileAway" | "Recovering" | "Terminated" |
// "Internal: …". Pure, so the decision is unit-testable.

export type AttachErrorAction =
  | "ended-while-away"
  | "recovering"
  | "terminated"
  | "retry"
  | "spawn";

/**
 * Classify an attach error. `Recovering` (the server is being recovered) parks
 * the pane quietly and never falls through to a spawn, even after the deadline;
 * `Terminated` (the user terminated the session) leaves it down until launched;
 * `NotConnected` retries until `deadlineMs`, then spawns; everything else
 * (`NoLiveSession` and the rest) spawns.
 */
export function classifyAttachError(
  msg: string,
  nowMs: number,
  deadlineMs: number,
): AttachErrorAction {
  if (msg.includes("EndedWhileAway") || msg.includes("ended-while-away")) {
    return "ended-while-away";
  }
  if (msg.includes("Recovering")) return "recovering";
  if (msg.includes("Terminated")) return "terminated";
  if (msg.includes("NotConnected") && nowMs < deadlineMs) return "retry";
  return "spawn";
}
