// First-run nudge: when the `keitora` CLI isn't on PATH but a binary is available to
// link, offer (once per launch, via a native dialog) to install it into ~/.local/bin
// so session hooks resolve `keitora` in every project. Fully declinable and non-fatal —
// it never blocks app use.

import { ask, message } from "@tauri-apps/plugin-dialog";
import { useEffect, useRef } from "react";

import { installKeitoraCli, keitoraCliStatus } from "./ipc";

export function useKeitoraCliInstall() {
  const asked = useRef(false);
  useEffect(() => {
    if (asked.current) return; // once per app launch (also guards StrictMode double-mount)
    asked.current = true;

    void (async () => {
      try {
        const status = await keitoraCliStatus();
        if (status.state !== "installable") return; // on_path / missing → no prompt

        const accept = await ask(
          "The `keitora` CLI isn't on your PATH. Install it to ~/.local/bin so worktree " +
            "session hooks work in every project?",
          {
            title: "Install keitora CLI",
            kind: "info",
            okLabel: "Install",
            cancelLabel: "Not now",
          },
        );
        if (!accept) return;

        const result = await installKeitoraCli();
        if (result.state === "installed") {
          const where = result.path ? ` at ${result.path}` : "";
          await message(result.note ?? `Installed keitora${where}.`, {
            title: "keitora CLI installed",
            kind: "info",
          });
        } else if (result.state !== "on_path") {
          await message("Couldn't install the keitora CLI automatically.", {
            title: "keitora CLI",
            kind: "warning",
          });
        }
      } catch (e) {
        // Non-fatal — the installer is a convenience, not a requirement.
        console.warn("[keitora] CLI install check failed:", e);
      }
    })();
  }, []);
}
