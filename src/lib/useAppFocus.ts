import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect, useState } from "react";

/**
 * Reactive app-window focus.
 *
 * `attention.ts` needs to know whether Keitora currently has OS focus so it can
 * (a) suppress/acknowledge the worktree the user is actively viewing and
 * (b) route alerts (in-app banner when focused, OS notification when not).
 *
 * We track Tauri's OS-window focus (`onFocusChanged`), not DOM `focus`/`blur`:
 * in a webview those are element-/document-level and don't reliably reflect
 * OS-window activation (e.g. alt-tab), so DOM events can leave focus stuck.
 * `document.hasFocus()` is used only as a synchronous initial seed; the real
 * state is read from `isFocused()` and kept current via `onFocusChanged`.
 */
export function useAppFocus(): boolean {
  const [focused, setFocused] = useState<boolean>(() =>
    typeof document !== "undefined" ? document.hasFocus() : true,
  );

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    const win = getCurrentWindow();

    // Correct the seed from the real OS-window state, then subscribe to changes.
    void win.isFocused().then((f) => {
      if (!cancelled) setFocused(f);
    });
    void win
      .onFocusChanged(({ payload }) => setFocused(payload))
      .then((un) => {
        if (cancelled) un();
        else unlisten = un;
      });

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  return focused;
}
