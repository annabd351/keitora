// Which backend attaches a pane still holds. Every attach registers an output
// sink in the backend, under an attach id, until a scoped `ptyDetach` releases it
// or a newer attach of the session replaces it. A pane re-attaching keeps its
// previous attach registered until the new one succeeds (which replaces it in the
// backend); when the new one fails nothing replaces it, so the pane must release
// it itself. On unmount it releases whatever it still holds.

export interface PaneAttachments {
  /** The attach whose channel feeds this pane, if any. */
  readonly live: number | null;
  /** A new attach is starting: the live one stops feeding the pane but stays registered. */
  begin(): void;
  /** The new attach succeeded under `id`; it replaced any earlier one in the backend. */
  succeeded(id: number): void;
  /** The new attach failed: the earlier attach it would have replaced, to detach now. */
  failed(): number | null;
  /** The pane is going away: every attach it still holds, to detach. */
  release(): number[];
}

export function createPaneAttachments(): PaneAttachments {
  let live: number | null = null;
  let superseded: number | null = null;

  return {
    get live() {
      return live;
    },
    begin() {
      // An earlier superseded attach is still the registered one when no attach
      // succeeded since: keep it (the live one, if any, is newer only after a success).
      if (live != null) superseded = live;
      live = null;
    },
    succeeded(id) {
      live = id;
      superseded = null;
    },
    failed() {
      const id = superseded;
      superseded = null;
      return id;
    },
    release() {
      const ids = [live, superseded].filter((id): id is number => id != null);
      live = null;
      superseded = null;
      return ids;
    },
  };
}
