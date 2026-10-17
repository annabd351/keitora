import type { UnlistenFn } from "@tauri-apps/api/event";
import { beforeEach, describe, expect, it, vi } from "vitest";

// Mock the Tauri primitives so localTransport's pass-through + channel mapping is
// observable without a Tauri runtime.
const invokeMock = vi.hoisted(() => vi.fn());
const listenMock = vi.hoisted(() => vi.fn());
// Stands in for the Tauri `Channel`: keeps the handler so a test can play messages.
const MockChannel = vi.hoisted(
  () =>
    class {
      constructor(public onmessage: (message: unknown) => void) {}
    },
);

vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
  Channel: MockChannel,
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: listenMock,
}));

import {
  getTransport,
  liveListen,
  localTransport,
  setTransport,
  type Transport,
} from "./transport";

beforeEach(() => {
  invokeMock.mockReset().mockResolvedValue(undefined);
  listenMock.mockReset().mockResolvedValue((() => {}) as UnlistenFn);
  // Reset the module singleton to the default between tests.
  setTransport(localTransport);
});

describe("localTransport — pass-through to Tauri", () => {
  it("invoke forwards the command and args verbatim", async () => {
    invokeMock.mockResolvedValue(42);
    const out = await localTransport.invoke<number>("pty_spawn", { project: "p" });
    expect(invokeMock).toHaveBeenCalledWith("pty_spawn", { project: "p" });
    expect(out).toBe(42);
  });

  it("listen unwraps the event payload for the callback", async () => {
    let received: unknown;
    listenMock.mockImplementation((_channel: string, cb: (e: { payload: unknown }) => void) => {
      cb({ payload: { hello: "world" } });
      return Promise.resolve((() => {}) as UnlistenFn);
    });
    await localTransport.listen("sessions://changed", (p) => {
      received = p;
    });
    expect(listenMock).toHaveBeenCalledWith("sessions://changed", expect.any(Function));
    expect(received).toEqual({ hello: "world" });
  });

  it("channel builds a Tauri Channel that hands each message to the handler", () => {
    const got: unknown[] = [];
    const channel = localTransport.channel<string>((m) => got.push(m));
    expect(channel).toBeInstanceOf(MockChannel);
    (channel as InstanceType<typeof MockChannel>).onmessage("hi");
    expect(got).toEqual(["hi"]);
  });
});

describe("transport singleton", () => {
  it("defaults to localTransport", () => {
    expect(getTransport()).toBe(localTransport);
  });

  it("setTransport swaps the active transport (used by tests)", () => {
    const fake = { invoke: vi.fn() } as unknown as Transport;
    setTransport(fake);
    expect(getTransport()).toBe(fake);
  });

  it("liveListen routes through the active transport's listen", async () => {
    const hits: string[] = [];
    await liveListen("tasks://changed", (e) => hits.push(e.event));
    expect(listenMock).toHaveBeenCalledWith("tasks://changed", expect.any(Function));
  });
});
