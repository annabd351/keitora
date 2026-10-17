import { useEffect, useLayoutEffect, useReducer, useRef } from "react";

// ── State machine ─────────────────────────────────────────────────────────────

export type HintState = "idle" | "pending" | "visible";
export type HintAction =
  | { type: "MODIFIER_DOWN" }
  | { type: "TIMER_FIRED" }
  | { type: "OTHER_KEY" }
  | { type: "RELEASE" };

export function hintReducer(state: HintState, action: HintAction): HintState {
  switch (action.type) {
    case "MODIFIER_DOWN":
      return state === "idle" ? "pending" : state;
    case "TIMER_FIRED":
      return state === "pending" ? "visible" : state;
    case "OTHER_KEY":
      // A chord key only cancels the *pending* reveal, so a quick tap (e.g. a fast
      // Cmd+1) never flashes the overlay. Once the hint is visible, pressing chord
      // keys while still holding the modifier keeps it up — the user is actively
      // navigating with it. Only RELEASE hides a visible hint.
      return state === "pending" ? "idle" : state;
    case "RELEASE":
      return "idle";
    default:
      return state;
  }
}

// ── Hook ──────────────────────────────────────────────────────────────────────

/**
 * Returns `true` while the user has held Cmd/Ctrl for ≥300ms without pressing
 * another key. Resets to `false` on release, window blur, or tab hide.
 *
 * Listeners are attached at capture phase so they fire even when an xterm
 * <textarea> has focus (same reason global chords work in TerminalPane).
 */
export function useModifierHint(): boolean {
  const [state, dispatch] = useReducer(hintReducer, "idle");

  // stateRef lets the once-attached listeners read the current state without
  // closing over the initial value.
  const stateRef = useRef<HintState>("idle");
  useLayoutEffect(() => {
    stateRef.current = state;
  }, [state]);

  // timerRef tracks the pending 300ms timeout.
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      // Ignore auto-repeat while modifier is held (prevents re-arming the timer).
      if (e.repeat) return;

      if (e.key === "Meta" || e.key === "Control") {
        // Only arm when idle AND no timer is already pending. The extra timer
        // guard covers a near-simultaneous left+right modifier press: both
        // keydowns can observe stateRef still "idle" (the dispatch hasn't
        // committed yet), and without it the second would overwrite — and leak —
        // the first timer.
        if (stateRef.current === "idle" && timerRef.current === null) {
          dispatch({ type: "MODIFIER_DOWN" });
          timerRef.current = setTimeout(() => {
            timerRef.current = null;
            dispatch({ type: "TIMER_FIRED" });
          }, 300);
        }
      } else {
        // A chord key: cancel a pending reveal (no flash on a quick tap). If the
        // hint is already visible, OTHER_KEY is a no-op in the reducer — it stays
        // up until the modifier is released.
        if (timerRef.current !== null) {
          clearTimeout(timerRef.current);
          timerRef.current = null;
        }
        dispatch({ type: "OTHER_KEY" });
      }
    };

    const onKeyUp = (e: KeyboardEvent) => {
      if (e.key === "Meta" || e.key === "Control") {
        if (timerRef.current !== null) {
          clearTimeout(timerRef.current);
          timerRef.current = null;
        }
        dispatch({ type: "RELEASE" });
      }
    };

    const onBlur = (e: FocusEvent) => {
      // Capture-phase listeners on window also receive `blur` from descendant
      // elements (blur doesn't bubble, but it has a capture phase). Pressing a
      // chord moves focus to the terminal's <textarea>, which blurs the previously
      // focused element — that must NOT hide the hint. Only react when the window
      // itself loses focus (e.target === window), e.g. alt-tabbing to another app.
      if (e.target !== window) return;
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
      dispatch({ type: "RELEASE" });
    };

    const onVisibilityChange = () => {
      if (document.visibilityState === "hidden") {
        if (timerRef.current !== null) {
          clearTimeout(timerRef.current);
          timerRef.current = null;
        }
        dispatch({ type: "RELEASE" });
      }
    };

    const opts = { capture: true } as const;
    window.addEventListener("keydown", onKeyDown, opts);
    window.addEventListener("keyup", onKeyUp, opts);
    window.addEventListener("blur", onBlur, opts);
    document.addEventListener("visibilitychange", onVisibilityChange);

    return () => {
      window.removeEventListener("keydown", onKeyDown, opts);
      window.removeEventListener("keyup", onKeyUp, opts);
      window.removeEventListener("blur", onBlur, opts);
      document.removeEventListener("visibilitychange", onVisibilityChange);
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
      }
    };
  }, []);

  return state === "visible";
}
