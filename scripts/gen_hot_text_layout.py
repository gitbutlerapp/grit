#!/usr/bin/env python3
"""Regenerate grit-lib/hot-text.ld and grit-cli/hot-text.ld: the hot code path, laid out together.

Short commands' peak RSS is mostly the binary's own code pages. The kernel maps file-backed
text in 64 KiB fault-around windows, and the functions an operation runs are spread over the
whole `.text`: `grit status` on 10,000 files executes ~900 functions (1.4 MiB of code) that
touched ~135 windows, so ~8.5 MiB of code became resident. Each window is also a page fault
that maps 16 pages, which is most of a short command's wall time beyond `exec`.

Two fragments come out of one profile:

- grit-lib/hot-text.ld holds library code only: functions of grit-lib, the Rust runtime and
  grit-lib's own dependencies. With grit-lib's `hot-text-layout` feature the linker places
  them in one `.text.grit_hot` output section in front of `.text` in every binary that links
  grit-lib (see grit-lib/build.rs), embedders included.
- grit-cli/hot-text.ld holds everything else the shipped `grit` binary runs: the CLI's own
  code, clap, and library generics the CLI instantiates. grit-cli/build.rs links it into the
  `grit` binary only, as a `.text.grit_cli_hot` section next to the library's. Every command
  runs argument parsing and the CLI's start-up before any library code, so without it those
  functions alone fault in dozens of windows.

Both lists are the union of what common operations run (repository discovery and config,
refs and branches, object reads, revision walks, diffs, blame, the index and work tree,
staging, committing, switching, stashing), profiled with callgrind on a clone through the
shipped `grit` binary so inlining matches what is linked. Functions are ordered by how many
workloads run them, so the core every operation shares comes first and stays dense.

Each function's input section (`.text.<v0 symbol>`, or `.text.unlikely.<v0 symbol>` for
`#[cold]` functions) is matched with the crate disambiguators (`Cs<hash>_`), back-references
(`B<offset>_`) and numbered local-copy suffixes wildcarded, so the list keeps matching across
version bumps and toolchain updates. The instantiating crate a generic's symbol ends with is
wildcarded too: LLVM merges identical instantiations from different crates into one copy,
and which crate's name the survivor keeps changes from build to build (a stripped build
differs from the unstripped one profiled here). A renamed or removed function simply stops matching and
stays in `.text`; nothing breaks, the layout only degrades. Regenerate after large changes:

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
from collections import Counter
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_LIB_OUTPUT = REPO_ROOT / "grit-lib" / "hot-text.ld"
DEFAULT_CLI_OUTPUT = REPO_ROOT / "grit-cli" / "hot-text.ld"

# The Rust runtime, linked into every binary.
RUNTIME_CRATES = {"std", "core", "alloc", "compiler_builtins", "std_detect", "panic_unwind"}
# Normal dependencies of grit-lib's dependencies that grit-lib never runs: merge3 ships a CLI.
UNUSED_SUBTREES = ["clap"]
# `crate::` path heads in a demangled v0 name with crate hashes removed.
CRATE_HEAD = re.compile(r"(?:^|[<(,\s&*\[;])([a-z_][a-z0-9_]*)::")
HASH = re.compile(r"\[[0-9a-f]+\]")
CRATE_ROOT = re.compile(r"Cs[0-9A-Za-z]*_([0-9]+)")

FILE = "data/d0000/f000000.txt"
OTHER = "data/d0001/f000100.txt"

# Each workload starts from a reset work tree in the given shape and runs its steps in order
# (so a step sees what the previous one left). Every step is profiled.
WORKLOADS: list[tuple[str, str, list[list[str]]]] = [
    ("status", "dirty", [["status"], ["status"], ["status", "--json"]]),
    ("status-clean", "clean", [["status"], ["status", "-s"], ["status", "--markdown"]]),
    ("status-renamed", "renamed", [["status"]]),
    ("status-paths", "dirty", [["status", "--", "data/d0000"]]),
    ("diff", "dirty", [["diff"], ["diff", "--json"], ["diff", "HEAD~1"]]),
    ("diff-staged", "renamed", [["diff", "--cached"]]),
    ("log", "clean", [["log", "--limit", "20"], ["log", "--oneline", "--limit", "100"]]),
    ("log-json", "clean", [["log", "--json", "--limit", "20"]]),
    ("show", "clean", [["show", "HEAD"], ["show", "--json", "HEAD"]]),
    ("revs", "clean", [["revs", "--count", "HEAD"], ["merge-base", "main", "origin/main"]]),
    ("shortlog", "clean", [["shortlog"]]),
    ("blame", "clean", [["blame", FILE]]),
    ("refs", "clean", [["branch"], ["tag"], ["remote"], ["reflog"], ["worktree", "list"]]),
    (
        "branch",
        "clean",
        [
            ["branch", "--json"],
            ["branch", "--merged", "main"],
            ["branch", "hot-text"],
            ["branch", "--move", "hot-text", "hot-text-renamed"],
            ["branch", "--force-delete", "hot-text-renamed"],
        ],
    ),
    ("resolve", "clean", [["resolve", "HEAD"], ["cat", f"HEAD:{FILE}"]]),
    ("config", "clean", [["config", "core.bare"]]),
    ("add", "dirty", [["add", FILE], ["restore", OTHER]]),
    ("commit", "dirty", [["commit", "-m", "hot-text workload"]]),
    ("switch", "clean", [["switch", "synth/b000001"], ["switch", "main"]]),
    ("stash", "dirty", [["stash"], ["stash", "pop"]]),
]

CRATE_DISAMBIGUATOR = re.compile(r"Cs[0-9A-Za-z]+_")
BACKREF = re.compile(r"B[0-9A-Za-z]*_")
# The instantiating crate root that ends a generic's v0 symbol (`...Cs*_4grit`), once the
# disambiguator is wildcarded.
INSTANTIATING_CRATE = re.compile(r"Cs\*_([0-9]+)([A-Za-z_][A-Za-z0-9_]*)$")
# Numbered local copies (`name.1234`, `name.llvm.5678`) are renumbered by every build.
LOCAL_SUFFIX = re.compile(r"(\.llvm)?(\.[0-9]+)+$")
CALLGRIND_FN = re.compile(r"^c?fn=\(\d+\) (.+)$")


def run_env(isolated: bool) -> dict[str, str]:
    """A plain user environment, or the census's isolated config (`grit-utils` bench_env.rs)."""
    env = {
        k: v for k, v in os.environ.items() if not k.startswith(("GIT_", "GRIT_", "RUST_"))
    }
    env.update(
        GIT_AUTHOR_NAME="hot-text",
        GIT_AUTHOR_EMAIL="hot-text@example.com",
        GIT_COMMITTER_NAME="hot-text",
        GIT_COMMITTER_EMAIL="hot-text@example.com",
    )
    if isolated:
        env.update(
            GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_SYSTEM="/dev/null"
        )
    return env


def git(repo: Path, *args: str) -> None:
    subprocess.run(
        ["git", *args], cwd=repo, env=run_env(isolated=False), check=True, stdout=subprocess.DEVNULL
    )


def build_fixture(root: Path, files: int) -> tuple[Path, list[Path]]:
    """A clone with `files` small files in 100-file directories, history and many refs.

    Every object is packed; remote-tracking refs and lightweight and annotated tags are in
    packed-refs, `main` tracks `origin/main`, and local branches exist to switch to.
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
    git(repo, "branch", "-q", "synth/b000001", "origin/synth/b000001")
    return repo, sorted(repo.glob("data/*/*.txt"))


def apply_shape(repo: Path, paths: list[Path], shape: str) -> None:
    """Back on `main` at `origin/main` with a clean tree, then `shape` applied."""
    git(repo, "checkout", "-q", "-f", "main")
    git(repo, "reset", "-q", "--hard", "origin/main")
    git(repo, "clean", "-fdq")
    git(repo, "stash", "clear")
    count = max(len(paths) // 100, 1)
    if shape == "dirty":
        for path in paths[:count]:
            with path.open("a") as f:
                f.write("dirty\n")
        for i in range(count):
            (repo / f"untracked-{i}.txt").write_text(f"untracked {i}\n")
    elif shape == "renamed":
        git(repo, "mv", str(paths[0].relative_to(repo)), "data/renamed.txt")
    # Past the second the reset wrote, so the first command re-checks racy entries and
    # rewrites the index, as the first command after a checkout or edit does.
    time.sleep(1.1)


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


def crate_libs(package: str, features: str = "") -> set[str]:
    """Library names in `package`'s normal dependency graph, `package` included."""
    args = ["cargo", "tree", "-p", package, "-e", "normal", "--prefix", "none", "-f", "{lib}"]
    if features:
        args += ["--features", features]
    out = subprocess.run(args, cwd=REPO_ROOT, check=True, capture_output=True, text=True).stdout
    return {line.split()[0] for line in out.splitlines() if line.strip()}


def library_crates() -> set[str]:
    """Crates whose code counts as grit-lib's: the library, its dependencies and the runtime."""
    libs = crate_libs("grit-lib", "http-ureq,tracing")
    for root in UNUSED_SUBTREES:
        libs -= crate_libs(root)
    return libs | RUNTIME_CRATES


def library_symbols(symbols: set[str], allowed: set[str]) -> set[str]:
    """The subset of `symbols` whose demangled paths name only `allowed` crates."""
    names = sorted(symbols)
    demangled = subprocess.run(
        ["c++filt"], input="\n".join(names), check=True, capture_output=True, text=True
    ).stdout.splitlines()
    keep = set()
    for name, readable in zip(names, demangled):
        crates = set(CRATE_HEAD.findall(HASH.sub("", readable)))
        if crates and crates <= allowed and mangled_crates(name) <= allowed:
            keep.add(name)
    return keep


def mangled_crates(symbol: str) -> set[str]:
    """Every crate root (`Cs<hash>_<len><name>`) in a v0 symbol, including the instantiating
    crate of a generic, which the demangled form leaves out."""
    crates = set()
    for m in CRATE_ROOT.finditer(symbol):
        length = int(m.group(1))
        crates.add(symbol[m.end(1) : m.end(1) + length])
    return crates


def section_pattern(symbol: str) -> str:
    """Input-section globs for `symbol`; `#[cold]` functions live in `.text.unlikely.*`."""
    pattern = CRATE_DISAMBIGUATOR.sub("Cs*_", symbol)
    pattern = BACKREF.sub("B*_", pattern)
    suffix = LOCAL_SUFFIX.search(pattern)
    if suffix:
        pattern = pattern[: suffix.start()]
    crate = INSTANTIATING_CRATE.search(pattern)
    if crate and int(crate.group(1)) == len(crate.group(2)):
        pattern = pattern[: crate.start()] + "Cs*_*"
    if suffix:
        pattern += ".*"
    return f".text.{pattern} .text.unlikely.{pattern}"


LIB_HEADER = [
    " * grit-lib's hot code path (library, runtime and dependency functions its common",
    " * operations run) in one output section ahead of .text, ordered by how many",
    " * workloads share them, so an operation faults in a few contiguous code windows",
    " * instead of scattered ones. Linked through grit-lib's `hot-text-layout` feature",
    " * (grit-lib/build.rs). Patterns that stop matching are harmless: the function stays",
    " * in .text. */",
]
CLI_HEADER = [
    " * The rest of the `grit` binary's hot code path (the CLI, clap, and library generics",
    " * the CLI instantiates) in one output section ahead of .text, ordered by how many",
    " * workloads share them. grit-lib/hot-text.ld holds the library's own functions; this",
    " * fragment is linked into the `grit` binary only (grit-cli/build.rs). Patterns that",
    " * stop matching are harmless: the function stays in .text. */",
]


def render(section: str, header: list[str], patterns: list[str]) -> str:
    lines = ["/* Generated by scripts/gen_hot_text_layout.py; do not edit by hand.", " *"]
    lines += header
    lines += ["SECTIONS", "{", f"  {section} : {{"]
    lines += [f"    *({p})" for p in patterns]
    lines += ["  }", "}", "INSERT BEFORE .text;", ""]
    return "\n".join(lines)


def ordered(first_seen: dict[str, None], shared: Counter[str]) -> list[str]:
    """Patterns most workloads share first, ties in first-seen order."""
    order = {p: i for i, p in enumerate(first_seen)}
    return sorted(first_seen, key=lambda p: (-shared[p], order[p]))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--grit", type=Path, required=True, help="unstripped grit binary")
    parser.add_argument("--lib-output", type=Path, default=DEFAULT_LIB_OUTPUT)
    parser.add_argument("--cli-output", type=Path, default=DEFAULT_CLI_OUTPUT)
    # Large enough that index parsing and the work-tree scans take their threaded paths.
    parser.add_argument("--files", type=int, default=10000, help="tracked files in the fixture")
    args = parser.parse_args()

    if shutil.which("valgrind") is None:
        print("valgrind is required", file=sys.stderr)
        return 1
    grit = args.grit.resolve()
    symbols = defined_text_symbols(grit)
    if not any(s.startswith("_R") for s in symbols):
        print(f"{grit} has no Rust symbols; build it with strip = false", file=sys.stderr)
        return 1
    library = library_symbols(symbols, library_crates())

    # pattern -> number of workloads that ran it; dict order is first-seen order.
    lib_seen: dict[str, None] = {}
    cli_seen: dict[str, None] = {}
    shared: Counter[str] = Counter()
    with tempfile.TemporaryDirectory(prefix="grit-hot-text-") as tmp:
        root = Path(tmp)
        repo, paths = build_fixture(root, args.files)
        for name, shape, steps in WORKLOADS:
            apply_shape(repo, paths, shape)
            seen: set[str] = set()
            for i, argv in enumerate(steps):
                # The first step runs with the census's isolated config, later ones with the
                # usual config cascade.
                env = run_env(isolated=i == 0)
                out = root / f"{name}-{i}.callgrind"
                for n in executed_functions(grit, repo, argv, out, env):
                    if n not in symbols:
                        continue
                    p = section_pattern(n)
                    (lib_seen if n in library else cli_seen).setdefault(p, None)
                    seen.add(p)
            shared.update(seen)
            print(
                f"{name}: {len(seen)} functions, {len(lib_seen)} library and "
                f"{len(cli_seen)} CLI total",
                file=sys.stderr,
            )

    # Wildcarded patterns can coincide; the library's fragment keeps a shared one.
    cli_seen = {p: None for p in cli_seen if p not in lib_seen}
    for section, header, seen_patterns, output in [
        (".text.grit_hot", LIB_HEADER, lib_seen, args.lib_output),
        (".text.grit_cli_hot", CLI_HEADER, cli_seen, args.cli_output),
    ]:
        patterns = ordered(seen_patterns, shared)
        output.write_text(render(section, header, patterns))
        print(f"wrote {len(patterns)} patterns to {output}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
