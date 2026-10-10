import { Fragment } from "react";
import { hrefTo, isLibrarySlug, LIBRARY_QUICKSTART_SLUG, LIBRARY_SLUG, librarySlugs, type Site, type TocItem } from "@/lib/docs";
import { DOCS_RS, GITHUB_URL } from "@/lib/repo";
import { Search, type SearchEntry } from "./Search";

function searchEntries(site: Site): SearchEntry[] {
  return site.pages.map((p) => [p.title, p.summary, p.slug === "index" ? "" : `${p.slug}/`]);
}

/** Docs header: brand, CLI | Library switch, ref label, search, links. */
export function DocsHeader({ site, slug }: { site: Site; slug: string }) {
  const library = isLibrarySlug(slug);
  const docsHome = hrefTo(slug, "index");
  return (
    <header className="dh">
      <a className="brand" href="/" aria-label="grit homepage">
        grit
      </a>
      <div className="switch" role="group" aria-label="Docs half">
        {library ? <a href={docsHome}>CLI</a> : <span aria-current="true">CLI</span>}
        {library ? <span aria-current="true">Library</span> : <a href={hrefTo(slug, LIBRARY_SLUG)}>Library</a>}
      </div>
      <span className="ref">HEAD → docs/{slug}</span>
      <Search entries={searchEntries(site)} root={docsHome} />
      <nav aria-label="Primary">
        {library ? <a href={DOCS_RS}>docs.rs ↗</a> : <a href="/blog/">Blog</a>}
        <a className="pill" href={GITHUB_URL}>
          GitHub
        </a>
      </nav>
    </header>
  );
}

/** The overview's header: tagline instead of the switch and search. */
export function OverviewHeader() {
  return (
    <header className="dh">
      <a className="brand" href="/" aria-label="grit homepage">
        grit
      </a>
      <span className="ref">HEAD → docs</span>
      <span className="tagline">How to use grit, a simple Git client built on grit-lib.</span>
      <nav aria-label="Primary">
        <a href="install/">Install</a>
        <a href="benchmarks/">Benchmarks</a>
        <a href="/blog/">Blog</a>
        <a href="index.md">Markdown</a>
        <a className="pill" href={GITHUB_URL}>
          GitHub
        </a>
      </nav>
    </header>
  );
}

function NavLink({ current, to, label, className }: { current: string; to: string; label: string; className?: string }) {
  const active = to === current;
  const cls = [className, active ? "current" : ""].filter(Boolean).join(" ") || undefined;
  return (
    <a className={cls} aria-current={active ? "page" : undefined} href={hrefTo(current, to)}>
      {label}
    </a>
  );
}

function SubNav({ toc }: { toc: TocItem[] }) {
  const items = toc.filter((t) => t.level === 2);
  if (!items.length) return null;
  return (
    <div className="subnav">
      {items.map((t) => (
        <a key={t.anchor} href={`#${t.anchor}`} data-spy={t.anchor}>
          {t.text}
        </a>
      ))}
    </div>
  );
}

/**
 * Docs navigation: paper for the CLI half, ink for the library half. With
 * `toc`, the current page's headings are listed under it (scroll-spy).
 */
export function Sidebar({ site, current, toc = [] }: { site: Site; current: string; toc?: TocItem[] }) {
  if (isLibrarySlug(current)) return <LibrarySidebar site={site} current={current} toc={toc} />;
  return (
    <nav className="docnav" aria-label="Docs">
      {site.sections.map((section) => (
        <Fragment key={section.title}>
          <h2>{section.title}</h2>
          {section.pages.map((spec) => (
            <Fragment key={spec.slug}>
              <NavLink current={current} to={spec.slug} label={spec.label} />
              {spec.slug === current ? <SubNav toc={toc} /> : null}
            </Fragment>
          ))}
          {section.commandGroups.map((group) => {
            const cmds = site.commandPages.filter((p) => p.group === group);
            if (!cmds.length) return null;
            return (
              <Fragment key={group}>
                <h3>{group}</h3>
                {cmds.map((p) => (
                  <NavLink key={p.slug} current={current} to={p.slug} label={p.title} className="cmd" />
                ))}
              </Fragment>
            );
          })}
          {section.directory ? <a href={hrefTo(current, LIBRARY_SLUG)}>grit-lib API</a> : null}
        </Fragment>
      ))}
    </nav>
  );
}

function LibrarySidebar({ site, current, toc }: { site: Site; current: string; toc: TocItem[] }) {
  const titles = new Map(site.pages.map((p) => [p.slug, p.title]));
  return (
    <nav className="docnav ink" aria-label="Library docs">
      <a className="label" href={hrefTo(current, LIBRARY_SLUG)}>
        use grit_lib;
      </a>
      <NavLink current={current} to={LIBRARY_QUICKSTART_SLUG} label={titles.get(LIBRARY_QUICKSTART_SLUG) ?? "Library quick start"} />
      {current === LIBRARY_QUICKSTART_SLUG ? <SubNav toc={toc} /> : null}
      <h3>Guides</h3>
      {librarySlugs(site).map((slug) => (
        <NavLink key={slug} current={current} to={slug} label={slug === LIBRARY_SLUG ? "Overview" : titles.get(slug)!} />
      ))}
      <a className="ext" href={DOCS_RS}>
        API reference ↗
      </a>
    </nav>
  );
}

export function Breadcrumb({ parts }: { parts: string[] }) {
  return (
    <div className="crumbs">
      <span>{parts.filter(Boolean).join(" / ")}</span>
      <a href="index.md">Markdown</a>
    </div>
  );
}

export function Pager({ site, slug }: { site: Site; slug: string }) {
  const i = site.pages.findIndex((p) => p.slug === slug);
  if (i < 0) return null;
  const prev = site.pages[i - 1];
  const next = site.pages[i + 1];
  return (
    <div className="pager">
      {prev ? <a href={hrefTo(slug, prev.slug)}>← {prev.title}</a> : <a href={hrefTo(slug, "index")}>← Overview</a>}
      {next ? <a href={hrefTo(slug, next.slug)}>{next.title} →</a> : <span />}
    </div>
  );
}

export function Html({ html, className, as: Tag = "div", id }: { html: string; className?: string; as?: "div" | "article" | "p" | "span"; id?: string }) {
  return <Tag className={className} id={id} dangerouslySetInnerHTML={{ __html: html }} />;
}
