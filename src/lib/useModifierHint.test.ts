import { describe, expect, it } from "vitest";
import { type HintAction, type HintState, hintReducer } from "./useModifierHint";

// Helper to apply a sequence of actions and return the final state.
function run(initial: HintState, ...actions: HintAction[]): HintState {
  return actions.reduce((s, a) => hintReducer(s, a), initial);
}

describe("hintReducer", () => {
  // ── Basic transitions ──────────────────────────────────────────────────────

  it("idle → pending on MODIFIER_DOWN", () => {
    expect(hintReducer("idle", { type: "MODIFIER_DOWN" })).toBe("pending");
  });

  it("pending → visible on TIMER_FIRED", () => {
    expect(hintReducer("pending", { type: "TIMER_FIRED" })).toBe("visible");
  });

  // ── RELEASE from every state → idle (modifier up / blur / tab hide) ─────────

  it("idle → idle on RELEASE", () => {
    expect(hintReducer("idle", { type: "RELEASE" })).toBe("idle");
  });

  it("pending → idle on RELEASE", () => {
    expect(hintReducer("pending", { type: "RELEASE" })).toBe("idle");
  });

  it("visible → idle on RELEASE", () => {
    expect(hintReducer("visible", { type: "RELEASE" })).toBe("idle");
  });

  // ── OTHER_KEY: cancels a pending reveal, but never hides a visible hint ──────

  it("pending → idle on OTHER_KEY (no flash on a quick tap)", () => {
    expect(hintReducer("pending", { type: "OTHER_KEY" })).toBe("idle");
  });

  it("visible STAYS visible on OTHER_KEY (chord pressed while still holding)", () => {
    expect(hintReducer("visible", { type: "OTHER_KEY" })).toBe("visible");
  });

  it("idle stays idle on OTHER_KEY", () => {
    expect(hintReducer("idle", { type: "OTHER_KEY" })).toBe("idle");
  });

  it("visible survives repeated chords, hides only on RELEASE", () => {
    expect(
      run("visible", { type: "OTHER_KEY" }, { type: "OTHER_KEY" }, { type: "OTHER_KEY" }),
    ).toBe("visible");
    expect(run("visible", { type: "OTHER_KEY" }, { type: "RELEASE" })).toBe("idle");
  });

  // ── MODIFIER_DOWN ignored when not idle ────────────────────────────────────

  it("pending stays pending on MODIFIER_DOWN (no re-arm)", () => {
    expect(hintReducer("pending", { type: "MODIFIER_DOWN" })).toBe("pending");
  });

  it("visible stays visible on MODIFIER_DOWN", () => {
    expect(hintReducer("visible", { type: "MODIFIER_DOWN" })).toBe("visible");
  });

  // ── TIMER_FIRED ignored when not pending ───────────────────────────────────

  it("idle stays idle on TIMER_FIRED (stale timer)", () => {
    expect(hintReducer("idle", { type: "TIMER_FIRED" })).toBe("idle");
  });

  it("visible stays visible on TIMER_FIRED (duplicate fire)", () => {
    expect(hintReducer("visible", { type: "TIMER_FIRED" })).toBe("visible");
  });

  // ── Full happy-path sequence ───────────────────────────────────────────────

  it("full cycle: idle → pending → visible → idle", () => {
    expect(
      run("idle", { type: "MODIFIER_DOWN" }, { type: "TIMER_FIRED" }, { type: "RELEASE" }),
    ).toBe("idle");
  });

  // ── Chord shortcut: OTHER_KEY before TIMER_FIRED keeps it idle ──────────────

  it("quick chord (MODIFIER_DOWN → OTHER_KEY) never reaches visible", () => {
    expect(run("idle", { type: "MODIFIER_DOWN" }, { type: "OTHER_KEY" })).toBe("idle");
  });
});
