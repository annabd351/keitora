// Pure size/fit decision helpers for the embedded terminal panes.
// No xterm dependency — callers pass plain numbers so this stays unit-testable.

/** Minimum usable PTY geometry — claude's TUI corrupts below this. */
export const MIN_PTY = { rows: 4, cols: 10 } as const;

export interface Dims {
  rows: number;
  cols: number;
}

/** Clamp measured xterm dims up to the minimum the PTY/claude renders sanely. */
export function clampPtySize(d: Dims): Dims {
  return {
    rows: Math.max(MIN_PTY.rows, d.rows),
    cols: Math.max(MIN_PTY.cols, d.cols),
  };
}

/**
 * A fit proposal is usable only if both axes are positive, finite integers. Guards
 * against FitAddon.proposeDimensions() returning undefined (renderer cell metrics
 * still 0) or a degenerate 0/NaN measurement from a not-yet-laid-out host.
 */
export function dimsAreSane(d: Dims | undefined | null): d is Dims {
  return !!d && Number.isFinite(d.rows) && Number.isFinite(d.cols) && d.rows > 0 && d.cols > 0;
}

/** Two proposals describe the same geometry (stability check across frames). */
export function dimsEqual(a: Dims, b: Dims): boolean {
  return a.rows === b.rows && a.cols === b.cols;
}

/**
 * Sticky-bottom: the xterm viewport was pinned to the live tail before a refit.
 * viewportY >= baseY means the bottom page is showing (xterm buffer semantics); we
 * use >= rather than === so a transient over-scroll still re-pins to the tail.
 */
export function isAtBottom(viewportY: number, baseY: number): boolean {
  return viewportY >= baseY;
}

/** Defensive row height when the terminal has not produced a usable measurement. */
export const FALLBACK_ROW_PX = 17;

export interface WheelLineResult {
  lines: number;
  remainder: number;
}

/**
 * Convert pixel wheel movement to whole lines while carrying the sub-line remainder.
 * Invalid measurements and inputs are normalized so xterm never receives NaN or Infinity.
 */
export function accumulateWheelLines(
  deltaPixels: number,
  rowHeightPx: number,
  prevRemainder: number,
): WheelLineResult {
  const safeRow = Number.isFinite(rowHeightPx) && rowHeightPx > 0 ? rowHeightPx : FALLBACK_ROW_PX;
  const safeDelta = Number.isFinite(deltaPixels) ? deltaPixels : 0;
  const safePrevRaw = Number.isFinite(prevRemainder) ? prevRemainder : 0;
  // Do not let carry from the prior direction delay a reversal.
  const safePrev =
    safePrevRaw !== 0 && safeDelta !== 0 && Math.sign(safePrevRaw) !== Math.sign(safeDelta)
      ? 0
      : safePrevRaw;

  const acc = safePrev + safeDelta / safeRow;
  // Math.trunc keeps the sign symmetric; `|| 0` normalizes -0 → 0 for a canonical result.
  const lines = Math.trunc(acc) || 0;
  return { lines, remainder: acc - lines };
}
