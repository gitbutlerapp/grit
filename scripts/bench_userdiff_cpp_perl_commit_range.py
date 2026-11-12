#!/usr/bin/env python3
"""Cold-start wall time for `grit diff` / `git diff` on a cpp/perl commit range.

Factory review fixture (#2573 / #7558): synthetic repo with many `.c` (diff=cpp) and `.pl`
(diff=perl) files, two commits. Each sample spawns a fresh process (cold start).

Usage (after building both grit binaries under distinct targets):

  python3 scripts/bench_userdiff_cpp_perl_commit_range.py \\
    --repo /tmp/userdiff-cpp-perl-bench \\
    --grit-main /path/to/grit-main-release \\
    --grit-branch /path/to/grit-branch-release \\
    --runs 200

Build mains independently, e.g.:

  git worktree add /tmp/grit-pre-userdiff <sha-without-userdiff>  # optional baseline
  cargo build --release -p grit-cli
"""

from __future__ import annotations

import argparse
import json
import os
import random
import statistics
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def build_fixture(repo: Path, dirs: int, files_per_dir: int) -> None:
    stamp = repo / ".git" / "refs" / "heads" / "main"
    meta = repo / ".bench_userdiff_fixture.json"
    want = json.dumps({"dirs": dirs, "files_per_dir": files_per_dir})
    if stamp.exists() and meta.exists() and meta.read_text(encoding="utf-8") == want:
        return
    if repo.exists():
        import shutil

        shutil.rmtree(repo)
    (repo / "src").mkdir(parents=True, exist_ok=True)
    (repo / "lib").mkdir(parents=True, exist_ok=True)
    (repo / ".gitattributes").write_text("*.c diff=cpp\n*.pl diff=perl\n", encoding="utf-8")
    subprocess.run(["git", "init", "-q", "-b", "main"], cwd=repo, check=True)
    subprocess.run(["git", "config", "user.email", "t@e.com"], cwd=repo, check=True)
    subprocess.run(["git", "config", "user.name", "T"], cwd=repo, check=True)
    for d in range(dirs):
        for i in range(files_per_dir):
            (repo / "src" / f"d{d:02d}_f{i:03d}.c").write_text(
                f"int d{d}_f{i}(void) {{ return {d}+{i}; }}\n", encoding="utf-8"
            )
            (repo / "lib" / f"d{d:02d}_f{i:03d}.pl").write_text(
                f"sub d{d}_f{i} {{ return {d}+{i}; }}\n", encoding="utf-8"
            )
    subprocess.run(["git", "add", "."], cwd=repo, check=True)
    subprocess.run(["git", "commit", "-qm", "base"], cwd=repo, check=True)
    for d in range(dirs):
        for i in range(files_per_dir):
            (repo / "src" / f"d{d:02d}_f{i:03d}.c").write_text(
                f"int d{d}_f{i}(void) {{ return {d}+{i}+1; }}\n", encoding="utf-8"
            )
            (repo / "lib" / f"d{d:02d}_f{i:03d}.pl").write_text(
                f"sub d{d}_f{i} {{ return {d}+{i}+1; }}\n", encoding="utf-8"
            )
    subprocess.run(["git", "add", "."], cwd=repo, check=True)
    subprocess.run(["git", "commit", "-qm", "tip"], cwd=repo, check=True)
    meta.write_text(want, encoding="utf-8")


def rev_range(repo: Path) -> tuple[str, str]:
    base = subprocess.check_output(["git", "-C", repo, "rev-parse", "HEAD~1"], text=True).strip()
    tip = subprocess.check_output(["git", "-C", repo, "rev-parse", "HEAD"], text=True).strip()
    return base, tip


def run_diff(tool: str, repo: Path, base: str, tip: str) -> float:
    env = {
        **os.environ,
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_CONFIG_SYSTEM": "/dev/null",
    }
    t0 = time.perf_counter()
    if tool == "git":
        cmd = ["git", "-C", str(repo), "diff", f"{base}..{tip}", "-U0", "--no-color"]
    else:
        cmd = [tool, "-C", str(repo), "diff", f"{base}..{tip}", "-U0"]
    subprocess.run(
        cmd,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=True,
        env=env,
    )
    return (time.perf_counter() - t0) * 1000.0


def interleaved_medians(
    repo: Path,
    grit_main: str | None,
    grit_branch: str,
    runs: int,
    seed: int,
) -> dict[str, float]:
    base, tip = rev_range(repo)
    rng = random.Random(seed)
    buckets: dict[str, list[float]] = {"git": [], "grit_main": [], "grit_branch": []}
    tools: list[tuple[str, str]] = [("git", "git")]
    if grit_main:
        tools.append(("grit_main", grit_main))
    tools.append(("grit_branch", grit_branch))
    for _ in range(runs):
        order = tools[:]
        rng.shuffle(order)
        for label, tool in order:
            buckets[label].append(run_diff(tool, repo, base, tip))
    return {k: statistics.median(v) for k, v in buckets.items() if v}


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--repo", type=Path, default=Path("/tmp/userdiff-cpp-perl-bench"))
    p.add_argument("--dirs", type=int, default=25)
    p.add_argument("--files-per-dir", type=int, default=10)
    p.add_argument("--grit-branch", type=Path, default=ROOT / "target/release/grit")
    p.add_argument("--grit-main", type=Path, default=None)
    p.add_argument("--runs", type=int, default=200)
    p.add_argument("--seed", type=int, default=42)
    p.add_argument("--json-out", type=Path, default=None)
    args = p.parse_args()
    build_fixture(args.repo, args.dirs, args.files_per_dir)
    grit_main = str(args.grit_main) if args.grit_main else None
    grit_branch = str(args.grit_branch)
    medians = interleaved_medians(
        args.repo,
        grit_main,
        grit_branch,
        args.runs,
        args.seed,
    )
    out = {
        "fixture": {
            "repo": str(args.repo),
            "dirs": args.dirs,
            "files_per_dir": args.files_per_dir,
            "attributes": "*.c diff=cpp, *.pl diff=perl",
        },
        "methodology": (
            f"{args.runs} rounds; each round shuffles git / grit order (seed {args.seed}); "
            "fresh process per invocation; median wall ms"
        ),
        "medians_ms": medians,
    }
    if "grit_main" in medians and "grit_branch" in medians:
        base = medians["grit_main"]
        out["branch_vs_main_ratio"] = medians["grit_branch"] / base if base else None
    text = json.dumps(out, indent=2)
    print(text)
    if args.json_out:
        args.json_out.write_text(text + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
