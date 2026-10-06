#!/usr/bin/env python3
"""Generate dashboard docs from the per-test status TOMLs in data/tests/."""

from __future__ import annotations

import html
import subprocess
import sys
import urllib.parse
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_status import load_all  # noqa: E402

REPO = Path(__file__).resolve().parent.parent
OUT_INDEX = REPO / "docs" / "progress" / "index.html"
OUT_FILES = REPO / "docs" / "testfiles.html"
OUT_SVG = REPO / "docs" / "test-progress.svg"

# Published site root (GitHub Pages) for absolute og/twitter image URLs on index.html.
GITHUB_PAGES_SITE = "https://gitbutlerapp.github.io/grit"

# Harness file family labels (first digit after ``t`` = group t0–t9).
GROUP_DESC: dict[str, str] = {
    "t0": "Absolute basics and global stuff",
    "t1": "Basic commands concerning the database",
    "t2": "Basic commands concerning the working tree",
    "t3": "Other basic commands (e.g. ls-files)",
    "t4": "Diff commands",
    "t5": "Pull and exporting commands",
    "t6": "Revision tree commands (e.g. merge-base)",
    "t7": "Porcelainish commands concerning the working tree",
    "t8": "Porcelainish commands concerning forensics",
    "t9": "Git tools",
}

def git_full_sha() -> str:
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=REPO, text=True
        ).strip()
    except Exception:
        return ""


def git_short_sha() -> str:
    full = git_full_sha()
    return full[:7] if len(full) >= 7 else (full if full else "unknown")


def github_commit_url(sha: str) -> str | None:
    """Return an https://github.com/.../commit/SHA URL if ``origin`` is GitHub."""
    if not sha or sha == "unknown":
        return None
    try:
        raw = subprocess.check_output(
            ["git", "config", "--get", "remote.origin.url"],
            cwd=REPO,
            text=True,
        ).strip()
    except Exception:
        return None
    raw = raw.rstrip("/")
    if raw.endswith(".git"):
        raw = raw[:-4]
    owner: str | None = None
    repo: str | None = None
    if raw.startswith("git@"):
        host_and_rest = raw.partition("@")[2]
        domain, _, path = host_and_rest.partition(":")
        if domain != "github.com" or "/" not in path:
            return None
        owner, repo = path.split("/", 1)
    elif "github.com/" in raw:
        after = raw.split("github.com/", 1)[1]
        segs = after.strip("/").split("/")
        if len(segs) >= 2:
            owner, repo = segs[0], segs[1]
    if not owner or not repo:
        return None
    repo = repo.removesuffix(".git")
    return f"https://github.com/{owner}/{repo}/commit/{sha}"


def generated_time_element(now: datetime) -> str:
    """Markup for build time: absolute in ``title``, relative text set by JS."""
    gen_time = now.strftime("%Y-%m-%d %H:%M UTC")
    iso = now.replace(microsecond=0).isoformat()
    return (
        f'<time datetime="{html.escape(iso)}" class="gen-time" title="{html.escape(gen_time)}">'
        f"{html.escape(gen_time)}</time>"
    )


def sha_link_html(sha_short: str, sha_full: str) -> str:
    """Short SHA, linking to GitHub commit when ``origin`` is GitHub."""
    url = github_commit_url(sha_full)
    if url:
        return (
            f'<a href="{html.escape(url)}" style="color:#58a6ff">{html.escape(sha_short)}</a>'
        )
    return html.escape(sha_short)


RELATIVE_TIME_JS = """
<script>
(function() {
  document.querySelectorAll('time.gen-time').forEach(function(el) {
    var dt = el.getAttribute('datetime');
    if (!dt) return;
    var d = new Date(dt);
    if (isNaN(d.getTime())) return;
    var rtf = new Intl.RelativeTimeFormat('en', { numeric: 'auto' });
    var now = new Date();
    var diffSec = (d - now) / 1000;
    var abs = Math.abs(diffSec);
    var v;
    var unit;
    if (abs < 60) { v = Math.round(diffSec); unit = 'second'; }
    else if (abs < 3600) { v = Math.round(diffSec / 60); unit = 'minute'; }
    else if (abs < 86400) { v = Math.round(diffSec / 3600); unit = 'hour'; }
    else if (abs < 604800) { v = Math.round(diffSec / 86400); unit = 'day'; }
    else if (abs < 2629800) { v = Math.round(diffSec / 604800); unit = 'week'; }
    else if (abs < 31536000) { v = Math.round(diffSec / 2629800); unit = 'month'; }
    else { v = Math.round(diffSec / 31536000); unit = 'year'; }
    el.textContent = rtf.format(v, unit);
  });
})();
</script>
"""


def load_rows() -> list[dict[str, str]]:
    """Status rows with all values as strings (legacy CSV-shaped rows)."""
    rows: list[dict[str, str]] = []
    for r in load_all().values():
        rows.append(
            {
                "file": r["file"],
                "group": r["group"],
                "in_scope": r["in_scope"],
                "tests_total": str(r["tests_total"]),
                "passed_last": str(r["passed_last"]),
                "failing": str(r["failing"]),
                "fully_passing": "true" if r["fully_passing"] else "false",
                "status": r["status"],
                "expect_failure": str(r["expect_failure"]),
            }
        )
    return rows


def pct(n: int, d: int) -> float:
    return round(100.0 * n / d, 1) if d > 0 else 0.0


def harness_summary(rows: list[dict[str, str]]) -> dict[str, int | float]:
    """Aggregate counts for in-scope harness files (same rules as the dashboard hero)."""
    in_scope = [r for r in rows if r.get("in_scope", "yes").strip().lower() != "skip"]
    skipped = [r for r in rows if r.get("in_scope", "yes").strip().lower() == "skip"]
    total_tests = 0
    total_pass = 0
    full_files = 0
    for r in in_scope:
        try:
            tt = int(r.get("tests_total") or 0)
            pl = int(r.get("passed_last") or 0)
        except ValueError:
            continue
        total_tests += tt
        total_pass += pl
        fp = (r.get("fully_passing") or "").strip().lower() == "true"
        if fp and tt > 0:
            full_files += 1
    return {
        "file_count": len(in_scope),
        "total_tests": total_tests,
        "total_pass": total_pass,
        "full_files": full_files,
        "skipped_files": len(skipped),
        "pass_rate": pct(total_pass, total_tests),
    }


def pass_rate_band(pc: float) -> str:
    """CSS class suffix for pass-rate coloring: red / orange / blue / green."""
    if pc < 40.0:
        return "pct-red"
    if pc < 60.0:
        return "pct-orange"
    if pc < 80.0:
        return "pct-blue"
    return "pct-green"


SVG_BAR_FILL: dict[str, str] = {
    "pct-red": "#da3633",
    "pct-orange": "#d29922",
    "pct-blue": "#58a6ff",
    "pct-green": "#3fb950",
}


def xml_escape(text: str) -> str:
    return (
        text.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
    )


def generate_progress_svg(rows: list[dict[str, str]]) -> str:
    """Static SVG summary of overall harness pass rate (for README and embedding)."""
    stats = harness_summary(rows)
    total_tests = int(stats["total_tests"])
    total_pass = int(stats["total_pass"])
    file_count = int(stats["file_count"])
    full_files = int(stats["full_files"])
    skipped_n = int(stats["skipped_files"])
    pr = float(stats["pass_rate"])
    band = pass_rate_band(pr)
    fill = SVG_BAR_FILL[band]
    bar_w = 700
    fg_w = max(0, min(bar_w, int(round(bar_w * pr / 100.0))))
    sub = (
        f"{pr:g}% pass rate · {total_pass:,} / {total_tests:,} tests · "
        f"{file_count:,} files in scope · {full_files:,} fully passing · "
        f"{skipped_n:,} skipped"
    )
    desc = (
        f"{total_pass} of {total_tests} tests passing across {file_count} harness files "
        f"({pr:g}% pass rate)."
    )
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 780 132" width="780" height="132" role="img" aria-labelledby="svg-title svg-desc">
  <title id="svg-title">{xml_escape("Grit harness: upstream test progress")}</title>
  <desc id="svg-desc">{xml_escape(desc)}</desc>
  <rect x="0" y="0" width="780" height="132" rx="6" fill="#0d1117" stroke="#30363d"/>
  <text x="40" y="38" fill="#f0f6fc" font-family="ui-sans-serif,system-ui,sans-serif" font-size="20" font-weight="600">{xml_escape("Grit harness test progress")}</text>
  <text x="40" y="64" fill="#8b949e" font-family="ui-sans-serif,system-ui,sans-serif" font-size="13">{xml_escape(sub)}</text>
  <rect x="40" y="80" width="{bar_w}" height="18" rx="9" fill="#21262d" stroke="#30363d"/>
  <rect x="40" y="80" width="{fg_w}" height="18" rx="9" fill="{fill}"/>
  <text x="40" y="118" fill="#6e7681" font-family="ui-monospace,monospace" font-size="11">{xml_escape("Generated from data/tests/")}</text>
</svg>
"""


def group_summaries(rows: list[dict[str, str]]) -> dict[str, dict[str, int]]:
    """Return aggregate test/file counts for each in-scope test group."""
    groups: dict[str, dict[str, int]] = {}
    for r in rows:
        if r.get("in_scope", "yes").strip().lower() == "skip":
            continue
        g = r.get("group") or "t?"
        if g not in groups:
            groups[g] = {"tests": 0, "pass": 0, "files": 0, "full": 0}
        try:
            tt = int(r.get("tests_total") or 0)
            pl = int(r.get("passed_last") or 0)
        except ValueError:
            tt, pl = 0, 0
        groups[g]["tests"] += tt
        groups[g]["pass"] += pl
        groups[g]["files"] += 1
        if (r.get("fully_passing") or "").lower() == "true" and tt > 0:
            groups[g]["full"] += 1
    return groups


def generate_index(rows: list[dict[str, str]]) -> str:
    stats = harness_summary(rows)
    total_tests = int(stats["total_tests"])
    total_pass = int(stats["total_pass"])
    full_files = int(stats["full_files"])
    file_count = int(stats["file_count"])
    pass_rate = float(stats["pass_rate"])
    skipped_n = int(stats["skipped_files"])

    now = datetime.now(timezone.utc)
    sha_full = git_full_sha()
    sha = git_short_sha()
    time_el = generated_time_element(now)
    sha_l = sha_link_html(sha, sha_full)

    groups = group_summaries(rows)
    total_remaining = max(total_tests - total_pass, 0)

    skipped_by_group: dict[str, int] = {}
    for r in rows:
        if r.get("in_scope", "yes").strip().lower() != "skip":
            continue
        g = r.get("group") or "t?"
        skipped_by_group[g] = skipped_by_group.get(g, 0) + 1

    order = sorted(groups.keys(), key=lambda x: (len(x), x))

    overall_band = pass_rate_band(pass_rate)

    og_desc = (
        f"{pass_rate:g}% of harness tests passing "
        f"({total_pass:,} / {total_tests:,})."
    )
    card_image_url = f"{GITHUB_PAGES_SITE}/test-progress.svg"

    group_html = ""
    for g in order:
        st = groups[g]
        desc = GROUP_DESC.get(g, "Tests")
        ttot, tpass = st["tests"], st["pass"]
        remaining = max(ttot - tpass, 0)
        remaining_share = pct(remaining, total_remaining)
        pc = pct(tpass, ttot)
        band = pass_rate_band(pc)
        q = urllib.parse.urlencode({"group": g})
        href = f"../testfiles.html?{q}"
        n_skip = skipped_by_group.get(g, 0)
        group_html += f"""
    <a class="group-card" href="{html.escape(href)}">
      <div class="group-line1">
        <span class="group-id">{html.escape(g)}</span>
        <span class="group-desc">{html.escape(desc)}</span>
      </div>
      <div class="group-line2">
        <div class="bar-bg"><div class="bar-fg {band}" style="width:{pc}%"></div></div>
        <span class="group-pct {band}">{pc}%</span>
      </div>
      <footer class="group-footer">
        <span class="group-footer-main">{st["full"]}/{st["files"]} files · {tpass:,}/{ttot:,} tests · {n_skip} skipped</span>
        <span class="group-footer-todo" aria-label="Remaining tests">
          <svg class="group-footer-icon" viewBox="0 0 24 24" aria-hidden="true">
            <path d="M9 6h11M9 12h11M9 18h11M4 6l1 1 2-2M4 12l1 1 2-2M4 18l1 1 2-2"></path>
          </svg>
          {remaining:,} ({remaining_share}%)
        </span>
      </footer>
    </a>"""

    return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Grit Project Progress</title>
<meta property="og:title" content="Grit Project Progress">
<meta property="og:description" content="{html.escape(og_desc)}">
<meta property="og:image" content="{html.escape(card_image_url)}">
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:title" content="Grit Project Progress">
<meta name="twitter:description" content="{html.escape(og_desc)}">
<meta name="twitter:image" content="{html.escape(card_image_url)}">
<style>
* {{ margin: 0; padding: 0; box-sizing: border-box; }}
body {{
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', system-ui, sans-serif;
  background: #0d1117;
  color: #e6edf3;
  padding: 2rem;
  max-width: 960px;
  margin: 0 auto;
}}
h1 {{ font-size: 1.75rem; margin-bottom: 0.25rem; color: #f0f6fc; }}
.sub {{ color: #7d8590; font-size: 0.9rem; margin-bottom: 1.75rem; }}
.sub time.gen-time {{ color: inherit; }}
.summary-hero {{
  margin-bottom: 2rem;
}}
.summary-big {{
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  gap: 1rem 1.5rem;
  margin-bottom: 0.75rem;
  text-align: center;
}}
.summary-big-val {{
  font-size: 2.25rem;
  font-weight: 700;
  color: #f0f6fc;
  line-height: 1.15;
  letter-spacing: -0.02em;
}}
.summary-big-val.pct-red {{ color: #f85149; }}
.summary-big-val.pct-orange {{ color: #e3b341; }}
.summary-big-val.pct-blue {{ color: #79c0ff; }}
.summary-big-val.pct-green {{ color: #3fb950; }}
.summary-big-lbl {{
  font-size: 0.72rem;
  color: #7d8590;
  margin-top: 0.4rem;
  text-transform: uppercase;
  letter-spacing: 0.06em;
}}
.summary-bar-wrap {{
  margin-bottom: 0.85rem;
}}
.summary-bar-bg {{
  height: 12px;
  border-radius: 6px;
  background: #21262d;
  overflow: hidden;
  border: 1px solid #30363d;
}}
.summary-bar-fg {{
  height: 100%;
  background: linear-gradient(90deg, #238636, #2ea043);
  border-radius: 6px 0 0 6px;
}}
.summary-meta {{
  font-size: 0.78rem;
  color: #6e7681;
  display: flex;
  flex-wrap: wrap;
  gap: 0.35rem 0.5rem;
  justify-content: center;
  align-items: baseline;
}}
.summary-meta-sep {{
  color: #484f58;
  user-select: none;
}}
.section-head {{
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: 0.5rem 1rem;
  margin-bottom: 1rem;
}}
.section-head h2 {{
  font-size: 1.1rem;
  margin: 0;
  color: #f0f6fc;
  font-weight: 600;
}}
.section-hint {{
  color: #7d8590;
  font-size: 0.85rem;
  margin: 0;
  flex: 1 1 auto;
  min-width: min(100%, 12rem);
}}
.group-line2 .bar-fg.pct-red {{ background: linear-gradient(90deg, #8b2020, #da3633); }}
.group-line2 .bar-fg.pct-orange {{ background: linear-gradient(90deg, #8b6914, #d29922); }}
.group-line2 .bar-fg.pct-blue {{ background: linear-gradient(90deg, #1f4f8f, #58a6ff); }}
.group-line2 .bar-fg.pct-green {{ background: linear-gradient(90deg, #238636, #2ea043); }}
.group-grid {{
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 0.6rem;
}}
.group-card {{
  display: flex;
  flex-direction: column;
  background: #161b22;
  border: 1px solid #30363d;
  border-radius: 8px;
  padding: 0.78rem 0.9rem;
  overflow: hidden;
  text-decoration: none;
  color: inherit;
  transition: border-color 0.15s;
}}
.group-card:hover {{ border-color: #58a6ff; }}
.group-line1 {{
  display: flex;
  align-items: baseline;
  flex-wrap: nowrap;
  gap: 0.45rem 0.65rem;
  margin-bottom: 0.52rem;
  min-width: 0;
}}
.group-id {{ flex-shrink: 0; font-weight: 700; color: #58a6ff; font-size: 1rem; }}
.group-desc {{
  flex: 1 1 0;
  min-width: 0;
  font-size: 0.85rem;
  color: #8b949e;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}}
.group-line2 {{ display: flex; align-items: center; gap: 0.65rem; margin-bottom: 0.68rem; min-width: 0; }}
.group-line2 .bar-bg {{ flex: 1 1 auto; min-width: 0; }}
.bar-bg {{ background: #1a2028; border-radius: 6px; height: 8px; overflow: hidden; border: 1px solid #27313d; }}
.group-line2 .bar-fg {{ height: 100%; border-radius: 6px 0 0 6px; }}
.group-pct {{ flex-shrink: 0; font-size: 0.8rem; font-weight: 600; }}
.group-pct.pct-red {{ color: #f85149; }}
.group-pct.pct-orange {{ color: #e3b341; }}
.group-pct.pct-blue {{ color: #79c0ff; }}
.group-pct.pct-green {{ color: #3fb950; }}
.group-footer {{
  margin-top: auto;
  margin-right: -0.9rem;
  margin-bottom: -0.78rem;
  margin-left: -0.9rem;
  padding: 0.52rem 0.9rem;
  background: #0f141b;
  border-top: 1px solid #27313d;
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.65rem;
  font-size: 0.72rem;
  line-height: 1.45;
  color: #7d8590;
}}
.group-footer-main {{
  flex: 1 1 auto;
  min-width: 0;
}}
.group-footer-todo {{
  margin-left: auto;
  display: inline-flex;
  align-items: center;
  gap: 0.3rem;
  flex-shrink: 0;
  color: #8b949e;
}}
.group-footer-icon {{
  width: 0.86rem;
  height: 0.86rem;
  fill: none;
  stroke: currentColor;
  stroke-width: 2;
  stroke-linecap: round;
  stroke-linejoin: round;
}}
@media (max-width: 760px) {{
  body {{ padding: 1.25rem; }}
  .summary-big {{ grid-template-columns: 1fr; text-align: left; }}
  .summary-big-val {{ font-size: 1.8rem; }}
  .summary-meta {{ justify-content: flex-start; }}
  .group-grid {{ grid-template-columns: 1fr; }}
}}
</style>
</head>
<body>
<h1>Grit Project Progress</h1>
<p class="sub">Generated {time_el} · {sha_l} · <a href="../testfiles.html" style="color:#58a6ff">All test files</a></p>

<section class="summary-hero" aria-label="Overall test progress">
  <div class="summary-big">
    <div class="summary-big-item">
      <div class="summary-big-val {overall_band}">{pass_rate}%</div>
      <div class="summary-big-lbl">Passing</div>
    </div>
    <div class="summary-big-item">
      <div class="summary-big-val">{total_pass:,}</div>
      <div class="summary-big-lbl">Tests passed</div>
    </div>
    <div class="summary-big-item">
      <div class="summary-big-val">{total_tests:,}</div>
      <div class="summary-big-lbl">Total tests</div>
    </div>
  </div>
  <div class="summary-bar-wrap" aria-hidden="true">
    <div class="summary-bar-bg"><div class="summary-bar-fg" style="width:{pass_rate}%"></div></div>
  </div>
  <p class="summary-meta">
    <span>{file_count:,} test files (in scope)</span>
    <span class="summary-meta-sep" aria-hidden="true">·</span>
    <span>{full_files:,} files fully passing</span>
    <span class="summary-meta-sep" aria-hidden="true">·</span>
    <span>{skipped_n:,} skipped files</span>
  </p>
</section>

<div class="section-head">
  <h2>Git Testing Family Groups</h2>
  <p class="section-hint">Click a group for per-file detail. Counts exclude manually skipped files.</p>
</div>
<div class="group-grid">
{group_html}
</div>
{RELATIVE_TIME_JS}
</body>
</html>
"""


def generate_testfiles(rows: list[dict[str, str]]) -> str:
    now = datetime.now(timezone.utc)
    sha_full = git_full_sha()
    sha = git_short_sha()
    time_el = generated_time_element(now)
    sha_l = sha_link_html(sha, sha_full)

    in_scope = [r for r in rows if r.get("in_scope", "yes").strip().lower() != "skip"]
    skipped_rows = [r for r in rows if r.get("in_scope", "yes").strip().lower() == "skip"]

    stats = harness_summary(rows)
    total_tests = int(stats["total_tests"])
    total_pass = int(stats["total_pass"])
    file_count = int(stats["file_count"])
    pass_rate = float(stats["pass_rate"])

    groups_order = sorted({r.get("group") or "t?" for r in rows}, key=lambda x: (len(x), x))

    table_rows = ""
    for r in sorted(rows, key=lambda x: x.get("file", "")):
        base = r.get("file", "")
        g = r.get("group", "")
        iscope = r.get("in_scope", "yes").strip().lower()
        is_skip = iscope == "skip"
        try:
            tt = int(r.get("tests_total") or 0)
            pl = int(r.get("passed_last") or 0)
            fl = int(r.get("failing") or 0)
        except ValueError:
            tt, pl, fl = 0, 0, 0
        fp = (r.get("fully_passing") or "").strip().lower() == "true"
        st = r.get("status", "")
        ef = r.get("expect_failure", "0")
        skip_badge = (
            '<span class="badge skip">skipped</span>' if is_skip else ""
        )
        fp_badge = (
            '<span class="badge ok">full pass</span>'
            if fp and tt > 0 and not is_skip
            else ""
        )
        pc = pct(pl, tt) if tt > 0 else 0.0
        row_cls = "row-skip" if is_skip else ""
        full_pass_attr = (
            ' data-full-pass="1"' if fp and tt > 0 and not is_skip else ""
        )
        table_rows += f"""
<tr class="{row_cls}" data-group="{html.escape(g)}" data-tests="{tt}" data-passed="{pl}"{full_pass_attr}>
  <td class="mono">{html.escape(base)}</td>
  <td>{html.escape(g)}</td>
  <td>{skip_badge}{fp_badge}</td>
  <td class="right">{tt if tt or not is_skip else "—"}</td>
  <td class="right">{pl if not is_skip else "—"}</td>
  <td class="right">{"—" if is_skip else html.escape(st)}</td>
  <td class="bar"><div class="bar-bg"><div class="bar-fg" style="width:{pc if not is_skip else 0}%"></div></div></td>
  <td class="right small">{html.escape(ef)}</td>
</tr>"""

    options = '<option value="">All groups</option>\n'
    for g in groups_order:
        lab = f"{g} — {GROUP_DESC.get(g, '')}"
        options += f'  <option value="{html.escape(g)}">{html.escape(lab)}</option>\n'

    return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Grit test files</title>
<style>
* {{ margin: 0; padding: 0; box-sizing: border-box; }}
body {{
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', system-ui, sans-serif;
  background: #0d1117;
  color: #e6edf3;
  padding: 2rem;
  max-width: 1200px;
  margin: 0 auto;
}}
h1 {{ font-size: 1.5rem; margin-bottom: 0.25rem; }}
.sub {{ color: #7d8590; margin-bottom: 1.25rem; font-size: 0.9rem; }}
.sub time.gen-time {{ color: inherit; }}
a {{ color: #58a6ff; text-decoration: none; }}
a:hover {{ text-decoration: underline; }}
.toolbar {{ display: flex; flex-wrap: wrap; gap: 0.75rem; align-items: center; margin-bottom: 1rem; }}
.toolbar label.work-only {{
  display: inline-flex;
  align-items: center;
  gap: 0.45rem;
  font-size: 0.85rem;
  color: #e6edf3;
  cursor: pointer;
  user-select: none;
}}
.toolbar label.work-only input {{
  width: auto;
  min-width: unset;
  margin: 0;
  cursor: pointer;
  accent-color: #58a6ff;
}}
select, input {{
  background: #161b22;
  border: 1px solid #30363d;
  border-radius: 6px;
  color: #e6edf3;
  padding: 0.45rem 0.65rem;
  font-size: 0.85rem;
}}
input {{ min-width: 220px; }}
table {{ width: 100%; border-collapse: collapse; }}
th {{
  text-align: left;
  padding: 0.5rem 0.5rem;
  font-size: 0.72rem;
  color: #7d8590;
  text-transform: uppercase;
  letter-spacing: 0.04em;
  border-bottom: 1px solid #21262d;
}}
td {{ padding: 0.45rem 0.5rem; font-size: 0.84rem; border-bottom: 1px solid #161b22; }}
tr:hover td {{ background: #161b22; }}
tr.row-skip td {{ opacity: 0.65; }}
.mono {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 0.82rem; }}
.right {{ text-align: right; }}
.small {{ font-size: 0.78rem; color: #7d8590; }}
.bar {{ width: 100px; }}
.bar-bg {{ background: #21262d; border-radius: 4px; height: 8px; overflow: hidden; }}
.bar-fg {{ height: 100%; background: linear-gradient(90deg, #238636, #2ea043); border-radius: 4px 0 0 4px; }}
.badge {{ font-size: 0.72rem; padding: 0.15rem 0.4rem; border-radius: 4px; margin-right: 0.35rem; }}
.badge.skip {{ background: #3d2f00; color: #d29922; border: 1px solid #6e4b0a; }}
.badge.ok {{ background: #0d2818; color: #3fb950; border: 1px solid #238636; }}
.hint {{ color: #7d8590; font-size: 0.82rem; margin-top: 1rem; }}
.summary-cards {{
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(130px, 1fr));
  gap: 1rem;
  margin-bottom: 1.5rem;
}}
.summary-cards .card {{
  background: #161b22;
  border: 1px solid #30363d;
  border-radius: 8px;
  padding: 1rem;
  text-align: center;
}}
.summary-cards .card .n {{ font-size: 1.5rem; font-weight: 700; color: #f0f6fc; }}
.summary-cards .card.accent .n {{ color: #3fb950; }}
.summary-cards .card .lbl {{
  font-size: 0.72rem;
  color: #7d8590;
  margin-top: 0.35rem;
  text-transform: uppercase;
  letter-spacing: 0.04em;
}}
.summary-note {{
  color: #7d8590;
  font-size: 0.82rem;
  margin: -0.5rem 0 1rem;
  min-height: 1.2em;
}}
</style>
</head>
<body>
<h1>Test files</h1>
<p class="sub"><a href="progress/">Dashboard</a> · {time_el} · {sha_l}</p>

<div class="summary-cards" id="summaryCards" aria-label="Aggregate counts for the current view (group, search, and Remaining work only)" aria-live="polite">
  <div class="card"><div class="n" id="sum-files">{file_count:,}</div><div class="lbl">Files in scope</div></div>
  <div class="card"><div class="n" id="sum-tests">{total_tests:,}</div><div class="lbl">Unskipped tests</div></div>
  <div class="card accent"><div class="n" id="sum-passed">{total_pass:,}</div><div class="lbl">Tests passed</div></div>
  <div class="card accent"><div class="n" id="sum-pct">{pass_rate}%</div><div class="lbl">Pass rate</div></div>
</div>
<p class="summary-note" id="summaryNote"></p>

<div class="toolbar">
  <label for="groupSel">Group</label>
  <select id="groupSel" aria-label="Filter by group">{options}</select>
  <input type="search" id="search" placeholder="Filter by file name…" aria-label="Search">
  <label class="work-only" title="Hide manually skipped files and in-scope files that fully pass all tests">
    <input type="checkbox" id="workOnly" aria-describedby="summaryNote">
    Remaining work only
  </label>
  <span id="count" class="sub"></span>
</div>

<table>
<thead>
<tr>
  <th>File</th>
  <th>Group</th>
  <th>Scope</th>
  <th class="right">Tests</th>
  <th class="right">Passed</th>
  <th>Status</th>
  <th>Progress</th>
  <th class="right">expect_failure</th>
</tr>
</thead>
<tbody id="tbody">
{table_rows}
</tbody>
</table>
<p class="hint">Manually skipped files are marked and excluded from the aggregate cards (totals follow visible rows when you filter). Use <strong>Remaining work only</strong> to hide skipped and fully passing files and scope the cards to work left to do. The same exclusions apply to dashboard totals on the main page. Rows with <code>expect_failure</code> count known-breakage stubs in the harness.</p>

<script>
(function() {{
  const params = new URLSearchParams(window.location.search);
  const initial = params.get('group') || '';
  const sel = document.getElementById('groupSel');
  const search = document.getElementById('search');
  const workOnly = document.getElementById('workOnly');
  sel.value = initial;
  workOnly.checked = params.get('remaining') === '1' || params.get('remaining') === 'true';

  function apply() {{
    const g = sel.value;
    const q = (search.value || '').toLowerCase();
    const onlyWork = workOnly.checked;
    const rows = document.querySelectorAll('#tbody tr');
    let nShown = 0;
    let files = 0, tests = 0, passed = 0;
    rows.forEach(row => {{
      const rg = row.dataset.group || '';
      const file = row.cells[0].textContent.toLowerCase();
      const okG = !g || rg === g;
      const okQ = !q || file.includes(q);
      const isSkip = row.classList.contains('row-skip');
      const isFullPass = row.dataset.fullPass === '1';
      const isWorkRow = !isSkip && !isFullPass;
      const show = okG && okQ && (!onlyWork || isWorkRow);
      row.style.display = show ? '' : 'none';
      if (show) {{
        nShown++;
        if (onlyWork) {{
          files++;
          tests += parseInt(row.dataset.tests || '0', 10);
          passed += parseInt(row.dataset.passed || '0', 10);
        }} else if (!isSkip) {{
          files++;
          tests += parseInt(row.dataset.tests || '0', 10);
          passed += parseInt(row.dataset.passed || '0', 10);
        }}
      }}
    }});
    const nf = (x) => x.toLocaleString('en-US');
    const pctVal = tests > 0 ? Math.round(1000 * passed / tests) / 10 : 0;
    document.getElementById('sum-files').textContent = nf(files);
    document.getElementById('sum-tests').textContent = nf(tests);
    document.getElementById('sum-passed').textContent = nf(passed);
    document.getElementById('sum-pct').textContent = pctVal + '%';
    const filtered = !!(g || q);
    let note = '';
    if (onlyWork) {{
      note = filtered
        ? 'Totals reflect remaining work in this filter (skipped and fully passing files hidden).'
        : 'Totals reflect remaining work (skipped and fully passing files hidden).';
    }} else if (filtered) {{
      note = 'Totals reflect visible rows (manually skipped files still excluded).';
    }}
    document.getElementById('summaryNote').textContent = note;
    document.getElementById('count').textContent = nShown + ' files shown';
    const u = new URL(window.location.href);
    if (g) u.searchParams.set('group', g); else u.searchParams.delete('group');
    if (onlyWork) u.searchParams.set('remaining', '1'); else u.searchParams.delete('remaining');
    history.replaceState(null, '', u.pathname + u.search);
  }}

  sel.addEventListener('change', apply);
  search.addEventListener('input', apply);
  workOnly.addEventListener('change', apply);
  apply();
}})();
</script>
{RELATIVE_TIME_JS}
</body>
</html>
"""


def main() -> None:
    rows = load_rows()
    OUT_INDEX.parent.mkdir(parents=True, exist_ok=True)
    OUT_INDEX.write_text(generate_index(rows), encoding="utf-8")
    OUT_FILES.write_text(generate_testfiles(rows), encoding="utf-8")
    OUT_SVG.write_text(generate_progress_svg(rows), encoding="utf-8")
    print(f"Wrote {OUT_INDEX}")
    print(f"Wrote {OUT_FILES}")
    print(f"Wrote {OUT_SVG}")


if __name__ == "__main__":
    main()
