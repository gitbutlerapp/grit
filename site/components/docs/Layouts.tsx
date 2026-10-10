/**
 * Docs page layouts. The rule throughout: prose on paper, anything runnable in
 * an ink column.
 *
 * - Overview: the CLI on paper and the library on ink, side by side.
 * - Command pages: reference in the center; examples, JSON and See also in ink.
 * - Step guides (tutorial, install, …): one row per H2, its code beside it.
 * - Library guides: prose in the center, the compiled example in ink.
 * - Everything else: prose with code inline.
 */
import type { RootContent } from "mdast";
import { toString } from "mdast-util-to-string";
import {
  API_MAP_SLUG,
  hrefTo,
  isLibrarySlug,
  LIBRARY_QUICKSTART_SLUG,
  LIBRARY_SLUG,
  librarySlugs,
  type Page,
  type Site,
} from "@/lib/docs";
import { highlightHtml } from "@/lib/highlight";
import { agentFiles } from "@/lib/llms";
import { collectLinks, includePath, parseDoc, pullCode, renderHtml, splitSections, type CodeBlock, type Section } from "@/lib/markdown";
import { DOCS_RS, GITHUB_BLOB, SITE_URL } from "@/lib/repo";
import { Breadcrumb, Html, Pager } from "./Chrome";

/** Step guides move only short lead-ins ("On macOS and Linux:") next to their code. */
const STEP_CAPTION_MAX = 60;
const OVERVIEW_TILE_LABELS: Record<string, string> = {
  "global-options": "flags",
  scripting: "--json / --filter",
  agents: "automation",
};
const MODULE_URL = /^https:\/\/docs\.rs\/grit-lib\/[^/]+\/(grit_lib(?:\/[a-z0-9_]+)*)\/index\.html$/;

function escapeHtml(text: string): string {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/** Mark tables as reference rows (no header row, sized columns), by column count. */
function tagTables(html: string): string {
  return html.replace(/<table>(\s*<thead>[\s\S]*?<\/thead>)/g, (_m, head: string) => {
    const cols = Math.min((head.match(/<th[\s>]/g) ?? []).length, 3);
    return `<table class="rows rows-${cols}">${head}`;
  });
}

function heading(section: Section): string {
  return renderHtml([section.heading]);
}

function Example({ block, aside }: { block: CodeBlock; aside?: React.ReactNode }) {
  const lang = [block.lang, block.meta].filter(Boolean).join(" ");
  return (
    <div className="ex">
      {block.caption || aside ? (
        <div className="cap">
          <span dangerouslySetInnerHTML={{ __html: block.caption }} />
          {aside}
        </div>
      ) : null}
      <Html html={highlightHtml(lang, block.value)} />
    </div>
  );
}

function Lede({ page }: { page: Page }) {
  return page.summary ? <p className="lede">{page.summary}</p> : null;
}

/** Screen 2a: reference in the center, examples and JSON in an ink column. */
export function CommandLayout({ page, site }: { page: Page; site: Site }) {
  const { intro, sections } = splitSections(parseDoc(page.markdown).root);
  const center: string[] = [renderHtml(intro)];
  const examples: React.ReactNode[] = [];
  let seeAlso: React.ReactNode = null;
  sections.forEach((section) => {
    const name = section.title.toLowerCase();
    if (name === "synopsis") {
      const lines = section.nodes
        .filter((n) => n.type === "code")
        .flatMap((n) => (n as { value: string }).value.split("\n"))
        .map((line) => `<div>${escapeHtml(line)}</div>`);
      center.push(`<div class="synopsis" id="${section.anchor}">${lines.join("")}</div>`);
    } else if (name === "examples") {
      const { prose, blocks } = pullCode(section.nodes);
      blocks.forEach((block, i) => examples.push(<Example key={`ex${i}`} block={block} />));
      if (prose.length) examples.push(<Html key="exprose" className="ink-prose" html={renderHtml(prose)} />);
    } else if (name === "json output" || name === "markdown output") {
      const { prose, blocks } = pullCode(section.nodes);
      const flag = name.startsWith("json") ? "--json" : "--markdown";
      blocks.forEach((block, i) => {
        const caption = block.caption.length > 12 ? block.caption : `${flag} output`;
        const aside = flag === "--json" ? <a href={hrefTo(page.slug, "scripting")}>Scripting →</a> : null;
        examples.push(<Example key={`${flag}${i}`} block={{ ...block, caption }} aside={aside} />);
      });
      center.push(heading(section) + tagTables(renderHtml(prose)));
    } else if (name === "see also") {
      seeAlso = (
        <div className="see-also" id={section.anchor}>
          <span>See also</span>
          {collectLinks(section.nodes).map((link) => (
            <a key={link.href} href={link.href}>
              {link.text}
            </a>
          ))}
        </div>
      );
    } else {
      center.push(heading(section) + tagTables(renderHtml(section.nodes)));
    }
  });
  return (
    <div className="pane three">
      <main className="col main">
        <Breadcrumb parts={["Commands", page.group]} />
        <h1 className="cmd">{page.title}</h1>
        <Lede page={page} />
        <Html className="content" html={center.join("")} />
        <Pager site={site} slug={page.slug} />
      </main>
      <aside className="col ink-col" aria-label="Examples">
        <div className="label">Examples</div>
        {examples}
        {seeAlso}
      </aside>
    </div>
  );
}

/** Screen 2b: one row per H2, with that step's code in a continuous ink strip. */
export function StepsLayout({ page, site }: { page: Page; site: Site }) {
  const { intro, sections } = splitSections(parseDoc(page.markdown).root);
  const { prose: introProse, blocks: introBlocks } = pullCode(intro, STEP_CAPTION_MAX);
  // "Regenerated on …" notes describe the code, so they sit in the ink column.
  const isMeta = (n: RootContent) => n.type === "paragraph" && toString(n).startsWith("Regenerated");
  const meta = introProse.filter(isMeta);
  return (
    <main className="col steps">
      <div className="step intro">
        <Breadcrumb parts={[page.sectionTitle, page.title]} />
        <h1>{page.title}</h1>
        <Lede page={page} />
        <Html className="content" html={renderHtml(introProse.filter((n) => !isMeta(n)))} />
      </div>
      <div className="step-code intro">
        {introBlocks.map((block, i) => (
          <Example key={i} block={block} />
        ))}
        {meta.map((n, i) => (
          <Html key={`m${i}`} as="p" className="meta" html={renderHtml([n]).replace(/^<p>|<\/p>$/g, "")} />
        ))}
      </div>
      {sections.flatMap((section, i) => {
        const { prose, blocks } = pullCode(section.nodes, STEP_CAPTION_MAX);
        return [
          <div className="step" key={`s${i}`}>
            <span className="num">{String(i + 1).padStart(2, "0")}</span>
            <Html className="content" html={heading(section) + renderHtml(prose)} />
          </div>,
          <div className="step-code" key={`c${i}`}>
            {blocks.map((block, j) => (
              <Example key={j} block={block} />
            ))}
          </div>,
        ];
      })}
      <div className="step last">
        <Pager site={site} slug={page.slug} />
      </div>
      <div className="step-code" />
    </main>
  );
}

function isExampleSection(section: Section): boolean {
  return section.nodes.some((n) => n.type === "code" && includePath(n.meta ?? ""));
}

/**
 * Screen 2c: guide prose on paper, the compiled example (an `<!-- include -->`
 * of a grit-examples file) in an ink column. Returns null when the guide has no
 * included example.
 */
export function LibraryLayout({ page, site }: { page: Page; site: Site }) {
  const { intro, sections } = splitSections(parseDoc(page.markdown).root);
  if (!sections.some(isExampleSection)) return null;
  const center: string[] = [renderHtml(intro)];
  const examples: React.ReactNode[] = [];
  let takeRun = false;
  sections.forEach((section, si) => {
    const example = isExampleSection(section);
    if (!example && !(takeRun && section.title.toLowerCase().startsWith("run"))) {
      takeRun = false;
      center.push(heading(section) + renderHtml(section.nodes));
      return;
    }
    takeRun = example;
    const { prose, blocks } = pullCode(section.nodes);
    let lead = prose.length ? renderHtml(prose) : "";
    blocks.forEach((block, bi) => {
      const rel = includePath(block.meta);
      if (rel) {
        examples.push(
          <div className="ex-head" key={`h${si}-${bi}`}>
            <span>Example · {rel.split("/").pop()}</span>
            <a href={`${GITHUB_BLOB}/${rel}`}>full source ↗</a>
          </div>,
        );
        if (section.title.toLowerCase() !== "example") {
          examples.push(
            <div className="ex-title" id={section.anchor} key={`t${si}-${bi}`}>
              {section.title}
            </div>,
          );
        }
        if (lead) examples.push(<Html key={`p${si}-${bi}`} className="ink-prose" html={lead} />);
        lead = "";
      }
      examples.push(<Example key={`e${si}-${bi}`} block={block} />);
    });
    if (lead) examples.push(<Html key={`l${si}`} className="ink-prose" html={lead} />);
  });
  const modules = [
    ...new Set(collectLinks(intro).map((l) => MODULE_URL.exec(l.href)?.[1]).filter((m): m is string => Boolean(m))),
  ].sort();
  return (
    <div className="pane three lib">
      <main className="col main">
        <Breadcrumb parts={["Library guide", page.title]} />
        <h1>{page.title}</h1>
        <Lede page={page} />
        {modules.length ? (
          <div className="modules">
            {modules.map((m) => (
              <a key={m} href={`${DOCS_RS}/latest/${m}/index.html`}>
                {m.replace(/\//g, "::")}
              </a>
            ))}
          </div>
        ) : null}
        <Html className="content" html={center.join("")} />
        <Pager site={site} slug={page.slug} />
      </main>
      <aside className="col ink-col lib" aria-label="Example">
        {examples}
      </aside>
    </div>
  );
}

/** Guides without step code: prose with code blocks inline. */
export function PlainLayout({ page, site }: { page: Page; site: Site }) {
  const section = isLibrarySlug(page.slug) ? "Library guide" : page.sectionTitle;
  return (
    <div className="pane">
      <main className="col main">
        <Breadcrumb parts={[section, page.title === section ? "" : page.title]} />
        <h1>{page.title}</h1>
        <Lede page={page} />
        <Html className="content" html={renderHtml(parseDoc(page.markdown).root.children)} />
        <Pager site={site} slug={page.slug} />
      </main>
    </div>
  );
}

/** Screen 1c: the CLI on paper and the library on ink, side by side. */
export function OverviewLayout({ site }: { site: Site }) {
  const titles = new Map(site.pages.map((p) => [p.slug, p.title]));
  const cliPages = site.sections.find((s) => s.commandGroups.length)?.pages ?? [];
  return (
    <>
      <main className="col half cli">
        <div className="label">$ grit</div>
        <h1>The command line</h1>
        <p className="lede">
          Works on any Git repository and talks to any remote. Plain-language output and <code>--json</code> on every
          command.
        </p>
        <div className="tiles four">
          <a className="tile ink" href="tutorial/">
            <span>start here</span>
            <b>Tutorial →</b>
          </a>
          {cliPages.map((spec) => (
            <a key={spec.slug} className="tile" href={`${spec.slug}/`}>
              <span>{OVERVIEW_TILE_LABELS[spec.slug] ?? "guide"}</span>
              <b>{spec.label}</b>
            </a>
          ))}
        </div>
        <h2 id="commands" className="visually-hidden">
          Commands
        </h2>
        <div className="cmd-index">
          {site.commandGroups.map((group) => {
            const rows = site.commandPages.filter((p) => p.group === group);
            if (!rows.length) return null;
            return (
              <div className="group" key={group}>
                <h3 id={group.toLowerCase().replace(/[^a-z0-9]+/g, "-")}>{group}</h3>
                {rows.map((p) => (
                  <a key={p.slug} href={`${p.slug}/`}>
                    <code>{p.title}</code>
                    <span>{p.summary}</span>
                  </a>
                ))}
              </div>
            );
          })}
        </div>
        <section className="agents" aria-labelledby="for-agents">
          <h2 id="for-agents">For agents</h2>
          <ul>
            {agentFiles().map((f) => (
              <li key={f.label}>
                <a href={f.url.replace(`${SITE_URL}/docs/`, "").replace(SITE_URL, "")}>{f.label}</a> — {f.what}
              </li>
            ))}
          </ul>
          <p>
            Every page has a Markdown twin: swap <code>index.html</code> for <code>index.md</code>.{" "}
            <code>grit skill</code> prints an agent skill for the CLI.
          </p>
        </section>
      </main>
      <section className="col half lib" aria-label="The library">
        <div className="label">use grit_lib;</div>
        <h2>The library</h2>
        <p className="lede">
          A fast, linkable Git library for Rust. Everything the CLI does goes through it: open repositories, read
          objects and refs, diff, revwalk, fetch and push.
        </p>
        <div className="tiles two">
          <a className="tile accent" href={`${LIBRARY_QUICKSTART_SLUG}/`}>
            <span>start here</span>
            <b>Library quick start →</b>
          </a>
          <a className="tile" href={`${API_MAP_SLUG}/`}>
            <span>every public type</span>
            <b>{titles.get(API_MAP_SLUG) ?? "grit-lib API map"}</b>
          </a>
        </div>
        <div className="sub">Guides</div>
        <div className="guides">
          {librarySlugs(site)
            .filter((slug) => slug !== LIBRARY_SLUG && slug !== API_MAP_SLUG)
            .map((slug) => (
              <a key={slug} href={`${slug}/`}>
                {titles.get(slug)}
              </a>
            ))}
        </div>
        <a className="ext" href={DOCS_RS}>
          API reference on docs.rs ↗
        </a>
        <p className="note">
          On-disk formats and the wire protocol stay compatible with Git. <a href="benchmarks/">See benchmarks →</a>
        </p>
      </section>
    </>
  );
}
