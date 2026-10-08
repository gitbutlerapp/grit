#!/usr/bin/env python3
"""Generate the static Grit docs site from Markdown and content/docs/site.toml.

Sources live in content/docs/:

- site.toml    section order, page list, and validation rules
- index.md, tutorial.md, install.md, …  guides and CLI topics
- commands/    one man page per `grit` command
- library/     grit-lib usage guides

Output goes to docs/docs/, served at https://grit-scm.com/docs/. Command pages
keep their existing URLs (/docs/<command>/). New sections use /docs/library/<page>/
and /docs/benchmarks/. Page chrome and Markdown rendering are shared with
scripts/blog.py.
"""
from __future__ import annotations

import argparse
import html
import re
import shutil
import sys
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import benchpage  # noqa: E402
import blog  # noqa: E402
import rustdoc_links  # noqa: E402
import site_util  # noqa: E402

ROOT = blog.ROOT
DOC_ROOT = ROOT / "target" / "doc"
CONTENT_DIR = ROOT / "content" / "docs"
MANIFEST_PATH = CONTENT_DIR / "site.toml"
OUT_DIR = ROOT / "docs" / "docs"
SITE_TITLE = "Grit docs"
DESCRIPTION = "How to use grit, a simple Git client built on grit-lib: a short tutorial and a man page for every command."
LIBRARY_GUIDE_SLUG = "library"
TOC_MIN_HEADINGS = 2
INCLUDE_RE = re.compile(r"<!--\s*include:\s*(\S+)\s*-->")
MARKDOWN_LINK_RE = re.compile(r"\[([^\]]*)\]\(([^)]+)\)")
HTML_CHROME_MARKERS = (
    "<nav",
    'class="docnav"',
    'class="pager"',
    'class="toc"',
    "<aside",
    "<table class=\"cmds\"",
)


def expand_includes(body: str) -> str:
    """Replace ``<!-- include: path -->`` with a fenced copy of that file."""

    def replace(match: re.Match[str]) -> str:
        rel = match.group(1)
        source = (ROOT / rel).resolve()
        try:
            source.relative_to(ROOT.resolve())
        except ValueError as err:
            raise SystemExit(f"include path escapes repository: {rel}") from err
        if not source.is_file():
            raise SystemExit(f"include missing file: {rel}")
        text = source.read_text(encoding="utf-8").rstrip()
        lang = "rust" if source.suffix == ".rs" else ""
        fence = f"```{lang}\n{text}\n```"
        return fence

    return INCLUDE_RE.sub(replace, body)


@dataclass(frozen=True)
class PageSpec:
    """One page entry from the site manifest."""

    file: str
    slug: str
    label: str
    section_title: str

    def source_at(self, content_dir: Path) -> Path:
        return content_dir / self.file


@dataclass(frozen=True)
class SectionSpec:
    title: str
    pages: tuple[PageSpec, ...] = ()
    command_groups: tuple[str, ...] = ()
    directory: str | None = None


@dataclass(frozen=True)
class Page:
    slug: str
    title: str
    summary: str
    section_title: str
    group: str
    order: int
    body_html: str
    toc: tuple[blog.TocItem, ...]
    is_command: bool = False


@dataclass
class Site:
    sections: list[SectionSpec]
    pages: list[Page]
    command_pages: list[Page]

    @property
    def pager_order(self) -> list[Page]:
        return self.pages


def doc_depth(slug: str) -> int:
    if slug == "index":
        return 0
    return len(slug.split("/"))


def href_to(from_slug: str, to_slug: str) -> str:
    ups = "../" * doc_depth(from_slug)
    if to_slug == "index":
        return ups or "./"
    return f"{ups}{to_slug}/"


def output_path(out_dir: Path, slug: str) -> Path:
    if slug == "index":
        return out_dir / "index.html"
    parts = slug.split("/")
    return out_dir.joinpath(*parts, "index.html")


def markdown_output_path(out_dir: Path, slug: str) -> Path:
    if slug == "index":
        return out_dir / "index.md"
    parts = slug.split("/")
    return out_dir.joinpath(*parts, "index.md")


def markdown_canonical_url(slug: str) -> str:
    base = f"{blog.SITE_URL}/docs"
    if slug == "index":
        return f"{base}/index.md"
    return f"{base}/{slug}/index.md"


def relative_link_to_slug(from_slug: str, url_path: str) -> str:
    """Map a relative docs link path to a page slug."""
    parts: list[str] = [] if from_slug == "index" else from_slug.split("/")
    for segment in url_path.replace("\\", "/").split("/"):
        if segment in ("", "."):
            continue
        if segment == "..":
            if parts:
                parts.pop()
            continue
        parts.append(segment)
    return "/".join(parts) if parts else "index"


def rewrite_markdown_links(body: str, from_slug: str) -> str:
    """Rewrite internal relative links to absolute grit-scm.com Markdown URLs."""

    def replace(match: re.Match[str]) -> str:
        text, url = match.group(1), match.group(2).strip()
        fragment = ""
        path = url
        if "#" in url:
            path, frag = url.split("#", 1)
            fragment = f"#{frag}" if frag else ""
        if not path:
            return match.group(0)
        lowered = path.lower()
        if lowered.startswith(("http://", "https://", "mailto:", "tel:")):
            return match.group(0)
        if path.startswith("#"):
            return match.group(0)
        target_slug = relative_link_to_slug(from_slug, path)
        return f"[{text}]({markdown_canonical_url(target_slug)}{fragment})"

    return MARKDOWN_LINK_RE.sub(replace, body)


def load_manifest() -> list[SectionSpec]:
    return load_manifest_from(MANIFEST_PATH, CONTENT_DIR)


def load_manifest_from(manifest_path: Path, content_dir: Path) -> list[SectionSpec]:
    if not manifest_path.is_file():
        raise SystemExit(f"missing site manifest {manifest_path}")
    data = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    sections: list[SectionSpec] = []
    for raw in data.get("section", []):
        title = raw["title"]
        pages: list[PageSpec] = []
        for entry in raw.get("page", []):
            rel = entry["file"]
            pages.append(
                PageSpec(
                    file=rel,
                    slug=entry.get("slug") or Path(rel).stem,
                    label=entry.get("label") or entry.get("slug") or Path(rel).stem,
                    section_title=title,
                )
            )
        command_groups: tuple[str, ...] = ()
        if "commands" in raw:
            command_groups = tuple(raw["commands"]["groups"])
        directory = raw.get("directory")
        sections.append(
            SectionSpec(
                title=title,
                pages=tuple(pages),
                command_groups=command_groups,
                directory=directory,
            )
        )
    return sections


def validate_manifest(sections: list[SectionSpec], *, content_dir: Path | None = None) -> None:
    """Fail if a manifest page is missing or a Markdown file is not listed."""
    root = content_dir or CONTENT_DIR
    manifest = root / "site.toml"
    if not manifest.is_file():
        raise SystemExit(f"missing site manifest {manifest}")

    listed = collect_listed_sources_for_dir(sections, root)

    for spec in listed.values():
        source = spec.source_at(root)
        if not source.is_file():
            raise SystemExit(f"site.toml lists missing page {spec.file}")

    for path in sorted(root.rglob("*.md")):
        if path.name.upper() == "README.MD" and path.parent.name == "commands":
            continue
        if path.resolve() not in listed:
            raise SystemExit(f"Markdown file not listed in site.toml: {path.relative_to(root)}")


def collect_listed_sources_for_dir(sections: list[SectionSpec], root: Path) -> dict[Path, PageSpec]:
    listed: dict[Path, PageSpec] = {}
    for section in sections:
        for page in section.pages:
            source = page.source_at(root)
            listed[source.resolve()] = page
        if section.directory:
            lib_dir = root / section.directory
            for path in sorted(lib_dir.glob("*.md")):
                rel = path.relative_to(root).as_posix()
                if path.name == "index.md":
                    slug = section.directory
                    label = "Overview"
                else:
                    slug = f"{section.directory}/{path.stem}"
                    label = path.stem.replace("-", " ").title()
                listed[path.resolve()] = PageSpec(rel, slug, label, section.title)
        if section.command_groups:
            for path in sorted((root / "commands").glob("*.md")):
                if path.name.upper() == "README.MD":
                    continue
                rel = path.relative_to(root).as_posix()
                listed[path.resolve()] = PageSpec(rel, path.stem, path.stem, section.title)
    return listed


def prepare_markdown_body(body: str) -> str:
    """Expand includes and ``rustdoc:`` links for Markdown body text."""
    body = expand_includes(body)
    rustdoc_links.ensure_local_rustdoc(DOC_ROOT, repo_root=ROOT)
    return rustdoc_links.expand_rustdoc_links(body, doc_root=DOC_ROOT)


def load_page(
    path: Path,
    spec: PageSpec,
    *,
    command_groups: tuple[str, ...],
    manifest_path: Path,
) -> Page:
    meta, body = blog.parse_front_matter(path.read_text(encoding="utf-8"))
    title = meta.get("title") or spec.label
    group = meta.get("group", "")
    is_command = path.parent.name == "commands"
    if is_command and group not in command_groups:
        raise SystemExit(f"{path}: group {group!r} must be one of {command_groups}")
    if spec.slug == "benchmarks":
        body = expand_includes(body)
        before, after = benchpage.split_benchmark_markdown(body)
        before_html, toc_before = blog.markdown_to_html(before)
        after_html, toc_after = blog.markdown_to_html(after)
        tables = benchpage.benchmark_html_for_manifest(manifest_path)
        body_html = before_html + tables + after_html
        toc = toc_before + toc_after
    else:
        body = prepare_markdown_body(body)
        body_html, toc = blog.markdown_to_html(body)
    return Page(
        spec.slug,
        title,
        meta.get("summary", ""),
        spec.section_title,
        group,
        int(meta.get("order", "0")),
        body_html,
        tuple(toc),
        is_command=is_command,
    )


def load_site(*, content_dir: Path | None = None) -> Site:
    root = content_dir or CONTENT_DIR
    manifest_path = root / "site.toml"
    sections = load_manifest_from(manifest_path, root)
    validate_manifest(sections, content_dir=root)
    listed = collect_listed_sources_for_dir(sections, root)
    command_groups: tuple[str, ...] = ()
    for section in sections:
        if section.command_groups:
            command_groups = section.command_groups
            break
    if not command_groups:
        raise SystemExit(f"{manifest_path}: no [section.commands] block with groups")

    by_slug: dict[str, Page] = {}
    for spec in listed.values():
        page = load_page(
            spec.source_at(root),
            spec,
            command_groups=command_groups,
            manifest_path=manifest_path,
        )
        by_slug[page.slug] = page

    ordered_pages: list[Page] = []
    command_pages: list[Page] = []

    for section in sections:
        for spec in section.pages:
            ordered_pages.append(by_slug[spec.slug])
        if section.command_groups:
            commands = [p for p in by_slug.values() if p.is_command]
            commands.sort(key=lambda p: (command_groups.index(p.group), p.order, p.slug))
            command_pages = commands
            ordered_pages.extend(commands)
        if section.directory:
            lib_slugs = sorted(
                slug
                for slug in by_slug
                if slug == section.directory or slug.startswith(f"{section.directory}/")
            )
            for slug in lib_slugs:
                ordered_pages.append(by_slug[slug])

    return Site(sections, ordered_pages, command_pages)


def section_landing_href(section: SectionSpec) -> str | None:
    if section.pages:
        return section.pages[0].slug
    if section.directory:
        return section.directory
    if section.command_groups and section.pages:
        return section.pages[0].slug
    return None


def sidebar(site: Site, current: str) -> str:
    def nav_link(to_slug: str, label: str) -> str:
        cls = ' class="current"' if to_slug == current else ""
        return f'<li><a{cls} href="{href_to(current, to_slug)}">{html.escape(label)}</a></li>'

    parts = ['<nav class="docnav" aria-label="Docs">']
    for section in site.sections:
        landing = section_landing_href(section)
        if landing:
            heading = (
                f'<h2><a href="{href_to(current, landing)}">{html.escape(section.title)}</a></h2>'
            )
        else:
            heading = f"<h2>{html.escape(section.title)}</h2>"
        blocks: list[str] = []
        if section.pages:
            page_links = []
            for spec in section.pages:
                label = spec.label
                page_links.append(nav_link(spec.slug, label))
            blocks.append(f"<ol>{''.join(page_links)}</ol>")
        if section.command_groups:
            for group in section.command_groups:
                group_cmds = [p for p in site.command_pages if p.group == group]
                if not group_cmds:
                    continue
                cmd_items = "".join(nav_link(p.slug, p.title) for p in group_cmds)
                blocks.append(f"<h3>{html.escape(group)}</h3><ol>{cmd_items}</ol>")
        if section.directory:
            lib_links = []
            lib_slugs = sorted(
                p.slug
                for p in site.pages
                if p.slug == section.directory or p.slug.startswith(f"{section.directory}/")
            )
            for slug in lib_slugs:
                page = next(p for p in site.pages if p.slug == slug)
                label = "Overview" if slug == section.directory else page.title
                lib_links.append(nav_link(slug, label))
            lib_links.append(
                f'<li><a href="{href_to(current, LIBRARY_GUIDE_SLUG)}">grit-lib API</a></li>'
            )
            blocks.append(f"<ol>{''.join(lib_links)}</ol>")
        parts.append(heading + "".join(blocks))
    parts.append("</nav>")
    return "".join(parts)


def command_index_markdown(site: Site) -> str:
    command_groups: tuple[str, ...] = ()
    for section in site.sections:
        if section.command_groups:
            command_groups = section.command_groups
            break
    lines = ["## Commands", ""]
    for group in command_groups:
        rows = [p for p in site.command_pages if p.group == group]
        if not rows:
            continue
        lines.append(f"### {group}")
        lines.append("")
        lines.append("| Command | Summary |")
        lines.append("| --- | --- |")
        for page in rows:
            url = markdown_canonical_url(page.slug)
            lines.append(f"| [`{page.title}`]({url}) | {page.summary} |")
        lines.append("")
    return "\n".join(lines).rstrip() + "\n"


def command_index(site: Site, from_slug: str) -> str:
    sections = []
    command_groups: tuple[str, ...] = ()
    for section in site.sections:
        if section.command_groups:
            command_groups = section.command_groups
            break
    for group in command_groups:
        rows = "".join(
            f'<tr><td><a href="{href_to(from_slug, p.slug)}"><code>{html.escape(p.title)}</code></a></td>'
            f"<td>{html.escape(p.summary)}</td></tr>"
            for p in site.command_pages
            if p.group == group
        )
        if rows:
            anchor = blog.slugify(group)
            sections.append(
                f'<h3 id="{anchor}">{html.escape(group)}</h3>'
                f'<div class="table"><table class="cmds"><tbody>{rows}</tbody></table></div>'
            )
    return '<h2 id="commands">Commands</h2>' + "".join(sections)


def pager(site: Site, current: str) -> str:
    order = site.pager_order
    slugs = [p.slug for p in order]
    titles = {p.slug: p.title for p in order}
    if current not in slugs:
        return ""
    i = slugs.index(current)
    prev_slug = slugs[i - 1] if i > 0 else None
    next_slug = slugs[i + 1] if i + 1 < len(slugs) else None
    prev_link = (
        f'<a href="{href_to(current, prev_slug)}">← {html.escape(titles[prev_slug])}</a>'
        if prev_slug
        else f'<a href="{href_to(current, "index")}">← Overview</a>'
    )
    next_link = (
        f'<a href="{href_to(current, next_slug)}">{html.escape(titles[next_slug])} →</a>'
        if next_slug
        else "<span></span>"
    )
    return f'<div class="pager">{prev_link}{next_link}</div>'


def page_toc(toc: tuple[blog.TocItem, ...]) -> str:
    if len(toc) < TOC_MIN_HEADINGS:
        return ""
    items = "".join(
        f'<li class="toc-level-{item.level}"><a href="#{item.anchor}">{html.escape(item.text)}</a></li>'
        for item in toc
    )
    return f'<aside class="toc" aria-label="On this page"><h2>On this page</h2><ol>{items}</ol></aside>'


def shell(title: str, description: str, body: str, base: str, *, extra_head: str = "") -> str:
    home = f"{base}/"
    library_href = f"{base}/docs/library/"
    head_extra = extra_head
    if head_extra and not head_extra.endswith("\n"):
        head_extra += "\n"
    return f"""<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\" />
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />
<title>{html.escape(title)}</title>
<meta name=\"description\" content=\"{html.escape(description, quote=True)}\" />
{head_extra}<script async src=\"https://u.gitbutler.com/script.js\" data-website-id=\"2c6f680c-eaf5-4cd7-a419-1032ffab6bbc\"></script>
{blog.FONTS}
<style>{blog.CSS}{CSS}</style>
</head>
<body>
<div class=\"wrap\">
<header class=\"topbar\">
  <a class=\"brand\" href=\"{home}\" aria-label=\"grit homepage\">grit</a>
  <nav class=\"nav\" aria-label=\"Primary\">
    <a href=\"{base}/docs/\">Docs</a>
    <a href=\"{library_href}\">Library</a>
    <a href=\"{base}/blog/\">Blog</a>
    <a class=\"pill\" href=\"https://github.com/gitbutlerapp/grit\">GitHub</a>
  </nav>
</header>
{body}
</div>
</body>
</html>
"""


def render(page: Page, site: Site, *, is_index: bool) -> str:
    if is_index:
        base = ".."
    elif doc_depth(page.slug) == 1:
        base = "../.."
    else:
        base = "../" * (doc_depth(page.slug) + 1)
    docs_home = href_to(page.slug, "index")
    markdown_href = "index.md"
    ref = (
        f'<span class="ref">HEAD → docs</span><a class="ref" href="{markdown_href}">Markdown</a>'
        if is_index
        else (
            f'<a class="ref" href="{docs_home}">← docs</a>'
            f'<span>{"man page" if page.is_command else "guide"}</span>'
            f'<a class="ref" href="{markdown_href}">Markdown</a>'
        )
    )
    h1_class = ' class="cmd"' if page.is_command else ""
    lede = f'<p class="lede">{html.escape(page.summary)}</p>' if page.summary else ""
    extra = command_index(site, page.slug) if is_index else pager(site, page.slug)
    content = page.body_html + extra
    toc_aside = page_toc(page.toc)
    body_class = "doc-body has-toc" if toc_aside else "doc-body"
    hero_class = "hero" if is_index else "hero post-hero"
    links = (
        f'<a href="{docs_home}">Docs</a>'
        f'<a href="{base}/">Home</a>'
        f'<a href="{base}/blog/">Blog</a>'
        f'<a href="https://github.com/gitbutlerapp/grit">GitHub</a>'
    )
    body = f"""<main>
<section class=\"commit\">
  {blog.rail("line from-head", "head-dot")}
  <div class=\"{hero_class}\">
    <div class=\"refs\">{ref}</div>
    <h1{h1_class}>{html.escape(page.title)}</h1>
    {lede}
  </div>
</section>
<section class=\"commit\">
  {blog.rail("line")}
  <div class=\"{body_class}\">
    <article class=\"content\">{content}</article>
    {sidebar(site, page.slug)}
    {toc_aside}
  </div>
</section>
</main>
{blog.footer(links)}"""
    title = SITE_TITLE if is_index else f"{page.title} - {SITE_TITLE}"
    alternate = '<link rel="alternate" type="text/markdown" href="index.md" />'
    return shell(title, page.summary or DESCRIPTION, body, base, extra_head=alternate)


def page_markdown_body(
    page: Page,
    site: Site,
    *,
    source: Path,
    manifest_path: Path,
    is_index: bool,
) -> str:
    _meta, body = blog.parse_front_matter(source.read_text(encoding="utf-8"))
    if page.slug == "benchmarks":
        body = expand_includes(body)
        before, after = benchpage.split_benchmark_markdown(body)
        before = prepare_markdown_body(before)
        after = prepare_markdown_body(after)
        tables = benchpage.benchmark_markdown_for_manifest(manifest_path)
        body = before + "\n\n" + tables + "\n" + after
    else:
        body = prepare_markdown_body(body)
    body = rewrite_markdown_links(body, page.slug)
    parts = [f"# {page.title}"]
    if page.summary:
        parts.append(f"> {page.summary}")
    parts.append(body.rstrip())
    if is_index:
        parts.append(command_index_markdown(site).rstrip())
    return "\n\n".join(parts) + "\n"


def render_markdown_twin(
    page: Page,
    site: Site,
    *,
    source: Path,
    manifest_path: Path,
    is_index: bool,
) -> str:
    return page_markdown_body(
        page,
        site,
        source=source,
        manifest_path=manifest_path,
        is_index=is_index,
    )


CSS = r'''
.doc-body{display:grid;grid-template-columns:180px minmax(0,720px);gap:56px;padding:48px 0 64px}
.doc-body.has-toc{grid-template-columns:180px minmax(0,720px) 180px}
.post-hero h1.cmd{font-family:var(--mono);font-weight:500;letter-spacing:-.04em;max-width:none}
.docnav{order:-1;position:sticky;top:24px;align-self:start;font:13px/1.5 var(--mono);color:var(--muted);max-height:calc(100vh - 48px);overflow-y:auto}
.docnav h2{margin:20px 0 8px;font:inherit;color:var(--accent)}
.docnav h2 a{color:inherit;text-decoration:none}
.docnav h3{margin:16px 0 6px 14px;font:inherit;color:var(--muted);font-size:12px}
.docnav ol{list-style:none;margin:0;padding:0;border-left:1px solid var(--line)}
.docnav li{margin:0 0 6px;padding-left:14px}.docnav a{text-decoration:none}
.docnav a.current{color:var(--ink);font-weight:700}
.toc{position:sticky;top:24px;align-self:start;font:13px/1.5 var(--mono);color:var(--muted)}
.toc h2{margin:0 0 12px;font:inherit;color:var(--accent)}
.toc ol{list-style:none;margin:0;padding:0;border-left:1px solid var(--line)}
.toc li{margin:0 0 8px;padding-left:14px}.toc a{text-decoration:none}.toc-level-3{padding-left:28px}
.content h3 code{font-size:.9em}
.content table.cmds{table-layout:fixed}.content table.cmds td:first-child{width:210px}
.pager{display:flex;justify-content:space-between;gap:16px;margin-top:3em;padding-top:1.4em;border-top:1px solid var(--line);font:15px var(--mono)}
.pager a{text-decoration:none;color:var(--ink)}
.content table.bench-results,.content table.bench-summary,.content table.bench-env{font:14px/1.5 var(--mono)}
.content table.bench-results td,.content table.bench-results th,.content table.bench-summary td,.content table.bench-summary th{text-align:right}
.content table.bench-results td:first-child,.content table.bench-results th:first-child,.content table.bench-summary td:first-child,.content table.bench-summary th:first-child{text-align:left}
.content tr.bench-slow td{color:var(--accent);font-weight:600}
.bench-meta{margin:1.5em 0 2em}
@media(max-width:900px){.doc-body{display:flex;flex-direction:column}.doc-body.has-toc{display:flex}.docnav{order:1;position:static;max-height:none;border-top:1px solid var(--line);padding-top:24px}.toc{display:none}}
'''


def generate(out_dir: Path, *, content_dir: Path | None = None) -> None:
    rustdoc_links.ensure_local_rustdoc(DOC_ROOT, repo_root=ROOT)
    root = content_dir or CONTENT_DIR
    manifest_path = root / "site.toml"
    site = load_site(content_dir=content_dir)
    listed = collect_listed_sources_for_dir(load_manifest_from(manifest_path, root), root)
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)
    all_pages = site.pager_order
    for page in all_pages:
        path = output_path(out_dir, page.slug)
        path.parent.mkdir(parents=True, exist_ok=True)
        is_index = page.slug == "index"
        path.write_text(render(page, site, is_index=is_index), encoding="utf-8")
        spec = next(s for s in listed.values() if s.slug == page.slug)
        md_path = markdown_output_path(out_dir, page.slug)
        md_path.write_text(
            render_markdown_twin(
                page,
                site,
                source=spec.source_at(root),
                manifest_path=manifest_path,
                is_index=is_index,
            ),
            encoding="utf-8",
        )


def check_committed() -> int:
    rustdoc_links.ensure_local_rustdoc(DOC_ROOT, repo_root=ROOT)
    rustdoc_links.validate_rustdoc_links(CONTENT_DIR, DOC_ROOT)
    with tempfile.TemporaryDirectory(prefix="grit-docs-check-") as tmp:
        generated = Path(tmp) / "docs"
        generate(generated)
        issues = site_util.compare_directories(generated, OUT_DIR, label="docs")
        if issues:
            for line in issues:
                print(line, file=sys.stderr)
            print("docs output is stale; run: make docs", file=sys.stderr)
            return 1
    print(f"docs output is up to date ({OUT_DIR.relative_to(ROOT)})")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="Render to a temp directory and fail if docs/docs/ differs from committed output.",
    )
    args = parser.parse_args(argv)
    if args.check:
        return check_committed()
    generate(OUT_DIR)
    site = load_site()
    print(
        f"generated {len(site.pager_order)} doc page(s) "
        f"({len(site.command_pages)} command(s)) in {OUT_DIR.relative_to(ROOT)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
