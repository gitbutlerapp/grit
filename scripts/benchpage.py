#!/usr/bin/env python3
"""Generate benchmark comparison tables from committed grit-bench JSON baselines."""
from __future__ import annotations

import html
import json
import re
import statistics
import sys
from dataclasses import dataclass
from pathlib import Path

import tomllib

BENCHMARK_MARKER = "<!-- grit:benchmark-tables -->"
SCHEMA_VERSION = 1
SLOW_RATIO_THRESHOLD = 2.0


@dataclass(frozen=True)
class Scenario:
    id: str
    group: str
    fixture: str
    description: str
    git_mean_ms: float
    grit_mean_ms: float
    git_stddev_ms: float
    grit_stddev_ms: float
    ratio: float

    @staticmethod
    def from_json(raw: object, *, baseline: Path, index: int) -> Scenario:
        label = f"benchmark baseline {baseline}: scenario[{index}]"
        if not isinstance(raw, dict):
            raise SystemExit(f"{label} must be an object")
        scenario_id = raw.get("id", index)
        for key in ("id", "group", "fixture", "git", "grit"):
            if key not in raw:
                raise SystemExit(f"{label} (id={scenario_id!r}) missing required field {key!r}")

        git = raw["git"]
        grit = raw["grit"]
        if not isinstance(git, dict) or not isinstance(grit, dict):
            raise SystemExit(f"{label} (id={scenario_id!r}) git/grit must be objects")

        def require_ms(block: dict, tool: str, field: str) -> float:
            if field not in block:
                raise SystemExit(
                    f"{label} (id={scenario_id!r}) missing {tool}.{field}"
                )
            try:
                return float(block[field])
            except (TypeError, ValueError) as exc:
                raise SystemExit(
                    f"{label} (id={scenario_id!r}) invalid {tool}.{field}: {block[field]!r}"
                ) from exc

        git_mean = require_ms(git, "git", "mean_ms")
        grit_mean = require_ms(grit, "grit", "mean_ms")
        git_stddev = require_ms(git, "git", "stddev_ms")
        grit_stddev = require_ms(grit, "grit", "stddev_ms")
        ratio = grit_mean / git_mean if git_mean else float("inf")
        return Scenario(
            id=str(raw["id"]),
            group=str(raw["group"]),
            fixture=str(raw["fixture"]),
            description=str(raw.get("description", "")),
            git_mean_ms=git_mean,
            grit_mean_ms=grit_mean,
            git_stddev_ms=git_stddev,
            grit_stddev_ms=grit_stddev,
            ratio=ratio,
        )


@dataclass(frozen=True)
class BenchBundle:
    timestamp: str
    git_version: str
    grit_version: str
    cpu_model: str
    os_label: str
    scenarios: tuple[Scenario, ...]


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def load_baseline_paths(manifest_path: Path) -> list[Path]:
    if not manifest_path.is_file():
        raise SystemExit(f"missing site manifest {manifest_path}")
    data = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    block = data.get("benchmarks")
    if not block:
        raise SystemExit(f"{manifest_path}: missing [benchmarks] section with baseline paths")
    paths = block.get("baseline")
    if not paths:
        raise SystemExit(f"{manifest_path}: [benchmarks].baseline must list at least one JSON file")
    root = repo_root()
    resolved: list[Path] = []
    for entry in paths:
        path = (root / str(entry)).resolve()
        resolved.append(path)
    return resolved


def load_report(path: Path) -> dict:
    if not path.is_file():
        raise SystemExit(f"benchmark baseline missing: {path}")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise SystemExit(f"benchmark baseline is not valid JSON ({path}): {exc}") from exc
    if not isinstance(data, dict):
        raise SystemExit(f"benchmark baseline {path} must be a JSON object")
    version = data.get("schema_version")
    if version != SCHEMA_VERSION:
        raise SystemExit(
            f"benchmark baseline {path} has unsupported schema_version {version!r} "
            f"(expected {SCHEMA_VERSION})"
        )
    for key in ("timestamp", "machine", "tools", "scenarios"):
        if key not in data:
            raise SystemExit(f"benchmark baseline {path} missing required field {key!r}")
    machine = data["machine"]
    if not isinstance(machine, dict):
        raise SystemExit(f"benchmark baseline {path}: field 'machine' must be an object")
    tools = data["tools"]
    if not isinstance(tools, dict):
        raise SystemExit(f"benchmark baseline {path}: field 'tools' must be an object")
    scenarios = data["scenarios"]
    if not isinstance(scenarios, list) or not scenarios:
        raise SystemExit(f"benchmark baseline {path} must include at least one scenario")
    return data


def merge_baselines(paths: list[Path]) -> BenchBundle:
    scenarios: list[Scenario] = []
    seen: set[str] = set()
    reports: list[dict] = []
    for path in paths:
        report = load_report(path)
        reports.append(report)
        for index, raw in enumerate(report["scenarios"]):
            scenario = Scenario.from_json(raw, baseline=path, index=index)
            if scenario.id in seen:
                raise SystemExit(
                    f"duplicate scenario id {scenario.id!r} across baseline files"
                )
            seen.add(scenario.id)
            scenarios.append(scenario)
    scenarios.sort(key=lambda s: (s.group, s.fixture, s.id))
    primary = reports[0]
    machine = primary["machine"]
    tools = primary["tools"]
    os_label = f"{machine.get('os', '?')} ({machine.get('kernel', '?')})"
    return BenchBundle(
        timestamp=str(primary["timestamp"]),
        git_version=str(tools.get("git", "?")),
        grit_version=str(tools.get("grit", "?")),
        cpu_model=str(machine.get("cpu_model", "?")),
        os_label=os_label,
        scenarios=tuple(scenarios),
    )


def format_ms(value: float) -> str:
    if value >= 100:
        return f"{value:,.0f}"
    if value >= 10:
        return f"{value:.1f}"
    return f"{value:.2f}"


def format_ratio(ratio: float) -> tuple[str, bool]:
    text = f"{ratio:.2f}×"
    return text, ratio > SLOW_RATIO_THRESHOLD


def render_header(bundle: BenchBundle) -> str:
    run_date = bundle.timestamp.replace("T", " ").rstrip("Z")
    rows = [
        ("Git", bundle.git_version),
        ("Grit", bundle.grit_version),
        ("CPU", bundle.cpu_model),
        ("OS", bundle.os_label),
        ("Recorded", run_date),
    ]
    body = "".join(
        f"<tr><th scope=\"row\">{html.escape(label)}</th><td>{html.escape(value)}</td></tr>"
        for label, value in rows
    )
    return (
        '<div class="bench-meta table"><table class="bench-env"><tbody>'
        f"{body}</tbody></table></div>"
    )


def render_group_table(group: str, scenarios: list[Scenario]) -> str:
    rows: list[str] = []
    for scenario in scenarios:
        ratio_text, slow = format_ratio(scenario.ratio)
        spread = f"±{format_ms(scenario.grit_stddev_ms)} ms"
        row_class = ' class="bench-slow"' if slow else ""
        rows.append(
            f"<tr{row_class}>"
            f"<td><code>{html.escape(scenario.id)}</code></td>"
            f"<td>{html.escape(scenario.fixture)}</td>"
            f"<td>{format_ms(scenario.git_mean_ms)}</td>"
            f"<td>{format_ms(scenario.grit_mean_ms)}</td>"
            f"<td>{ratio_text}</td>"
            f"<td>{spread}</td>"
            f"</tr>"
        )
    title = html.escape(group)
    return (
        f'<h3 id="bench-{html.escape(group, quote=True)}">{title}</h3>'
        '<div class="table"><table class="bench-results">'
        "<thead><tr>"
        "<th>Scenario</th><th>Fixture</th>"
        "<th>Git mean (ms)</th><th>Grit mean (ms)</th>"
        "<th>Grit / Git</th><th>Spread</th>"
        "</tr></thead><tbody>"
        f"{''.join(rows)}</tbody></table></div>"
    )


def render_summary(bundle: BenchBundle) -> str:
    by_group: dict[str, list[Scenario]] = {}
    for scenario in bundle.scenarios:
        by_group.setdefault(scenario.group, []).append(scenario)
    rows: list[str] = []
    for group in sorted(by_group):
        items = by_group[group]
        ratios = [s.ratio for s in items]
        median = statistics.median(ratios)
        worst = max(ratios)
        median_text, _ = format_ratio(median)
        worst_text, worst_slow = format_ratio(worst)
        row_class = ' class="bench-slow"' if worst_slow else ""
        rows.append(
            f"<tr{row_class}>"
            f"<td>{html.escape(group)}</td>"
            f"<td>{len(items)}</td>"
            f"<td>{median_text}</td>"
            f"<td>{worst_text}</td>"
            f"</tr>"
        )
    return (
        '<h2 id="bench-summary">Summary by operation</h2>'
        '<div class="table"><table class="bench-summary">'
        "<thead><tr>"
        "<th>Operation</th><th>Scenarios</th>"
        "<th>Median Grit / Git</th><th>Worst Grit / Git</th>"
        "</tr></thead><tbody>"
        f"{''.join(rows)}</tbody></table></div>"
    )


def render_tables(bundle: BenchBundle) -> str:
    by_group: dict[str, list[Scenario]] = {}
    for scenario in bundle.scenarios:
        by_group.setdefault(scenario.group, []).append(scenario)
    parts = [render_header(bundle), render_summary(bundle)]
    for group in sorted(by_group):
        parts.append(render_group_table(group, by_group[group]))
    return "\n".join(parts)


def render_header_markdown(bundle: BenchBundle) -> str:
    run_date = bundle.timestamp.replace("T", " ").rstrip("Z")
    rows = [
        ("Git", bundle.git_version),
        ("Grit", bundle.grit_version),
        ("CPU", bundle.cpu_model),
        ("OS", bundle.os_label),
        ("Recorded", run_date),
    ]
    lines = ["| | |", "| --- | --- |"]
    for label, value in rows:
        lines.append(f"| {label} | {value} |")
    return "\n".join(lines)


def render_group_table_markdown(group: str, scenarios: list[Scenario]) -> str:
    lines = [
        f"### {group}",
        "",
        "| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |",
        "| --- | --- | ---: | ---: | ---: | --- |",
    ]
    for scenario in scenarios:
        ratio_text, _slow = format_ratio(scenario.ratio)
        spread = f"±{format_ms(scenario.grit_stddev_ms)} ms"
        lines.append(
            f"| `{scenario.id}` | {scenario.fixture} | {format_ms(scenario.git_mean_ms)} | "
            f"{format_ms(scenario.grit_mean_ms)} | {ratio_text} | {spread} |"
        )
    return "\n".join(lines)


def render_summary_markdown(bundle: BenchBundle) -> str:
    by_group: dict[str, list[Scenario]] = {}
    for scenario in bundle.scenarios:
        by_group.setdefault(scenario.group, []).append(scenario)
    lines = [
        "## Summary by operation",
        "",
        "| Operation | Scenarios | Median Grit / Git | Worst Grit / Git |",
        "| --- | ---: | ---: | ---: |",
    ]
    for group in sorted(by_group):
        items = by_group[group]
        ratios = [s.ratio for s in items]
        median = statistics.median(ratios)
        worst = max(ratios)
        median_text, _ = format_ratio(median)
        worst_text, _ = format_ratio(worst)
        lines.append(f"| {group} | {len(items)} | {median_text} | {worst_text} |")
    return "\n".join(lines)


def render_tables_markdown(bundle: BenchBundle) -> str:
    by_group: dict[str, list[Scenario]] = {}
    for scenario in bundle.scenarios:
        by_group.setdefault(scenario.group, []).append(scenario)
    parts = [render_header_markdown(bundle), render_summary_markdown(bundle)]
    for group in sorted(by_group):
        parts.append(render_group_table_markdown(group, by_group[group]))
    return "\n\n".join(parts)


def benchmark_html_for_manifest(manifest_path: Path) -> str:
    paths = load_baseline_paths(manifest_path)
    bundle = merge_baselines(paths)
    return render_tables(bundle)


def benchmark_markdown_for_manifest(manifest_path: Path) -> str:
    paths = load_baseline_paths(manifest_path)
    bundle = merge_baselines(paths)
    return render_tables_markdown(bundle)


def split_benchmark_markdown(markdown: str) -> tuple[str, str]:
    if BENCHMARK_MARKER not in markdown:
        raise SystemExit(f"benchmarks page must contain marker {BENCHMARK_MARKER}")
    before, after = markdown.split(BENCHMARK_MARKER, 1)
    return before, after


def count_data_rows(html_block: str) -> int:
    """Count scenario rows in generated group tables (excludes summary and header rows)."""
    total = 0
    for match in re.finditer(
        r'class="bench-results"[^>]*>\s*<thead>.*?</thead>\s*<tbody>(.*?)</tbody>',
        html_block,
        flags=re.DOTALL,
    ):
        total += match.group(1).count("<tr")
    return total


def main(argv: list[str] | None = None) -> int:
    manifest = repo_root() / "content" / "docs" / "site.toml"
    sys.stdout.write(benchmark_html_for_manifest(manifest))
    sys.stdout.write("\n")
    if argv is not None:
        del argv
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
