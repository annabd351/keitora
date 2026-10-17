// Transient, centered "zoom indicator" for the terminal font size. Shown briefly
// when the user steps the size with the keyboard shortcuts (Cmd +/-/0), so it's
// obvious what size you're at — and, via the "default" marker, when you're back at
// the baseline.
//
// Self-contained: it owns its own visibility + auto-hide timer and exposes an
// imperative `show(px)` via ref, so the parent (App) can flash it without holding
// font-size state itself (which would re-render the whole app shell on every step).

import { forwardRef, useCallback, useEffect, useImperativeHandle, useRef, useState } from "react";
import { TERM_FONT_DEFAULT } from "../lib/fontSize";

export interface FontSizeHudHandle {
  /** Flash the HUD showing `px`; resets the auto-hide timer on each call. */
  show: (px: number) => void;
}

// `seq` increments on every show() so the card gets a fresh React key and the
// scale-in animation replays on each press — even when the px value is unchanged
// (e.g. repeated presses at the min/max clamp boundary).
interface HudState {
  px: number;
  seq: number;
}

export const FontSizeHud = forwardRef<FontSizeHudHandle>(function FontSizeHud(_props, ref) {
  const [state, setState] = useState<HudState | null>(null);
  const timer = useRef<number | null>(null);
  const seq = useRef(0);

  const show = useCallback((px: number) => {
    seq.current += 1;
    setState({ px, seq: seq.current });
    if (timer.current !== null) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setState(null), 1000);
  }, []);

  useImperativeHandle(ref, () => ({ show }), [show]);

  useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    },
    [],
  );

  // The live region stays mounted and announces the latest value on change, so
  // screen readers coalesce to the current size instead of replaying a fresh
  // announcement on every mount during a rapid press sequence.
  return (
    <div className="fontsize-hud-root" aria-live="polite" aria-atomic="true">
      {state !== null && (
        <div className="fontsize-hud">
          <div className="fontsize-hud-card" key={state.seq}>
            <span className="fontsize-hud-value">{state.px}px</span>
            {state.px === TERM_FONT_DEFAULT && (
              <span className="fontsize-hud-default">default</span>
            )}
          </div>
        </div>
      )}
    </div>
  );
});
