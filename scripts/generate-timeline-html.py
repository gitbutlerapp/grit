#!/usr/bin/env python3
"""Generate site/public/timeline.html: hourly stacked bars (grit vs grit-lib) and commits by day.

Runs full ``git log HEAD`` with ``--format='%H %ct %s' --stat --no-merges`` (no ``--since``),
then keeps commits whose committer epoch ``%ct`` is on or after the configured UTC start.
``git log --since`` can omit reachable commits when history contains merges, so the date
cutoff is applied in Python. Only commits that touch at least one path under ``grit/``,
``grit-lib/``, or the pre-rename ``gust/``, ``gust-lib/`` trees are included; per-file
``--stat`` lines attribute counts to the main crate vs library buckets.
"""

from __future__ import annotations

import html
import json
import re
import subprocess
from collections import defaultdict
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OUT = REPO / "site" / "public" / "timeline.html"

# Inclusive start of the timeline window (UTC). Data shown from this instant through “now”.
TIMELINE_START_UTC = datetime(2026, 4, 1, 0, 0, 0, tzinfo=timezone.utc)

def github_commit_url(sha: str) -> str | None:
    """Return an https://github.com/.../commit/SHA URL if ``origin`` is GitHub."""
    if not sha:
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


@dataclass
class Commit:
    sha: str
    ts: int
    subject: str
    grit_lines: int
    grit_lib_lines: int


def _parse_file_stat_line(line: str) -> tuple[str, int] | None:
    """Parse one ``--stat`` file line; skip binary and non-stat lines."""
    if "|" not in line:
        return None
    left, right = line.rsplit("|", 1)
    path = left.strip()
    right = right.strip()
    if right.startswith("Bin"):
        return None
    m = re.match(r"^(\d+)\s+", right)
    if not m:
        return None
    return (path, int(m.group(1)))


def _lines_for_path(path: str, n: int) -> tuple[int, int]:
    """Split per-file stat counts into main-crate vs lib buckets (grit and legacy gust names)."""
    if path.startswith("grit-lib/") or path.startswith("gust-lib/"):
        return (0, n)
    if path.startswith("grit/") or path.startswith("gust/"):
        return (n, 0)
    return (0, 0)


def commit_touches_grit_roots(commit: Commit) -> bool:
    """True if any ``--stat`` path is under grit/gust main or lib trees (including pre-rename)."""
    return commit.grit_lines > 0 or commit.grit_lib_lines > 0


def parse_git_log_stat(text: str) -> list[Commit]:
    """Parse output of ``git log --format='%H %ct %s' --stat``."""
    commits: list[Commit] = []
    lines = text.splitlines()
    i = 0
    header = re.compile(r"^([0-9a-f]{40}) (\d+) (.*)$")

    while i < len(lines):
        line = lines[i]
        m = header.match(line)
        if not m:
            i += 1
            continue
        sha, ts_s, subject = m.group(1), int(m.group(2)), m.group(3)
        i += 1
        grit_lines = 0
        grit_lib_lines = 0
        while i < len(lines):
            nline = lines[i]
            if header.match(nline):
                break
            if nline.strip():
                parsed = _parse_file_stat_line(nline)
                if parsed:
                    p, n = parsed
                    dg, dgl = _lines_for_path(p, n)
                    grit_lines += dg
                    grit_lib_lines += dgl
            i += 1
        commits.append(
            Commit(
                sha=sha,
                ts=ts_s,
                subject=subject,
                grit_lines=grit_lines,
                grit_lib_lines=grit_lib_lines,
            )
        )
    return commits


def run_git_log() -> str:
    """Run ``git log HEAD`` with stat and no merges (full history; no ``--since``).

    Callers filter by ``%ct`` in Python. Git's ``--since`` can skip reachable commits
    across merge topology, so we do not use it for the timeline window.
    """
    return subprocess.check_output(
        [
            "git",
            "log",
            "HEAD",
            "--format=%H %ct %s",
            "--stat",
            "--no-merges",
        ],
        cwd=REPO,
        text=True,
    )


def hour_start_ts(ts: int) -> int:
    """Floor ``ts`` to UTC hour boundary."""
    return (ts // 3600) * 3600


def build_hourly_series(
    commits: list[Commit],
    now: datetime,
    range_start: datetime,
) -> tuple[list[str], list[int], list[int], list[int], list[int], list[int]]:
    """Build labels, lines per crate per hour, commit counts, UTC day stripe, midnight indices."""
    end = now.astimezone(timezone.utc).replace(minute=0, second=0, microsecond=0)
    end_ts = int(end.timestamp())
    start_ts = hour_start_ts(int(range_start.astimezone(timezone.utc).timestamp()))
    hour_count = max(0, (end_ts - start_ts) // 3600)
    labels: list[str] = []
    day_stripe: list[int] = []
    midnight_at: list[int] = []
    grit: list[int] = [0] * hour_count
    grit_lib: list[int] = [0] * hour_count
    commits_per_hour: list[int] = [0] * hour_count

    prev_date = None
    day_index = -1
    for h in range(hour_count):
        t = start_ts + h * 3600
        dt = datetime.fromtimestamp(t, tz=timezone.utc)
        labels.append(dt.strftime("%m/%d %H:00"))
        d = dt.date()
        if d != prev_date:
            day_index += 1
            prev_date = d
        day_stripe.append(day_index % 2)
        if dt.hour == 0:
            midnight_at.append(h)

    for c in commits:
        hs = hour_start_ts(c.ts)
        idx = (hs - start_ts) // 3600
        if 0 <= idx < hour_count:
            grit[idx] += c.grit_lines
            grit_lib[idx] += c.grit_lib_lines
            commits_per_hour[idx] += 1

    return labels, grit, grit_lib, commits_per_hour, day_stripe, midnight_at


def format_range_caption_utc(dt: datetime) -> str:
    """Human-readable UTC timestamp for the timeline start (e.g. ``2026-04-02 12:00 UTC``)."""
    return dt.astimezone(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")


def format_commit_time_utc(ts: int) -> str:
    """Commit time as ``YYYY-MM-DD HH:MM UTC`` for list rows."""
    return datetime.fromtimestamp(ts, tz=timezone.utc).strftime("%Y-%m-%d %H:%M UTC")


def commits_by_day(commits: list[Commit]) -> dict[str, list[Commit]]:
    """Group commits by UTC date string YYYY-MM-DD (newest day first)."""
    by_day: dict[str, list[Commit]] = defaultdict(list)
    for c in commits:
        day = datetime.fromtimestamp(c.ts, tz=timezone.utc).strftime("%Y-%m-%d")
        by_day[day].append(c)
    for day in by_day:
        by_day[day].sort(key=lambda x: -x.ts)
    return dict(sorted(by_day.items(), key=lambda kv: kv[0], reverse=True))


def write_timeline_html(
    path: Path,
    commits: list[Commit],
    labels: list[str],
    grit: list[int],
    grit_lib: list[int],
    commits_per_hour: list[int],
    day_stripe: list[int],
    midnight_at: list[int],
    generated: datetime,
    range_start_caption: str,
    chart_min_width_px: int,
) -> None:
    by_day = commits_by_day(commits)
    chart_payload = {
        "labels": labels,
        "grit": grit,
        "gritLib": grit_lib,
        "commitsPerHour": commits_per_hour,
        "dayStripe": day_stripe,
        "midnightAt": midnight_at,
    }
    chart_json = json.dumps(chart_payload)

    gen_iso = generated.replace(microsecond=0).isoformat()
    gen_human = generated.strftime("%Y-%m-%d %H:%M UTC")

    day_sections: list[str] = []
    for i, (day, day_commits) in enumerate(by_day.items()):
        stripe = "day-stripe-a" if i % 2 == 0 else "day-stripe-b"
        items: list[str] = []
        for c in day_commits:
            short = c.sha[:7]
            url = github_commit_url(c.sha)
            if url:
                sha_cell = (
                    f'<a href="{html.escape(url)}" class="sha" target="_blank" '
                    f'rel="noopener noreferrer">{html.escape(short)}</a>'
                )
            else:
                sha_cell = f'<span class="sha">{html.escape(short)}</span>'
            when = html.escape(format_commit_time_utc(c.ts))
            subj = html.escape(c.subject)
            gl = c.grit_lines + c.grit_lib_lines
            items.append(
                f"<li>{sha_cell} <span class=\"when\">{when}</span> "
                f"<span class=\"subj\">{subj}</span> "
                f"<span class=\"meta\">grit {c.grit_lines} · grit-lib {c.grit_lib_lines} · Σ {gl}</span></li>"
            )
        day_sections.append(
            f'<section class="day-block {stripe}">'
            f'<details class="day-details" id="{html.escape(day)}">'
            f'<summary class="day-summary">'
            f'<span class="day-date">{html.escape(day)}</span> '
            f'<span class="count">({len(day_commits)} commits)</span>'
            f"</summary>"
            f'<ul class="day-commit-list">{"".join(items)}</ul>'
            f"</details></section>"
        )

    body_days = "\n".join(day_sections)

    html_doc = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Grit — activity timeline</title>
<style>
* {{ margin: 0; padding: 0; box-sizing: border-box; }}
body {{
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', system-ui, sans-serif;
  background: #0d1117;
  color: #e6edf3;
  padding: 2rem;
  max-width: 1100px;
  margin: 0 auto;
}}
h1 {{ font-size: 1.75rem; margin-bottom: 0.25rem; color: #f0f6fc; }}
.sub {{ color: #7d8590; font-size: 0.9rem; margin-bottom: 1.5rem; }}
.chart-wrap {{
  overflow-x: auto;
  width: 100%;
  margin-bottom: 2rem;
  border: 1px solid #30363d;
  border-radius: 8px;
  background: #161b22;
  padding: 1rem;
}}
.chart-inner {{
  min-width: min(100%, {chart_min_width_px}px);
  height: 360px;
}}
h2 {{ font-size: 1.15rem; margin: 1.5rem 0 0.75rem; color: #f0f6fc; }}
.day-block {{
  margin-bottom: 1.5rem;
  padding: 0.65rem 1rem 0.85rem;
  margin-left: -1rem;
  margin-right: -1rem;
  border-radius: 8px;
}}
.day-block.day-stripe-a {{ background: rgba(22, 27, 34, 0.65); border: 1px solid #21262d; }}
.day-block.day-stripe-b {{ background: rgba(13, 17, 23, 0.5); border: 1px solid #21262d; }}
.day-details {{ margin: 0; }}
.day-summary {{
  cursor: pointer;
  font-size: 1rem;
  color: #58a6ff;
  padding: 0.2rem 0;
  list-style-position: outside;
}}
.day-summary:focus {{ outline: none; }}
.day-summary:focus-visible {{ outline: 2px solid #58a6ff; outline-offset: 2px; border-radius: 4px; }}
.day-block .count {{ color: #7d8590; font-weight: 400; font-size: 0.9rem; }}
.day-details[open] .day-summary {{ margin-bottom: 0.45rem; }}
.day-commit-list {{ list-style: none; padding-left: 0; }}
.day-block li {{
  font-size: 0.88rem;
  padding: 0.35rem 0;
  border-bottom: 1px solid #21262d;
  display: flex;
  flex-wrap: wrap;
  gap: 0.35rem 0.75rem;
  align-items: baseline;
}}
.day-block a {{ color: #58a6ff; text-decoration: none; }}
.day-block a:hover {{ text-decoration: underline; }}
.sha, .day-block a.sha {{
  font-family: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace;
  font-size: 0.86em;
  letter-spacing: -0.02em;
}}
.when {{ color: #7d8590; font-size: 0.8rem; white-space: nowrap; }}
.subj {{ color: #e6edf3; flex: 1 1 200px; }}
.meta {{ color: #6e7681; font-size: 0.8rem; white-space: nowrap; }}
.chart-toolbar {{
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.5rem 1rem;
  margin-bottom: 0.75rem;
}}
.chart-toolbar span.lbl {{ color: #7d8590; font-size: 0.85rem; }}
.toggle {{
  display: inline-flex;
  border: 1px solid #30363d;
  border-radius: 6px;
  overflow: hidden;
}}
.toggle label {{
  display: block;
  margin: 0;
  cursor: pointer;
}}
.toggle input {{ position: absolute; opacity: 0; width: 0; height: 0; }}
.toggle .opt {{
  display: block;
  padding: 0.35rem 0.75rem;
  font-size: 0.82rem;
  color: #8b949e;
  background: #0d1117;
  border-right: 1px solid #30363d;
}}
.toggle label:last-child .opt {{ border-right: none; }}
.toggle input:focus + .opt {{ outline: 2px solid #58a6ff; outline-offset: -2px; }}
.toggle input:checked + .opt {{
  background: #21262d;
  color: #f0f6fc;
}}
</style>
</head>
<body>
<h1>Activity timeline</h1>
<p class="sub">Non-merge commits from <strong>{html.escape(range_start_caption)}</strong> through now that touch <code>grit/</code>, <code>grit-lib/</code>, or the pre-rename <code>gust/</code> / <code>gust-lib/</code> trees. Toggle the chart: total <strong>commits per hour</strong>, or stacked <strong>lines per crate per hour</strong> (<code>git log --stat</code>). Generated <time datetime="{html.escape(gen_iso)}">{html.escape(gen_human)}</time>.</p>

<div class="chart-wrap">
  <div class="chart-toolbar">
    <span class="lbl">Chart</span>
    <div class="toggle" role="group" aria-label="Chart metric">
      <label><input type="radio" name="chartMode" value="lines"><span class="opt">Lines / crate / hour</span></label>
      <label><input type="radio" name="chartMode" value="commits" checked><span class="opt">Commits / hour</span></label>
    </div>
  </div>
  <div class="chart-inner"><canvas id="hourly" aria-label="Hourly activity"></canvas></div>
</div>

<h2>Commits by day</h2>
{body_days}

<script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.6/dist/chart.umd.min.js" crossorigin="anonymous"></script>
<script>
(function() {{
  var data = {chart_json};
  var dayStripe = data.dayStripe || [];
  var el = document.getElementById('hourly');
  if (!el || typeof Chart === 'undefined') return;
  if (!data.labels || !data.labels.length) {{
    el.parentElement.innerHTML = '<p class="chart-empty" style="color:#7d8590;font-size:0.9rem;padding:2rem;text-align:center">No hourly range (start is at or after the current hour).</p>';
    return;
  }}

  var dayBandPlugin = {{
    id: 'dayBands',
    beforeDatasetsDraw: function(chart) {{
      var stripes = dayStripe;
      if (!stripes.length) return;
      var chartArea = chart.chartArea;
      if (!chartArea || chartArea.width <= 0) return;
      var meta = chart.getDatasetMeta(0);
      if (!meta || !meta.data || !meta.data.length) return;
      var labels = chart.data.labels || [];
      var n = Math.min(stripes.length, labels.length, meta.data.length);
      var ctx = chart.ctx;
      ctx.save();
      for (var i = 0; i < n; i++) {{
        var left = i === 0 ? chartArea.left : (meta.data[i - 1].x + meta.data[i].x) / 2;
        var right = i === n - 1 ? chartArea.right : (meta.data[i].x + meta.data[i + 1].x) / 2;
        ctx.fillStyle = stripes[i] % 2 === 0
          ? 'rgba(22, 27, 34, 0.65)'
          : 'rgba(13, 17, 23, 0.5)';
        ctx.fillRect(left, chartArea.top, right - left, chartArea.bottom - chartArea.top);
      }}
      ctx.restore();
    }},
  }};
  Chart.register(dayBandPlugin);

  var midnightAt = data.midnightAt || [];
  var midnightLinesPlugin = {{
    id: 'midnightLines',
    afterDatasetsDraw: function(chart) {{
      if (!midnightAt.length) return;
      var chartArea = chart.chartArea;
      if (!chartArea || chartArea.width <= 0) return;
      var meta = chart.getDatasetMeta(0);
      if (!meta || !meta.data || !meta.data.length) return;
      var ctx = chart.ctx;
      ctx.save();
      ctx.strokeStyle = 'rgba(88, 166, 255, 0.45)';
      ctx.lineWidth = 1;
      midnightAt.forEach(function(i) {{
        var idx = Number(i);
        if (idx !== idx || idx < 0 || idx >= meta.data.length) return;
        var x;
        if (idx === 0) {{
          var b0 = meta.data[0];
          if (!b0) return;
          x = (chartArea.left + b0.x) / 2;
        }} else {{
          var a = meta.data[idx - 1];
          var b = meta.data[idx];
          if (!a || !b) return;
          x = (a.x + b.x) / 2;
        }}
        ctx.beginPath();
        ctx.moveTo(x, chartArea.top);
        ctx.lineTo(x, chartArea.bottom);
        ctx.stroke();
      }});
      ctx.restore();
    }},
  }};
  Chart.register(midnightLinesPlugin);

  var currentMode = 'commits';

  function footerTotal(items, mode) {{
    if (mode === 'commits') {{
      var v = items[0] && items[0].parsed.y;
      return v != null ? 'Commits: ' + v : '';
    }}
    var t = 0;
    items.forEach(function(i) {{ t += i.parsed.y || 0; }});
    return 'Total lines: ' + t;
  }}

  function baseOptions() {{
    return {{
      responsive: true,
      maintainAspectRatio: false,
      interaction: {{ mode: 'index', intersect: false }},
      plugins: {{
        legend: {{ labels: {{ color: '#e6edf3' }} }},
        tooltip: {{
          callbacks: {{
            footer: function(items) {{ return footerTotal(items, currentMode); }},
          }},
        }},
      }},
      scales: {{
        x: {{
          stacked: true,
          ticks: {{
            color: '#8b949e',
            maxRotation: 60,
            minRotation: 60,
            autoSkip: true,
            maxTicksLimit: 48,
          }},
          grid: {{ color: '#21262d' }},
        }},
        y: {{
          stacked: true,
          ticks: {{ color: '#8b949e' }},
          grid: {{ color: '#21262d' }},
          beginAtZero: true,
          title: {{
            display: true,
            text: 'Lines touched (stat)',
            color: '#7d8590',
          }},
        }},
      }},
    }};
  }}

  function applyMode(chart, mode) {{
    currentMode = mode;
    var stacked = mode === 'lines';
    chart.data.datasets = datasetsFor(mode);
    chart.options.plugins.legend.display = stacked;
    chart.options.scales.x.stacked = stacked;
    chart.options.scales.y.stacked = stacked;
    chart.options.scales.y.title.text =
      mode === 'commits' ? 'Commits' : 'Lines touched (stat)';
    chart.update();
  }}

  function datasetsFor(mode) {{
    if (mode === 'commits') {{
      return [
        {{
          label: 'Commits',
          data: data.commitsPerHour,
          backgroundColor: 'rgba(210, 153, 34, 0.9)',
          borderColor: 'rgba(227, 179, 65, 1)',
          borderWidth: 1,
        }},
      ];
    }}
    return [
      {{
        label: 'grit',
        data: data.grit,
        backgroundColor: 'rgba(63, 185, 80, 0.85)',
        borderColor: 'rgba(46, 160, 67, 1)',
        borderWidth: 1,
      }},
      {{
        label: 'grit-lib',
        data: data.gritLib,
        backgroundColor: 'rgba(121, 192, 255, 0.85)',
        borderColor: 'rgba(88, 166, 255, 1)',
        borderWidth: 1,
      }},
    ];
  }}

  var chart = new Chart(el, {{
    type: 'bar',
    data: {{ labels: data.labels, datasets: datasetsFor('commits') }},
    options: baseOptions(),
  }});
  applyMode(chart, 'commits');
  document.querySelectorAll('input[name="chartMode"]').forEach(function(radio) {{
    radio.addEventListener('change', function() {{
      if (!this.checked) return;
      var mode = this.value === 'commits' ? 'commits' : 'lines';
      applyMode(chart, mode);
    }});
  }});
}})();
</script>
</body>
</html>
"""
    path.write_text(html_doc, encoding="utf-8")


def main() -> None:
    raw = run_git_log()
    commits = parse_git_log_stat(raw)
    start_ts = hour_start_ts(int(TIMELINE_START_UTC.timestamp()))
    commits = [
        c
        for c in commits
        if c.ts >= start_ts and commit_touches_grit_roots(c)
    ]
    now = datetime.now(timezone.utc)
    labels, grit, grit_lib, commits_per_hour, day_stripe, midnight_at = (
        build_hourly_series(commits, now=now, range_start=TIMELINE_START_UTC)
    )
    hour_count = len(labels)
    chart_min_width_px = min(2400, max(400, hour_count * 10))
    range_caption = format_range_caption_utc(TIMELINE_START_UTC)
    write_timeline_html(
        OUT,
        commits,
        labels,
        grit,
        grit_lib,
        commits_per_hour,
        day_stripe,
        midnight_at,
        generated=now,
        range_start_caption=range_caption,
        chart_min_width_px=chart_min_width_px,
    )
    print(f"Wrote {OUT} ({len(commits)} commits)")


if __name__ == "__main__":
    main()
