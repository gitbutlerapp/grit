/**
 * grit-lib API data from local rustdoc (`cargo doc -p grit-lib --no-deps`).
 *
 * `rustdoc:grit_lib::refs::resolve_ref` links in the docs become docs.rs URLs,
 * and the API map page lists every public module, struct, enum and trait. CI
 * builds rustdoc first and runs in strict mode, so a renamed or removed item
 * fails the build. Without rustdoc (a quick local preview), links fall back to
 * a docs.rs search and the API map shows a notice.
 */
import fs from "node:fs";
import path from "node:path";
import { DOCS_RS, rustdocRoot, strict } from "./repo";

const DOCS_RS_LATEST = `${DOCS_RS}/latest`;
const RUSTDOC_PATH = /^grit_lib(::[A-Za-z_][A-Za-z0-9_]*)*$/;
const ITEM_KINDS = ["struct", "enum", "fn", "trait", "type", "constant", "union", "macro", "mod"];
export const RUSTDOC_LINK = /\]\(rustdoc:([^)]+)\)/g;

function crateDir(): string {
  return path.join(rustdocRoot(), "grit_lib");
}

export function rustdocAvailable(): boolean {
  return fs.existsSync(path.join(crateDir(), "index.html"));
}

function requireRustdoc(): void {
  if (strict() && !rustdocAvailable()) {
    throw new Error(`grit-lib rustdoc missing at ${crateDir()}; run: cargo doc -p grit-lib --no-deps`);
  }
}

function isFile(p: string): boolean {
  return fs.existsSync(p) && fs.statSync(p).isFile();
}

/** Rust type or enum variant segment (`MergeCommitInReplay`, not `try_from`). */
function isRustTypeSegment(segment: string): boolean {
  return /^[A-Z][A-Za-z0-9_]*$/.test(segment);
}

/** Field or method segment (`base`, `try_from`), not a module or type name. */
function isRustMemberSegment(segment: string): boolean {
  return /^[a-z][A-Za-z0-9_]*$/.test(segment);
}

/**
 * `Type::member` suffix on a rustdoc path: resolve the type path and member name separately.
 */
function splitAssociatedItemSuffix(segments: string[]): { pathSegments: string[]; member?: string } {
  if (segments.length < 2) return { pathSegments: segments };
  const member = segments[segments.length - 1]!;
  const typeName = segments[segments.length - 2]!;
  if (!isRustTypeSegment(typeName)) return { pathSegments: segments };
  if (isRustTypeSegment(member) || isRustMemberSegment(member)) {
    return { pathSegments: segments.slice(0, -1), member };
  }
  return { pathSegments: segments };
}

/** Pick a rustdoc fragment that exists on the generated type page. */
function memberFragmentInRustdocHtml(htmlPath: string, member: string): string | undefined {
  const html = fs.readFileSync(htmlPath, "utf8");
  const prefixes = isRustTypeSegment(member)
    ? [`variant.${member}`]
    : [`method.${member}`, `tymethod.${member}`, `structfield.${member}`, `variant.${member}`];
  for (const frag of prefixes) {
    if (html.includes(`id="${frag}"`) || html.includes(`href="#${frag}"`)) {
      return frag;
    }
  }
  return undefined;
}

/** The rustdoc HTML path (relative to `grit_lib/`) for a qualified item path. */
export function resolveRustdocPath(qualified: string): string {
  if (!RUSTDOC_PATH.test(qualified)) throw new Error(`invalid rustdoc path syntax: ${qualified}`);
  const [, ...rawSegments] = qualified.split("::");
  const { pathSegments, member } = splitAssociatedItemSuffix(rawSegments);
  const root = crateDir();
  if (pathSegments.length === 0) return "index.html";
  let current = root;
  const modules = pathSegments.slice(0, -1);
  const last = pathSegments[pathSegments.length - 1]!;
  for (const mod of modules) {
    const nested = path.join(current, mod);
    if (isFile(path.join(nested, "index.html"))) {
      current = nested;
      continue;
    }
    throw new Error(`rustdoc module not found in ${qualified}: ${mod}`);
  }
  const attachFragment = (relative: string, htmlPath: string, qualifiedPath: string): string => {
    if (!member) return relative;
    const fragment = memberFragmentInRustdocHtml(htmlPath, member);
    if (!fragment) {
      throw new Error(`rustdoc associated item not found: ${qualifiedPath} (no anchor on ${relative})`);
    }
    return `${relative}#${fragment}`;
  };

  for (const kind of ITEM_KINDS) {
    const candidate = path.join(current, `${kind}.${last}.html`);
    if (isFile(candidate)) {
      const relative = path.relative(root, candidate).split(path.sep).join("/");
      return attachFragment(relative, candidate, qualified);
    }
  }
  const modIndex = path.join(current, last, "index.html");
  if (isFile(modIndex)) {
    const relative = path.relative(root, modIndex).split(path.sep).join("/");
    return attachFragment(relative, modIndex, qualified);
  }
  throw new Error(`rustdoc item not found: ${qualified}`);
}

export function docsRsUrl(relative: string): string {
  return `${DOCS_RS_LATEST}/grit_lib/${relative}`;
}

/** docs.rs URL for a qualified path, or a docs.rs search when rustdoc isn't built. */
export function rustdocUrl(qualified: string): string {
  requireRustdoc();
  if (!rustdocAvailable()) {
    const name = qualified.split("::").pop() ?? qualified;
    return `${DOCS_RS_LATEST}/grit_lib/?search=${encodeURIComponent(name)}`;
  }
  return docsRsUrl(resolveRustdocPath(qualified.trim()));
}

/** Replace `](rustdoc:…)` link targets with docs.rs URLs. */
export function expandRustdocLinks(markdown: string): string {
  return markdown.replace(RUSTDOC_LINK, (_whole, qualified: string) => `](${rustdocUrl(qualified.trim())})`);
}

/** Every `rustdoc:` path that doesn't resolve, for CI validation. */
export function unresolvedRustdocLinks(markdown: string): string[] {
  const bad: string[] = [];
  for (const match of markdown.matchAll(RUSTDOC_LINK)) {
    try {
      resolveRustdocPath(match[1].trim());
    } catch (err) {
      bad.push(`${match[1].trim()}: ${(err as Error).message}`);
    }
  }
  return bad;
}

export interface ApiRow {
  qualified: string;
  kind: "module" | "struct" | "enum" | "trait";
  summary: string;
  url: string;
}

const MOD_LINK = /<a class="mod" href="([^"]+)" title="mod ([^"]+)">/g;
const ITEM_ROW =
  /<a class="(struct|enum|trait)" href="([^"]+)" title="(?:struct|enum|trait) ([^"]+)">[\s\S]*?<\/a><\/dt><dd>([\s\S]*?)<\/dd>/g;
const TOP_DOC_FIRST_P = /class="toggle top-doc"[^>]*>[\s\S]*?<div class="docblock">[\s\S]*?<p>([\s\S]*?)<\/p>/;
const TITLE = /<title>(grit_lib(?:::[A-Za-z0-9_]+)+) - Rust<\/title>/;

function stripHtml(fragment: string): string {
  return fragment
    .replace(/<code[^>]*>([\s\S]*?)<\/code>/g, "$1")
    .replace(/<[^>]+>/g, "")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;|&#x27;/g, "'")
    .replace(/&amp;/g, "&")
    .trim();
}

function firstSentence(text: string): string {
  const flat = text.split(/\s+/).join(" ").trim();
  const match = /^(.+?[.!?])(?:\s|$)/.exec(flat);
  return match ? match[1] : flat;
}

function moduleIndexPaths(root: string): string[] {
  const seen = new Set<string>();
  const pending: string[] = [];
  const crateIndex = fs.readFileSync(path.join(root, "index.html"), "utf8");
  for (const m of crateIndex.matchAll(MOD_LINK)) pending.push(path.resolve(root, m[1]));
  while (pending.length) {
    const index = pending.pop()!;
    if (seen.has(index) || !isFile(index)) continue;
    seen.add(index);
    const text = fs.readFileSync(index, "utf8");
    for (const m of text.matchAll(MOD_LINK)) pending.push(path.resolve(path.dirname(index), m[1]));
  }
  return [...seen].sort();
}

/**
 * Rows for the API map: every public module and its structs, enums and traits.
 * Throws when a module lacks a `//!` summary, so new modules get documented.
 */
export function apiMapRows(): ApiRow[] {
  const root = crateDir();
  const rows: ApiRow[] = [];
  const missing: string[] = [];
  for (const index of moduleIndexPaths(root)) {
    const text = fs.readFileSync(index, "utf8");
    const rel = path.relative(root, index).split(path.sep).join("/");
    const qualified =
      TITLE.exec(text)?.[1] ?? `grit_lib::${path.dirname(rel).split("/").join("::")}`;
    const top = TOP_DOC_FIRST_P.exec(text);
    const summary = top ? firstSentence(stripHtml(top[1])) : "";
    if (!summary) missing.push(qualified);
    rows.push({ qualified, kind: "module", summary, url: docsRsUrl(rel) });
    const items: ApiRow[] = [];
    for (const m of text.matchAll(ITEM_ROW)) {
      const itemRel = path.relative(root, path.resolve(path.dirname(index), m[2])).split(path.sep).join("/");
      items.push({
        qualified: m[3],
        kind: m[1] as ApiRow["kind"],
        summary: firstSentence(stripHtml(m[4])),
        url: docsRsUrl(itemRel),
      });
    }
    items.sort((a, b) => a.kind.localeCompare(b.kind) || a.qualified.localeCompare(b.qualified));
    rows.push(...items);
  }
  if (missing.length) {
    throw new Error(
      `grit-lib modules missing a //! summary (add module docs):\n  ${[...new Set(missing)].sort().join("\n  ")}`,
    );
  }
  rows.sort((a, b) => (a.qualified < b.qualified ? -1 : a.qualified > b.qualified ? 1 : a.kind.localeCompare(b.kind)));
  return rows;
}

/** The API map as a Markdown table (spliced into `library/api-map.md`). */
export function apiMapMarkdown(): string {
  requireRustdoc();
  if (!rustdocAvailable()) {
    return "> The API map is generated from local rustdoc. Run `cargo doc -p grit-lib --no-deps` and reload to see it.\n";
  }
  const lines = ["| Item | Kind | Summary | docs.rs |", "| --- | --- | --- | --- |"];
  for (const row of apiMapRows()) {
    lines.push(`| \`${row.qualified}\` | ${row.kind} | ${row.summary.replace(/\|/g, "\\|")} | [API](${row.url}) |`);
  }
  return lines.join("\n") + "\n";
}
