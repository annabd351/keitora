import { describe, expect, it } from "vitest";
import { suppressesCompositionDoubleCommit } from "./terminalComposition";

// KeyboardEvent-shaped object. suppressesCompositionDoubleCommit only reads `key`, so a
// plain object suffices (no DOM) — same approach as terminalClipboard.test.ts.
function ev(key: string): KeyboardEvent {
  return { key } as KeyboardEvent;
}

const suppress = (key: string, isComposing: boolean): boolean =>
  suppressesCompositionDoubleCommit(ev(key), isComposing);

describe("suppressesCompositionDoubleCommit — the 中/英 double commit", () => {
  it("Caps Lock while composing → suppressed (this is the bug: 你好 → 你好你好)", () => {
    expect(suppress("CapsLock", true)).toBe(true);
  });

  it("Caps Lock while NOT composing → not suppressed (normal Caps Lock press)", () => {
    expect(suppress("CapsLock", false)).toBe(false);
  });
});

describe("suppressesCompositionDoubleCommit — other keys while composing", () => {
  // Only Caps Lock is suppressed. Enter in particular must NOT be — xterm clears the
  // textarea for C0.CR/C0.ETX after the keydown finalize, so compositionend's re-read is
  // already empty and Enter emits exactly once. Suppressing it would drop that emission.
  // See the note on suppressesCompositionDoubleCommit.
  it.each([
    "Enter",
    "a",
    "Process",
    "Shift",
    "Escape",
    "Tab",
    "2",
    "Backspace",
  ])("%s while composing → not suppressed", (key) => {
    expect(suppress(key, true)).toBe(false);
  });
});

describe("suppressesCompositionDoubleCommit — exact key match", () => {
  // e.key is a fixed spec value; only the exact spelling is the Caps Lock key.
  it.each(["capslock", "CAPSLOCK", "Caps", "CapsLock "])("%o → not suppressed", (key) => {
    expect(suppress(key, true)).toBe(false);
  });
});
