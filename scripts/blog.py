#!/usr/bin/env python3
"""Generate the static Grit blog from Markdown content."""
from __future__ import annotations

import argparse
import email.utils
import hashlib
import html
import re
import shutil
import sys
import tempfile
import unicodedata
from dataclasses import dataclass
from datetime import date, datetime, timezone
from pathlib import Path
from xml.sax.saxutils import escape as xml_escape

sys.path.insert(0, str(Path(__file__).resolve().parent))
import site_util  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
CONTENT_DIR = ROOT / "content" / "blog"
OUT_DIR = ROOT / "docs" / "blog"
SITE_URL = "https://grit-scm.com"
POST_MARKDOWN_LINK_RE = re.compile(
    rf"^{re.escape(SITE_URL)}/blog/(?P<slug>[^/#]+)/?$"
)
DOCS_MARKDOWN_LINK_RE = re.compile(
    rf"^{re.escape(SITE_URL)}/docs/(?P<slug>.*?)/?$"
)
# Fixed RSS timestamp when there are no posts (deterministic site generation).
EMPTY_FEED_LAST_BUILD = datetime(1970, 1, 1, tzinfo=timezone.utc)
SITE_TITLE = "the Grit project"
BLOG_TITLE = "project notes from Grit"
BLOG_DESCRIPTION = "Short deep dives into building a Git-compatible, library-oriented Rust implementation."
AUTHOR = "the Grit project"

FRONT_MATTER_RE = re.compile(r"\A---\s*\n(.*?)\n---\s*\n", re.DOTALL)
HEADING_RE = re.compile(r"^(#{1,6})\s+(.+?)\s*#*\s*$")
LINK_RE = re.compile(r"\[([^\]]+)\]\(([^)]+)\)")
BOLD_RE = re.compile(r"\*\*([^*]+)\*\*")
EM_RE = re.compile(r"(?<!\*)\*([^*]+)\*(?!\*)")
CODE_RE = re.compile(r"`([^`]+)`")


@dataclass(frozen=True)
class TocItem:
    level: int
    text: str
    anchor: str


@dataclass(frozen=True)
class Post:
    slug: str
    title: str
    summary: str
    author: str
    published: date
    updated: datetime
    source: Path
    url: str
    body_html: str
    toc: list[TocItem]

    @property
    def rfc822_date(self) -> str:
        dt = datetime.combine(self.published, datetime.min.time(), tzinfo=timezone.utc)
        return email.utils.format_datetime(dt)

    @property
    def iso_datetime(self) -> str:
        return datetime.combine(self.published, datetime.min.time(), tzinfo=timezone.utc).isoformat()

    @property
    def display_date(self) -> str:
        return self.published.strftime("%B %-d, %Y")


def parse_front_matter(text: str) -> tuple[dict[str, str], str]:
    match = FRONT_MATTER_RE.match(text)
    if not match:
        return {}, text
    meta: dict[str, str] = {}
    for raw_line in match.group(1).splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        key, sep, value = line.partition(":")
        if not sep:
            raise ValueError(f"invalid front matter line: {raw_line!r}")
        meta[key.strip()] = value.strip().strip('"\'')
    return meta, text[match.end():]


def slugify(value: str) -> str:
    normalized = unicodedata.normalize("NFKD", value).encode("ascii", "ignore").decode("ascii")
    slug = re.sub(r"[^a-zA-Z0-9]+", "-", normalized.lower()).strip("-")
    return slug or "section"


def inline_md(value: str) -> str:
    protected: list[str] = []

    def protect_code(match: re.Match[str]) -> str:
        # The text is already HTML-escaped, so it goes in as is.
        protected.append(f"<code>{match.group(1)}</code>")
        return f"\u0000{len(protected) - 1}\u0000"

    escaped = html.escape(value)
    escaped = CODE_RE.sub(protect_code, escaped)
    escaped = LINK_RE.sub(lambda m: f'<a href="{html.escape(m.group(2), quote=True)}">{m.group(1)}</a>', escaped)
    escaped = BOLD_RE.sub(r"<strong>\1</strong>", escaped)
    escaped = EM_RE.sub(r"<em>\1</em>", escaped)
    for index, replacement in enumerate(protected):
        escaped = escaped.replace(f"\u0000{index}\u0000", replacement)
    return escaped


FENCE_LINE = re.compile(r"^(?P<ticks>`{3,})(?P<info>\S*)?\s*$")


def markdown_to_html(markdown: str) -> tuple[str, list[TocItem]]:
    lines = markdown.splitlines()
    output: list[str] = []
    toc: list[TocItem] = []
    anchors: dict[str, int] = {}
    paragraph: list[str] = []
    list_kind: str | None = None
    in_code = False
    code_lines: list[str] = []
    table_rows: list[list[str]] = []

    def unique_anchor(text: str) -> str:
        base = slugify(re.sub(r"<[^>]+>", "", text))
        count = anchors.get(base, 0)
        anchors[base] = count + 1
        return base if count == 0 else f"{base}-{count + 1}"

    def flush_paragraph() -> None:
        nonlocal paragraph
        if paragraph:
            output.append(f"<p>{inline_md(' '.join(paragraph))}</p>")
            paragraph = []

    def close_list() -> None:
        nonlocal list_kind
        if list_kind:
            output.append(f"</{list_kind}>")
            list_kind = None

    def flush_table() -> None:
        nonlocal table_rows
        if not table_rows:
            return
        head, *rest = table_rows
        if rest and all(re.fullmatch(r":?-+:?", cell) for cell in rest[0]):
            rest = rest[1:]
        cells = "".join(f"<th>{inline_md(cell)}</th>" for cell in head)
        body = "".join("<tr>" + "".join(f"<td>{inline_md(cell)}</td>" for cell in row) + "</tr>" for row in rest)
        output.append(f'<div class="table"><table><thead><tr>{cells}</tr></thead><tbody>{body}</tbody></table></div>')
        table_rows = []

    for line in lines:
        if line.strip().startswith("|") and not in_code:
            flush_paragraph(); close_list()
            row = line.strip().strip("|")
            table_rows.append([cell.strip().replace("\\|", "|") for cell in re.split(r"(?<!\\)\|", row)])
            continue
        flush_table()
        fence = FENCE_LINE.match(line.strip())
        if fence and line.strip().startswith("`"):
            if in_code:
                output.append(f"<pre><code>{html.escape(chr(10).join(code_lines))}</code></pre>")
                code_lines = []
                in_code = False
            else:
                # Opening info string (e.g. ```console) is not copied into HTML.
                flush_paragraph()
                close_list()
                in_code = True
                code_lines = []
            continue
        if in_code:
            code_lines.append(line)
            continue
        if not line.strip():
            flush_paragraph(); close_list(); continue
        heading = HEADING_RE.match(line)
        if heading:
            flush_paragraph(); close_list()
            level = len(heading.group(1))
            text = heading.group(2).strip()
            anchor = unique_anchor(text)
            if level >= 2:
                toc.append(TocItem(level, re.sub(r"[`*_]", "", text), anchor))
            output.append(f'<h{level} id="{anchor}">{inline_md(text)}</h{level}>')
            continue
        unordered = re.match(r"^[-*]\s+(.+)$", line)
        ordered = re.match(r"^\d+[.)]\s+(.+)$", line)
        if unordered or ordered:
            flush_paragraph()
            wanted = "ul" if unordered else "ol"
            if list_kind != wanted:
                close_list(); output.append(f"<{wanted}>"); list_kind = wanted
            item = unordered.group(1) if unordered else ordered.group(1)
            output.append(f"<li>{inline_md(item)}</li>")
            continue
        if line.startswith("> "):
            flush_paragraph(); close_list()
            output.append(f"<blockquote>{inline_md(line[2:].strip())}</blockquote>")
            continue
        paragraph.append(line.strip())
    flush_paragraph(); close_list(); flush_table()
    if in_code:
        output.append(f"<pre><code>{html.escape(chr(10).join(code_lines))}</code></pre>")
    return "\n".join(output), toc


def post_markdown_url(slug: str) -> str:
    """Absolute URL of a blog post Markdown twin."""
    return f"{SITE_URL}/blog/{slug}/index.md"


def docs_markdown_url_from_web_path(slug: str) -> str:
    """Map a docs web slug to the Markdown twin URL."""
    slug = slug.strip("/") or "index"
    return f"{SITE_URL}/docs/{slug}/index.md"


def rewrite_blog_markdown_links(body: str) -> str:
    """Rewrite grit-scm.com and relative docs links to Markdown twin URLs."""

    def url_for_path(path: str) -> str | None:
        blog_match = POST_MARKDOWN_LINK_RE.match(path)
        if blog_match:
            return post_markdown_url(blog_match.group("slug"))
        docs_match = DOCS_MARKDOWN_LINK_RE.match(path)
        if docs_match:
            return docs_markdown_url_from_web_path(docs_match.group("slug"))
        if path.startswith("/docs/"):
            rel = path.removeprefix("/docs/").strip("/")
            return docs_markdown_url_from_web_path(rel)
        return None

    return site_util.rewrite_markdown_links(body, url_for_path)


def render_post_markdown_twin(post: Post) -> str:
    """Render the Markdown twin for one blog post."""
    _meta, body = parse_front_matter(post.source.read_text(encoding="utf-8"))
    body = rewrite_blog_markdown_links(body)
    return site_util.compose_markdown_twin(
        post.title,
        body,
        summary=post.summary,
        published=post.published,
    )


def load_posts() -> list[Post]:
    posts: list[Post] = []
    for path in sorted(CONTENT_DIR.glob("*.md")):
        meta, body = parse_front_matter(path.read_text(encoding="utf-8"))
        title = meta.get("title")
        if not title:
            first_heading = next((HEADING_RE.match(line) for line in body.splitlines() if HEADING_RE.match(line)), None)
            title = first_heading.group(2) if first_heading else path.stem.replace("-", " ").title()
        slug = meta.get("slug") or path.stem
        raw_date = meta.get("date")
        if not raw_date:
            raise SystemExit(f"{path}: missing required front matter field 'date'")
        published = date.fromisoformat(raw_date)
        summary = meta.get("summary", "")
        author = meta.get("author", AUTHOR)
        body_html, toc = markdown_to_html(body)
        updated = datetime.fromtimestamp(path.stat().st_mtime, tz=timezone.utc)
        posts.append(Post(slug, title, summary, author, published, updated, path, f"blog/{slug}/", body_html, toc))
    return sorted(posts, key=lambda p: (p.published, p.slug), reverse=True)


FONTS = (
    '<link rel="preconnect" href="https://fonts.googleapis.com" />\n'
    '<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin />\n'
    '<link href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;500;700&family=Space+Grotesk:wght@400;500;700&display=swap" rel="stylesheet" />'
)


def short_sha(post: Post) -> str:
    """Decorative commit-style id for a post, stable across rebuilds."""
    return hashlib.sha1(post.slug.encode("utf-8")).hexdigest()[:7]


def rail(*parts: str) -> str:
    """Render the commit-graph rail; each part is a CSS class for one mark."""
    marks = "".join(f'<span class="{part}"></span>' for part in parts)
    return f'<div class="rail" aria-hidden="true">{marks}</div>'


def page_shell(title: str, description: str, body: str, base: str, blog_href: str, feed_href: str, extra_head: str = "") -> str:
    home_href = f"{base}/" if base != "." else "./"
    return f"""<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\" />
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />
<title>{html.escape(title)}</title>
<meta name=\"description\" content=\"{html.escape(description, quote=True)}\" />
<link rel=\"alternate\" type=\"application/rss+xml\" title=\"Grit blog feed\" href=\"{feed_href}\" />
<script async src=\"https://u.gitbutler.com/script.js\" data-website-id=\"2c6f680c-eaf5-4cd7-a419-1032ffab6bbc\"></script>
{FONTS}
{extra_head}
<style>{CSS}</style>
</head>
<body>
<div class=\"wrap\">
<header class=\"topbar\">
  <a class=\"brand\" href=\"{home_href}\" aria-label=\"grit homepage\">grit</a>
  <nav class=\"nav\" aria-label=\"Primary\">
    <a href=\"{base}/docs/\">Docs</a>
    <a href=\"https://crates.io/crates/grit-lib\">Library</a>
    <a href=\"{blog_href}\">Blog</a>
    <a class=\"pill\" href=\"https://github.com/gitbutlerapp/grit\">GitHub</a>
  </nav>
</header>
{body}
</div>
</body>
</html>
"""


def footer(links: str) -> str:
    return f"""<footer class=\"commit\">
  {rail("line stub", "root-dot")}
  <div class=\"foot\">
    <div class=\"sha\">0000001 · initial commit · by <a href=\"https://gitbutler.com\">GitButler</a></div>
    <div class=\"links\">{links}</div>
  </div>
</footer>"""


def render_index(posts: list[Post]) -> str:
    rows = "\n".join(
        f"""<section class=\"commit\">
  {rail("line", "head-dot small" if i == 0 else "dot")}
  <div class=\"entry\">
    <div class=\"sha{' hot' if i == 0 else ''}\">{short_sha(post)} · <time datetime=\"{post.published.isoformat()}\">{post.published.strftime("%b %-d, %Y")}</time> · {html.escape(post.author)}</div>
    <h2><a href=\"{post.slug}/\">{html.escape(post.title)}</a></h2>
    {f'<p>{html.escape(post.summary)}</p>' if post.summary else ''}
  </div>
</section>"""
        for i, post in enumerate(posts)
    ) or '<section class="commit"><div class="rail"></div><div class="entry"><p>No posts yet.</p></div></section>'
    links = '<a href="../">Home</a><a href="feed.xml">RSS feed</a><a href="https://github.com/gitbutlerapp/grit">GitHub</a>'
    body = f"""<main>
<section class=\"commit\">
  {rail("line from-head", "head-dot")}
  <div class=\"hero\">
    <div class=\"refs\"><span class=\"ref\">HEAD → blog</span><a href=\"feed.xml\">feed.xml</a></div>
    <h1>Blog</h1>
    <p class=\"tagline\">{html.escape(BLOG_TITLE[0].upper() + BLOG_TITLE[1:])}.</p>
    <p class=\"lede\">{html.escape(BLOG_DESCRIPTION)}</p>
  </div>
</section>
{rows}
</main>
{footer(links)}"""
    return page_shell(f"Blog - {SITE_TITLE}", BLOG_DESCRIPTION, body, "..", "./", "feed.xml", '<link rel="canonical" href="./" />')


def render_post(post: Post) -> str:
    toc = "\n".join(f'<li class="toc-level-{item.level}"><a href="#{item.anchor}">{html.escape(item.text)}</a></li>' for item in post.toc)
    aside = f'<aside class="toc" aria-label="On this page"><h2>On this page</h2><ol>{toc}</ol></aside>' if toc else ""
    lede = f'<p class="lede">{html.escape(post.summary)}</p>' if post.summary else ""
    links = '<a href="../">All posts</a><a href="../../">Home</a><a href="../feed.xml">RSS feed</a>'
    body = f"""<main>
<section class=\"commit\">
  {rail("line from-head", "head-dot")}
  <div class=\"hero post-hero\">
    <div class=\"refs\"><a class=\"ref\" href=\"../\">← blog</a><span>{short_sha(post)}</span><time datetime=\"{post.published.isoformat()}\">{post.display_date}</time><span>{html.escape(post.author)}</span></div>
    <h1>{html.escape(post.title)}</h1>
    {lede}
  </div>
</section>
<section class=\"commit\">
  {rail("line")}
  <div class=\"post-body\">
    <article class=\"content\">{post.body_html}</article>
    {aside}
  </div>
</section>
</main>
{footer(links)}"""
    extra = (
        '<link rel="canonical" href="./" />\n'
        '<link rel="alternate" type="text/markdown" href="index.md" />\n'
        f'<meta property="og:title" content="{html.escape(post.title, quote=True)}" />'
    )
    return page_shell(f"{post.title} - {SITE_TITLE}", post.summary or BLOG_DESCRIPTION, body, "../..", "../", "../feed.xml", extra)


def render_feed(posts: list[Post]) -> str:
    items = "\n".join(f"""  <item>
    <title>{xml_escape(post.title)}</title>
    <link>{SITE_URL}/{post.url}</link>
    <guid isPermaLink=\"true\">{SITE_URL}/{post.url}</guid>
    <pubDate>{post.rfc822_date}</pubDate>
    <description>{xml_escape(post.summary)}</description>
    <content:encoded><![CDATA[{post.body_html}]]></content:encoded>
  </item>""" for post in posts)
    latest = posts[0].rfc822_date if posts else email.utils.format_datetime(EMPTY_FEED_LAST_BUILD)
    return f"""<?xml version=\"1.0\" encoding=\"utf-8\"?>
<rss version=\"2.0\" xmlns:content=\"http://purl.org/rss/1.0/modules/content/\">
<channel>
  <title>{xml_escape(BLOG_TITLE)}</title>
  <link>{SITE_URL}/blog/</link>
  <description>{xml_escape(BLOG_DESCRIPTION)}</description>
  <lastBuildDate>{latest}</lastBuildDate>
{items}
</channel>
</rss>
"""


CSS = r'''
:root{--bg:#f4f1ea;--ink:#1b1915;--soft:#3d3830;--muted:#7b7466;--line:#d9d3c5;--accent:#e2481f;--code-fg:#e9e4d8;--chip:#e9e4d8;--rail:clamp(48px,8vw,120px);--mono:"JetBrains Mono",ui-monospace,SFMono-Regular,Menlo,monospace}
*{box-sizing:border-box}html{scroll-behavior:smooth}
body{margin:0;min-height:100vh;background:var(--bg);color:var(--ink);font-family:"Space Grotesk",system-ui,sans-serif}
a{color:inherit}a:hover{opacity:.7}
.wrap{max-width:1200px;margin:0 auto;padding:0 clamp(20px,4vw,56px)}
.topbar{display:flex;justify-content:space-between;align-items:center;flex-wrap:wrap;gap:16px;padding:24px 0}
.brand{display:flex;align-items:center;gap:10px;font-size:20px;font-weight:700;text-decoration:none}
.brand::before{content:"";width:14px;height:14px;border-radius:50%;background:var(--accent)}
.nav{display:flex;flex-wrap:wrap;align-items:center;gap:28px;font-size:15px}.nav a{text-decoration:none}
.nav .pill{background:var(--ink);color:var(--bg);padding:9px 16px;border-radius:999px}
.commit{display:grid;grid-template-columns:var(--rail) minmax(0,1fr)}
.commit+.commit{border-top:1px solid var(--line)}
.rail{position:relative}.rail>span{position:absolute;display:block}
.line{left:20px;top:0;bottom:0;width:3px;background:var(--ink)}
.line.from-head{top:110px}.line.stub{bottom:auto;height:46px}
.dot{left:15px;top:44px;width:13px;height:13px;border-radius:50%;background:var(--ink)}
.head-dot{left:8px;top:96px;width:27px;height:27px;border-radius:50%;background:var(--accent);box-shadow:0 0 0 4px var(--bg),0 0 0 7px var(--ink)}
.head-dot.small{left:13px;top:41px;width:17px;height:17px;box-shadow:0 0 0 3px var(--bg),0 0 0 6px var(--ink)}
.root-dot{left:14px;top:44px;width:15px;height:15px;border-radius:50%;background:var(--bg);box-shadow:0 0 0 3px var(--ink)}
.hero{padding:56px 0 72px}
.refs{display:flex;flex-wrap:wrap;gap:14px;font:14px var(--mono);color:var(--muted)}
.refs a{text-decoration:none}.refs .ref{color:var(--accent)}
h1{margin:20px 0 0;font-size:clamp(80px,14vw,200px);line-height:.85;letter-spacing:-.06em;font-weight:700}
.post-hero h1{font-size:clamp(44px,7vw,88px);line-height:.95;letter-spacing:-.045em;max-width:16ch;text-wrap:balance}
.tagline{margin:24px 0 0;font-size:clamp(26px,3.4vw,40px);line-height:1.1;letter-spacing:-.025em;font-weight:500}
.lede{max-width:640px;margin:20px 0 0;font-size:clamp(17px,1.6vw,20px);line-height:1.5;color:var(--soft);text-wrap:pretty}
.entry{padding:36px 0 40px}
.sha{font:13px var(--mono);color:var(--muted)}.sha.hot{color:var(--accent)}.sha a{color:inherit}
.entry h2{margin:12px 0 0;font-size:clamp(28px,3.4vw,40px);line-height:1.05;letter-spacing:-.03em}
.entry h2 a{text-decoration:none}
.entry p{max-width:680px;margin:14px 0 0;font-size:18px;line-height:1.55;color:var(--soft);text-wrap:pretty}
.post-body{display:grid;grid-template-columns:minmax(0,720px) minmax(0,1fr);gap:56px;padding:48px 0 64px}
.content{font-size:19px;line-height:1.65;color:var(--soft)}
.content>:first-child{margin-top:0}
.content h2{margin:2.2em 0 .5em;font-size:clamp(26px,3vw,34px);line-height:1.1;letter-spacing:-.025em;color:var(--ink)}
.content h3{margin:1.8em 0 .4em;font-size:22px;letter-spacing:-.015em;color:var(--ink)}
.content p{margin:0 0 1.1em;text-wrap:pretty}
.content ul,.content ol{margin:0 0 1.1em;padding-left:1.2em}.content li{margin:.3em 0}
.content li::marker{color:var(--accent)}
.content a{color:var(--ink);text-decoration-color:var(--accent);text-underline-offset:3px}
.content strong{color:var(--ink)}
.content code{font:.86em var(--mono);background:var(--chip);border-radius:6px;padding:.1em .35em}
.content pre{margin:1.4em 0;background:var(--ink);color:var(--code-fg);border-radius:14px;padding:22px 26px;font:14px/1.75 var(--mono);overflow-x:auto}
.content pre code{background:none;padding:0;font-size:inherit;color:inherit}
.content blockquote{margin:1.4em 0;padding:.1em 0 .1em 20px;border-left:3px solid var(--accent);color:var(--ink);font-size:21px;line-height:1.45}
.content .table{margin:1.4em 0;overflow-x:auto}
.content table{width:100%;border-collapse:collapse;font-size:16px;line-height:1.5}
.content th{text-align:left;font:13px var(--mono);color:var(--muted);font-weight:500;padding:0 16px 10px 0;border-bottom:1px solid var(--line)}
.content td{padding:12px 16px 12px 0;border-bottom:1px solid var(--line);vertical-align:top}
.content td{overflow-wrap:anywhere}.content td code{white-space:nowrap}
.content td:first-child{overflow-wrap:normal}
.toc{position:sticky;top:24px;align-self:start;font:13px/1.5 var(--mono);color:var(--muted)}
.toc h2{margin:0 0 12px;font:inherit;color:var(--accent)}
.toc ol{list-style:none;margin:0;padding:0;border-left:1px solid var(--line)}
.toc li{margin:0 0 8px;padding-left:14px}.toc a{text-decoration:none}.toc-level-3{padding-left:28px}
.foot{padding:40px 0 64px}
.foot .links{display:flex;flex-wrap:wrap;gap:28px;margin-top:14px;font-size:17px}
@media(max-width:900px){.post-body{display:block}.toc{display:none}}
'''


def generate(out_dir: Path) -> None:
    posts = load_posts()
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)
    (out_dir / "index.html").write_text(render_index(posts), encoding="utf-8")
    (out_dir / "feed.xml").write_text(render_feed(posts), encoding="utf-8")
    for post in posts:
        post_dir = out_dir / post.slug
        post_dir.mkdir(parents=True)
        (post_dir / "index.html").write_text(render_post(post), encoding="utf-8")
        (post_dir / "index.md").write_text(render_post_markdown_twin(post), encoding="utf-8")


def check_committed() -> int:
    with tempfile.TemporaryDirectory(prefix="grit-blog-check-") as tmp:
        generated = Path(tmp) / "blog"
        generate(generated)
        issues = site_util.compare_directories(generated, OUT_DIR, label="blog")
        if issues:
            for line in issues:
                print(line, file=sys.stderr)
            print("blog output is stale; run: make docs", file=sys.stderr)
            return 1
    print(f"blog output is up to date ({OUT_DIR.relative_to(ROOT)})")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="Render to a temp directory and fail if docs/blog/ differs from committed output.",
    )
    args = parser.parse_args(argv)
    if args.check:
        return check_committed()
    generate(OUT_DIR)
    posts = load_posts()
    print(f"generated {len(posts)} blog post(s) in {OUT_DIR.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
