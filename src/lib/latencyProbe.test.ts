import { describe, expect, it, vi } from "vitest";
import { createAggregator, percentile } from "./latencyProbe";

vi.mock("./ipc", () => ({ latencyProbeEnabled: vi.fn().mockResolvedValue(false) }));

describe("percentile", () => {
  it("uses nearest rank", () => {
    const v = Array.from({ length: 100 }, (_, i) => i + 1);
    expect(percentile(v, 50)).toBe(50);
    expect(percentile(v, 95)).toBe(95);
    expect(percentile(v, 99)).toBe(99);
    expect(percentile(v, 0)).toBe(1);
  });

  it("handles empty and single-element input", () => {
    expect(percentile([], 50)).toBe(0);
    expect(percentile([7], 99)).toBe(7);
  });
});

describe("createAggregator", () => {
  it("logs once per batch per hop and resets", () => {
    const log = vi.fn();
    const record = createAggregator(log, 5);
    for (const ms of [5, 1, 3, 2]) record("a", ms);
    record("b", 1);
    expect(log).not.toHaveBeenCalled();
    record("a", 4);
    expect(log).toHaveBeenCalledTimes(1);
    expect(log).toHaveBeenCalledWith("latency-probe hop=a n=5 p50=3.00 p95=5.00 p99=5.00");
    record("a", 1);
    expect(log).toHaveBeenCalledTimes(1);
  });
});
