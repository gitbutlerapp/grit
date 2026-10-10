/**
 * Syntax highlighting for code blocks on ink.
 *
 * Real languages (Rust, JSON, TOML, PowerShell, …) go through Shiki with a
 * theme in the site palette. Console transcripts and shell scripts use a small
 * tokenizer that knows `$` prompts, multi-line quoted arguments and output
 * lines. Both produce the same token stream, so HTML pages and the share-card
 * images color code identically.
 *
 * Token classes: c comment, s string, n number/literal, k keyword, t type,
 * f function/macro/command, a attribute/lifetime/flag, p prompt.
 * Line classes: cmd (a command line), out (program output).
 */
import { createHighlighter, type Highlighter, type ThemeRegistration } from "shiki";

export type TokenClass = "c" | "s" | "n" | "k" | "t" | "f" | "a" | "p" | "";
export interface Token {
  text: string;
  cls: TokenClass;
}
export interface Line {
  cls: "cmd" | "out" | "";
  tokens: Token[];
}

/** Colors for each token class on the ink background (also used by OG images). */
export const PALETTE: Record<TokenClass | "cmd" | "out", string> = {
  "": "#e9e4d8",
  c: "#a39b8b",
  s: "#b7c98f",
  n: "#e8b96d",
  k: "#e2481f",
  t: "#86c3b8",
  f: "#f3d8a2",
  a: "#c8a6d8",
  p: "#e2481f",
  cmd: "#f4f1ea",
  out: "#a39b8b",
};

const THEME: ThemeRegistration = {
  name: "grit-ink",
  type: "dark",
  colors: { "editor.background": "#1b1915", "editor.foreground": PALETTE[""] },
  tokenColors: [
    { scope: ["comment", "punctuation.definition.comment"], settings: { foreground: "#a39b8a", fontStyle: "italic" } },
    { scope: ["string", "constant.character", "string.quoted"], settings: { foreground: PALETTE.s } },
    { scope: ["constant.numeric", "constant.language"], settings: { foreground: PALETTE.n } },
    {
      scope: ["keyword", "storage", "storage.type", "storage.modifier", "support.type.property-name.json", "entity.name.tag.toml", "support.type.property-name.table.toml", "entity.other.attribute-name.table.toml"],
      settings: { foreground: PALETTE.k },
    },
    { scope: ["keyword.operator", "punctuation"], settings: { foreground: PALETTE[""] } },
    { scope: ["entity.name.type", "support.type", "entity.name.class", "entity.name.struct", "entity.name.enum", "entity.name.trait", "support.type.property-name.toml", "variable.other.key.toml"], settings: { foreground: PALETTE.t } },
    { scope: ["entity.name.function", "support.function", "meta.function-call entity.name.function", "entity.name.function.macro"], settings: { foreground: PALETTE.f } },
    { scope: ["meta.attribute", "storage.modifier.lifetime", "entity.name.lifetime", "punctuation.definition.attribute"], settings: { foreground: PALETTE.a } },
  ],
};

const COLOR_CLASS = new Map<string, TokenClass>([
  ["#a39b8a", "c"],
  [PALETTE.s, "s"],
  [PALETTE.n, "n"],
  [PALETTE.k, "k"],
  [PALETTE.t, "t"],
  [PALETTE.f, "f"],
  [PALETTE.a, "a"],
]);

const SHIKI_LANGS = ["rust", "json", "toml", "powershell", "yaml", "diff", "markdown", "javascript", "python", "c"] as const;
const ALIASES: Record<string, string> = { rs: "rust", ps1: "powershell", pwsh: "powershell", md: "markdown", yml: "yaml", js: "javascript", py: "python" };

let highlighter: Highlighter | undefined;
let loading: Promise<Highlighter> | undefined;

/** Load Shiki once; call before rendering code. */
export async function initHighlighter(): Promise<void> {
  if (highlighter) return;
  loading ??= createHighlighter({ themes: [THEME], langs: [...SHIKI_LANGS] });
  highlighter = await loading;
}

/** Lines of `code` with every token classified, by fence language. */
export function tokenize(lang: string, code: string): Line[] {
  const base = (lang.split(/[\s:]/)[0] || "text").toLowerCase();
  if (base === "console" || base === "shell-session") return shellLines(code, true);
  if (base === "bash" || base === "sh" || base === "shell" || base === "zsh") return shellLines(code, false);
  if (base === "rust" || base === "rs") code = hideDoctestLines(code);
  if (base === "text" || base === "txt" || base === "plain") {
    return code.split("\n").map((text) => ({ cls: "out", tokens: [{ text, cls: "" }] }));
  }
  const shikiLang = ALIASES[base] ?? base;
  if (!highlighter) throw new Error("initHighlighter() must run before highlighting");
  if (!(SHIKI_LANGS as readonly string[]).includes(shikiLang)) {
    return code.split("\n").map((text) => ({ cls: "", tokens: [{ text, cls: "" }] }));
  }
  return highlighter.codeToTokensBase(code, { lang: shikiLang as any, theme: "grit-ink" as any }).map((tokens) => ({
    cls: "",
    tokens: tokens.map((t) => ({ text: t.content, cls: COLOR_CLASS.get((t.color ?? "").toLowerCase()) ?? "" })),
  }));
}

/** Lines starting with `# ` are doctest scaffolding that rustdoc hides; hide them too. */
function hideDoctestLines(code: string): string {
  return code
    .split("\n")
    .filter((line) => !/^#(?: |$)/.test(line.trimStart()))
    .join("\n");
}

function escapeHtml(text: string): string {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

/** Highlighted HTML for a code block: `<pre class="code"><code>…</code></pre>`. */
export function highlightHtml(lang: string, code: string): string {
  const lines = tokenize(lang, code).map((line) => {
    const inner = line.tokens
      .map((t) => (t.cls ? `<span class="${t.cls}">${escapeHtml(t.text)}</span>` : escapeHtml(t.text)))
      .join("");
    return line.cls ? `<span class="${line.cls}">${inner}</span>` : inner;
  });
  return `<pre class="code"><code>${lines.join("\n")}</code></pre>`;
}

// Shell: one command line. Command words get `f`, flags `a`, quoted text `s`,
// variables `t`, operators `k`, trailing comments `c`.
const SHELL_TOKEN =
  /(?<c>(?<!\S)#.*$)|(?<s>"(?:\\.|[^"\\])*"?|'[^']*'?)|(?<t>\$\{[^}]*\}|\$\(|\$\w+)|(?<k>&&|\|\||>>|[|;<>]|\))|(?<a>(?<!\S)--?[A-Za-z0-9][\w-]*(?:=\S*)?)|(?<word>[^\s|;&<>"'$()]+)/g;

export function commandTokens(line: string): Token[] {
  const out: Token[] = [];
  let pos = 0;
  let expectCommand = true;
  for (const m of line.matchAll(SHELL_TOKEN)) {
    if (m.index! > pos) out.push({ text: line.slice(pos, m.index), cls: "" });
    const groups = m.groups!;
    const kind = (Object.keys(groups).find((k) => groups[k] !== undefined) ?? "word") as string;
    let cls: TokenClass = "";
    if (kind === "word") {
      cls = expectCommand ? "f" : "";
      // `VAR=value cmd` keeps waiting for the command word.
      expectCommand = expectCommand && m[0].includes("=");
    } else {
      cls = kind as TokenClass;
      if (kind === "k") expectCommand = ["&&", "||", "|", ";"].includes(m[0]);
      else if (kind !== "t") expectCommand = false;
    }
    out.push({ text: m[0], cls });
    pos = m.index! + m[0].length;
  }
  if (pos < line.length) out.push({ text: line.slice(pos), cls: "" });
  return out;
}

/** Track an open shell quote across lines so multi-line arguments stay part of the command. */
function quoteState(line: string, quote: string | null): string | null {
  let escaped = false;
  for (const ch of line) {
    if (escaped) escaped = false;
    else if (ch === "\\" && quote !== "'") escaped = true;
    else if (quote === null && (ch === '"' || ch === "'")) quote = ch;
    else if (ch === quote) quote = null;
  }
  return quote;
}

/**
 * Console transcripts (`prompted`): only `$` lines are commands, the rest is
 * output. Shell scripts: every line is a command and gets a `$` prompt.
 */
function shellLines(code: string, prompted: boolean): Line[] {
  const lines: Line[] = [];
  let quote: string | null = null;
  let continued = false;
  for (const line of code.split("\n")) {
    if (quote) {
      lines.push({ cls: "cmd", tokens: [{ text: line, cls: "s" }] });
    } else if (continued) {
      lines.push({ cls: "cmd", tokens: commandTokens(line) });
    } else if (prompted && line.startsWith("$")) {
      lines.push({ cls: "cmd", tokens: [{ text: "$", cls: "p" }, ...commandTokens(line.slice(1))] });
    } else if (prompted || !line.trim()) {
      lines.push({ cls: line ? "out" : "", tokens: [{ text: line, cls: "" }] });
      continue;
    } else if (line.trimStart().startsWith("#")) {
      lines.push({ cls: "", tokens: [{ text: line, cls: "c" }] });
      continue;
    } else {
      lines.push({ cls: "cmd", tokens: [{ text: "$", cls: "p" }, { text: " ", cls: "" }, ...commandTokens(line)] });
    }
    quote = quoteState(line, quote);
    continued = quote === null && line.endsWith("\\");
  }
  return lines;
}
