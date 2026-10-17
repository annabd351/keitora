import { describe, expect, it } from "vitest";
import { createPaneAttachments } from "./paneAttach";

describe("createPaneAttachments", () => {
  it("a successful re-attach replaces the earlier attach, leaving nothing to detach", () => {
    const a = createPaneAttachments();
    a.begin();
    a.succeeded(1);
    a.begin();
    expect(a.live).toBeNull();
    a.succeeded(2);
    expect(a.live).toBe(2);
    expect(a.release()).toEqual([2]);
  });

  it("a failed re-attach hands back the earlier attach to detach", () => {
    const a = createPaneAttachments();
    a.begin();
    a.succeeded(1);
    a.begin();
    expect(a.failed()).toBe(1);
    // Released once: the next failure has nothing left.
    a.begin();
    expect(a.failed()).toBeNull();
    expect(a.release()).toEqual([]);
  });

  it("unmount mid-attach releases the earlier attach still registered", () => {
    const a = createPaneAttachments();
    a.begin();
    a.succeeded(1);
    a.begin();
    expect(a.release()).toEqual([1]);
    expect(a.release()).toEqual([]);
  });

  it("a first attach that fails has nothing to release", () => {
    const a = createPaneAttachments();
    a.begin();
    expect(a.failed()).toBeNull();
    expect(a.release()).toEqual([]);
  });
});
