// One strip under the title chrome that says what happened to herdr and what
// Keitora is doing about it, in place of every pane reporting its own failure.
// Renders nothing while recovery is idle and no session waits suspended.

import { Loader2 } from "lucide-react";
import { type RecoveryInfo, recoveryResumeNow } from "../lib/ipc";
import { bannerCount, bannerResumeLabel, bannerVisible } from "../lib/recovery";
import { useToast } from "./Toast";

const BUSY: readonly RecoveryInfo["phase"][] = ["Restarting", "Reconciling", "Resuming"];

export function RecoveryBanner({ info }: { info: RecoveryInfo }) {
  const { showToast } = useToast();
  if (!bannerVisible(info)) return null;

  const resume = () => {
    void recoveryResumeNow().catch((err: unknown) => {
      showToast(`Resume failed: ${err instanceof Error ? err.message : String(err)}`);
    });
  };
  const count = bannerCount(info);

  return (
    <div
      className="recovery-banner"
      data-phase={info.phase}
      role={info.phase === "Paused" ? "alert" : "status"}
      aria-live="polite"
    >
      {BUSY.includes(info.phase) && <Loader2 size={12} className="animate-spin flex-none" />}
      <span className="recovery-banner-text">{info.reason}</span>
      {count && <span className="recovery-banner-count">{count}</span>}
      {info.can_resume_now && (
        <button type="button" className="recovery-banner-btn" onClick={resume}>
          {bannerResumeLabel(info)}
        </button>
      )}
    </div>
  );
}
