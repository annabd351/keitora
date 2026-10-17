import { describe, expect, it } from "vitest";
import { classifyDispatch } from "./useDispatchActions";

describe("classifyDispatch", () => {
  it("treats a fresh inject as 'injected'", () => {
    expect(classifyDispatch({ dispatched: true, reason: null }, 5, "wt-a")).toEqual({
      kind: "injected",
    });
  });

  it("treats an inject with a non-'already dispatched' reason as 'injected'", () => {
    expect(classifyDispatch({ dispatched: true, reason: "claimed+injected" }, 5, "wt-a")).toEqual({
      kind: "injected",
    });
  });

  it("treats an idempotent re-apply (dispatched + 'already dispatched') as a no-op", () => {
    expect(classifyDispatch({ dispatched: true, reason: "already dispatched" }, 5, "wt-a")).toEqual(
      { kind: "noop" },
    );
  });

  it("treats a non-dispatched outcome as 'queued' with a per-task/worktree toast", () => {
    expect(classifyDispatch({ dispatched: false, reason: "busy" }, 42, "wt-b")).toEqual({
      kind: "queued",
      toast: "Queued #42 — @wt-b busy",
    });
  });

  it("queues even when a non-dispatched outcome carries no reason", () => {
    expect(classifyDispatch({ dispatched: false, reason: null }, 7, "wt-c")).toEqual({
      kind: "queued",
      toast: "Queued #7 — @wt-c busy",
    });
  });
});
