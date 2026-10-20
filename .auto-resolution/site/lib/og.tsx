/**
 * Social share cards (Open Graph / Twitter), 1200×630, rendered at build time
 * by the `opengraph-image.tsx` routes. Cards use the site fonts and the same
 * highlighter as the pages, so a command card shows that page's real example.
 */
import fs from "node:fs";
import path from "node:path";
import { ImageResponse } from "next/og";
import type { RootContent } from "mdast";
import { isLibrarySlug, LIBRARY_SLUG, type Page } from "./docs";
import { initHighlighter, PALETTE, tokenize, type Line } from "./highlight";
import { parseDoc } from "./markdown";

export const OG_SIZE = { width: 1200, height: 630 };
export const OG_CONTENT_TYPE = "image/png";
const CODE_LINES = 11;

/** File name of a docs page's card under /og/docs/ (`library/refs` → `library__refs.png`). */
export function docsCardFile(slug: string): string {
  return `${slug.replace(/\//g, "__")}.png`;
}

const C = {
  bg: "#f4f1ea",
  ink: "#1b1915",
  soft: "#3d3830",
  muted: "#7b7466",
  line: "#d9d3c5",
  accent: "#e2481f",
  chip: "#e9e4d8",
  inkSoft: "#c9c2b3",
  inkMuted: "#a39b8b",
};

function font(pkg: string, file: string): Buffer {
  return fs.readFileSync(path.join(process.cwd(), "node_modules", "@fontsource", pkg, "files", file));
}

function fonts() {
  return [
    { name: "Space Grotesk", data: font("space-grotesk", "space-grotesk-latin-500-normal.woff"), weight: 500 as const },
    { name: "Space Grotesk", data: font("space-grotesk", "space-grotesk-latin-700-normal.woff"), weight: 700 as const },
    { name: "JetBrains Mono", data: font("jetbrains-mono", "jetbrains-mono-latin-400-normal.woff"), weight: 400 as const },
    { name: "JetBrains Mono", data: font("jetbrains-mono", "jetbrains-mono-latin-700-normal.woff"), weight: 700 as const },
  ];
}

const MONO = "JetBrains Mono";
const SANS = "Space Grotesk";

function card(body: React.ReactElement): ImageResponse {
  return new ImageResponse(body, { ...OG_SIZE, fonts: fonts() });
}

function Brand({ refLabel }: { refLabel: string }) {
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 18 }}>
      <div style={{ display: "flex", alignItems: "center", gap: 10, fontFamily: SANS, fontWeight: 700, fontSize: 30 }}>
        <div style={{ width: 18, height: 18, borderRadius: 9, background: C.accent }} />
        grit
      </div>
      <div style={{ fontFamily: MONO, fontSize: 20, color: C.accent }}>{refLabel}</div>
    </div>
  );
}

function CodeBox({ lines, fontSize = 19 }: { lines: Line[]; fontSize?: number }) {
  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        border: `1px solid ${C.soft}`,
        borderRadius: 14,
        padding: "20px 22px",
        fontFamily: MONO,
        fontSize,
        lineHeight: 1.65,
        overflow: "hidden",
        width: "100%",
      }}
    >
      {lines.map((line, i) => (
        <div key={i} style={{ display: "flex", whiteSpace: "pre", overflow: "hidden", color: line.cls ? PALETTE[line.cls] : PALETTE[""], height: fontSize * 1.65 }}>
          {line.tokens.map((t, j) => (
            <span
              key={j}
              style={{
                // Wide lines are clipped; never squeeze tokens on top of each other.
                flexShrink: 0,
                whiteSpace: "pre",
                color: t.cls ? PALETTE[t.cls] : undefined,
                fontWeight: line.cls === "cmd" && t.cls === "f" ? 700 : 400,
                ...(line.cls === "cmd" && t.cls === "f" ? { color: C.bg } : {}),
              }}
            >
              {t.text || " "}
            </span>
          ))}
        </div>
      ))}
    </div>
  );
}

function titleSize(title: string, mono: boolean): number {
  const n = title.length * (mono ? 1.15 : 1);
  for (const [limit, size] of [
    [9, 120],
    [13, 104],
    [18, 88],
    [26, 72],
    [36, 60],
  ] as const) {
    if (n <= limit) return size;
  }
  return 52;
}

/** The page's first useful code example, trimmed to fit a card. */
function firstExample(page: Page): { caption: string; lines: Line[] } | null {
  const find = (nodes: RootContent[]): { lang: string; meta: string; value: string } | null => {
    for (const node of nodes) {
      if (node.type === "code" && ["console", "bash", "sh", "rust", "json", "toml"].includes(node.lang ?? "")) {
        return { lang: node.lang!, meta: node.meta ?? "", value: node.value };
      }
      if ("children" in node) {
        const found = find(node.children as RootContent[]);
        if (found) return found;
      }
    }
    return null;
  };
  const code = find(parseDoc(page.markdown).root.children);
  if (!code) return null;
  let lines = code.value.split("\n");
  if (code.lang === "rust") {
    // Skip the file's doc comments, attributes and imports; start at real code.
    const start = lines.findIndex((l) => l.trim() && !/^\s*(\/\/|#!?\[|use |$)/.test(l));
    lines = lines.slice(Math.max(start, 0));
  }
  while (lines.length && !lines[0].trim()) lines.shift();
  const text = lines.slice(0, CODE_LINES).join("\n").trimEnd();
  if (!text) return null;
  const include = /include=(\S+)/.exec(code.meta)?.[1];
  const caption = include ? `Example · ${include.split("/").pop()}` : page.isCommand ? "Examples" : "Example";
  return { caption, lines: tokenize(code.lang, text).slice(0, CODE_LINES) };
}

/** Card for a docs page: title and summary on paper, its first example on ink. */
export async function docsPageCard(page: Page): Promise<ImageResponse> {
  await initHighlighter();
  const library = isLibrarySlug(page.slug);
  const crumb = page.isCommand
    ? `Commands / ${page.group}`
    : library
      ? page.slug === LIBRARY_SLUG
        ? "Library guide"
        : `Library guide / ${page.title}`
      : page.sectionTitle;
  const example = firstExample(page);
  const headings = page.toc.filter((t) => t.level === 2).slice(0, 6);
  const mark = library ? "use grit_lib;" : "$ grit";
  return card(
    <div style={{ display: "flex", width: "100%", height: "100%", background: C.bg, color: C.ink, fontFamily: SANS }}>
      <div style={{ flex: 1, display: "flex", flexDirection: "column", padding: "56px 56px 48px 64px", minWidth: 0 }}>
        <Brand refLabel={`HEAD → docs/${page.slug}`} />
        <div style={{ marginTop: "auto", fontFamily: MONO, fontSize: 20, color: C.muted }}>{crumb}</div>
        <div
          style={{
            marginTop: 10,
            fontFamily: page.isCommand ? MONO : SANS,
            fontWeight: page.isCommand ? 400 : 700,
            fontSize: titleSize(page.title, page.isCommand),
            letterSpacing: page.isCommand ? "-0.045em" : "-0.05em",
            lineHeight: 0.95,
          }}
        >
          {page.title}
        </div>
        <div style={{ marginTop: 20, fontSize: 28, lineHeight: 1.3, color: C.soft, fontWeight: 500, display: "flex", maxHeight: 112, overflow: "hidden" }}>
          {page.summary}
        </div>
        <div style={{ marginTop: 28, fontFamily: MONO, fontSize: 19, color: C.muted }}>
          {`grit-scm.com/docs/${page.slug === "index" ? "" : `${page.slug}/`}`}
        </div>
      </div>
      <div style={{ width: 500, background: C.ink, display: "flex", flexDirection: "column", gap: 18, padding: "56px 44px 48px" }}>
        {example ? (
          <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
            <div style={{ fontFamily: MONO, fontSize: 19, color: C.accent }}>{example.caption}</div>
            <CodeBox lines={example.lines} />
          </div>
        ) : headings.length < 2 ? (
          <div style={{ marginTop: "auto", fontFamily: MONO, fontSize: 56, color: C.accent, letterSpacing: "-0.03em" }}>{mark}</div>
        ) : (
          <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
            <div style={{ fontFamily: MONO, fontSize: 19, color: C.accent }}>{library ? "use grit_lib;" : "On this page"}</div>
            <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
              {headings.map((h, i) => (
                <div key={h.anchor} style={{ display: "flex", gap: 14, fontSize: 22, color: C.bg, paddingBottom: 12, borderBottom: `1px solid ${C.soft}` }}>
                  <span style={{ fontFamily: MONO, color: C.accent }}>{String(i + 1).padStart(2, "0")}</span>
                  {h.text}
                </div>
              ))}
            </div>
          </div>
        )}
        <div style={{ marginTop: "auto", fontFamily: MONO, fontSize: 18, color: C.inkMuted }}>
          {library ? "grit-lib · Rust" : "grit · Git client"}
        </div>
      </div>
    </div>,
  );
}

/** Card for the docs overview: the two halves side by side. */
export function docsOverviewCard(): ImageResponse {
  const half = (dark: boolean, label: string, title: string, text: string, url: string) => (
    <div
      style={{
        flex: dark ? "0 0 520px" : 1,
        display: "flex",
        flexDirection: "column",
        padding: "56px 56px 48px 64px",
        background: dark ? C.ink : C.bg,
        color: dark ? C.bg : C.ink,
      }}
    >
      <div style={{ display: "flex", opacity: dark ? 0 : 1 }}>
        <Brand refLabel="HEAD → docs" />
      </div>
      <div style={{ marginTop: "auto", fontFamily: MONO, fontSize: 20, color: C.accent }}>{label}</div>
      <div style={{ marginTop: 10, fontWeight: 700, fontSize: 76, letterSpacing: "-0.05em", lineHeight: 0.95 }}>{title}</div>
      <div style={{ marginTop: 20, fontSize: 28, lineHeight: 1.3, fontWeight: 500, color: dark ? C.inkSoft : C.soft, height: 110 }}>{text}</div>
      <div style={{ marginTop: 28, fontFamily: MONO, fontSize: 19, color: dark ? C.inkMuted : C.muted }}>{url}</div>
    </div>
  );
  return card(
    <div style={{ display: "flex", width: "100%", height: "100%", fontFamily: SANS }}>
      {half(false, "$ grit", "The command line", "Works on any Git repository and talks to any remote, with --json on every command.", "grit-scm.com/docs")}
      {half(true, "use grit_lib;", "The library", "A fast, linkable Git library for Rust. Everything the CLI does goes through it.", "docs.rs/grit-lib")}
    </div>,
  );
}

const HOME_LOG: [string, string][] = [
  ["feedface", "Docs your agent can read."],
  ["deadbeef", "Simpler on purpose."],
  ["ca11ab1e", "Link it. Don't shell out."],
  ["c0deba5e", "Every repo you already have."],
];

/** Card for the homepage. */
export function homeCard(): ImageResponse {
  const log: Line[] = [
    { cls: "cmd", tokens: [{ text: "$", cls: "p" }, { text: " ", cls: "" }, { text: "grit", cls: "f" }, { text: " log", cls: "" }] },
    ...HOME_LOG.map(([sha, msg]): Line => ({ cls: "", tokens: [{ text: `  ${sha}`, cls: "n" }, { text: `  ${msg}`, cls: "" }] })),
  ];
  const install: Line[] = [
    { cls: "cmd", tokens: [{ text: "$", cls: "p" }, { text: " ", cls: "" }, { text: "curl", cls: "f" }, { text: " -fsSL grit-scm.com/install ", cls: "" }, { text: "|", cls: "k" }, { text: " ", cls: "" }, { text: "sh", cls: "f" }] },
    { cls: "cmd", tokens: [{ text: "$", cls: "p" }, { text: " ", cls: "" }, { text: "cargo", cls: "f" }, { text: " add grit-lib", cls: "" }] },
  ];
  return card(
    <div style={{ display: "flex", width: "100%", height: "100%", background: C.bg, color: C.ink, fontFamily: SANS }}>
      <div style={{ flex: 1, display: "flex", flexDirection: "column", padding: "56px 56px 48px 96px", position: "relative" }}>
        <div style={{ position: "absolute", left: 30, top: 0, bottom: 0, width: 4, background: C.ink }} />
        <div style={{ position: "absolute", left: 16, top: 150, width: 32, height: 32, borderRadius: 16, background: C.accent, border: `5px solid ${C.bg}`, boxShadow: `0 0 0 4px ${C.ink}` }} />
        <div style={{ display: "flex", alignItems: "center", gap: 18, fontFamily: MONO, fontSize: 22 }}>
          <span style={{ color: C.accent }}>HEAD → main</span>
          <span style={{ color: C.muted }}>c0ffee1</span>
        </div>
        <div style={{ marginTop: 24, fontWeight: 700, fontSize: 200, letterSpacing: "-0.06em", lineHeight: 0.85 }}>Grit</div>
        <div style={{ marginTop: 22, fontWeight: 500, fontSize: 40, letterSpacing: "-0.025em", lineHeight: 1.1 }}>
          A new Git client and library, in Rust.
        </div>
        <div style={{ marginTop: "auto", fontSize: 23, lineHeight: 1.45, color: C.soft, maxWidth: 560 }}>
          Compatible with every repository and every remote. Simpler to use, quick to run, and MIT-licensed throughout.
        </div>
      </div>
      <div style={{ width: 560, background: C.ink, display: "flex", flexDirection: "column", gap: 18, padding: "56px 36px 48px 40px" }}>
        <div style={{ fontFamily: MONO, fontSize: 19, color: C.accent }}>grit-scm.com</div>
        <CodeBox lines={log} fontSize={18} />
        <CodeBox lines={install} fontSize={18} />
      </div>
    </div>,
  );
}

/** Card for a blog post. */
export function blogPostCard(post: { title: string; summary: string; published: string; sha: string }): ImageResponse {
  return card(
    <div style={{ display: "flex", width: "100%", height: "100%", background: C.bg, color: C.ink, fontFamily: SANS, position: "relative" }}>
      <div style={{ position: "absolute", left: 30, top: 0, bottom: 0, width: 4, background: C.ink }} />
      <div style={{ position: "absolute", left: 16, top: 150, width: 32, height: 32, borderRadius: 16, background: C.accent, border: `5px solid ${C.bg}`, boxShadow: `0 0 0 4px ${C.ink}` }} />
      <div style={{ display: "flex", flexDirection: "column", padding: "56px 72px 48px 96px", width: "100%" }}>
        <Brand refLabel="HEAD → blog" />
        <div style={{ marginTop: "auto", fontFamily: MONO, fontSize: 20, color: C.muted }}>{`${post.sha} · ${post.published}`}</div>
        <div style={{ marginTop: 12, fontWeight: 700, fontSize: titleSize(post.title, false) * 0.85, letterSpacing: "-0.045em", lineHeight: 0.98 }}>
          {post.title}
        </div>
        <div style={{ marginTop: 22, fontSize: 28, lineHeight: 1.3, color: C.soft, fontWeight: 500, maxHeight: 112, overflow: "hidden" }}>{post.summary}</div>
        <div style={{ marginTop: 28, fontFamily: MONO, fontSize: 19, color: C.muted }}>grit-scm.com/blog</div>
      </div>
    </div>,
  );
}
