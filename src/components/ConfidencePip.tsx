// 5-segment confidence pip — filled to round(confidence * 5) — used in both
// ProposalCard (tooltip-only, no percent) and the Inspector proposal panel
// (showPercent=true for the detail surface).

import * as Tooltip from "@radix-ui/react-tooltip";

interface ConfidencePipProps {
  /** 0..1 float from the backend Proposal. */
  confidence: number;
  /** If true, show the exact percent next to the pips (Inspector use). */
  showPercent?: boolean;
}

/** The five pip segments. */
function Pips({ filled }: { filled: number }) {
  return (
    <span className="inline-flex items-center gap-[3px]" aria-hidden>
      {Array.from({ length: 5 }, (_, i) => (
        <span
          // biome-ignore lint/suspicious/noArrayIndexKey: static fixed-length array (length=5); never reordered
          key={i}
          className={`inline-block h-1.5 w-2.5 rounded-full ${i < filled ? "bg-accent" : "bg-line"}`}
        />
      ))}
    </span>
  );
}

export function ConfidencePip({ confidence, showPercent = false }: ConfidencePipProps) {
  const filled = Math.round(confidence * 5);
  const pct = Math.round(confidence * 100);
  const label = `${pct}% confidence`;

  if (showPercent) {
    // Inspector surface: pips + literal percent, no tooltip needed.
    return (
      <span className="inline-flex items-center gap-2" aria-label={label}>
        <Pips filled={filled} />
        <span className="text-[11px] text-tx-2">{pct}%</span>
      </span>
    );
  }

  // Board surface: tooltip reveals the exact percent; no raw float visible.
  return (
    <Tooltip.Root delayDuration={300}>
      <Tooltip.Trigger asChild>
        <span className="inline-flex cursor-default items-center" aria-label={label}>
          <Pips filled={filled} />
        </span>
      </Tooltip.Trigger>
      <Tooltip.Portal>
        <Tooltip.Content
          className="rounded-chip border border-line bg-bg-2 px-2 py-1 text-[11px] text-tx-2 shadow-md"
          sideOffset={4}
        >
          {label}
          <Tooltip.Arrow className="fill-bg-2" />
        </Tooltip.Content>
      </Tooltip.Portal>
    </Tooltip.Root>
  );
}
