import { describe, expect, it } from "vitest";
import {
  classifyAttention,
  isWorktreeFocused,
  latchIdleFromTurn,
  needsInput,
  nextAcknowledged,
  projectChanged,
  shouldSuppressNotification,
} from "./attention";
import type { SessionState } from "./sessions";

describe("latchIdleFromTurn", () => {
  it("latches on busy→idle (turn finished)", () => {
    expect(latchIdleFromTurn("busy", "idle", false)).toBe(true);
  });

  it("latches on waiting→idle (permission resolved, then done)", () => {
    expect(latchIdleFromTurn("waiting", "idle", false)).toBe(true);
  });

  it("does NOT latch a fresh idle (SessionStart, no prior state)", () => {
    expect(latchIdleFromTurn(undefined, "idle", false)).toBe(false);
  });

  it("preserves the latch on idle→idle", () => {
    expect(latchIdleFromTurn("idle", "idle", true)).toBe(true);
    expect(latchIdleFromTurn("idle", "idle", false)).toBe(false);
  });

  it("clears the latch when work resumes (→busy)", () => {
    expect(latchIdleFromTurn("idle", "busy", true)).toBe(false);
  });

  it("clears the latch when the session goes offline", () => {
    expect(latchIdleFromTurn("idle", "offline", true)).toBe(false);
  });
});

describe("needsInput", () => {
  it("is true for waiting regardless of latch", () => {
    expect(needsInput("waiting", false)).toBe(true);
    expect(needsInput("waiting", true)).toBe(true);
  });

  it("is true for idle only when latched from a turn", () => {
    expect(needsInput("idle", true)).toBe(true);
    expect(needsInput("idle", false)).toBe(false);
  });

  it("is false for busy and offline", () => {
    expect(needsInput("busy", true)).toBe(false);
    expect(needsInput("offline", true)).toBe(false);
  });
});

describe("classifyAttention", () => {
  it("classifies waiting as 'waiting' regardless of latch", () => {
    expect(classifyAttention("waiting", false)).toBe("waiting");
    expect(classifyAttention("waiting", true)).toBe("waiting");
  });

  it("classifies idle as 'finished' only when latched from a turn", () => {
    expect(classifyAttention("idle", true)).toBe("finished");
    expect(classifyAttention("idle", false)).toBeNull();
  });

  it("returns null for busy and offline", () => {
    expect(classifyAttention("busy", true)).toBeNull();
    expect(classifyAttention("offline", true)).toBeNull();
  });

  it("is non-null exactly when needsInput is true (stays consistent)", () => {
    const states: SessionState[] = ["idle", "busy", "waiting", "offline"];
    for (const state of states) {
      for (const latch of [false, true]) {
        expect(classifyAttention(state, latch) !== null).toBe(needsInput(state, latch));
      }
    }
  });
});

describe("isWorktreeFocused", () => {
  it("is true only for the selected worktree in a non-board view", () => {
    expect(isWorktreeFocused("focus", "a", "a")).toBe(true);
    expect(isWorktreeFocused("grid", "a", "a")).toBe(true);
  });

  it("is false for a non-selected worktree", () => {
    expect(isWorktreeFocused("grid", "a", "b")).toBe(false);
    expect(isWorktreeFocused("focus", "a", "b")).toBe(false);
  });

  it("is false in board view (no terminal focused)", () => {
    expect(isWorktreeFocused("board", "a", "a")).toBe(false);
  });
});

describe("shouldSuppressNotification", () => {
  // The exact regression behind #21: in grid view with the app focused, a NON-selected
  // worktree must still notify (it was being suppressed before this fix).
  it("does NOT suppress a non-selected worktree in grid view (the bug)", () => {
    expect(shouldSuppressNotification(true, "grid", "a", "b")).toBe(false);
  });

  it("suppresses the selected/focused worktree when the app is focused", () => {
    expect(shouldSuppressNotification(true, "grid", "a", "a")).toBe(true);
    expect(shouldSuppressNotification(true, "focus", "a", "a")).toBe(true);
  });

  it("does NOT suppress when the app is not focused (any view)", () => {
    expect(shouldSuppressNotification(false, "grid", "a", "a")).toBe(false);
    expect(shouldSuppressNotification(false, "focus", "a", "a")).toBe(false);
    expect(shouldSuppressNotification(false, "board", "a", "a")).toBe(false);
  });

  it("never suppresses in board view", () => {
    expect(shouldSuppressNotification(true, "board", "a", "a")).toBe(false);
  });
});

describe("nextAcknowledged", () => {
  // focused === appFocused && isWorktreeFocused(...) — the worktree the user is viewing.
  it("acknowledges a worktree the user is viewing while it needs input", () => {
    expect(nextAcknowledged(false, "waiting", true, true)).toBe(true);
    expect(nextAcknowledged(false, "idle", true, true)).toBe(true);
  });

  it("re-arms on a new turn (→busy) even if previously acknowledged", () => {
    expect(nextAcknowledged(true, "busy", false, false)).toBe(false);
    expect(nextAcknowledged(true, "busy", false, true)).toBe(false);
  });

  it("re-arms when the session goes offline", () => {
    expect(nextAcknowledged(true, "offline", false, false)).toBe(false);
  });

  it("drops the ack once the worktree no longer needs input", () => {
    expect(nextAcknowledged(true, "idle", false, true)).toBe(false);
  });

  it("preserves the prior ack for an unfocused worktree still needing input", () => {
    expect(nextAcknowledged(true, "waiting", true, false)).toBe(true);
    expect(nextAcknowledged(false, "waiting", true, false)).toBe(false);
  });
});

describe("projectChanged", () => {
  it("is true when transitioning from null to a project name", () => {
    expect(projectChanged(null, "a")).toBe(true);
  });

  it("is false when the project name is unchanged", () => {
    expect(projectChanged("a", "a")).toBe(false);
  });

  it("is true when switching between two different project names", () => {
    expect(projectChanged("a", "b")).toBe(true);
  });

  it("is true when transitioning from a project name back to null", () => {
    expect(projectChanged("a", null)).toBe(true);
  });
});

describe("classifyAttention seed-path invariant", () => {
  // The seed path calls classifyAttention(state, false) for every worktree.
  // This invariant ensures a reseed after a project switch can never emit a stale
  // 'finished' kind (which would require idleFromTurn=true, impossible on a fresh seed).
  it("idle with idleFromTurn=false yields null (no stale finished on reseed)", () => {
    expect(classifyAttention("idle", false)).toBeNull();
  });

  it("waiting with idleFromTurn=false still yields 'waiting' (genuine need preserved)", () => {
    expect(classifyAttention("waiting", false)).toBe("waiting");
  });
});
