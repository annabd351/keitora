import { describe, expect, it } from "vitest";
import { classifyAttachError } from "./attachErrors";

const NOW = 1_000;
const BEFORE = NOW + 5_000;
const AFTER = NOW - 1;

describe("classifyAttachError", () => {
  it("parks a recovering pane before and after the deadline, never spawning", () => {
    expect(classifyAttachError("Recovering", NOW, BEFORE)).toBe("recovering");
    expect(classifyAttachError("Recovering", NOW, AFTER)).toBe("recovering");
  });

  it("retries NotConnected until the deadline, then spawns", () => {
    expect(classifyAttachError("NotConnected", NOW, BEFORE)).toBe("retry");
    expect(classifyAttachError("NotConnected", NOW, AFTER)).toBe("spawn");
  });

  it("spawns on NoLiveSession and other errors", () => {
    expect(classifyAttachError("NoLiveSession", NOW, BEFORE)).toBe("spawn");
    expect(classifyAttachError("Internal: boom", NOW, BEFORE)).toBe("spawn");
  });

  it("leaves a terminated session down, never spawning", () => {
    expect(classifyAttachError("Terminated", NOW, BEFORE)).toBe("terminated");
    expect(classifyAttachError("Terminated", NOW, AFTER)).toBe("terminated");
  });

  it("prefers Recovering over Terminated", () => {
    expect(classifyAttachError("Recovering; Terminated", NOW, BEFORE)).toBe("recovering");
  });

  it("reports a session that ended while away", () => {
    expect(classifyAttachError("EndedWhileAway", NOW, BEFORE)).toBe("ended-while-away");
    expect(classifyAttachError("ended-while-away", NOW, AFTER)).toBe("ended-while-away");
  });
});
