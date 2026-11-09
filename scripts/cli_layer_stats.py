#!/usr/bin/env python3
"""Count non-comment, non-blank Rust lines in grit-cli layer directories."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CLI_SRC = ROOT / "grit-cli" / "src"

LAYERS = (
    ("render", CLI_SRC / "render"),
    ("args", CLI_SRC / "args"),
    ("dispatch", CLI_SRC / "main.rs"),
)


def count_file(path: Path) -> int:
    text = path.read_text(encoding="utf-8")
    total = 0
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("//"):
            continue
        total += 1
    return total


def count_tree(root: Path) -> int:
    if not root.exists():
        return 0
    if root.is_file():
        return count_file(root)
    total = 0
    for path in sorted(root.rglob("*.rs")):
        total += count_file(path)
    return total


def snapshot() -> dict[str, int]:
    render = count_tree(CLI_SRC / "render")
    args = count_tree(CLI_SRC / "args")
    dispatch = count_file(CLI_SRC / "main.rs")
    all_src = count_tree(CLI_SRC)
    other = all_src - render - args - dispatch
    return {
        "render": render,
        "args": args,
        "dispatch": dispatch,
        "other": other,
        "total": all_src,
    }


def load_baseline(path: Path) -> dict[str, int]:
    import tomllib

    data = tomllib.loads(path.read_text(encoding="utf-8"))
    return {k: int(v) for k, v in data["counts"].items()}


def cmd_measure() -> int:
    counts = snapshot()
    for key in ("render", "args", "dispatch", "other", "total"):
        print(f"{key}={counts[key]}")
    return 0


def cmd_update(path: Path) -> int:
    import datetime

    counts = snapshot()
    lines = [
        "# grit-cli layer line counts — ratchet baseline",
        "# Regenerate with: make cli-layer-ratchet",
        "",
        "[meta]",
        f'updated = "{datetime.date.today().isoformat()}"',
        "",
        "[counts]",
    ]
    for key in ("render", "args", "dispatch", "other", "total"):
        lines.append(f"{key} = {counts[key]}")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"Wrote {path}")
    return 0


def cmd_check(path: Path) -> int:
    baseline = load_baseline(path)
    current = snapshot()
    failed = False
    for key in ("args", "dispatch", "other"):
        if current[key] > baseline[key]:
            print(
                f"cli layer ratchet: {key} grew from {baseline[key]} to {current[key]} "
                f"(update {path} only after intentional shrink)",
                file=sys.stderr,
            )
            failed = True
    if current["render"] < baseline["render"]:
        print(
            f"warning: render layer shrank {baseline['render']} -> {current['render']}",
            file=sys.stderr,
        )
    return 1 if failed else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "command",
        choices=("measure", "update", "check"),
        help="measure prints counts; update writes baseline; check fails when non-render layers grow",
    )
    parser.add_argument(
        "--baseline",
        type=Path,
        default=ROOT / "grit-cli" / "layer-ratchet.toml",
        help="baseline TOML path",
    )
    args = parser.parse_args()
    if args.command == "measure":
        return cmd_measure()
    if args.command == "update":
        return cmd_update(args.baseline)
    return cmd_check(args.baseline)


if __name__ == "__main__":
    raise SystemExit(main())
