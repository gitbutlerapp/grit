#!/usr/bin/env python3
"""Regenerate grit-cli/hot-text.ld: the functions `grit status` runs, laid out together.

Most of `grit status`'s peak RSS is not heap: it is the binary's own code pages. The kernel
maps file-backed text in 64 KiB fault-around windows, and the ~900 functions a status run
executes (about 1.4 MiB of code) were spread over ~135 such windows of a 25 MiB `.text`,
so ~8.5 MiB of code becomes resident. `grit-cli/build.rs` passes hot-text.ld to the Linux
linker, which places these functions in one `.text.grit_hot` output section in front of
`.text`, so a status run touches a few contiguous windows instead.

The fragment matches each function's input section (`.text.<v0 symbol>`, or
`.text.unlikely.<v0 symbol>` for `#[cold]` functions) with the crate
disambiguators (`Cs<hash>_`), back-references (`B<offset>_`) and numbered local-copy
suffixes wildcarded, so it keeps
matching across version bumps and toolchain updates. A renamed or removed function simply
stops matching and stays in `.text`; nothing breaks, the layout only degrades. Regenerate
after large changes to the status path:

    CARGO_PROFILE_DIST_STRIP=false CARGO_TARGET_DIR=target/hot-text \\
        cargo build --profile dist -p grit-cli
    python3 scripts/gen_hot_text_layout.py --grit target/hot-text/dist/grit

Requires valgrind (callgrind) and the system `git` to build the fixture.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_OUTPUT = REPO_ROOT / "grit-cli" / "hot-text.ld"

# Mirrors the daily census status rows: clean, ~1% dirty with ~1% untracked, path-limited,
# staged rename, plus the alternate output modes.
SCENARIOS: list[tuple[str, str, list[str]]] = [
    ("dirty", "dirty", ["status"]),
    ("clean", "clean", ["status"]),
    ("dirty-short", "dirty", ["status", "-s"]),
    ("dirty-json", "dirty", ["status", "--json"]),
    ("dirty-markdown", "dirty", ["status", "--markdown"]),
    ("paths", "dirty", ["status", "--", "data/d0000"]),
    ("renamed", "renamed", ["status"]),
]

CRATE_DISAMBIGUATOR = re.compile(r"Cs[0-9A-Za-z]+_")
BACKREF = re.compile(r"B[0-9A-Za-z]*_")
# Numbered local copies (`name.1234`, `name.llvm.5678`) are renumbered by every build.
LOCAL_SUFFIX = re.compile(r"(\.llvm)?(\.[0-9]+)+$")
CALLGRIND_FN = re.compile(r"^c?fn=\(\d+\) (.+)$")


def git(repo: Path, *args: str) -> None:
    subprocess.run(
        ["git", "-c", "user.name=hot-text", "-c", "user.email=hot-text@example.com", *args],
        cwd=repo,
        env=run_env(isolated=False),
        check=True,
        stdout=subprocess.DEVNULL,
    )


def build_fixture(root: Path, files: int) -> tuple[Path, list[Path]]:
    """A synth-like clone: `files` small files in 100-file directories, history, many refs.

    Like the census fixtures, the clone has every object packed, remote-tracking refs and
    lightweight and annotated tags in packed-refs, and `main` tracking `origin/main`.
    """
    source = root / "source"
    source.mkdir()
    git(source, "init", "-q", "-b", "main")
    for i in range(files):
        path = source / "data" / f"d{i // 100:04}" / f"f{i:06}.txt"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(f"file {i}\n" * 8)
    git(source, "add", "-A")
    git(source, "commit", "-q", "-m", "fixture")
    for i in range(50):
        git(source, "branch", f"synth/b{i:06}")
        git(source, "tag", f"light-{i:06}")
        git(source, "tag", "-a", "-m", f"v{i}", f"v{i:06}")
    for round_ in range(3):
        for path in sorted(source.glob("data/*/*.txt"))[round_::97]:
            with path.open("a") as f:
                f.write(f"round {round_}\n")
        git(source, "commit", "-q", "-a", "-m", f"round {round_}")
    git(source, "repack", "-adq")
    repo = root / "repo"
    git(root, "clone", "-q", "--no-hardlinks", str(source), str(repo))
    git(repo, "config", "core.untrackedCache", "false")
    git(repo, "config", "core.fsmonitor", "false")
    return repo, sorted(repo.glob("data/*/*.txt"))


def apply_shape(repo: Path, paths: list[Path], shape: str) -> None:
    """Reset to HEAD, then shape the work tree the way the census prepare hooks do."""
    git(repo, "reset", "-q", "--hard")
    git(repo, "clean", "-fdq")
    count = max(len(paths) // 100, 1)
    if shape == "dirty":
        for path in paths[:count]:
            with path.open("a") as f:
                f.write("dirty\n")
        for i in range(count):
            (repo / f"untracked-{i}.txt").write_text(f"untracked {i}\n")
    elif shape == "renamed":
        git(repo, "mv", str(paths[0].relative_to(repo)), "data/renamed.txt")
    # Past the second the reset wrote, status re-checks the racy entries and rewrites the
    # index, as the census's first run after each prepare does.
    time.sleep(1.1)


def run_env(isolated: bool) -> dict[str, str]:
    """A plain user environment, or the census's isolated config (`grit-utils` bench_env.rs)."""
    env = {
        k: v for k, v in os.environ.items() if not k.startswith(("GIT_", "GRIT_", "RUST_"))
    }
    if isolated:
        env.update(
            GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_SYSTEM="/dev/null"
        )
    return env


def executed_functions(
    grit: Path, repo: Path, argv: list[str], out: Path, env: dict[str, str]
) -> list[str]:
    """Mangled names of every function callgrind saw run, in first-seen order."""
    subprocess.run(
        [
            "valgrind",
            "--tool=callgrind",
            "--demangle=no",
            f"--callgrind-out-file={out}",
            str(grit),
            *argv,
        ],
        cwd=repo,
        env=env,
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    names: dict[str, None] = {}
    for line in out.read_text().splitlines():
        m = CALLGRIND_FN.match(line)
        if m:
            names.setdefault(m.group(1), None)
    return list(names)


def defined_text_symbols(grit: Path) -> set[str]:
    out = subprocess.run(
        ["nm", "--defined-only", str(grit)], check=True, capture_output=True, text=True
    ).stdout
    symbols = set()
    for line in out.splitlines():
        parts = line.split(" ", 2)
        if len(parts) == 3 and parts[1] in "tTwW":
            symbols.add(parts[2])
    return symbols


def section_pattern(symbol: str) -> str:
    """Input-section globs for `symbol`; `#[cold]` functions live in `.text.unlikely.*`."""
    pattern = CRATE_DISAMBIGUATOR.sub("Cs*_", symbol)
    pattern = BACKREF.sub("B*_", pattern)
    pattern = LOCAL_SUFFIX.sub(".*", pattern)
    return f".text.{pattern} .text.unlikely.{pattern}"


def render(patterns: list[str]) -> str:
    lines = [
        "/* Generated by scripts/gen_hot_text_layout.py; do not edit by hand.",
        " *",
        " * Places the functions `grit status` executes in one output section ahead of .text so a",
        " * status run faults in a few contiguous code pages instead of ~135 scattered 64 KiB",
        " * windows. Patterns that stop matching are harmless: the function stays in .text. */",
        "SECTIONS",
        "{",
        "  .text.grit_hot : {",
    ]
    lines += [f"    *({p})" for p in patterns]
    lines += ["  }", "}", "INSERT BEFORE .text;", ""]
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--grit", type=Path, required=True, help="unstripped grit binary")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    # Large enough that index parsing and the worktree scans take their threaded paths.
    parser.add_argument("--files", type=int, default=10000, help="tracked files per fixture")
    args = parser.parse_args()

    if shutil.which("valgrind") is None:
        print("valgrind is required", file=sys.stderr)
        return 1
    grit = args.grit.resolve()
    symbols = defined_text_symbols(grit)
    if not any(s.startswith("_R") for s in symbols):
        print(f"{grit} has no Rust symbols; build it with strip = false", file=sys.stderr)
        return 1

    patterns: dict[str, None] = {}
    with tempfile.TemporaryDirectory(prefix="grit-hot-text-") as tmp:
        root = Path(tmp)
        repo, paths = build_fixture(root, args.files)
        for name, shape, argv in SCENARIOS:
            apply_shape(repo, paths, shape)
            # First run after the prepare (census config isolation), then a second one on the
            # refreshed index with the user's usual config cascade.
            for run, isolated in (("fresh", True), ("settled", False)):
                out = root / f"{name}-{run}.callgrind"
                names = executed_functions(grit, repo, argv, out, run_env(isolated))
                hot = [n for n in names if n in symbols]
                before = len(patterns)
                for n in hot:
                    patterns.setdefault(section_pattern(n), None)
                new = len(patterns) - before
                print(f"{name} ({run}): {len(hot)} functions, {new} new", file=sys.stderr)

    args.output.write_text(render(list(patterns)))
    print(f"wrote {len(patterns)} patterns to {args.output}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
