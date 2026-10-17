/**
 * IME composition handling for the terminal input path.
 * Pure module — no side effects, fully testable.
 */

/**
 * True when this keydown would make xterm's CompositionHelper prematurely finalize an
 * in-flight composition while a native `compositionend` will ALSO fire — the double commit
 * behind 你好 → 你好你好 (xterm 5.5.0 `CompositionHelper.keydown` → `_finalizeComposition(false)`,
 * then `compositionend` → `_finalizeComposition(true)`; `_dataAlreadySent` only guards the
 * keyCode-229 punctuation path, so nothing subtracts the first emission).
 *
 * Caps Lock is the macOS 中/英 IME toggle: it commits the composition and fires
 * `compositionend`, so xterm's keydown-path finalize is the redundant emission. Suppressing
 * this one keydown (the custom handler returns false, which runs *before*
 * `CompositionHelper.keydown`) leaves `compositionend` as the sole emitter.
 *
 * Gated on `isComposing` so a normal Caps Lock press is untouched.
 *
 * Scope is deliberately narrow — Caps Lock only. Do NOT widen this to Enter: Enter is
 * already safe by a different mechanism. `Terminal.ts:1066` clears `textarea.value` for
 * `C0.CR`/`C0.ETX` (nominally so screen readers announce deleted characters), which runs
 * *after* `CompositionHelper.keydown`. So Enter mid-composition emits once via the keydown
 * path, and by the time `compositionend`'s `setTimeout` runs the textarea is empty — the
 * `input.length > 0` guard (`CompositionHelper.ts:170`) suppresses the second emission.
 * Suppressing Enter here would drop that single legitimate emission entirely. Caps Lock gets
 * no such clearing, which is exactly why it duplicates and Enter does not.
 */
export function suppressesCompositionDoubleCommit(
  e: Pick<KeyboardEvent, "key">,
  isComposing: boolean,
): boolean {
  return isComposing && e.key === "CapsLock";
}
