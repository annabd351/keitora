import type { SearchAddon } from "@xterm/addon-search";
import { useEffect, useRef, useState } from "react";

interface TerminalSearchProps {
  addon: SearchAddon;
  onClose: () => void;
}

export function TerminalSearch({ addon, onClose }: TerminalSearchProps) {
  const [query, setQuery] = useState("");
  const [caseSensitive, setCaseSensitive] = useState(false);
  const [regex, setRegex] = useState(false);
  const [matchIndex, setMatchIndex] = useState<number | undefined>(undefined);
  const [matchCount, setMatchCount] = useState<number>(0);
  const inputRef = useRef<HTMLInputElement | null>(null);

  // Focus input on mount
  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  // Subscribe to result changes
  useEffect(() => {
    const disp = addon.onDidChangeResults((e: { resultIndex: number; resultCount: number }) => {
      setMatchIndex(e.resultIndex);
      setMatchCount(e.resultCount);
    });
    return () => disp.dispose();
  }, [addon]);

  const opts = { caseSensitive, regex, incremental: false };

  const findNext = () => {
    if (query) addon.findNext(query, opts);
  };

  const findPrev = () => {
    if (query) addon.findPrevious(query, opts);
  };

  const handleQueryChange = (val: string) => {
    setQuery(val);
    if (val) {
      addon.findNext(val, { caseSensitive, regex, incremental: false });
    } else {
      // Empty query: clear highlights and blank the counter (don't run an empty
      // search, which would report "No results" over an empty field).
      addon.clearDecorations();
      setMatchIndex(undefined);
      setMatchCount(0);
    }
  };

  const handleToggleCaseSensitive = () => {
    const next = !caseSensitive;
    setCaseSensitive(next);
    if (query) addon.findNext(query, { caseSensitive: next, regex, incremental: false });
  };

  const handleToggleRegex = () => {
    const next = !regex;
    setRegex(next);
    if (query) addon.findNext(query, { caseSensitive, regex: next, incremental: false });
  };

  const countDisplay =
    matchIndex === undefined
      ? ""
      : matchCount === 0
        ? "No results"
        : matchIndex < 0
          ? // xterm reports -1 when there are matches but none is "current" yet.
            `${matchCount} match${matchCount === 1 ? "" : "es"}`
          : `${matchIndex + 1} / ${matchCount}`;

  return (
    <div className="search-bar">
      <input
        ref={inputRef}
        type="text"
        value={query}
        placeholder="Find..."
        onChange={(e) => handleQueryChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            findNext();
          }
          if (e.key === "Enter" && e.shiftKey) {
            e.preventDefault();
            findPrev();
          }
          if (e.key === "Escape") {
            e.preventDefault();
            onClose();
          }
        }}
      />
      <span className="search-count">{countDisplay}</span>
      <button onClick={findPrev} title="Previous match (Shift+Enter)">
        ↑
      </button>
      <button onClick={findNext} title="Next match (Enter)">
        ↓
      </button>
      <button
        className={caseSensitive ? "active" : undefined}
        onClick={handleToggleCaseSensitive}
        title="Case sensitive"
      >
        Aa
      </button>
      <button
        className={regex ? "active" : undefined}
        onClick={handleToggleRegex}
        title="Regular expression"
      >
        .*
      </button>
      <button onClick={onClose} title="Close (Esc)">
        ✕
      </button>
    </div>
  );
}
