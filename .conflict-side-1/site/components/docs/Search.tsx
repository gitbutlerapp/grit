"use client";

import { useEffect, useMemo, useRef, useState } from "react";

/** One searchable page: title, summary, path from the docs root. */
export type SearchEntry = [string, string, string];

/** Header search over page titles and summaries; ⌘K, Ctrl+K or / focuses it. */
export function Search({ entries, root }: { entries: SearchEntry[]; root: string }) {
  const [query, setQuery] = useState("");
  const [open, setOpen] = useState(false);
  const [selected, setSelected] = useState(0);
  const box = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLInputElement>(null);

  const hits = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return [];
    const byTitle = entries.filter((e) => e[0].toLowerCase().includes(q));
    const bySummary = entries.filter((e) => !byTitle.includes(e) && e[1].toLowerCase().includes(q));
    return [...byTitle, ...bySummary].slice(0, 8);
  }, [entries, query]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const typing = /input|textarea/i.test((document.activeElement as HTMLElement | null)?.tagName ?? "");
      if (((e.metaKey || e.ctrlKey) && e.key === "k") || (e.key === "/" && !typing)) {
        e.preventDefault();
        input.current?.focus();
        input.current?.select();
      }
    };
    const onClick = (e: MouseEvent) => {
      if (!box.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("click", onClick);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("click", onClick);
    };
  }, []);

  const onKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if ((e.key === "ArrowDown" || e.key === "ArrowUp") && hits.length) {
      e.preventDefault();
      setSelected((s) => (s + (e.key === "ArrowDown" ? 1 : hits.length - 1)) % hits.length);
    } else if (e.key === "Enter" && hits[selected]) {
      window.location.href = root + hits[selected][2];
    } else if (e.key === "Escape") {
      input.current?.blur();
      setOpen(false);
    }
  };

  return (
    <div className="search" ref={box}>
      <input
        ref={input}
        type="search"
        placeholder="Search"
        aria-label="Search the docs"
        autoComplete="off"
        value={query}
        onChange={(e) => {
          setQuery(e.target.value);
          setSelected(0);
          setOpen(true);
        }}
        onFocus={() => setOpen(true)}
        onKeyDown={onKeyDown}
      />
      <kbd>⌘K</kbd>
      {open && query.trim() ? (
        <ul>
          {hits.length ? (
            hits.map((e, i) => (
              <li key={e[2]}>
                <a href={root + e[2]} className={i === selected ? "on" : undefined}>
                  <b>{e[0]}</b>
                  <span>{e[1]}</span>
                </a>
              </li>
            ))
          ) : (
            <li className="none">No matches</li>
          )}
        </ul>
      ) : null}
    </div>
  );
}
