// A project's identity hue in the rail: a stable hash of its registry key onto a fixed
// set of OKLCH hue angles. The set steers clear of the status colours (the gold accent and
// busy ~80°, idle green and the teal accent ~150–180°, waiting blue ~260°) so a project's
// colour never reads as a status.

/** OKLCH hue angles (degrees) a project can get, in the gaps between the status hues. */
export const PROJECT_HUES: readonly number[] = [10, 30, 50, 120, 210, 290, 310, 330, 350];

/** 32-bit FNV-1a over the UTF-16 code units of `s`. */
function fnv1a(s: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

/** The identity hue (degrees) for the project with registry key `key`. */
export function projectHue(key: string): number {
  return PROJECT_HUES[fnv1a(key) % PROJECT_HUES.length];
}

/** The monogram letter for `key`: its first letter or digit, uppercased; "?" if none. */
export function projectMonogram(key: string): string {
  const m = key.match(/[\p{L}\p{N}]/u);
  return m ? m[0].toUpperCase() : "?";
}
