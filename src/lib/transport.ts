// Transport seam. The four frontend bridges (ipc/tasks/dispatch/sessions) call
// through a `Transport` rather than importing the Tauri `invoke`/`listen`
// primitives inline. The desktop app uses `localTransport` — a verbatim
// pass-through to `@tauri-apps/api`. The seam is retained so tests can inject a
// mock transport (see tasks.test.ts); production always uses `localTransport`.

import { Channel, invoke as tauriInvoke } from "@tauri-apps/api/core";
import { type Event, listen as tauriListen, type UnlistenFn } from "@tauri-apps/api/event";

// ── The seam ───────────────────────────────────────────────────────────────────

export interface Transport {
  /** Invoke a logical backend command. Mirrors Tauri `invoke`. */
  invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
  /** Subscribe to a logical event channel. Mirrors Tauri `listen`. */
  listen<T>(channel: string, cb: (payload: T) => void): Promise<UnlistenFn>;
  /**
   * A one-way backend → webview channel delivering each message to `onMessage`,
   * in order. Pass the returned value as a command argument (Tauri `Channel`).
   */
  channel<T>(onMessage: (message: T) => void): unknown;
}

// ── localTransport — verbatim pass-through to the Tauri backend ─────────────────

export const localTransport: Transport = {
  invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
    return tauriInvoke<T>(cmd, args);
  },
  listen<T>(channel: string, cb: (payload: T) => void): Promise<UnlistenFn> {
    return tauriListen<T>(channel, (e) => cb(e.payload));
  },
  channel<T>(onMessage: (message: T) => void): unknown {
    return new Channel<T>(onMessage);
  },
};

// ── Module singleton ────────────────────────────────────────────────────────────

let active: Transport = localTransport;

/** Swap the active transport. Retained for tests (inject a mock before a bridge runs). */
export function setTransport(t: Transport): void {
  active = t;
}

/** The active transport (default: `localTransport`). */
export function getTransport(): Transport {
  return active;
}

/**
 * A `listen` adapter shaped like the Tauri `listen` so it slots into
 * `useLiveResource`'s `options.listen` injection point WITHOUT changing its
 * lifecycle. The hook only uses it to trigger a reload (it ignores the payload),
 * so we wrap the active transport's `listen` and re-package the bare payload into
 * the `Event` envelope the Tauri signature expects.
 */
export const liveListen: typeof tauriListen = (<T>(
  channel: string,
  handler: (event: Event<T>) => void,
): Promise<UnlistenFn> =>
  active.listen<T>(channel, (payload) =>
    handler({ event: channel, id: 0, payload } as Event<T>),
  )) as typeof tauriListen;
