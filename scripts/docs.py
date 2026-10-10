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
import json
import re
import shutil
import sys
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import apimap  # noqa: E402
import benchpage  # noqa: E402
import blog  # noqa: E402
import rustdoc_links  # noqa: E402
import site_util  # noqa: E402

ROOT = blog.ROOT
DOC_ROOT = ROOT / "target" / "doc"
CONTENT_DIR = ROOT / "content" / "docs"
MANIFEST_PATH = CONTENT_DIR / "site.toml"
OUT_DIR = ROOT / "docs" / "docs"
LLMS_DIR = ROOT / "docs"
LLMS_TXT_PATH = LLMS_DIR / "llms.txt"
LLMS_FULL_TXT_PATH = LLMS_DIR / "llms-full.txt"
SITE_TITLE = "Grit docs"
DESCRIPTION = "How to use grit, a simple Git client built on grit-lib: a short tutorial and a man page for every command."
LLMS_TAGLINE = (
    "Grit is a fast Git implementation in Rust: grit-lib is a linkable library for Rust "
    "programs, and grit is a modern Git client built on that library. On-disk formats and "
    "the wire protocol stay compatible with Git."
)
LLMS_USAGE = (
    "Each docs page has a Markdown twin at the same URL with `index.md` instead of `index.html`. "
    "Use `grit --json` (and `--markdown` on many commands) for script- and agent-friendly CLI output."
)
LIBRARY_GUIDE_SLUG = "library"
LIBRARY_QUICKSTART_SLUG = "library-quickstart"
# Guides laid out as steps, each H2's code beside its prose (screen 2b).
STEP_GUIDE_SLUGS = frozenset({"tutorial", "install", "scripting", "agents", LIBRARY_QUICKSTART_SLUG})
# Step guides move only short lead-ins ("On macOS and Linux:") next to their code.
STEP_CAPTION_MAX = 60
GITHUB_URL = "https://github.com/gitbutlerapp/grit"
GITHUB_BLOB = f"{GITHUB_URL}/blob/main"
# One Markdown file per half of the docs, for agents that want a whole topic at once.
CLI_BUNDLE = "grit-cli.md"
LIB_BUNDLE = "grit-lib.md"
CLI_BUNDLE_INTRO = (
    "Everything an agent needs to use `grit` as a Git client: install, the tutorial, global "
    "options, scripting with `--json` and `--filter`, the agent guide, and the reference page "
    "for every command. `grit` works on any Git repository and talks to any Git remote."
)
LIB_BUNDLE_INTRO = (
    "Everything an agent needs to write Git-compatible Rust programs with `grit-lib`: the "
    "quick start, every library guide with its compiled example, and the generated API map. "
    "On-disk formats and the wire protocol match Git, so repositories you write work with "
    "`git` and any Git host. Full API reference: https://docs.rs/grit-lib"
)
OVERVIEW_TILE_LABELS = {"global-options": "flags", "scripting": "--json / --filter", "agents": "automation"}
OVERVIEW_CLI_LEDE = (
    "Works on any Git repository and talks to any remote. Plain-language output and "
    "<code>--json</code> on every command."
)
OVERVIEW_LIB_LEDE = (
    "A fast, linkable Git library for Rust. Everything the CLI does goes through it: open "
    "repositories, read objects and refs, diff, revwalk, fetch and push."
)
API_MAP_SLUG = "library/api-map"
DOCS_RS_GRIT_LIB = "https://docs.rs/grit-lib"
BLOG_INDEX_URL = f"{blog.SITE_URL}/blog/"
TOC_MIN_HEADINGS = 2
INCLUDE_RE = re.compile(r"<!--\s*include:\s*(\S+)\s*-->")
HTML_CHROME_MARKERS = (
    "<nav",
    'class="docnav"',
    'class="pager"',
    'class="toc"',
    "<aside",
    "<table class=\"cmds\"",
)
FENCE_LINE = re.compile(r"^(?P<ticks>`{3,})(?P<info>\S*)?\s*$")


def validate_fence_languages(*, content_dir: Path | None = None) -> None:
    """Fail if any Markdown code fence opens without a language tag."""
    root = content_dir or CONTENT_DIR
    for path in sorted(root.rglob("*.md")):
        in_code = False
        rel = path.relative_to(root)
        for line_no, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
            stripped = line.strip()
            if not stripped.startswith("`"):
                continue
            match = FENCE_LINE.match(stripped)
            if not match:
                continue
            info = match.group("info") or ""
            if not in_code:
                if not info:
                    raise SystemExit(
                        f"{rel}:{line_no}: code fence missing language tag "
                        "(use console, text, json, rust, toml, bash, or another info string)"
                    )
                in_code = True
            else:
                in_code = False


def expand_includes(body: str, *, tag_source: bool = False) -> str:
    """Replace ``<!-- include: path -->`` with a fenced copy of that file.

    With ``tag_source``, a Rust fence's info string becomes ``rust:include=<path>``
    so the HTML layout can place the example in its own column and link to the
    full source. Markdown twins use plain ``rust`` fences.
    """

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
        if lang and tag_source:
            lang = f"{lang}:include={rel}"
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

    def url_for_path(path: str) -> str | None:
        lowered = path.lower()
        if lowered.startswith(("http://", "https://", "mailto:", "tel:")):
            return None
        if path.startswith("#"):
            return None
        target_slug = relative_link_to_slug(from_slug, path)
        return markdown_canonical_url(target_slug)

    return site_util.rewrite_markdown_links(body, url_for_path)


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


def prepare_markdown_body(body: str, *, tag_includes: bool = False) -> str:
    """Expand includes and ``rustdoc:`` links for Markdown body text."""
    body = expand_includes(body, tag_source=tag_includes)
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
    elif spec.slug == API_MAP_SLUG:
        body = expand_includes(body)
        before, after = apimap.split_api_map_markdown(body)
        before_html, toc_before = blog.markdown_to_html(before)
        after_html, toc_after = blog.markdown_to_html(after)
        table_md = apimap.api_map_markdown(DOC_ROOT)
        table_html, toc_table = blog.markdown_to_html(table_md)
        body_html = before_html + table_html + after_html
        toc = toc_before + toc_table + toc_after
    else:
        body = prepare_markdown_body(body, tag_includes=True)
        body_html, toc = blog.markdown_to_html(body, code_lang=True)
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
    validate_fence_languages(content_dir=root)
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


def is_library_slug(slug: str) -> bool:
    """True for pages in the library half of the docs (ink navigation)."""
    return (
        slug == LIBRARY_QUICKSTART_SLUG
        or slug == LIBRARY_GUIDE_SLUG
        or slug.startswith(f"{LIBRARY_GUIDE_SLUG}/")
    )


def library_slugs(site: Site) -> list[str]:
    """Library guide slugs in nav order: the overview first, then by slug."""
    slugs = sorted(
        p.slug
        for p in site.pages
        if p.slug == LIBRARY_GUIDE_SLUG or p.slug.startswith(f"{LIBRARY_GUIDE_SLUG}/")
    )
    return sorted(slugs, key=lambda slug: slug != LIBRARY_GUIDE_SLUG)


def nav_link(current: str, to_slug: str, label: str, *, cls: str = "") -> str:
    classes = " ".join(c for c in (cls, "current" if to_slug == current else "") if c)
    attrs = f' class="{classes}"' if classes else ""
    if to_slug == current:
        attrs += ' aria-current="page"'
    return f'<a{attrs} href="{href_to(current, to_slug)}">{html.escape(label)}</a>'


def toc_sublist(toc: tuple[blog.TocItem, ...]) -> str:
    """H2 links under the current page; the scroll-spy script marks the active one."""
    items = "".join(
        f'<a href="#{item.anchor}" data-spy="{item.anchor}">{html.escape(item.text)}</a>'
        for item in toc
        if item.level == 2
    )
    return f'<div class="subnav">{items}</div>' if items else ""


def sidebar(site: Site, current: str, *, toc: tuple[blog.TocItem, ...] = ()) -> str:
    """Docs navigation: paper for the CLI half, ink for the library half.

    ``toc`` lists the current page's headings; when given they're shown as a
    sub-list under the current page.
    """
    if is_library_slug(current):
        return library_sidebar(site, current, toc=toc)
    parts = ['<nav class="docnav" aria-label="Docs">']
    for section in site.sections:
        parts.append(f"<h2>{html.escape(section.title)}</h2>")
        for spec in section.pages:
            parts.append(nav_link(current, spec.slug, spec.label))
            if spec.slug == current and toc:
                parts.append(toc_sublist(toc))
        for group in section.command_groups:
            group_cmds = [p for p in site.command_pages if p.group == group]
            if not group_cmds:
                continue
            parts.append(f"<h3>{html.escape(group)}</h3>")
            parts.extend(nav_link(current, p.slug, p.title, cls="cmd") for p in group_cmds)
        if section.directory:
            parts.append(f'<a href="{href_to(current, LIBRARY_GUIDE_SLUG)}">grit-lib API</a>')
    parts.append("</nav>")
    return "".join(parts)


def library_sidebar(site: Site, current: str, *, toc: tuple[blog.TocItem, ...] = ()) -> str:
    titles = {p.slug: p.title for p in site.pages}
    parts = [
        '<nav class="docnav ink" aria-label="Library docs">',
        f'<a class="label" href="{href_to(current, LIBRARY_GUIDE_SLUG)}">use grit_lib;</a>',
        nav_link(current, LIBRARY_QUICKSTART_SLUG, titles.get(LIBRARY_QUICKSTART_SLUG, "Library quick start")),
    ]
    if current == LIBRARY_QUICKSTART_SLUG and toc:
        parts.append(toc_sublist(toc))
    parts.append("<h3>Guides</h3>")
    for slug in library_slugs(site):
        label = "Overview" if slug == LIBRARY_GUIDE_SLUG else titles[slug]
        parts.append(nav_link(current, slug, label))
    parts.append(f'<a class="ext" href="{DOCS_RS_GRIT_LIB}">API reference ↗</a>')
    parts.append("</nav>")
    return "".join(parts)


def llms_link_line(title: str, slug: str, summary: str) -> str:
    url = markdown_canonical_url(slug)
    detail = summary.strip()
    if detail:
        return f"- [{title}]({url}): {detail}"
    return f"- [{title}]({url})"


def render_llms_txt(site: Site) -> str:
    """Build ``docs/llms.txt`` following https://llmstxt.org/."""
    pages_by_slug = {page.slug: page for page in site.pages}
    lines = ["# Grit", "", f"> {LLMS_TAGLINE}", "", LLMS_USAGE, "", "## Whole sections", ""]
    lines.extend(f"- [{label}]({url}): {what[0].upper()}{what[1:]}." for label, url, what in agent_files()[:2])
    lines.append("")

    for section in site.sections:
        lines.append(f"## {section.title}")
        lines.append("")
        for spec in section.pages:
            page = pages_by_slug[spec.slug]
            lines.append(llms_link_line(page.title, page.slug, page.summary))
        if section.command_groups:
            for group in section.command_groups:
                group_cmds = [p for p in site.command_pages if p.group == group]
                if not group_cmds:
                    continue
                lines.append(f"### {group}")
                lines.append("")
                for page in group_cmds:
                    lines.append(llms_link_line(page.title, page.slug, page.summary))
        if section.directory:
            lib_slugs = sorted(
                slug
                for slug in pages_by_slug
                if slug == section.directory or slug.startswith(f"{section.directory}/")
            )
            for slug in lib_slugs:
                page = pages_by_slug[slug]
                label = "Overview" if slug == section.directory else page.title
                lines.append(llms_link_line(label, slug, page.summary))
        lines.append("")

    lines.extend(["## Optional", ""])
    lines.append(
        f"- [grit-lib on docs.rs]({DOCS_RS_GRIT_LIB}): Rust API reference generated from crate rustdoc."
    )
    for post in blog.load_posts():
        url = blog.post_markdown_url(post.slug)
        detail = post.summary.strip()
        if detail:
            lines.append(f"- [{post.title}]({url}): {detail}")
        else:
            lines.append(f"- [{post.title}]({url})")
    lines.append(f"- [Blog]({BLOG_INDEX_URL}): release notes and project updates.")
    lines.append("")
    return "\n".join(lines).rstrip() + "\n"


def bundle_url(name: str) -> str:
    return f"{blog.SITE_URL}/docs/{name}"


def agent_files() -> list[tuple[str, str, str]]:
    """``(label, url, what it holds)`` for the machine-readable docs entry points."""
    return [
        (CLI_BUNDLE, bundle_url(CLI_BUNDLE), "the whole CLI in one file, to learn grit for everyday Git work"),
        (LIB_BUNDLE, bundle_url(LIB_BUNDLE), "the whole library guide in one file, to build Git-compatible Rust programs"),
        ("llms.txt", f"{blog.SITE_URL}/llms.txt", "an index of every page with a one-line summary"),
        ("llms-full.txt", f"{blog.SITE_URL}/llms-full.txt", "every docs page in one file"),
    ]


def agents_markdown() -> str:
    """The "For agents" section of the docs overview's Markdown twin."""
    lines = ["## For agents", ""]
    lines.extend(f"- [{label}]({url}): {what}." for label, url, what in agent_files())
    lines.extend(
        [
            "",
            "Every page also has a Markdown twin: replace `index.html` with `index.md` in its URL. "
            "Run `grit skill` to print an agent skill for the CLI.",
        ]
    )
    return "\n".join(lines) + "\n"


def agents_panel() -> str:
    items = "".join(
        f'<li><a href="{url.removeprefix(f"{blog.SITE_URL}/docs/").replace(blog.SITE_URL, "..")}">{html.escape(label)}</a> '
        f"— {html.escape(what)}</li>"
        for label, url, what in agent_files()
    )
    return (
        '<section class="agents" aria-labelledby="for-agents"><h2 id="for-agents">For agents</h2>'
        f"<ul>{items}</ul><p>Every page has a Markdown twin: swap <code>index.html</code> for "
        "<code>index.md</code>. <code>grit skill</code> prints an agent skill for the CLI.</p></section>"
    )


def bundle_slugs(site: Site, *, library: bool) -> list[str]:
    """Pages in one half of the docs, in pager order. Benchmarks belong to neither."""
    return [
        p.slug
        for p in site.pager_order
        if p.slug != "benchmarks" and is_library_slug(p.slug) == library
    ]


def render_bundle(
    site: Site,
    *,
    library: bool,
    root: Path,
    manifest_path: Path,
    listed: dict[Path, PageSpec],
) -> str:
    """One Markdown file holding every page of the CLI or library half."""
    title = "grit-lib: the library guide" if library else "grit: the command line"
    intro = LIB_BUNDLE_INTRO if library else CLI_BUNDLE_INTRO
    pages = {p.slug: p for p in site.pages}
    chunks = [f"# {title}\n\n> {intro}\n\nEach section below is one docs page; its heading is the page's URL.\n"]
    for slug in bundle_slugs(site, library=library):
        page = pages[slug]
        spec = next(s for s in listed.values() if s.slug == slug)
        twin = render_markdown_twin(
            page, site, source=spec.source_at(root), manifest_path=manifest_path, is_index=slug == "index"
        )
        chunks.append(f"# {markdown_canonical_url(slug)}\n\n{twin.rstrip()}\n")
    return "\n".join(chunks)


def render_llms_full_txt(
    site: Site,
    *,
    root: Path,
    manifest_path: Path,
    listed: dict[Path, PageSpec],
) -> str:
    """Concatenate every Markdown twin in pager order with the page URL as a heading."""
    chunks: list[str] = []
    for page in site.pager_order:
        url = markdown_canonical_url(page.slug)
        spec = next(s for s in listed.values() if s.slug == page.slug)
        twin = render_markdown_twin(
            page,
            site,
            source=spec.source_at(root),
            manifest_path=manifest_path,
            is_index=page.slug == "index",
        )
        chunks.append(f"# {url}\n\n{twin.rstrip()}\n")
    return "\n".join(chunks)


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


def _tokens(text: str, pattern: re.Pattern[str], classify) -> str:
    """Escape ``text``, wrapping each ``pattern`` match in the span ``classify`` picks."""
    out: list[str] = []
    pos = 0
    for match in pattern.finditer(text):
        out.append(html.escape(text[pos : match.start()]))
        cls = classify(match)
        piece = html.escape(match.group(0))
        out.append(f'<span class="{cls}">{piece}</span>' if cls else piece)
        pos = match.end()
    out.append(html.escape(text[pos:]))
    return "".join(out)


RUST_TOKEN = re.compile(
    r"(?P<comment>//[^\n]*)"
    r"|(?P<char>'(?:\\.|[^'\\\n])')"
    r'|(?P<string>"(?:\\.|[^"\\])*")'
    r"|(?P<kw>\b(?:as|async|await|break|const|continue|crate|dyn|else|enum|false|fn|for|if|impl|in|let|loop|match|mod|move|mut|pub|ref|return|self|Self|static|struct|super|trait|true|type|unsafe|use|where|while)\b)"
)
JSON_KEY = re.compile(r'"(?:\\.|[^"\\])*"(?=\s*:)')


def highlight_rust(text: str) -> str:
    def classify(m: re.Match[str]) -> str:
        if m.group("comment"):
            return "c"
        if m.group("kw"):
            return "k"
        return ""

    return _tokens(text, RUST_TOKEN, classify)


def highlight_json(text: str) -> str:
    return _tokens(text, JSON_KEY, lambda _m: "k")


def _quote_state(line: str, quote: str | None) -> str | None:
    """Track an open shell quote across lines so multi-line arguments stay commands."""
    escaped = False
    for ch in line:
        if escaped:
            escaped = False
        elif ch == "\\" and quote != "'":
            escaped = True
        elif quote is None and ch in "\"'":
            quote = ch
        elif ch == quote:
            quote = None
    return quote


def highlight_console(text: str) -> str:
    """``$`` lines are commands (accent prompt); everything else is output."""
    out: list[str] = []
    quote: str | None = None
    continued = False
    for line in text.split("\n"):
        if quote or continued:
            out.append(f'<span class="cmd">{html.escape(line)}</span>')
        elif line.startswith("$"):
            out.append(f'<span class="cmd"><span class="k">$</span>{html.escape(line[1:])}</span>')
        else:
            out.append(f'<span class="out">{html.escape(line)}</span>')
            continue
        quote = _quote_state(line, quote)
        continued = quote is None and line.endswith("\\")
    return "\n".join(out)


def highlight_shell(text: str) -> str:
    """Shell scripts: every command gets a ``$`` prompt; comments are muted."""
    out: list[str] = []
    quote: str | None = None
    continued = False
    for line in text.split("\n"):
        if quote or continued:
            out.append(f'<span class="cmd">{html.escape(line)}</span>')
        elif not line.strip():
            out.append("")
            continue
        elif line.lstrip().startswith("#"):
            out.append(f'<span class="out">{html.escape(line)}</span>')
            continue
        else:
            out.append(f'<span class="cmd"><span class="k">$</span> {html.escape(line)}</span>')
        quote = _quote_state(line, quote)
        continued = quote is None and line.endswith("\\")
    return "\n".join(out)


PRE_RE = re.compile(r'<pre data-lang="(?P<lang>[^"]*)"><code>(?P<code>.*?)</code></pre>', re.S)
H2_RE = re.compile(r'<h2 id="(?P<id>[^"]+)">(?P<title>.*?)</h2>\n?')
LAST_P_RE = re.compile(r"<p>((?:(?!<p>).)*?)</p>\s*$", re.S)
LINK_HREF_RE = re.compile(r'<a href="([^"]+)">(.*?)</a>', re.S)
MODULE_URL_RE = re.compile(r'href="https://docs\.rs/grit-lib/[^/]+/(grit_lib(?:/[a-z0-9_]+)*)/index\.html"')


def code_block(lang: str, code: str) -> str:
    """Render one fenced block (``code`` is HTML-escaped) with highlighting by language."""
    text = html.unescape(code)
    base = lang.split(":", 1)[0].lower()
    if base == "console":
        inner = highlight_console(text)
    elif base in ("bash", "sh", "shell", "powershell"):
        inner = highlight_shell(text)
    elif base == "json":
        inner = highlight_json(text)
    elif base == "rust":
        inner = highlight_rust(text)
    elif base == "text":
        inner = f'<span class="out">{html.escape(text)}</span>'
    else:
        inner = html.escape(text)
    return f'<pre class="code"><code>{inner}</code></pre>'


def restyle_code(fragment: str) -> str:
    return PRE_RE.sub(lambda m: code_block(m.group("lang"), m.group("code")), fragment)


@dataclass(frozen=True)
class HtmlSection:
    """One ``<h2>`` section of a rendered page."""

    anchor: str
    title_html: str
    body: str

    @property
    def title(self) -> str:
        return html.unescape(re.sub(r"<[^>]+>", "", self.title_html)).strip()

    def heading(self) -> str:
        return f'<h2 id="{self.anchor}">{self.title_html}</h2>'


def split_sections(body_html: str) -> tuple[str, list[HtmlSection]]:
    """Split rendered HTML into the intro and one section per ``<h2>``."""
    matches = list(H2_RE.finditer(body_html))
    if not matches:
        return body_html, []
    intro = body_html[: matches[0].start()]
    sections = []
    for i, match in enumerate(matches):
        end = matches[i + 1].start() if i + 1 < len(matches) else len(body_html)
        sections.append(HtmlSection(match.group("id"), match.group("title"), body_html[match.end() : end]))
    return intro, sections


def pull_code(fragment: str, *, max_caption: int | None = None) -> tuple[str, list[tuple[str, str, str]]]:
    """Remove code blocks, returning the rest and ``(caption, lang, code)`` per block.

    A block's caption is the paragraph right before it when that paragraph ends
    with a colon ("Commit everything:") and is at most ``max_caption`` characters
    of text; other prose stays in place.
    """
    blocks: list[tuple[str, str, str]] = []
    rest: list[str] = []
    pos = 0
    for match in PRE_RE.finditer(fragment):
        before = fragment[pos : match.start()]
        caption = ""
        last = LAST_P_RE.search(before)
        text = re.sub(r"<[^>]+>", "", last.group(1)).strip() if last else ""
        if last and text.endswith(":") and (max_caption is None or len(text) <= max_caption):
            caption = last.group(1).rstrip()[:-1]
            before = before[: last.start()]
        rest.append(before)
        blocks.append((caption, match.group("lang"), match.group("code")))
        pos = match.end()
    rest.append(fragment[pos:])
    return "".join(rest), blocks


def example_block(caption: str, lang: str, code: str, *, aside: str = "") -> str:
    cap = ""
    if caption or aside:
        cap = f'<div class="cap"><span>{caption}</span>{aside}</div>'
    return f'<div class="ex">{cap}{code_block(lang, code)}</div>'


def tag_tables(fragment: str) -> str:
    """Mark tables as reference rows, sized by their column count."""

    def replace(match: re.Match[str]) -> str:
        cols = min(match.group(1).count("<th>"), 3)
        return f'<table class="rows rows-{cols}"><thead>{match.group(1)}</thead>'

    return re.sub(r"<table><thead>(.*?)</thead>", replace, fragment, flags=re.S)


def breadcrumb(*parts: str) -> str:
    crumbs = " / ".join(html.escape(p) for p in parts if p)
    return f'<div class="crumbs"><span>{crumbs}</span><a href="index.md">Markdown</a></div>'


def lede(page: Page) -> str:
    return f'<p class="lede">{html.escape(page.summary)}</p>' if page.summary else ""


def render_command(page: Page, site: Site) -> str:
    """Screen 2a: reference in the center, examples and JSON in an ink column."""
    intro, sections = split_sections(page.body_html)
    center = [restyle_code(intro)]
    examples: list[str] = []
    see_also = ""
    for section in sections:
        name = section.title.lower()
        if name == "synopsis":
            lines = "".join(
                f"<div>{line}</div>"
                for m in PRE_RE.finditer(section.body)
                for line in m.group("code").split("\n")
            )
            center.append(f'<div class="synopsis" id="{section.anchor}">{lines}</div>')
        elif name == "examples":
            rest, blocks = pull_code(section.body)
            examples.extend(example_block(cap, lang, code) for cap, lang, code in blocks)
            if rest.strip():
                examples.append(f'<div class="ink-prose">{rest}</div>')
        elif name in ("json output", "markdown output"):
            rest, blocks = pull_code(section.body)
            flag = "--json" if name.startswith("json") else "--markdown"
            for cap, lang, code in blocks:
                caption = cap if len(cap) > 12 else f"{flag} output"
                link = f'<a href="{href_to(page.slug, "scripting")}">Scripting →</a>' if flag == "--json" else ""
                examples.append(example_block(caption, lang, code, aside=link))
            center.append(section.heading() + tag_tables(rest))
        elif name == "see also":
            pills = "".join(f'<a href="{href}">{text}</a>' for href, text in LINK_HREF_RE.findall(section.body))
            see_also = f'<div class="see-also" id="{section.anchor}"><span>See also</span>{pills}</div>'
        else:
            center.append(section.heading() + tag_tables(restyle_code(section.body)))
    crumbs = breadcrumb("Commands", page.group)
    center_html = "".join(center)
    examples_html = "".join(examples)
    main = f"""<main class="col main">
{crumbs}
<h1 class="cmd">{html.escape(page.title)}</h1>
{lede(page)}
<div class="content">{center_html}</div>
{pager(site, page.slug)}
</main>"""
    aside = f"""<aside class="col ink-col" aria-label="Examples">
<div class="label">Examples</div>
{examples_html}
{see_also}
</aside>"""
    return f'<div class="pane three">{main}{aside}</div>'


def render_steps(page: Page, site: Site) -> str:
    """Screen 2b: one row per H2, with that step's code in a continuous ink strip."""
    intro, sections = split_sections(page.body_html)
    intro_rest, intro_code = pull_code(intro, max_caption=STEP_CAPTION_MAX)
    meta = "".join(
        f'<p class="meta">{p}</p>' for p in re.findall(r"<p>(Regenerated.*?)</p>", intro_rest, re.S)
    )
    intro_rest = re.sub(r"<p>Regenerated.*?</p>\n?", "", intro_rest, flags=re.S)
    crumb = breadcrumb(page.section_title, page.title)
    intro_blocks = "".join(example_block(c, lang, x) for c, lang, x in intro_code)
    rows = [
        f'<div class="step intro">{crumb}<h1>{html.escape(page.title)}</h1>{lede(page)}'
        f'<div class="content">{intro_rest}</div></div>'
        f'<div class="step-code intro">{intro_blocks}{meta}</div>'
    ]
    for number, section in enumerate(sections, start=1):
        prose, blocks = pull_code(section.body, max_caption=STEP_CAPTION_MAX)
        code = "".join(example_block(c, lang, x) for c, lang, x in blocks)
        rows.append(
            f'<div class="step"><span class="num">{number:02d}</span>'
            f'<div class="content">{section.heading()}{prose}</div></div>'
            f'<div class="step-code">{code}</div>'
        )
    rows.append(f'<div class="step last">{pager(site, page.slug)}</div><div class="step-code"></div>')
    return '<main class="col steps">' + "".join(rows) + "</main>"


def is_example_section(section: HtmlSection) -> bool:
    return any(m.group("lang").startswith("rust:include=") for m in PRE_RE.finditer(section.body))


def render_library(page: Page, site: Site) -> str | None:
    """Screen 2c: guide prose on paper, the compiled example in an ink column.

    Returns ``None`` when the guide has no included example.
    """
    intro, sections = split_sections(page.body_html)
    if not any(is_example_section(s) for s in sections):
        return None
    center = [restyle_code(intro)]
    examples: list[str] = []
    take_run = False
    for section in sections:
        if is_example_section(section) or (take_run and section.title.lower().startswith("run")):
            take_run = is_example_section(section)
            rest, blocks = pull_code(section.body)
            prose = re.sub(r"</?p>", "", rest).strip()
            for cap, lang, code in blocks:
                if lang.startswith("rust:include="):
                    rel = lang.split("=", 1)[1]
                    title = "" if section.title.lower() == "example" else html.escape(section.title)
                    examples.append(
                        f'<div class="ex-head"><span>Example · {html.escape(Path(rel).name)}</span>'
                        f'<a href="{GITHUB_BLOB}/{html.escape(rel)}">full source ↗</a></div>'
                        + (f'<div class="ex-title" id="{section.anchor}">{title}</div>' if title else "")
                        + (f'<p class="ink-prose">{prose}</p>' if prose else "")
                    )
                    prose = ""
                    examples.append(example_block(cap, lang, code))
                else:
                    examples.append(example_block(cap, lang, code))
            continue
        take_run = False
        center.append(section.heading() + restyle_code(section.body))
    modules = sorted(dict.fromkeys(MODULE_URL_RE.findall(intro)))
    pills = "".join(
        f'<a href="{DOCS_RS_GRIT_LIB}/latest/{m}/index.html">{m.replace("/", "::")}</a>' for m in modules
    )
    crumbs = breadcrumb("Library guide", page.title)
    modules_html = f'<div class="modules">{pills}</div>' if pills else ""
    center_html = "".join(center)
    examples_html = "".join(examples)
    main = f"""<main class="col main">
{crumbs}
<h1>{html.escape(page.title)}</h1>
{lede(page)}
{modules_html}
<div class="content">{center_html}</div>
{pager(site, page.slug)}
</main>"""
    aside = f'<aside class="col ink-col lib" aria-label="Example">{examples_html}</aside>'
    return f'<div class="pane three lib">{main}{aside}</div>'


def render_plain(page: Page, site: Site) -> str:
    """Guides without step code: prose with code blocks inline."""
    section = "Library guide" if is_library_slug(page.slug) else page.section_title
    crumbs = breadcrumb(section, page.title if page.title != section else "")
    return f"""<div class="pane"><main class="col main">
{crumbs}
<h1>{html.escape(page.title)}</h1>
{lede(page)}
<div class="content">{restyle_code(page.body_html)}</div>
{pager(site, page.slug)}
</main></div>"""


def render_overview(site: Site) -> str:
    """Screen 1c: the CLI on paper and the library on ink, side by side."""
    labels = {p.slug: p.title for p in site.pages}
    cli_pages = next((s.pages for s in site.sections if s.command_groups), ())
    tutorial_href = href_to("index", "tutorial")
    tiles = [f'<a class="tile ink" href="{tutorial_href}"><span>start here</span><b>Tutorial →</b></a>']
    for spec in cli_pages:
        href = href_to("index", spec.slug)
        label = html.escape(OVERVIEW_TILE_LABELS.get(spec.slug, "guide"))
        tiles.append(f'<a class="tile" href="{href}"><span>{label}</span><b>{html.escape(spec.label)}</b></a>')
    command_groups = next((s.command_groups for s in site.sections if s.command_groups), ())
    groups = []
    for group in command_groups:
        rows = "".join(
            f'<a href="{p.slug}/"><code>{html.escape(p.title)}</code><span>{html.escape(p.summary)}</span></a>'
            for p in site.command_pages
            if p.group == group
        )
        if rows:
            groups.append(f'<div class="group"><h3 id="{blog.slugify(group)}">{html.escape(group)}</h3>{rows}</div>')
    guides = "".join(
        f'<a href="{slug}/">{html.escape(labels[slug])}</a>'
        for slug in library_slugs(site)
        if slug not in (LIBRARY_GUIDE_SLUG, API_MAP_SLUG)
    )
    tiles_html = "".join(tiles)
    groups_html = "".join(groups)
    api_map_label = html.escape(labels.get(API_MAP_SLUG, "grit-lib API map"))
    return f"""<div class="overview">
<main class="col half cli">
  <div class="label">$ grit</div>
  <h1>The command line</h1>
  <p class="lede">{OVERVIEW_CLI_LEDE}</p>
  <div class="tiles four">{tiles_html}</div>
  <h2 id="commands" class="visually-hidden">Commands</h2>
  <div class="cmd-index">{groups_html}</div>
  {agents_panel()}
</main>
<section class="col half lib" aria-label="The library">
  <div class="label">use grit_lib;</div>
  <h2>The library</h2>
  <p class="lede">{OVERVIEW_LIB_LEDE}</p>
  <div class="tiles two">
    <a class="tile accent" href="{LIBRARY_QUICKSTART_SLUG}/"><span>start here</span><b>Library quick start →</b></a>
    <a class="tile" href="{API_MAP_SLUG}/"><span>every public type</span><b>{api_map_label}</b></a>
  </div>
  <div class="sub">Guides</div>
  <div class="guides">{guides}</div>
  <a class="ext" href="{DOCS_RS_GRIT_LIB}">API reference on docs.rs ↗</a>
  <p class="note">On-disk formats and the wire protocol stay compatible with Git. <a href="benchmarks/">See benchmarks →</a></p>
</section>
</div>"""


def search_index(site: Site) -> str:
    """Page titles and summaries for the header search, as paths from the docs root."""
    entries = [
        [p.title, p.summary, "" if p.slug == "index" else f"{p.slug}/"] for p in site.pages
    ]
    data = json.dumps(entries, ensure_ascii=False, separators=(",", ":")).replace("</", "<\\/")
    return f'<script type="application/json" id="docs-index">{data}</script>'


def header(page: Page, base: str) -> str:
    home = f"{base}/"
    if page.slug == "index":
        return f"""<header class="dh">
  <a class="brand" href="{home}" aria-label="grit homepage">grit</a>
  <span class="ref">HEAD → docs</span>
  <span class="tagline">How to use grit, a simple Git client built on grit-lib.</span>
  <nav aria-label="Primary">
    <a href="install/">Install</a><a href="benchmarks/">Benchmarks</a><a href="{base}/blog/">Blog</a>
    <a href="index.md">Markdown</a><a class="pill" href="{GITHUB_URL}">GitHub</a>
  </nav>
</header>"""
    library = is_library_slug(page.slug)
    docs_home = href_to(page.slug, "index")
    library_home = href_to(page.slug, LIBRARY_GUIDE_SLUG)
    cli_seg = f'<a href="{docs_home}">CLI</a>' if library else '<span aria-current="true">CLI</span>'
    lib_seg = '<span aria-current="true">Library</span>' if library else f'<a href="{library_home}">Library</a>'
    extra = f'<a href="{DOCS_RS_GRIT_LIB}">docs.rs ↗</a>' if library else f'<a href="{base}/blog/">Blog</a>'
    return f"""<header class="dh">
  <a class="brand" href="{home}" aria-label="grit homepage">grit</a>
  <div class="switch" role="group" aria-label="Docs half">{cli_seg}{lib_seg}</div>
  <span class="ref">HEAD → docs/{html.escape(page.slug)}</span>
  <div class="search" data-docs-search data-root="{docs_home}">
    <input type="search" placeholder="Search" aria-label="Search the docs" autocomplete="off" />
    <kbd>⌘K</kbd>
    <ul hidden></ul>
  </div>
  <nav aria-label="Primary">{extra}<a class="pill" href="{GITHUB_URL}">GitHub</a></nav>
</header>"""


def shell(title: str, description: str, body: str, *, extra_head: str = "", scripts: str = "") -> str:
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
<style>{CSS}</style>
</head>
<body>
<div class=\"app\">
{body}
</div>
{scripts}</body>
</html>
"""


def render(page: Page, site: Site, *, is_index: bool) -> str:
    depth = 0 if is_index else doc_depth(page.slug)
    base = "/".join([".."] * (depth + 1))
    if is_index:
        layout = render_overview(site)
        nav = ""
    elif page.is_command:
        layout = render_command(page, site)
        nav = sidebar(site, page.slug)
    elif page.slug in STEP_GUIDE_SLUGS:
        layout = render_steps(page, site)
        nav = sidebar(site, page.slug, toc=page.toc)
    else:
        layout = (render_library(page, site) if is_library_slug(page.slug) else None) or render_plain(page, site)
        nav = sidebar(site, page.slug)
    grid = "docs-body overview-body" if is_index else "docs-body"
    body = f'{header(page, base)}\n<div class="{grid}">{nav}{layout}</div>'
    scripts = "" if is_index else f"{search_index(site)}\n<script>{SCRIPT}</script>\n"
    title = SITE_TITLE if is_index else f"{page.title} - {SITE_TITLE}"
    alternate = '<link rel="alternate" type="text/markdown" href="index.md" />'
    return shell(title, page.summary or DESCRIPTION, body, extra_head=alternate, scripts=scripts)


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
    elif page.slug == API_MAP_SLUG:
        body = expand_includes(body)
        before, after = apimap.split_api_map_markdown(body)
        before = prepare_markdown_body(before)
        after = prepare_markdown_body(after)
        table = apimap.api_map_markdown(DOC_ROOT)
        body = before + "\n\n" + table + after
    else:
        body = prepare_markdown_body(body)
    body = rewrite_markdown_links(body, page.slug)
    twin = site_util.compose_markdown_twin(page.title, body, summary=page.summary)
    if is_index:
        return (
            twin.rstrip()
            + "\n\n"
            + agents_markdown().rstrip()
            + "\n\n"
            + command_index_markdown(site).rstrip()
            + "\n"
        )
    return twin


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
:root{--bg:#f4f1ea;--ink:#1b1915;--soft:#3d3830;--muted:#7b7466;--line:#d9d3c5;--accent:#e2481f;--chip:#e9e4d8;--ink-soft:#c9c2b3;--ink-muted:#a39b8b;--mono:"JetBrains Mono",ui-monospace,SFMono-Regular,Menlo,monospace}
*{box-sizing:border-box}
html,body{height:100%}
body{margin:0;background:var(--bg);color:var(--ink);font-family:"Space Grotesk",system-ui,sans-serif}
a{color:inherit}a:hover{opacity:.7}
code,kbd{font-family:var(--mono)}
.visually-hidden{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap}
.app{height:100vh;display:flex;flex-direction:column}
.dh{height:52px;flex-shrink:0;display:flex;align-items:center;gap:20px;padding:0 24px;border-bottom:1px solid var(--line);white-space:nowrap}
.brand{display:flex;align-items:center;gap:8px;font-weight:700;font-size:19px;text-decoration:none}
.brand::before{content:"";width:12px;height:12px;border-radius:50%;background:var(--accent)}
.switch{display:flex;border:1px solid var(--ink);border-radius:999px;overflow:hidden;font-size:14px;margin-left:12px}
.switch>*{padding:5px 14px;text-decoration:none}.switch span{background:var(--ink);color:var(--bg)}
.ref{font:13px var(--mono);color:var(--accent)}
.tagline{font-size:15px;color:var(--soft);margin-left:16px;overflow:hidden;text-overflow:ellipsis}
.dh nav{margin-left:auto;display:flex;align-items:center;gap:20px;font-size:15px}
.dh nav a{text-decoration:none}
.dh .pill{background:var(--ink);color:var(--bg);padding:7px 14px;border-radius:999px}
.search{position:relative;flex:1;max-width:400px;margin-left:auto;display:flex;align-items:center;gap:8px;height:34px;padding:0 12px;border:1px solid var(--line);border-radius:8px;background:var(--chip)}
.search+nav{margin-left:0}
.search input{flex:1;min-width:0;border:0;background:none;outline:none;font:13px var(--mono);color:var(--ink)}
.search input::placeholder{color:var(--muted)}
.search kbd{border:1px solid var(--line);border-radius:4px;padding:1px 6px;font-size:11px;color:var(--muted)}
.search ul{position:absolute;z-index:10;top:38px;left:0;right:0;margin:0;padding:6px;list-style:none;background:var(--bg);border:1px solid var(--ink);border-radius:10px;white-space:normal}
.search li a{display:block;padding:8px 10px;border-radius:6px;text-decoration:none}
.search li a.on,.search li a:hover{background:var(--chip);opacity:1}
.search li b{display:block;font:500 13px var(--mono)}.search li span{display:block;font-size:13px;color:var(--soft);margin-top:2px}
.search li.none{padding:8px 10px;font:13px var(--mono);color:var(--muted)}
.docs-body{flex:1;min-height:0;display:grid;grid-template-columns:232px minmax(0,1fr)}
.col{overflow-y:auto;min-height:0}
.docnav{overflow-y:auto;min-height:0;padding:18px 14px 28px 18px;border-right:1px solid var(--line);font:13px/1.5 var(--mono);color:var(--muted);display:flex;flex-direction:column;gap:2px}
.docnav h2{margin:14px 0 0;padding:0 8px 4px;font:inherit;color:var(--accent)}.docnav h2:first-child{margin-top:0}
.docnav h3{margin:8px 0 0;padding:2px 8px;font:inherit;font-size:11px;text-transform:uppercase;letter-spacing:.06em}
.docnav a{text-decoration:none;padding:2px 8px;border-radius:5px;color:var(--soft)}
.docnav a.cmd{padding-left:16px}
.docnav a.current{background:var(--ink);color:var(--bg);opacity:1}
.subnav{display:flex;flex-direction:column;gap:2px;margin:4px 0 4px 14px;padding-left:10px;border-left:1px solid var(--line);font-size:12px}
.subnav a{padding:1px 0;color:var(--muted)}.subnav a.on{color:var(--ink);background:none}
.docnav.ink{background:var(--ink);color:var(--ink-muted);border-right:0;gap:3px}
.docnav.ink a{color:var(--chip)}.docnav.ink a.current{background:var(--bg);color:var(--ink)}
.docnav.ink .label{color:var(--accent);padding-bottom:4px}
.docnav.ink h3{margin-top:14px;padding-bottom:4px;color:var(--ink-muted)}
.docnav.ink .ext{color:var(--accent);margin-top:12px}
.docnav.ink .subnav{border-color:var(--soft)}.docnav.ink .subnav a{color:var(--ink-muted)}.docnav.ink .subnav a.on{color:var(--bg)}
.pane{min-height:0;display:grid;grid-template-columns:minmax(0,1fr)}
.pane.three{grid-template-columns:minmax(0,1fr) 540px}.pane.three.lib{grid-template-columns:minmax(0,1fr) 580px}
.main{padding:32px 44px 48px}
.crumbs{display:flex;gap:16px;font:13px var(--mono);color:var(--muted)}
.crumbs a{color:var(--accent);text-decoration:none}
h1{margin:8px 0 0;font-size:64px;line-height:.92;letter-spacing:-.05em;font-weight:700;text-wrap:balance}
h1.cmd{font:500 56px/1 var(--mono);letter-spacing:-.045em}
.lede{margin:12px 0 0;max-width:680px;font-size:21px;line-height:1.33;font-weight:500;letter-spacing:-.01em;text-wrap:pretty}
.content{margin-top:8px;font-size:17px;line-height:1.6;color:var(--soft)}
.content h2{margin:32px 0 8px;font-size:24px;line-height:1.15;letter-spacing:-.02em;color:var(--ink)}
.content h3{margin:24px 0 6px;font-size:19px;letter-spacing:-.015em;color:var(--ink)}
.content p,.content ul,.content ol,.content blockquote{max-width:720px}
.content p{margin:0 0 12px;text-wrap:pretty}
.content ul,.content ol{margin:0 0 12px;padding-left:1.2em}.content li{margin:.25em 0}.content li::marker{color:var(--accent)}
.content a{color:var(--ink);text-decoration-color:var(--accent);text-underline-offset:3px}
.content a code{background:none;padding:0}
.content a[href^="https://docs.rs/"]{font:.88em var(--mono)}
.content strong{color:var(--ink)}
.content code{font:.88em var(--mono);background:var(--chip);padding:1px 5px;border-radius:5px}
.content blockquote{margin:16px 0;padding-left:16px;border-left:3px solid var(--accent);color:var(--ink)}
.content .table{margin:12px 0 20px;overflow-x:auto}
.content table.rows{max-width:760px}
.content table{width:100%;border-collapse:collapse;font-size:15px;line-height:1.5;border-top:1px solid var(--ink)}
.content th{text-align:left;font:12px var(--mono);color:var(--muted);font-weight:400;padding:8px 16px 8px 0;border-bottom:1px solid var(--line)}
.content td{padding:10px 16px 10px 0;border-bottom:1px solid var(--line);vertical-align:top;overflow-wrap:break-word}
.content td:first-child{overflow-wrap:normal}.content td code{white-space:nowrap}
.content table.rows thead{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0)}
.content table.rows td:first-child code{background:none;padding:0;font:500 14px/1.7 var(--mono);color:var(--ink)}
.content table.rows-2 td:first-child{width:230px}
.content table.rows-3 td:first-child{width:150px}.content table.rows-3 td:nth-child(2){width:100px;font:13px var(--mono);color:var(--muted)}
.code{margin:12px 0 16px;background:var(--ink);color:var(--chip);border:1px solid var(--soft);border-radius:10px;padding:14px 16px;font:14px/1.75 var(--mono);overflow-x:auto;white-space:pre}
.code code{font:inherit}
.code .k{color:var(--accent)}.code .c,.code .out{color:var(--ink-muted)}.code .cmd{color:var(--bg)}
.synopsis{margin-top:24px;max-width:760px;background:var(--chip);border-radius:10px;padding:14px 18px;font:15px/1.8 var(--mono);color:var(--ink);overflow-x:auto;white-space:pre}
.pager{display:flex;justify-content:space-between;gap:16px;margin-top:36px;padding-top:16px;border-top:1px solid var(--line);font:15px var(--mono)}
.pager a{text-decoration:none}
.ink-col{background:var(--ink);color:var(--chip);padding:32px 32px 40px;display:flex;flex-direction:column;gap:22px}
.ink-col.lib{padding:28px 28px 36px;gap:18px;border-left:1px solid var(--soft)}
.ink-col .label{font:13px var(--mono);color:var(--accent)}
.ex{display:flex;flex-direction:column;gap:8px}
.ex .cap{display:flex;justify-content:space-between;gap:12px;font-size:15px;color:var(--ink-soft)}
.ex .cap a{font:13px var(--mono);color:var(--accent);text-decoration:none}
.ex .code{margin:0}
.ink-col code,.step-code code{font-size:.9em}
.ink-col .code code,.step-code .code code{font-size:inherit}
.ink-prose{font-size:15px;line-height:1.55;color:var(--ink-soft)}
.ink-prose code{background:none;color:var(--chip)}
.ink-prose a{color:var(--bg)}
.ex-head{display:flex;justify-content:space-between;align-items:baseline;font:13px var(--mono);color:var(--accent)}
.ex-head a{color:var(--ink-muted);text-decoration:none}
.ex-title{font-weight:700;color:var(--bg)}
.ink-col.lib .code{font-size:13px;line-height:1.7}
.see-also{margin-top:auto;padding-top:18px;border-top:1px solid var(--soft);display:flex;flex-wrap:wrap;gap:8px;font:13px var(--mono)}
.see-also span{width:100%;color:var(--ink-muted)}
.see-also a{color:var(--bg);border:1px solid var(--soft);border-radius:999px;padding:4px 10px;text-decoration:none}
.modules{display:flex;flex-wrap:wrap;gap:8px;margin-top:18px;font:13px var(--mono)}
.modules a{border:1px solid var(--ink);border-radius:999px;padding:4px 10px;text-decoration:none}
.steps{display:grid;grid-template-columns:minmax(0,1fr) 600px;align-content:start;background:linear-gradient(to left,var(--ink) 600px,transparent 600px) local}
.step{padding:28px 44px;border-top:1px solid var(--line);display:grid;grid-template-columns:44px minmax(0,1fr);gap:4px}
.step.intro{display:block;border-top:0;padding:36px 44px 32px}
.step.intro .lede{max-width:600px}.step.intro .content{margin-top:14px;font-size:16px}
.step.last{display:block;padding-top:0;border-top:0}
.step .num{font:500 14px/1.9 var(--mono);color:var(--accent)}
.step .content{margin:0}
.step .content h2:first-child{margin:0;font-size:26px}
.step .content>p:nth-child(2){margin-top:10px}
.step-code{background:var(--ink);padding:28px 32px;border-top:1px solid var(--soft);display:flex;flex-direction:column;gap:14px;min-width:0}
.step-code.intro{border-top:0;padding-top:36px;justify-content:flex-end}
.step-code .code{margin:0;white-space:pre-wrap}
.step-code .meta{margin:0;font:13px/1.5 var(--mono);color:var(--ink-muted)}
.step-code .meta code{background:none;color:var(--chip)}
.overview-body{grid-template-columns:minmax(0,1.25fr) minmax(0,1fr)}
.overview{display:contents}
.half{padding:32px 40px 40px}
.half .label{font:13px var(--mono);color:var(--accent)}
.half h1,.half>h2{margin:8px 0 0;font-size:56px;line-height:.95;letter-spacing:-.045em}
.half .lede{max-width:600px;font-size:17px;line-height:1.5;font-weight:400;color:var(--soft)}
.half .lede code{font:.88em var(--mono);background:var(--chip);padding:1px 5px;border-radius:5px}
.tiles{display:grid;gap:10px;margin-top:22px}.tiles.four{grid-template-columns:repeat(4,minmax(0,1fr))}.tiles.two{grid-template-columns:repeat(2,minmax(0,1fr))}
.tile{border:1px solid var(--line);border-radius:10px;padding:14px;text-decoration:none;display:flex;flex-direction:column;gap:18px}
.tile:hover{border-color:var(--ink);opacity:1}
.tile span{font:12px var(--mono);color:var(--muted)}.tile b{font-size:17px}
.tile.ink{background:var(--ink);color:var(--bg);border-color:var(--ink)}.tile.ink span{color:var(--accent)}
.cmd-index{column-count:2;column-gap:36px;margin-top:28px}
.group{break-inside:avoid;margin-bottom:20px}
.group h3{margin:0;font:13px var(--mono);color:var(--accent);padding-bottom:6px;border-bottom:1px solid var(--ink)}
.group a{display:grid;grid-template-columns:138px minmax(0,1fr);gap:10px;padding:7px 0;border-bottom:1px solid var(--line);text-decoration:none;font-size:14px;line-height:1.4;color:var(--soft)}
.group a:hover{background:var(--chip);opacity:1}
.group code{font:500 14px var(--mono);color:var(--ink)}
.agents{margin-top:12px;padding-top:16px;border-top:1px solid var(--ink);font-size:15px;line-height:1.55;color:var(--soft);break-inside:avoid}
.agents h2{margin:0 0 8px;font:13px var(--mono);color:var(--accent)}
.agents ul{margin:0;padding:0;list-style:none;display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:6px 24px}
.agents a{font:500 14px var(--mono);color:var(--ink);text-decoration-color:var(--accent);text-underline-offset:3px}
.agents p{margin:10px 0 0}
.half.lib{background:var(--ink);color:var(--chip);display:flex;flex-direction:column}
.half.lib>h2{color:var(--bg)}.half.lib .lede{color:var(--ink-soft)}
.half.lib .tile{border-color:var(--soft);color:var(--bg)}.half.lib .tile:hover{border-color:var(--chip)}.half.lib .tile span{color:var(--ink-muted)}
.half.lib .tile.accent{background:var(--accent);border-color:var(--accent);color:var(--ink)}.half.lib .tile.accent span{color:var(--ink)}
.half.lib .sub{font:13px var(--mono);color:var(--ink-muted);margin:28px 0 6px}
.guides{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));column-gap:24px}
.guides a{padding:9px 0;border-bottom:1px solid var(--soft);text-decoration:none;color:var(--bg);font-size:16px}
.guides a:hover{color:var(--accent);opacity:1}
.half.lib .ext{margin-top:16px;font:14px var(--mono);color:var(--accent);text-decoration:none}
.half.lib .note{margin:0;margin-top:auto;padding-top:28px;font-size:14px;line-height:1.5;color:var(--ink-muted)}
.half.lib .note a{color:var(--chip)}
@media(max-width:1200px){
.pane.three,.pane.three.lib{display:block;overflow-y:auto}
.pane.three>.col{overflow:visible}
.steps{display:block;background:none}
.step-code{border-top:0}
.tiles.four{grid-template-columns:repeat(2,minmax(0,1fr))}
.search{max-width:260px}.tagline{display:none}
}
@media(max-width:900px){
.app{height:auto;min-height:100vh}
.dh{height:auto;flex-wrap:wrap;padding:12px 16px;gap:12px;white-space:normal}
.dh .ref{display:none}
.search{order:9;flex-basis:100%;max-width:none;margin-left:0}
.docs-body{display:flex;flex-direction:column}
.docs-body>.docnav{order:2;border-right:0;border-top:1px solid var(--line)}
.col,.docnav,.pane{overflow:visible}
.pane,.pane.three{display:block}
.main,.half,.step,.step.intro{padding-left:16px;padding-right:16px}
.step-code{padding:20px 16px}
.ink-col,.ink-col.lib{padding:24px 16px}
h1{font-size:44px}h1.cmd{font-size:36px}.half h1,.half>h2{font-size:44px}
.cmd-index{column-count:1}
.group a{grid-template-columns:120px minmax(0,1fr)}
.guides,.agents ul{grid-template-columns:minmax(0,1fr)}
.content table.rows-2 td:first-child{width:auto}
}
'''

# Header search (⌘K or /) over page titles and summaries, and scroll-spy for
# the step guides' heading list. No dependencies.
SCRIPT = r'''
(()=>{const box=document.querySelector("[data-docs-search]");if(box){const input=box.querySelector("input"),list=box.querySelector("ul"),root=box.dataset.root;const idx=JSON.parse(document.getElementById("docs-index").textContent);let hits=[],sel=0;
const el=(tag,cls,text)=>{const n=document.createElement(tag);if(cls)n.className=cls;if(text)n.textContent=text;return n};
const draw=()=>{list.replaceChildren(...(hits.length?hits.map((e,i)=>{const li=el("li"),a=el("a",i===sel?"on":"");a.setAttribute("href",root+e[2]);a.append(el("b","",e[0]),el("span","",e[1]));li.append(a);return li}):[el("li","none","No matches")]));list.hidden=false};
const find=()=>{const q=input.value.trim().toLowerCase();if(!q){list.hidden=true;return}const t=idx.filter(e=>e[0].toLowerCase().includes(q)),s=idx.filter(e=>!t.includes(e)&&e[1].toLowerCase().includes(q));hits=t.concat(s).slice(0,8);sel=0;draw()};
input.addEventListener("input",find);input.addEventListener("focus",find);
input.addEventListener("keydown",e=>{if(e.key==="ArrowDown"||e.key==="ArrowUp"){if(!hits.length)return;e.preventDefault();sel=(sel+(e.key==="ArrowDown"?1:hits.length-1))%hits.length;draw()}else if(e.key==="Enter"&&hits[sel]){location.href=root+hits[sel][2]}else if(e.key==="Escape"){input.blur();list.hidden=true}});
document.addEventListener("click",e=>{if(!box.contains(e.target))list.hidden=true});
document.addEventListener("keydown",e=>{if(((e.metaKey||e.ctrlKey)&&e.key==="k")||(e.key==="/"&&!/input|textarea/i.test(document.activeElement.tagName))){e.preventDefault();input.focus();input.select()}})}
const links=[...document.querySelectorAll("[data-spy]")];if(links.length&&"IntersectionObserver" in window){const mark=id=>links.forEach(a=>a.classList.toggle("on",a.dataset.spy===id));const io=new IntersectionObserver(es=>{const v=es.filter(e=>e.isIntersecting).sort((a,b)=>a.boundingClientRect.top-b.boundingClientRect.top)[0];if(v)mark(v.target.id)},{rootMargin:"0px 0px -70% 0px"});links.forEach(a=>{const h=document.getElementById(a.dataset.spy);if(h)io.observe(h)});mark(links[0].dataset.spy)}})();
'''



def generate(
    out_dir: Path,
    *,
    content_dir: Path | None = None,
    llms_dir: Path | None = None,
) -> Site:
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

    for name, library in ((CLI_BUNDLE, False), (LIB_BUNDLE, True)):
        (out_dir / name).write_text(
            render_bundle(site, library=library, root=root, manifest_path=manifest_path, listed=listed),
            encoding="utf-8",
        )

    llms_root = llms_dir if llms_dir is not None else LLMS_DIR
    llms_root.mkdir(parents=True, exist_ok=True)
    (llms_root / "llms.txt").write_text(render_llms_txt(site), encoding="utf-8")
    (llms_root / "llms-full.txt").write_text(
        render_llms_full_txt(site, root=root, manifest_path=manifest_path, listed=listed),
        encoding="utf-8",
    )
    return site


def compare_llms_files(generated_dir: Path, committed_dir: Path) -> list[str]:
    issues: list[str] = []
    for name in ("llms.txt", "llms-full.txt"):
        gen = generated_dir / name
        com = committed_dir / name
        if not gen.is_file():
            issues.append(f"llms: missing generated {name}")
            continue
        if not com.is_file():
            issues.append(f"llms: missing committed {name} (run: make docs)")
            continue
        if not site_util.file_equals(gen, com):
            issues.append(f"llms: content differs for {name}")
    return issues


def check_committed() -> int:
    rustdoc_links.ensure_local_rustdoc(DOC_ROOT, repo_root=ROOT)
    rustdoc_links.validate_rustdoc_links(CONTENT_DIR, DOC_ROOT)
    with tempfile.TemporaryDirectory(prefix="grit-docs-check-") as tmp:
        tmp_path = Path(tmp)
        generated = tmp_path / "docs"
        llms_generated = tmp_path / "llms"
        generate(generated, llms_dir=llms_generated)
        issues = site_util.compare_directories(generated, OUT_DIR, label="docs")
        issues.extend(compare_llms_files(llms_generated, LLMS_DIR))
        if issues:
            for line in issues:
                print(line, file=sys.stderr)
            print("docs output is stale; run: make docs", file=sys.stderr)
            return 1
    print(
        f"docs output is up to date ({OUT_DIR.relative_to(ROOT)}, "
        f"{LLMS_TXT_PATH.relative_to(ROOT)})"
    )
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
    site = generate(OUT_DIR)
    print(
        f"generated {len(site.pager_order)} doc page(s) "
        f"({len(site.command_pages)} command(s)) in {OUT_DIR.relative_to(ROOT)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
