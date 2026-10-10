/**
 * The docs content model: `content/docs/site.toml` plus the Markdown pages it lists.
 *
 * - Guides are listed explicitly under `[[section.page]]`.
 * - Every `commands/*.md` page joins the CLI section; its `group` front matter
 *   must be one of `[section.commands].groups`.
 * - Every `library/*.md` page joins the library section (`directory = "library"`).
 *
 * Every Markdown file must be reachable from the manifest; `loadSite` throws
 * otherwise so a forgotten page fails the build.
 */
import fs from "node:fs";
import path from "node:path";
import { parse as parseToml } from "smol-toml";
import { BENCHMARK_MARKER, benchmarkMarkdown } from "./bench";
import { docsContentDir, readRepoFile, SITE_URL } from "./repo";
import { parseDoc, type TocItem } from "./markdown";
import { apiMapMarkdown, expandRustdocLinks } from "./rustdoc";
import { composeMarkdownTwin, isExternal, parseFrontMatter, rewriteMarkdownLinks } from "./text";

export const LIBRARY_SLUG = "library";
export const LIBRARY_QUICKSTART_SLUG = "library-quickstart";
export const API_MAP_SLUG = "library/api-map";
export const API_MAP_MARKER = "<!-- grit:api-map -->";
/** Guides laid out as steps, each H2's code beside its prose. */
export const STEP_GUIDE_SLUGS = new Set(["tutorial", "install", "scripting", "agents", LIBRARY_QUICKSTART_SLUG]);
const INCLUDE = /<!--\s*include:\s*(\S+)\s*-->/g;

export interface PageSpec {
  file: string;
  slug: string;
  label: string;
  sectionTitle: string;
}

export interface SectionSpec {
  title: string;
  pages: PageSpec[];
  commandGroups: string[];
  directory?: string;
}

export type { TocItem };

export interface Page {
  slug: string;
  title: string;
  summary: string;
  sectionTitle: string;
  group: string;
  order: number;
  isCommand: boolean;
  /** Markdown ready to render: includes expanded and tagged, rustdoc links resolved. */
  markdown: string;
  /** The page's Markdown twin (`index.md`), with absolute links. */
  twin: string;
  toc: TocItem[];
}

export interface Site {
  sections: SectionSpec[];
  /** Every page in reading (pager) order. */
  pages: Page[];
  commandPages: Page[];
  commandGroups: string[];
  baselines: string[];
}

export function isLibrarySlug(slug: string): boolean {
  return slug === LIBRARY_QUICKSTART_SLUG || slug === LIBRARY_SLUG || slug.startsWith(`${LIBRARY_SLUG}/`);
}

/** Relative href from one docs page to another (pages live at `/docs/<slug>/`). */
export function hrefTo(from: string, to: string): string {
  const depth = from === "index" ? 0 : from.split("/").length;
  const ups = "../".repeat(depth);
  if (to === "index") return ups || "./";
  return `${ups}${to}/`;
}

export function pageUrl(slug: string): string {
  return slug === "index" ? `${SITE_URL}/docs/` : `${SITE_URL}/docs/${slug}/`;
}

export function twinUrl(slug: string): string {
  return slug === "index" ? `${SITE_URL}/docs/index.md` : `${SITE_URL}/docs/${slug}/index.md`;
}

function loadManifest(contentDir: string): { sections: SectionSpec[]; baselines: string[] } {
  const manifestPath = path.join(contentDir, "site.toml");
  if (!fs.existsSync(manifestPath)) throw new Error(`missing site manifest ${manifestPath}`);
  const data = parseToml(fs.readFileSync(manifestPath, "utf8")) as any;
  const sections: SectionSpec[] = (data.section ?? []).map((raw: any) => ({
    title: raw.title,
    pages: (raw.page ?? []).map((entry: any) => {
      const stem = path.basename(entry.file, ".md");
      return {
        file: entry.file,
        slug: entry.slug ?? stem,
        label: entry.label ?? entry.slug ?? stem,
        sectionTitle: raw.title,
      };
    }),
    commandGroups: raw.commands?.groups ?? [],
    directory: raw.directory,
  }));
  return { sections, baselines: data.benchmarks?.baseline ?? [] };
}

/** Every page source the manifest reaches, keyed by absolute path. */
function listedSources(sections: SectionSpec[], contentDir: string): Map<string, PageSpec> {
  const listed = new Map<string, PageSpec>();
  for (const section of sections) {
    for (const page of section.pages) listed.set(path.resolve(contentDir, page.file), page);
    if (section.directory) {
      const dir = path.join(contentDir, section.directory);
      for (const name of fs.readdirSync(dir).filter((n) => n.endsWith(".md")).sort()) {
        const stem = name.slice(0, -3);
        const slug = stem === "index" ? section.directory : `${section.directory}/${stem}`;
        listed.set(path.resolve(dir, name), {
          file: `${section.directory}/${name}`,
          slug,
          label: stem === "index" ? "Overview" : stem,
          sectionTitle: section.title,
        });
      }
    }
    if (section.commandGroups.length) {
      const dir = path.join(contentDir, "commands");
      for (const name of fs.readdirSync(dir).filter((n) => n.endsWith(".md") && n.toUpperCase() !== "README.MD").sort()) {
        const stem = name.slice(0, -3);
        listed.set(path.resolve(dir, name), { file: `commands/${name}`, slug: stem, label: stem, sectionTitle: section.title });
      }
    }
  }
  return listed;
}

function allMarkdown(dir: string): string[] {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) return allMarkdown(full);
    return entry.name.endsWith(".md") ? [full] : [];
  });
}

/** Fail on manifest pages that don't exist and Markdown files the manifest doesn't reach. */
export function validateManifest(sections: SectionSpec[], contentDir: string): void {
  const listed = listedSources(sections, contentDir);
  for (const [source, spec] of listed) {
    if (!fs.existsSync(source)) throw new Error(`site.toml lists missing page ${spec.file}`);
  }
  for (const file of allMarkdown(contentDir).sort()) {
    const rel = path.relative(contentDir, file);
    if (path.basename(file).toUpperCase() === "README.MD" && path.basename(path.dirname(file)) === "commands") continue;
    if (!listed.has(path.resolve(file))) throw new Error(`Markdown file not listed in site.toml: ${rel}`);
  }
}

/** Fail when a code fence has no language tag (highlighting and layout depend on it). */
export function validateFenceLanguages(contentDir: string): void {
  for (const file of allMarkdown(contentDir).sort()) {
    let inCode = false;
    fs.readFileSync(file, "utf8")
      .split("\n")
      .forEach((line, i) => {
        const match = /^(`{3,})(\S*)\s*$/.exec(line.trim());
        if (!match) return;
        if (!inCode && !match[2]) {
          throw new Error(
            `${path.relative(contentDir, file)}:${i + 1}: code fence missing language tag (use console, text, json, rust, toml, bash, …)`,
          );
        }
        inCode = !inCode;
      });
  }
}

/**
 * Replace `<!-- include: path -->` with a fenced copy of that repository file.
 * With `tag`, the fence carries `include=<path>` so the layout can put the
 * example in its own column and link to the full source.
 */
export function expandIncludes(body: string, { tag }: { tag: boolean }): string {
  return body.replace(INCLUDE, (_whole, rel: string) => {
    const text = readRepoFile(rel).trimEnd();
    const lang = rel.endsWith(".rs") ? "rust" : "text";
    return "```" + lang + (tag ? ` include=${rel}` : "") + "\n" + text + "\n```";
  });
}

function spliceGenerated(slug: string, body: string, baselines: string[]): string {
  if (slug === "benchmarks") {
    if (!body.includes(BENCHMARK_MARKER)) throw new Error(`benchmarks page must contain ${BENCHMARK_MARKER}`);
    return body.replace(BENCHMARK_MARKER, benchmarkMarkdown(baselines));
  }
  if (slug === API_MAP_SLUG) {
    if (!body.includes(API_MAP_MARKER)) throw new Error(`API map page must contain ${API_MAP_MARKER}`);
    return body.replace(API_MAP_MARKER, apiMapMarkdown());
  }
  return body;
}

/** Map a relative docs link to the target page slug. */
export function linkTargetSlug(fromSlug: string, target: string): string {
  const parts = fromSlug === "index" ? [] : fromSlug.split("/");
  for (const seg of target.replace(/\\/g, "/").split("/")) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") parts.pop();
    else parts.push(seg);
  }
  return parts.length ? parts.join("/") : "index";
}

function loadPage(source: string, spec: PageSpec, commandGroups: string[], baselines: string[]): Page {
  const { meta, body } = parseFrontMatter(fs.readFileSync(source, "utf8"));
  const isCommand = path.basename(path.dirname(source)) === "commands";
  const group = meta.group ?? "";
  if (isCommand && !commandGroups.includes(group)) {
    throw new Error(`${spec.file}: group ${JSON.stringify(group)} must be one of ${commandGroups.join(", ")}`);
  }
  const generated = spliceGenerated(spec.slug, body, baselines);
  const markdown = expandRustdocLinks(expandIncludes(generated, { tag: true }));
  const twinBody = rewriteMarkdownLinks(expandRustdocLinks(expandIncludes(generated, { tag: false })), (target) =>
    isExternal(target) || target.startsWith("#") ? null : twinUrl(linkTargetSlug(spec.slug, target)),
  );
  const title = meta.title ?? spec.label;
  const summary = meta.summary ?? "";
  return {
    slug: spec.slug,
    title,
    summary,
    sectionTitle: spec.sectionTitle,
    group,
    order: Number(meta.order ?? 0),
    isCommand,
    markdown,
    twin: composeMarkdownTwin(title, twinBody, { summary }),
    toc: parseDoc(markdown).toc,
  };
}

let cached: Site | undefined;

/** Load and validate the whole docs site (cached per process). */
export function loadSite(): Site {
  if (cached) return cached;
  const contentDir = docsContentDir();
  const { sections, baselines } = loadManifest(contentDir);
  validateManifest(sections, contentDir);
  validateFenceLanguages(contentDir);
  const commandGroups = sections.find((s) => s.commandGroups.length)?.commandGroups ?? [];
  if (!commandGroups.length) throw new Error("site.toml: no [section.commands] block with groups");

  const bySlug = new Map<string, Page>();
  for (const [source, spec] of listedSources(sections, contentDir)) {
    bySlug.set(spec.slug, loadPage(source, spec, commandGroups, baselines));
  }
  const pages: Page[] = [];
  let commandPages: Page[] = [];
  for (const section of sections) {
    for (const spec of section.pages) pages.push(bySlug.get(spec.slug)!);
    if (section.commandGroups.length) {
      commandPages = [...bySlug.values()]
        .filter((p) => p.isCommand)
        .sort(
          (a, b) =>
            commandGroups.indexOf(a.group) - commandGroups.indexOf(b.group) || a.order - b.order || a.slug.localeCompare(b.slug),
        );
      pages.push(...commandPages);
    }
    if (section.directory) {
      const dir = section.directory;
      pages.push(
        ...[...bySlug.keys()]
          .filter((slug) => slug === dir || slug.startsWith(`${dir}/`))
          .sort()
          .map((slug) => bySlug.get(slug)!),
      );
    }
  }
  cached = { sections, pages, commandPages, commandGroups, baselines };
  return cached;
}

/** Test hook: forget the cached site after changing content env vars. */
export function resetSiteCache(): void {
  cached = undefined;
}

export function findPage(site: Site, slug: string): Page | undefined {
  return site.pages.find((p) => p.slug === slug);
}

/** Library guide slugs: the overview first, then by slug. */
export function librarySlugs(site: Site): string[] {
  const slugs = site.pages.map((p) => p.slug).filter((s) => s === LIBRARY_SLUG || s.startsWith(`${LIBRARY_SLUG}/`));
  return [LIBRARY_SLUG, ...slugs.filter((s) => s !== LIBRARY_SLUG).sort()].filter((s) => slugs.includes(s));
}

export function pagerLinks(site: Site, slug: string): { prev?: Page; next?: Page } {
  const i = site.pages.findIndex((p) => p.slug === slug);
  if (i < 0) return {};
  return { prev: site.pages[i - 1], next: site.pages[i + 1] };
}
