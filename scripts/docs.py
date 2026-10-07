#!/usr/bin/env python3
"""Generate the static Grit docs (tutorial and command reference) from Markdown.

Sources live in content/docs/:

- index.md     the docs landing page
- tutorial.md  a short walkthrough of everyday use
- commands/    one man page per `grit` command, named after the command

Output goes to docs/docs/, served at https://grit-scm.com/docs/. The page
chrome and Markdown renderer are shared with scripts/blog.py so the docs match
the rest of the site.
"""
from __future__ import annotations

import argparse
import html
import shutil
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import blog  # noqa: E402
import site_util  # noqa: E402

ROOT = blog.ROOT
CONTENT_DIR = ROOT / "content" / "docs"
OUT_DIR = ROOT / "docs" / "docs"
SITE_TITLE = "Grit docs"
DESCRIPTION = "How to use grit, a simple Git client built on grit-lib: a short tutorial and a man page for every command."

# Command groups, in sidebar order. Every command page names one of these.
GROUPS = [
    "Getting started",
    "Making changes",
    "History",
    "Branches and tags",
    "Remotes",
    "Maintenance",
    "Plumbing",
]


@dataclass(frozen=True)
class Page:
    slug: str
    title: str
    summary: str
    group: str
    order: int
    body_html: str


def load_page(path: Path, slug: str) -> Page:
    meta, body = blog.parse_front_matter(path.read_text(encoding="utf-8"))
    title = meta.get("title") or slug
    group = meta.get("group", "")
    if slug not in ("index", "tutorial") and group not in GROUPS:
        raise SystemExit(f"{path}: group {group!r} must be one of {GROUPS}")
    body_html, _ = blog.markdown_to_html(body)
    return Page(slug, title, meta.get("summary", ""), group, int(meta.get("order", "0")), body_html)


def load_commands() -> list[Page]:
    pages = [load_page(path, path.stem) for path in sorted((CONTENT_DIR / "commands").glob("*.md"))]
    return sorted(pages, key=lambda p: (GROUPS.index(p.group), p.order, p.slug))


def sidebar(commands: list[Page], current: str, prefix: str) -> str:
    """Render the docs navigation. `prefix` is the path back to /docs/."""

    def link(slug: str, label: str) -> str:
        cls = ' class="current"' if slug == current else ""
        href = prefix if slug == "index" else f"{prefix}{slug}/"
        return f'<li><a{cls} href="{href}">{label}</a></li>'

    parts = [
        '<nav class="docnav" aria-label="Docs">',
        f'<ol>{link("index", "Overview")}{link("tutorial", "Tutorial")}</ol>',
    ]
    for group in GROUPS:
        items = "".join(link(p.slug, html.escape(p.title)) for p in commands if p.group == group)
        if items:
            parts.append(f"<h2>{html.escape(group)}</h2><ol>{items}</ol>")
    parts.append('<h2>Library</h2><ol><li><a href="https://docs.rs/grit-lib">grit-lib API ↗</a></li></ol>')
    parts.append("</nav>")
    return "".join(parts)


def command_index(commands: list[Page]) -> str:
    """The grouped list of command pages shown on the docs landing page."""
    sections = []
    for group in GROUPS:
        rows = "".join(
            f'<tr><td><a href="{p.slug}/"><code>{html.escape(p.title)}</code></a></td><td>{html.escape(p.summary)}</td></tr>'
            for p in commands
            if p.group == group
        )
        if rows:
            anchor = blog.slugify(group)
            sections.append(f'<h3 id="{anchor}">{html.escape(group)}</h3><div class="table"><table class="cmds"><tbody>{rows}</tbody></table></div>')
    return '<h2 id="commands">Commands</h2>' + "".join(sections)


def pager(commands: list[Page], current: str) -> str:
    order = ["tutorial"] + [p.slug for p in commands]
    titles = {"tutorial": "Tutorial", **{p.slug: p.title for p in commands}}
    if current not in order:
        return ""
    i = order.index(current)
    prev_link = f'<a href="../{order[i - 1]}/">← {html.escape(titles[order[i - 1]])}</a>' if i > 0 else '<a href="../">← Overview</a>'
    next_link = f'<a href="../{order[i + 1]}/">{html.escape(titles[order[i + 1]])} →</a>' if i + 1 < len(order) else "<span></span>"
    return f'<div class="pager">{prev_link}{next_link}</div>'


def shell(title: str, description: str, body: str, base: str) -> str:
    home = f"{base}/"
    return f"""<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\" />
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />
<title>{html.escape(title)}</title>
<meta name=\"description\" content=\"{html.escape(description, quote=True)}\" />
<script async src=\"https://u.gitbutler.com/script.js\" data-website-id=\"2c6f680c-eaf5-4cd7-a419-1032ffab6bbc\"></script>
{blog.FONTS}
<style>{blog.CSS}{CSS}</style>
</head>
<body>
<div class=\"wrap\">
<header class=\"topbar\">
  <a class=\"brand\" href=\"{home}\" aria-label=\"grit homepage\">grit</a>
  <nav class=\"nav\" aria-label=\"Primary\">
    <a href=\"{base}/docs/\">Docs</a>
    <a href=\"https://crates.io/crates/grit-lib\">Library</a>
    <a href=\"{base}/blog/\">Blog</a>
    <a class=\"pill\" href=\"https://github.com/gitbutlerapp/grit\">GitHub</a>
  </nav>
</header>
{body}
</div>
</body>
</html>
"""


def render(page: Page, commands: list[Page], *, is_index: bool) -> str:
    prefix = "" if is_index else "../"
    base = ".." if is_index else "../.."
    ref = '<span class="ref">HEAD → docs</span>' if is_index else f'<a class="ref" href="../">← docs</a><span>{"man page" if page.group else "guide"}</span>'
    h1_class = ' class="cmd"' if page.group else ""
    lede = f'<p class="lede">{html.escape(page.summary)}</p>' if page.summary else ""
    content = page.body_html + (command_index(commands) if is_index else pager(commands, page.slug))
    hero_class = "hero" if is_index else "hero post-hero"
    links = f'<a href="{prefix or "./"}">Docs</a><a href="{base}/">Home</a><a href="{base}/blog/">Blog</a><a href="https://github.com/gitbutlerapp/grit">GitHub</a>'
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
  <div class=\"doc-body\">
    <article class=\"content\">{content}</article>
    {sidebar(commands, page.slug, prefix)}
  </div>
</section>
</main>
{blog.footer(links)}"""
    title = SITE_TITLE if is_index else f"{page.title} - {SITE_TITLE}"
    return shell(title, page.summary or DESCRIPTION, body, base)


CSS = r'''
.doc-body{display:grid;grid-template-columns:180px minmax(0,720px);gap:56px;padding:48px 0 64px}
.post-hero h1.cmd{font-family:var(--mono);font-weight:500;letter-spacing:-.04em;max-width:none}
.docnav{order:-1;position:sticky;top:24px;align-self:start;font:13px/1.5 var(--mono);color:var(--muted);max-height:calc(100vh - 48px);overflow-y:auto}
.docnav h2{margin:20px 0 8px;font:inherit;color:var(--accent)}
.docnav ol{list-style:none;margin:0;padding:0;border-left:1px solid var(--line)}
.docnav li{margin:0 0 6px;padding-left:14px}.docnav a{text-decoration:none}
.docnav a.current{color:var(--ink);font-weight:700}
.content h3 code{font-size:.9em}
.content table.cmds{table-layout:fixed}.content table.cmds td:first-child{width:210px}
.pager{display:flex;justify-content:space-between;gap:16px;margin-top:3em;padding-top:1.4em;border-top:1px solid var(--line);font:15px var(--mono)}
.pager a{text-decoration:none;color:var(--ink)}
@media(max-width:900px){.doc-body{display:flex;flex-direction:column}.docnav{order:1;position:static;max-height:none;border-top:1px solid var(--line);padding-top:24px}}
'''


def generate(out_dir: Path) -> None:
    commands = load_commands()
    index = load_page(CONTENT_DIR / "index.md", "index")
    tutorial = load_page(CONTENT_DIR / "tutorial.md", "tutorial")
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)
    (out_dir / "index.html").write_text(render(index, commands, is_index=True), encoding="utf-8")
    for page in [tutorial, *commands]:
        page_dir = out_dir / page.slug
        page_dir.mkdir()
        (page_dir / "index.html").write_text(render(page, commands, is_index=False), encoding="utf-8")


def check_committed() -> int:
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
    commands = load_commands()
    print(f"generated docs for {len(commands)} command(s) in {OUT_DIR.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
