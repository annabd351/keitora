// useState that mirrors a boolean to localStorage under `key`, so simple UI
// preferences (e.g. panel collapsed/expanded) survive reloads. Reads the stored
// value once on init; writes on every subsequent change (the initial mount is
// skipped — the value was just read from storage). All storage access is guarded
// so private-mode / quota errors fall back to the in-memory default instead of
// throwing. The setter is React's own dispatcher, so callers may pass a value or
// an updater function (`(prev) => !prev`).

import { type Dispatch, type SetStateAction, useEffect, useRef, useState } from "react";

// The read-once / write-on-change / silent-fallback core shared by the boolean and JSON
// hooks. `parse` runs once on init (given `null` when the key is absent); `serialize`
// must be stable (module-level), since it is an effect dependency.
function usePersistedState<T>(
  key: string,
  initial: T,
  parse: (raw: string | null) => T,
  serialize: (value: T) => string,
): [T, Dispatch<SetStateAction<T>>] {
  const [value, setValue] = useState<T>(() => {
    try {
      return parse(window.localStorage.getItem(key));
    } catch {
      return initial;
    }
  });

  const firstRender = useRef(true);
  useEffect(() => {
    if (firstRender.current) {
      firstRender.current = false;
      return;
    }
    try {
      window.localStorage.setItem(key, serialize(value));
    } catch {
      // ignore — preference just won't persist this session
    }
  }, [key, value, serialize]);

  return [value, setValue];
}

const serializeBool = (v: boolean) => (v ? "1" : "0");

export function useLocalStorageState(
  key: string,
  initial: boolean,
): [boolean, Dispatch<SetStateAction<boolean>>] {
  return usePersistedState(
    key,
    initial,
    (raw) => (raw === null ? initial : raw === "1"),
    serializeBool,
  );
}

// Clamp a raw parsed number into [min, max], returning `fallback` when the
// parsed value is NaN (corrupt storage). Exported so it can be unit-tested
// independently of the React hook.
export function clampStoredNumber(
  raw: string | null,
  fallback: number,
  min: number,
  max: number,
): number {
  if (raw === null) return fallback;
  const n = Number(raw);
  if (!Number.isFinite(n)) return fallback;
  return Math.min(max, Math.max(min, n));
}

// Parse a stored JSON value, returning `fallback` when the key is absent (`null`),
// the text is not valid JSON, or the parsed value fails `validate`. Exported so it
// can be unit-tested independently of the React hook (mirrors clampStoredNumber).
export function parseStoredJson<T>(
  raw: string | null,
  fallback: T,
  validate: (v: unknown) => v is T,
): T {
  if (raw === null) return fallback;
  try {
    const v: unknown = JSON.parse(raw);
    return validate(v) ? v : fallback;
  } catch {
    return fallback;
  }
}

const serializeJson = (v: unknown) => JSON.stringify(v);

// useState that mirrors a JSON-serializable value to localStorage under `key`. A
// stored value that does not parse or fails `validate` falls back to `initial`.
// Same read-once / write-on-change / silent-fallback contract as
// `useLocalStorageState`; no cross-instance broadcast (meant for a single consumer).
export function useLocalStorageJson<T>(
  key: string,
  initial: T,
  validate: (v: unknown) => v is T,
): [T, Dispatch<SetStateAction<T>>] {
  return usePersistedState(
    key,
    initial,
    (raw) => parseStoredJson(raw, initial, validate),
    serializeJson,
  );
}

// Same-document subscribers per key. The browser `storage` event only fires in
// OTHER documents (never the writer), so two hook instances on one key in the
// SAME window — e.g. the terminal's font stepper and the Settings font stepper —
// would otherwise drift. Each mounted hook registers a setter here and every
// write broadcasts to the set, keeping all live instances in sync.
const numberSubs = new Map<string, Set<(v: number) => void>>();

// useState that mirrors a number to localStorage under `key`, kept in sync across
// every mounted instance of the same key (same document via `numberSubs`, other
// windows via the `storage` event). The value is always clamped into [min, max]
// on read (corrupted storage values fall back to `initial`). Otherwise follows
// the same read-once / write-on-change / silent-fallback contract as
// `useLocalStorageState`.
export function useLocalStorageNumber(
  key: string,
  initial: number,
  opts?: { min?: number; max?: number },
): [number, Dispatch<SetStateAction<number>>] {
  const min = opts?.min ?? -Infinity;
  const max = opts?.max ?? Infinity;

  const [value, setValue] = useState<number>(() => {
    try {
      return clampStoredNumber(window.localStorage.getItem(key), initial, min, max);
    } catch {
      return initial;
    }
  });

  // Clamp bounds via refs so the cross-window listener stays subscribed on [key]
  // alone without going stale when an inline `opts` object changes identity.
  const minRef = useRef(min);
  minRef.current = min;
  const maxRef = useRef(max);
  maxRef.current = max;

  // Register as a same-document subscriber + listen for cross-window writes.
  useEffect(() => {
    const sub = (v: number) => setValue(v);
    let subs = numberSubs.get(key);
    if (!subs) {
      subs = new Set();
      numberSubs.set(key, subs);
    }
    subs.add(sub);

    const onStorage = (e: StorageEvent) => {
      if (e.key === key && e.newValue !== null) {
        setValue(clampStoredNumber(e.newValue, initial, minRef.current, maxRef.current));
      }
    };
    window.addEventListener("storage", onStorage);

    return () => {
      subs?.delete(sub);
      if (subs && subs.size === 0) numberSubs.delete(key);
      window.removeEventListener("storage", onStorage);
    };
  }, [key, initial]);

  const firstRender = useRef(true);
  useEffect(() => {
    if (firstRender.current) {
      firstRender.current = false;
      return;
    }
    try {
      window.localStorage.setItem(key, String(value));
    } catch {
      // ignore — preference just won't persist this session
    }
    // Notify every live instance of this key (includes self — a no-op since the
    // value is unchanged, so React bails and the broadcast converges in one hop).
    const subs = numberSubs.get(key);
    if (subs) for (const sub of subs) sub(value);
  }, [key, value]);

  return [value, setValue];
}

// Imperative read of a stored number, clamped — for call sites that need the
// current value WITHOUT subscribing (and therefore without re-rendering on every
// change). Mirrors the hook's read-once contract.
export function readStoredNumber(
  key: string,
  fallback: number,
  min = -Infinity,
  max = Infinity,
): number {
  try {
    return clampStoredNumber(window.localStorage.getItem(key), fallback, min, max);
  } catch {
    return fallback;
  }
}

// Imperative write of a number: persist + broadcast to every mounted
// useLocalStorageNumber(key) instance so they re-render — WITHOUT the caller
// holding a subscription of its own. The value is written as-is, so clamp before
// calling. Lets e.g. a keyboard shortcut update the terminal font size (which every
// TerminalPane reflects) without re-rendering the unrelated app shell.
export function writeStoredNumber(key: string, value: number): void {
  try {
    window.localStorage.setItem(key, String(value));
  } catch {
    // ignore — preference just won't persist this session
  }
  const subs = numberSubs.get(key);
  if (subs) for (const sub of subs) sub(value);
}
