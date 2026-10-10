/**
 * Machine-readable docs for agents:
 *
 * - every page's Markdown twin at `<page>/index.md`
 * - `/llms.txt` (https://llmstxt.org) and `/llms-full.txt` (every twin)
 * - `/docs/grit-cli.md` and `/docs/grit-lib.md`: each half of the docs in one file
 */
import { loadPosts, postTwinUrl } from "./blog";
import { isLibrarySlug, librarySlugs, loadSite, twinUrl, type Page, type Site } from "./docs";
import { DOCS_RS, SITE_URL } from "./repo";

export const CLI_BUNDLE = "grit-cli.md";
export const LIB_BUNDLE = "grit-lib.md";
const TAGLINE =
  "Grit is a fast Git implementation in Rust: grit-lib is a linkable library for Rust programs, and grit is a modern Git client built on that library. On-disk formats and the wire protocol stay compatible with Git.";
const USAGE =
  "Each docs page has a Markdown twin at the same URL with `index.md` instead of `index.html`. Use `grit --json` (and `--markdown` on many commands) for script- and agent-friendly CLI output.";
const CLI_INTRO =
  "Everything an agent needs to use `grit` as a Git client: install, the tutorial, global options, scripting with `--json` and `--filter`, the agent guide, and the reference page for every command. `grit` works on any Git repository and talks to any Git remote.";
const LIB_INTRO =
  "Everything an agent needs to write Git-compatible Rust programs with `grit-lib`: the quick start, every library guide with its compiled example, and the generated API map. On-disk formats and the wire protocol match Git, so repositories you write work with `git` and any Git host. Full API reference: https://docs.rs/grit-lib";

export function bundleUrl(name: string): string {
  return `${SITE_URL}/docs/${name}`;
}

/** The machine-readable entry points, with what each holds. */
export function agentFiles(): { label: string; url: string; what: string }[] {
  return [
    { label: CLI_BUNDLE, url: bundleUrl(CLI_BUNDLE), what: "the whole CLI in one file, to learn grit for everyday Git work" },
    { label: LIB_BUNDLE, url: bundleUrl(LIB_BUNDLE), what: "the whole library guide in one file, to build Git-compatible Rust programs" },
    { label: "llms.txt", url: `${SITE_URL}/llms.txt`, what: "an index of every page with a one-line summary" },
    { label: "llms-full.txt", url: `${SITE_URL}/llms-full.txt`, what: "every docs page in one file" },
  ];
}

function agentsMarkdown(): string {
  return [
    "## For agents",
    "",
    ...agentFiles().map((f) => `- [${f.label}](${f.url}): ${f.what}.`),
    "",
    "Every page also has a Markdown twin: replace `index.html` with `index.md` in its URL. Run `grit skill` to print an agent skill for the CLI.",
  ].join("\n");
}

function commandIndexMarkdown(site: Site): string {
  const lines = ["## Commands", ""];
  for (const group of site.commandGroups) {
    const rows = site.commandPages.filter((p) => p.group === group);
    if (!rows.length) continue;
    lines.push(`### ${group}`, "", "| Command | Summary |", "| --- | --- |");
    for (const p of rows) lines.push(`| [\`${p.title}\`](${twinUrl(p.slug)}) | ${p.summary} |`);
    lines.push("");
  }
  return lines.join("\n").trimEnd();
}

/** The Markdown twin served at `<page>/index.md`. */
export function pageTwin(page: Page, site: Site = loadSite()): string {
  if (page.slug !== "index") return page.twin;
  return `${page.twin.trimEnd()}\n\n${agentsMarkdown()}\n\n${commandIndexMarkdown(site)}\n`;
}

function linkLine(title: string, url: string, summary: string): string {
  return summary.trim() ? `- [${title}](${url}): ${summary.trim()}` : `- [${title}](${url})`;
}

export function llmsTxt(site: Site = loadSite()): string {
  const bySlug = new Map(site.pages.map((p) => [p.slug, p]));
  const lines = ["# Grit", "", `> ${TAGLINE}`, "", USAGE, "", "## Whole sections", ""];
  for (const f of agentFiles().slice(0, 2)) lines.push(`- [${f.label}](${f.url}): ${f.what[0].toUpperCase()}${f.what.slice(1)}.`);
  lines.push("");
  for (const section of site.sections) {
    lines.push(`## ${section.title}`, "");
    for (const spec of section.pages) {
      const p = bySlug.get(spec.slug)!;
      lines.push(linkLine(p.title, twinUrl(p.slug), p.summary));
    }
    for (const group of section.commandGroups) {
      const rows = site.commandPages.filter((p) => p.group === group);
      if (!rows.length) continue;
      lines.push(`### ${group}`, "");
      for (const p of rows) lines.push(linkLine(p.title, twinUrl(p.slug), p.summary));
    }
    if (section.directory) {
      for (const slug of librarySlugs(site)) {
        const p = bySlug.get(slug)!;
        lines.push(linkLine(slug === section.directory ? "Overview" : p.title, twinUrl(slug), p.summary));
      }
    }
    lines.push("");
  }
  lines.push("## Optional", "", `- [grit-lib on docs.rs](${DOCS_RS}): Rust API reference generated from crate rustdoc.`);
  for (const post of loadPosts()) lines.push(linkLine(post.title, postTwinUrl(post.slug), post.summary));
  lines.push(`- [Blog](${SITE_URL}/blog/): release notes and project updates.`, "");
  return lines.join("\n").trimEnd() + "\n";
}

export function llmsFullTxt(site: Site = loadSite()): string {
  return site.pages.map((p) => `# ${twinUrl(p.slug)}\n\n${pageTwin(p, site).trimEnd()}\n`).join("\n");
}

/** Pages in one half of the docs, in reading order. Benchmarks belong to neither. */
export function bundlePages(site: Site, library: boolean): Page[] {
  return site.pages.filter((p) => p.slug !== "benchmarks" && isLibrarySlug(p.slug) === library);
}

export function bundle(library: boolean, site: Site = loadSite()): string {
  const title = library ? "grit-lib: the library guide" : "grit: the command line";
  const intro = library ? LIB_INTRO : CLI_INTRO;
  const chunks = [`# ${title}\n\n> ${intro}\n\nEach section below is one docs page; its heading is the page's URL.\n`];
  for (const p of bundlePages(site, library)) chunks.push(`# ${twinUrl(p.slug)}\n\n${pageTwin(p, site).trimEnd()}\n`);
  return chunks.join("\n");
}
