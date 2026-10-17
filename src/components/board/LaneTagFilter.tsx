// LaneTagFilter — the Open column's multiselect tag filter. A Radix DropdownMenu
// of CheckboxItems (one per available tag) plus a Clear action, opened by a small
// Filter-icon button in the lane header. A search box at the top narrows the list
// and the list itself scrolls once it outgrows its cap. Purely presentational: it
// renders the options and reports toggles up; all filtering lives in tagFilter.ts /
// Lane.tsx. Styling mirrors DispatchMenu.tsx (Content/Item/Separator classes).

import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { Check, Filter, Search } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { filterTags } from "./tagFilter";

export function LaneTagFilter({
  tags,
  selected,
  onToggle,
  onClear,
}: {
  /** Available options (union of tags on the lane's tasks). */
  tags: string[];
  /** Currently selected tags. */
  selected: string[];
  onToggle: (tag: string) => void;
  onClear: () => void;
}) {
  // The search query and dropdown-open flag are transient view state owned here; the
  // component's public props are unchanged, so Lane.tsx stays untouched.
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  // Radix focuses the first menu item on open (its FocusScope mount-autofocus is a
  // private Content prop, so it can't be overridden declaratively). Grab focus for the
  // search box one frame later so typing filters immediately.
  useEffect(() => {
    if (!open) return;
    const id = requestAnimationFrame(() => inputRef.current?.focus());
    return () => cancelAnimationFrame(id);
  }, [open]);

  // If the lane loses all its tags while the menu is open, close it and reset the query.
  // Otherwise `open` stays true across the `return null` below and the menu would spring
  // back open on its own the moment a tagged task reappears in the lane.
  useEffect(() => {
    if (tags.length === 0) {
      setOpen(false);
      setQuery("");
    }
  }, [tags.length]);

  // Nothing to filter → no control at all.
  if (tags.length === 0) return null;

  const active = selected.length > 0;
  const matched = filterTags(tags, query);
  // Keep selected tags in view even when the query would hide them, so a filter can
  // always be un-toggled from the list — otherwise the only way out is Clear filter.
  // (Blank query → `matched` is all of `tags`, so this is a no-op passthrough.)
  const visible = tags.filter((t) => matched.includes(t) || selected.includes(t));
  const itemCls =
    "flex cursor-pointer items-center gap-2 rounded-chip px-2 py-1.5 text-[11.5px] outline-none data-[highlighted]:bg-bg-3";

  return (
    <DropdownMenu.Root
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        // Reset the search on close so the next open starts from the full list.
        if (!o) setQuery("");
      }}
    >
      <DropdownMenu.Trigger asChild>
        <button
          type="button"
          aria-label="Filter by tag"
          onClick={(e) => e.stopPropagation()}
          onPointerDown={(e) => e.stopPropagation()}
          className={`flex flex-none items-center gap-1 rounded outline-none transition-colors focus-visible:opacity-100 data-[state=open]:opacity-100 ${
            active ? "text-accent-text" : "text-tx-3 hover:text-tx-1"
          }`}
        >
          <Filter size={13} />
          {active && (
            <span className="rounded-full bg-accent/20 px-1.5 py-px text-[9px] font-semibold text-accent-text">
              {selected.length}
            </span>
          )}
        </button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="end"
          sideOffset={4}
          onClick={(e) => e.stopPropagation()}
          className="z-50 min-w-[180px] rounded-card border border-line bg-bg-2 p-1 shadow-[0_8px_24px_rgba(0,0,0,.45)]"
        >
          <DropdownMenu.Label className="px-2 py-1 text-[10px] uppercase tracking-wide text-tx-3">
            Filter by tag
          </DropdownMenu.Label>

          <div className="mx-1 mb-1 flex items-center gap-1.5 rounded-chip border border-line bg-bg-3 px-2 py-1">
            <Search size={12} className="flex-none text-tx-3" />
            <input
              ref={inputRef}
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="Search tags…"
              aria-label="Search tags"
              onKeyDown={(e) => {
                if (e.key === "ArrowDown") {
                  // The input isn't a roving-focus item, so Radix won't move focus into
                  // the list on its own. Hand keyboard control to the first tag.
                  e.preventDefault();
                  const menu = e.currentTarget.closest('[role="menu"]');
                  menu?.querySelector<HTMLElement>('[role="menuitemcheckbox"]')?.focus();
                  return;
                }
                // Suppress Radix's menu typeahead for printable keys so they type into
                // the input; let everything else (Escape to close, etc.) reach the menu.
                if (e.key.length === 1) e.stopPropagation();
              }}
              className="w-full bg-transparent text-[11.5px] text-tx-1 outline-none placeholder:text-tx-3"
            />
          </div>

          <div className="max-h-[240px] overflow-y-auto">
            {visible.length === 0 ? (
              <div className="px-2 py-1.5 text-[11.5px] text-tx-3">No tags match</div>
            ) : (
              visible.map((tag) => (
                <DropdownMenu.CheckboxItem
                  key={tag}
                  checked={selected.includes(tag)}
                  // Keep the menu open so multiple tags can be toggled in one pass.
                  onSelect={(e) => e.preventDefault()}
                  onCheckedChange={() => onToggle(tag)}
                  className={`${itemCls} text-tx-2 data-[state=checked]:text-tx-1`}
                >
                  <span className="flex h-3.5 w-3.5 flex-none items-center justify-center rounded border border-line">
                    <DropdownMenu.ItemIndicator>
                      <Check size={11} className="text-accent-text" />
                    </DropdownMenu.ItemIndicator>
                  </span>
                  <span className="truncate">{tag}</span>
                </DropdownMenu.CheckboxItem>
              ))
            )}
          </div>

          {active && (
            <>
              <DropdownMenu.Separator className="my-1 h-px bg-line" />
              <DropdownMenu.Item onSelect={() => onClear()} className={`${itemCls} text-tx-2`}>
                <span className="h-3.5 w-3.5 flex-none" />
                <span>Clear filter</span>
              </DropdownMenu.Item>
            </>
          )}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}
