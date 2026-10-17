// Single source of truth for the terminal font-size preference: the localStorage
// key, the default/min/max, a clamp helper, and a hook wrapping
// `useLocalStorageNumber`. Both the terminal pane (which renders at this size) and
// the App-level keyboard shortcuts read/write through
// this, so they can never drift on the key or the bounds. Writes broadcast to every
// mounted instance of the key via `useLocalStorageNumber`'s same-document subscriber
// registry, so an App-level shortcut update reaches every live TerminalPane.

import { readStoredNumber, useLocalStorageNumber, writeStoredNumber } from "./useLocalStorageState";

export const TERM_FONT_SIZE_KEY = "keitora:termFontSize";
export const TERM_FONT_DEFAULT = 13;
export const TERM_FONT_MIN = 8;
export const TERM_FONT_MAX = 32;

/** Clamp a font size into the supported [MIN, MAX] range. */
export const clampFontSize = (n: number): number =>
  Math.min(TERM_FONT_MAX, Math.max(TERM_FONT_MIN, n));

/** Terminal font size (px), persisted + synced across every mounted instance. */
export function useTerminalFontSize() {
  return useLocalStorageNumber(TERM_FONT_SIZE_KEY, TERM_FONT_DEFAULT, {
    min: TERM_FONT_MIN,
    max: TERM_FONT_MAX,
  });
}

/** Imperative read of the current terminal font size (no React subscription). */
export function getTerminalFontSize(): number {
  return readStoredNumber(TERM_FONT_SIZE_KEY, TERM_FONT_DEFAULT, TERM_FONT_MIN, TERM_FONT_MAX);
}

/**
 * Imperative set of the terminal font size: clamps, persists, and broadcasts to
 * every mounted `useTerminalFontSize` instance (so all panes resize) — without the
 * caller subscribing. Used by the keyboard shortcuts so adjusting the size doesn't
 * re-render the whole app shell.
 */
export function setTerminalFontSize(px: number): void {
  writeStoredNumber(TERM_FONT_SIZE_KEY, clampFontSize(px));
}
