#!/usr/bin/env python3
"""Line-coverage gate for grit-lib ODB/pack/MIDX/commit-graph modules."""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

_MOD_DECL_RE = re.compile(
    r"^\s*(?:pub(?:\(\s*crate\s*\))?\s+)?mod\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*;",
    re.MULTILINE,
)
_PATH_MOD_RE = re.compile(
    r'#\[(?:\s*path\s*=\s*"([^"]+)"[^\]]*)\]\s*'
    r"(?:\n\s*)*(?:pub(?:\(\s*crate\s*\))?\s+)?mod\s+[a-zA-Z_][a-zA-Z0-9_]*",
    re.MULTILINE,
)

ROOT = Path(__file__).resolve().parents[1]
GRIT_LIB_SRC = ROOT / "grit-lib" / "src"
FLOORS_PATH = ROOT / "grit-lib" / "coverage-floors.toml"
LLVM_COV_TARGET = ROOT / "target" / "llvm-cov-target"
DEFAULT_GRIT_BIN = LLVM_COV_TARGET / "debug" / ("grit.exe" if os.name == "nt" else "grit")


@dataclass(frozen=True)
class LineStats:
    count: int
    covered: int

    @property
    def missed(self) -> int:
        return self.count - self.covered

    @property
    def percent(self) -> float:
        if self.count == 0:
            return 100.0
        return 100.0 * self.covered / self.count


@dataclass
class Floors:
    meta: dict[str, str]
    core_groups: list[str]
    core_minimum: float
    total_minimum: float | None
    aggregates: dict[str, tuple[list[str], float]]
    groups: dict[str, tuple[list[str], float | None]]
    files: dict[str, float]


def ratchet_floor(percent: float) -> float:
    """Current coverage minus two points, rounded down to one decimal."""
    return math.floor((percent - 2.0) * 10.0) / 10.0


def load_floors(path: Path) -> Floors:
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    meta = {k: str(v) for k, v in data.get("meta", {}).items()}
    core = data.get("core", {})
    core_groups = [str(g) for g in core.get("groups", [])]
    core_minimum = float(core.get("minimum", 0.0))

    total_section = data.get("total", {})
    total_minimum_raw = total_section.get("minimum")
    total_minimum = (
        float(total_minimum_raw) if total_minimum_raw is not None else None
    )

    aggregates: dict[str, tuple[list[str], float]] = {}
    for name, value in data.get("aggregate", {}).items():
        group_names = [str(g) for g in value.get("groups", [])]
        aggregates[str(name)] = (group_names, float(value.get("minimum", 0.0)))

    groups: dict[str, tuple[list[str], float | None]] = {}
    for name, value in data.get("group", {}).items():
        files = [str(f) for f in value.get("files", [])]
        minimum = value.get("minimum")
        groups[str(name)] = (files, float(minimum) if minimum is not None else None)

    files: dict[str, float] = {}
    for name, value in data.get("file", {}).items():
        files[str(name)] = float(value["minimum"])

    return Floors(
        meta=meta,
        core_groups=core_groups,
        core_minimum=core_minimum,
        total_minimum=total_minimum,
        aggregates=aggregates,
        groups=groups,
        files=files,
    )


def _resolve_mod_file(parent_file: Path, name: str) -> Path | None:
    """Resolve `mod name;` from `parent_file` the way Rust does."""
    parent_dir = parent_file.parent
    candidates = [
        parent_dir / f"{name}.rs",
        parent_dir / name / "mod.rs",
    ]
    if parent_file.name != "mod.rs":
        module_dir = parent_dir / parent_file.stem
        candidates.extend(
            [
                module_dir / f"{name}.rs",
                module_dir / name / "mod.rs",
            ]
        )
    for path in candidates:
        if path.is_file():
            return path
    return None


def referenced_module_files(*, src_root: Path | None = None) -> set[str]:
    """Rust module files reachable from `lib.rs` via `mod` / `#[path]` declarations."""
    root = src_root or GRIT_LIB_SRC
    lib = root / "lib.rs"
    if not lib.is_file():
        return set()

    referenced: set[str] = set()
    pending: list[Path] = [lib]
    while pending:
        path = pending.pop()
        try:
            rel = path.relative_to(root).as_posix()
        except ValueError:
            continue
        if rel in referenced:
            continue
        referenced.add(rel)
        text = path.read_text(encoding="utf-8")
        for match in _MOD_DECL_RE.finditer(text):
            child = _resolve_mod_file(path, match.group(1))
            if child is not None:
                pending.append(child)
        for match in _PATH_MOD_RE.finditer(text):
            child = (path.parent / match.group(1)).resolve()
            try:
                child.relative_to(root.resolve())
            except ValueError:
                continue
            if child.is_file():
                pending.append(child)
    return referenced


def grit_lib_src_rs_files(*, src_root: Path | None = None) -> list[str]:
    """Every Rust source file under `grit-lib/src`, relative to that directory."""
    root = src_root or GRIT_LIB_SRC
    return sorted(
        path.relative_to(root).as_posix() for path in root.rglob("*.rs")
    )


def count_source_lines_for_coverage(path: Path) -> int:
    """Estimate instrumentable lines when llvm-cov omits a file from JSON."""
    count = 0
    for line in path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("//"):
            continue
        count += 1
    return count


def llvm_cov_missing_src_files(
    stats: dict[str, LineStats], *, src_root: Path | None = None
) -> list[str]:
    """Orphan or uncompiled sources missing from llvm-cov (not referenced module files)."""
    root = src_root or GRIT_LIB_SRC
    referenced = referenced_module_files(src_root=root)
    missing: list[str] = []
    for rel in grit_lib_src_rs_files(src_root=root):
        if rel in stats:
            continue
        if rel in referenced:
            # Compiled module omitted from JSON (zero regions) is not an orphan.
            continue
        path = root / rel
        if count_source_lines_for_coverage(path) > 0:
            missing.append(rel)
    return missing


def complete_library_stats(
    stats: dict[str, LineStats], *, src_root: Path | None = None
) -> dict[str, LineStats]:
    """Merge llvm-cov stats with zero-covered placeholders for orphan sources only.

    Referenced module files that llvm-cov omits from JSON (typically small `mod.rs`
    re-export shims) are left out of the merged map so they are not counted as 0%.
    """
    root = src_root or GRIT_LIB_SRC
    out = dict(stats)
    for rel in llvm_cov_missing_src_files(out, src_root=root):
        line_count = count_source_lines_for_coverage(root / rel)
        out[rel] = LineStats(count=line_count, covered=0)
    return out


def library_total_stats(
    stats: dict[str, LineStats], *, src_root: Path | None = None
) -> LineStats:
    """Line coverage for all of `grit-lib/src`, not only llvm-cov JSON keys."""
    complete = complete_library_stats(stats, src_root=src_root)
    return aggregate(grit_lib_src_rs_files(src_root=src_root), complete)


def parse_coverage_json(raw: str | bytes) -> dict[str, LineStats]:
    payload = json.loads(raw)
    by_file: dict[str, LineStats] = {}
    for entry in payload.get("data", []):
        for item in entry.get("files", []):
            filename = item.get("filename")
            if not filename:
                continue
            src_root = (ROOT / "grit-lib" / "src").resolve()
            try:
                rel = Path(filename).resolve().relative_to(src_root)
            except ValueError:
                continue
            if rel.suffix != ".rs":
                continue
            name = rel.as_posix()
            summary = item.get("summary", {}).get("lines", {})
            count = int(summary.get("count", 0))
            covered = int(summary.get("covered", 0))
            existing = by_file.get(name)
            if existing is not None:
                count = max(count, existing.count)
                covered = max(covered, existing.covered)
            by_file[name] = LineStats(count=count, covered=covered)
    return by_file


def aggregate(files: list[str], stats: dict[str, LineStats]) -> LineStats:
    total = 0
    covered = 0
    for name in files:
        if name not in stats:
            continue
        total += stats[name].count
        covered += stats[name].covered
    return LineStats(count=total, covered=covered)


def run_llvm_cov(*, grit_bin: Path, json_path: Path | None) -> str:
    env = os.environ.copy()
    env["GRIT_BIN"] = str(grit_bin)
    cmd = [
        "cargo",
        "llvm-cov",
        "-p",
        "grit-lib",
        "--lib",
        "--tests",
        "--json",
        "--summary-only",
    ]
    if json_path is not None:
        cmd.extend(["--output-path", str(json_path)])
    result = subprocess.run(
        cmd,
        cwd=ROOT,
        env=env,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stdout)
        sys.stderr.write(result.stderr)
        raise SystemExit(result.returncode)
    if json_path is not None:
        return json_path.read_text(encoding="utf-8")
    return result.stdout


def format_row(label: str, stats: LineStats, minimum: float | None, ok: bool) -> str:
    mark = "ok" if ok else "FAIL"
    floor = f"{minimum:.1f}" if minimum is not None else "-"
    return (
        f"{label:<28} {stats.count:>7} {stats.missed:>7} "
        f"{stats.percent:>6.1f}  floor {floor:>6}  [{mark}]"
    )


def write_floors(path: Path, floors: Floors) -> None:
    lines: list[str] = [
        "# grit-lib line-coverage floors (percent). Ratchet: scripts/coverage.py --update",
        "# Regenerate floors after intentional gains; never lower minimums by hand.",
        "",
        "[meta]",
    ]
    for key in sorted(floors.meta):
        lines.append(f'{key} = "{floors.meta[key]}"')
    lines.extend(
        [
            "",
            "[core]",
            "groups = [" + ", ".join(f'"{g}"' for g in floors.core_groups) + "]",
            f"minimum = {floors.core_minimum}",
            "",
        ]
    )
    if floors.total_minimum is not None:
        lines.append("[total]")
        lines.append(f"minimum = {floors.total_minimum}")
        lines.append("")
    for name in sorted(floors.aggregates):
        group_names, minimum = floors.aggregates[name]
        lines.append(f"[aggregate.{name}]")
        lines.append("groups = [" + ", ".join(f'"{g}"' for g in group_names) + "]")
        lines.append(f"minimum = {minimum}")
        lines.append("")
    for name in sorted(floors.groups):
        files, minimum = floors.groups[name]
        lines.append(f"[group.{name}]")
        lines.append("files = [")
        for f in files:
            lines.append(f'    "{f}",')
        lines.append("]")
        if minimum is not None:
            lines.append(f"minimum = {minimum}")
        lines.append("")
    for name in sorted(floors.files):
        lines.append(f'[file."{name}"]')
        lines.append(f"minimum = {floors.files[name]}")
        lines.append("")
    path.write_text("\n".join(lines).rstrip() + "\n", encoding="utf-8")


def apply_update(floors: Floors, stats: dict[str, LineStats]) -> Floors:
    new_files = dict(floors.files)
    for name, minimum in floors.files.items():
        if name in stats:
            new_files[name] = max(minimum, ratchet_floor(stats[name].percent))

    new_groups = dict(floors.groups)
    for name, (files, minimum) in floors.groups.items():
        agg = aggregate(files, stats)
        if minimum is None:
            new_groups[name] = (files, ratchet_floor(agg.percent))
        else:
            new_groups[name] = (files, max(minimum, ratchet_floor(agg.percent)))

    core_files: list[str] = []
    for group_name in floors.core_groups:
        if group_name in floors.groups:
            core_files.extend(floors.groups[group_name][0])
    core_stats = aggregate(core_files, stats)
    new_core_minimum = max(floors.core_minimum, ratchet_floor(core_stats.percent))

    new_aggregates = dict(floors.aggregates)
    for name, (group_names, minimum) in floors.aggregates.items():
        agg_files: list[str] = []
        for group_name in group_names:
            if group_name in floors.groups:
                agg_files.extend(floors.groups[group_name][0])
        agg_stats = aggregate(agg_files, stats)
        new_aggregates[name] = (
            group_names,
            max(minimum, ratchet_floor(agg_stats.percent)),
        )

    meta = dict(floors.meta)
    meta["updated"] = meta.get("updated", "")

    total_stats = library_total_stats(stats)
    new_total_minimum = floors.total_minimum
    if new_total_minimum is not None:
        new_total_minimum = max(
            new_total_minimum, ratchet_floor(total_stats.percent)
        )

    return Floors(
        meta=meta,
        core_groups=floors.core_groups,
        core_minimum=new_core_minimum,
        total_minimum=new_total_minimum,
        aggregates=new_aggregates,
        groups=new_groups,
        files=new_files,
    )


def aggregate_group_files(
    group_names: list[str], groups: dict[str, tuple[list[str], float | None]]
) -> list[str]:
    files: list[str] = []
    for group_name in group_names:
        if group_name in groups:
            files.extend(groups[group_name][0])
    return files


def check(
    floors: Floors,
    stats: dict[str, LineStats],
    *,
    validate_src_tree: bool = False,
) -> tuple[list[str], list[str]]:
    """Return (table_lines, failure_messages)."""
    table: list[str] = []
    failures: list[str] = []

    table.append(f"{'module':<28} {'lines':>7} {'missed':>7} {'%':>6}  {'floor':>11}  status")
    table.append("-" * 72)

    for name in sorted(floors.files):
        if name not in stats:
            failures.append(f"missing coverage data for {name}")
            continue
        st = stats[name]
        minimum = floors.files[name]
        ok = st.percent + 1e-9 >= minimum
        table.append(format_row(name, st, minimum, ok))
        if not ok:
            failures.append(
                f"{name}: {st.percent:.1f}% below floor {minimum:.1f}%"
            )

    for name in sorted(floors.groups):
        files, minimum = floors.groups[name]
        st = aggregate(files, stats)
        if minimum is None:
            table.append(format_row(f"group:{name}", st, None, True))
            continue
        ok = st.percent + 1e-9 >= minimum
        table.append(format_row(f"group:{name}", st, minimum, ok))
        if not ok:
            failures.append(
                f"group {name}: {st.percent:.1f}% below floor {minimum:.1f}%"
            )

    core_files: list[str] = []
    for group_name in floors.core_groups:
        if group_name in floors.groups:
            core_files.extend(floors.groups[group_name][0])
    core_stats = aggregate(core_files, stats)
    ok = core_stats.percent + 1e-9 >= floors.core_minimum
    table.append(format_row("core", core_stats, floors.core_minimum, ok))
    if not ok:
        failures.append(
            f"core set: {core_stats.percent:.1f}% below floor {floors.core_minimum:.1f}%"
        )

    for name in sorted(floors.aggregates):
        group_names, minimum = floors.aggregates[name]
        agg_files = aggregate_group_files(group_names, floors.groups)
        st = aggregate(agg_files, stats)
        ok = st.percent + 1e-9 >= minimum
        table.append(format_row(f"aggregate:{name}", st, minimum, ok))
        if not ok:
            failures.append(
                f"aggregate {name}: {st.percent:.1f}% below floor {minimum:.1f}%"
            )

    if validate_src_tree:
        for rel in llvm_cov_missing_src_files(stats):
            failures.append(f"missing coverage data for {rel}")

    if floors.total_minimum is not None:
        total_stats = library_total_stats(stats)
        ok = total_stats.percent + 1e-9 >= floors.total_minimum
        table.append(format_row("total", total_stats, floors.total_minimum, ok))
        if not ok:
            failures.append(
                f"total: {total_stats.percent:.1f}% below floor "
                f"{floors.total_minimum:.1f}%"
            )

    return table, failures


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--input",
        type=Path,
        help="Read cargo llvm-cov JSON summary from this file instead of running llvm-cov",
    )
    parser.add_argument(
        "--floors",
        type=Path,
        default=FLOORS_PATH,
        help="Path to coverage-floors.toml",
    )
    parser.add_argument(
        "--update",
        action="store_true",
        help="Raise floors to current coverage minus 2 points (never lower)",
    )
    parser.add_argument(
        "--grit-bin",
        type=Path,
        default=Path(os.environ.get("GRIT_BIN", DEFAULT_GRIT_BIN)),
        help="grit binary for integration tests (default: llvm-cov target dir)",
    )
    args = parser.parse_args(argv)

    floors = load_floors(args.floors)

    if args.input is not None:
        raw = args.input.read_text(encoding="utf-8")
    else:
        if not args.grit_bin.is_file():
            sys.stderr.write(
                f"grit binary not found at {args.grit_bin}; build grit-cli into "
                f"{LLVM_COV_TARGET} first (see `make coverage`).\n"
            )
            return 2
        raw = run_llvm_cov(grit_bin=args.grit_bin, json_path=None)

    stats = parse_coverage_json(raw)
    if args.update:
        floors = apply_update(floors, stats)
        write_floors(args.floors, floors)

    table, failures = check(floors, stats, validate_src_tree=True)
    for line in table:
        print(line)

    if failures:
        sys.stderr.write("\ncoverage gate failed:\n")
        for msg in failures:
            sys.stderr.write(f"  - {msg}\n")
        return 1
    return 0


# Expose helpers for unit tests.
__all__ = [
    "Floors",
    "LineStats",
    "aggregate",
    "aggregate_group_files",
    "apply_update",
    "check",
    "complete_library_stats",
    "count_source_lines_for_coverage",
    "grit_lib_src_rs_files",
    "library_total_stats",
    "llvm_cov_missing_src_files",
    "referenced_module_files",
    "load_floors",
    "parse_coverage_json",
    "ratchet_floor",
]


if __name__ == "__main__":
    raise SystemExit(main())
