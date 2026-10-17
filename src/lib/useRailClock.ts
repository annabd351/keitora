// Two slow-moving inputs for the rail's project headers: the user's home directory (to
// show project paths as `~/…`) and a clock that ticks on the minute (to age the "last
// activity" labels). Both are shared by every header rather than kept per group.

import { homeDir } from "@tauri-apps/api/path";
import { useEffect, useState } from "react";

let cachedHome: Promise<string | null> | null = null;

/** The home directory, or null until it resolves or when it cannot (outside Tauri). */
export function useHomeDir(): string | null {
  const [home, setHome] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    cachedHome ??= homeDir().catch(() => null);
    void cachedHome.then((h) => {
      if (live) setHome(h);
    });
    return () => {
      live = false;
    };
  }, []);
  return home;
}

/** `Date.now()`, refreshed at the start of every minute. */
export function useMinuteNow(): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout>;
    const schedule = () => {
      timer = setTimeout(
        () => {
          setNow(Date.now());
          schedule();
        },
        60_000 - (Date.now() % 60_000),
      );
    };
    schedule();
    return () => clearTimeout(timer);
  }, []);
  return now;
}
