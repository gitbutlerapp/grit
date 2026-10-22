"use client";

import { useEffect, useRef, useState } from "react";

/**
 * A `grit clone` that progresses as you read, drawn like grit's own clone output: four phases,
 * each with a throughput bar, finishing in order as the article scrolls by. Rates follow how
 * fast you scroll. Decorative only (aria-hidden); it sits under a blog post's table of contents.
 */

type Phase = { name: string; color: string; share: number; total: number; unit: string; rateUnit: string; took: string; amount: (n: number) => string };

const n0 = (n: number) => Math.round(n).toLocaleString("en-US");
const kilo = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(1)}k` : n.toFixed(0));

const PHASES: Phase[] = [
  { name: "Receiving", color: "#2a9fd6", share: 0.6, total: 110, unit: "MiB", rateUnit: "MiB/s", took: "7.7s", amount: (n) => `${n.toFixed(n < 10 ? 1 : 0)} MiB` },
  { name: "Indexing", color: "#b04cc8", share: 0.14, total: 0, unit: "objects", rateUnit: "obj/s", took: "557ms", amount: (n) => `${n0(n)} objects` },
  { name: "Resolving", color: "#c99a06", share: 0.12, total: 56217, unit: "deltas", rateUnit: "deltas/s", took: "247ms", amount: (n) => `${n0(n)} deltas` },
  { name: "Checkout", color: "#3a9a2e", share: 0.14, total: 9858, unit: "files", rateUnit: "files/s", took: "1.5s", amount: (n) => `${n0(n)} files` },
];
const CELLS = 18;
const SPINNER = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

/** Stable bar heights per phase, so the "throughput" looks the same on every visit. */
const HEIGHTS = PHASES.map((_, p) =>
  Array.from({ length: CELLS }, (_, i) => {
    let x = ((p + 1) * 7919 + i * 104729) >>> 0;
    x ^= x << 13;
    x ^= x >>> 17;
    x ^= x << 5;
    return 0.45 + ((x >>> 0) % 1000) / 1000 * 0.55;
  }),
);

function readProgress(article: Element) {
  const rect = article.getBoundingClientRect();
  // 100% once the article's last line is on screen.
  const span = rect.height - window.innerHeight;
  if (span <= 0) return 1;
  return Math.min(1, Math.max(0, -rect.top / span));
}

const FILLER = new Set(["a", "an", "the", "of", "and", "is", "to", "in", "on", "for", "with", "now"]);

/** A repo-style short name for a post: its slug without filler words, whole words up to ~18 chars. */
export function repoName(slug: string) {
  const words = slug.split("-").filter(Boolean);
  const kept = words.filter((w) => !FILLER.has(w));
  let name = "";
  for (const w of kept.length ? kept : words) {
    const next = name ? `${name}-${w}` : w;
    if (name && next.length > 18) break;
    name = next;
  }
  return name || "grit";
}

export function CloneProgress({ slug, words, target = "article.content" }: { slug: string; words: number; target?: string }) {
  const repo = repoName(slug);
  // The post's words are the objects being cloned.
  const phases = PHASES.map((p) => (p.name === "Indexing" ? { ...p, total: words } : p));
  const [progress, setProgress] = useState(0);
  // Article fraction per second, smoothed; drives every phase's rate.
  const [speed, setSpeed] = useState(0);
  const last = useRef<{ p: number; t: number } | null>(null);

  useEffect(() => {
    const article = document.querySelector(target);
    if (!article) return;
    let frame = 0;
    const update = () => {
      frame = 0;
      const p = readProgress(article);
      const t = performance.now();
      const prev = last.current;
      if (prev && t > prev.t && p > prev.p) {
        const instant = ((p - prev.p) / (t - prev.t)) * 1000;
        setSpeed((s) => (s ? s * 0.8 + instant * 0.2 : instant));
      }
      last.current = { p, t };
      setProgress(p);
    };
    const onScroll = () => {
      if (!frame) frame = requestAnimationFrame(update);
    };
    update();
    window.addEventListener("scroll", onScroll, { passive: true });
    window.addEventListener("resize", onScroll);
    return () => {
      window.removeEventListener("scroll", onScroll);
      window.removeEventListener("resize", onScroll);
      if (frame) cancelAnimationFrame(frame);
    };
  }, [target]);

  let start = 0;
  const rows = phases.map((phase, i) => {
    const f = Math.min(1, Math.max(0, (progress - start) / phase.share));
    start += phase.share;
    return { phase, f, i };
  });
  const spin = SPINNER[Math.floor(progress * 400) % SPINNER.length];

  return (
    <div className="clone" aria-hidden="true">
      <div className="clone-cmd">
        <span className="clone-prompt">❯</span> grit clone {repo}
      </div>
      <div className="clone-head">
        <b>Cloning</b> {repo.length <= 8 ? <><span className="clone-dim">{repo}.git →</span> <b>{repo}</b></> : <b>{repo}</b>}
      </div>
      <div className="clone-dim clone-remote">
        {/* No thousands separators, like git's own counts: it has to fit the sidebar. */}
        remote: {progress >= 1 ? `${words} objects, done.` : `${Math.round(progress * words)}/${words} objects, ${Math.floor(progress * 100)}%`}
      </div>
      {rows.map(({ phase, f, i }) => {
        const done = f >= 1;
        const active = f > 0 && !done;
        const rate = speed * (phase.total / phase.share);
        const filled = Math.round(f * CELLS);
        return (
          <div key={phase.name} className={`clone-phase${f > 0 ? "" : " clone-pending"}`}>
            <div className="clone-line">
              <span className="clone-mark" style={done ? undefined : { color: phase.color }}>
                {done ? "✓" : active ? spin : "·"}
              </span>
              <b>{phase.name}</b>
              <span className="clone-amount">{f > 0 ? phase.amount(f * phase.total) : ""}</span>
            </div>
            <div className="clone-line">
              <span className="clone-bars">
                {HEIGHTS[i].map((h, c) => (
                  <span
                    key={c}
                    style={
                      c < filled
                        ? { height: `${h * 100}%`, background: phase.color, opacity: h > 0.85 ? 0.6 : 1 }
                        : { height: "2px", background: phase.color, opacity: 0.25 }
                    }
                  />
                ))}
              </span>
              <span className="clone-rate">
                {done ? phase.took : active ? `${phase.unit === "MiB" ? Math.min(rate, 99.9).toFixed(1) : kilo(rate)} ${phase.rateUnit}` : ""}
              </span>
            </div>
          </div>
        );
      })}
    </div>
  );
}
