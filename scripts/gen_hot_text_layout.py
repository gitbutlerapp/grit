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
staging, committing, switching over many files and over fewer than
`checkout.thresholdForParallelism`, stashing, picking, reverting, merging), profiled with
callgrind on a clone through `grit` binaries built like the ones that run them, so inlining
matches what is linked: the shipped `dist` build (fat LTO, one codegen unit) and the `release` build that development
and local benchmarking use (ThinLTO over many codegen units, which inlines less and keeps
more functions apart). Functions that the same set of workloads runs are kept together, and
those groups follow each other in reflected binary (Gray code) order over the workloads in
`WORKLOADS` order: neighbouring groups differ in as few workloads as possible, so each
workload's code forms a few long runs instead of one slice per sharing count. The first two
workloads' code is one contiguous run each, and later workloads split into more runs the
further down the list they are. Against a sort by sharing count, the four status workloads
touch 16 to 19 of the library section's windows instead of 22 to 26 and all workloads
together 14% fewer, while commit and stash touch one to five more. The CLI section keeps
the sort by sharing count and uses the Gray order only between equally shared functions:
every command runs the CLI's start-up and argument parsing, so that shared core stays
dense, and the full Gray order there cost the later commands (config, refs, branch) a
window each.

Both orders are only the starting point of a search (`searched`): with each build's code
sizes, it moves groups of functions that run together and keeps a move when the windows
all workloads touch, summed over the profiled builds, do not rise and no workload's own count rises
above what the starting order gave it. On the profile it was added with, that cut the
library section's windows by 6 to 7% and the CLI section's by 10 to 13% over all workloads,
with the largest gains on the later workloads (refs, branch, show, revs, resolve, stash).

Each function's input section (`.text.<v0 symbol>`, or `.text.unlikely.<v0 symbol>` for
`#[cold]` functions) is matched with the crate disambiguators (`Cs<hash>_`), back-references
(`B<offset>_`) and numbered local-copy suffixes wildcarded, so the list keeps matching across
version bumps, toolchain updates and build profiles. The instantiating crate a generic's
symbol ends with is wildcarded too: LLVM merges identical instantiations from different
crates into one copy, and which crate's name the survivor keeps changes from build to build
(a stripped build differs from the unstripped one profiled here). A renamed or removed
function simply stops matching and stays in `.text`; nothing breaks, the layout only
degrades. Impl disambiguators (`Ms<n>_`) are not wildcarded, and they shift with grit-lib's
features: building `grit` together with `grit-utils`, as the census does, turns on
`serde-models` and its impls, while shipped binaries build `grit-cli` alone. Profile both
builds of each profile. Regenerate after large changes:

    for p in "-p grit-cli" "-p grit-cli -p grit-utils"; do
        CARGO_PROFILE_DIST_STRIP=false CARGO_TARGET_DIR=target/hot-text \\
            cargo build --profile dist $p
        CARGO_TARGET_DIR=target/hot-text cargo build --release $p
        n=$(echo "$p" | wc -w)
        cp target/hot-text/dist/grit target/hot-text/grit-dist-$n
        cp target/hot-text/release/grit target/hot-text/grit-release-$n
    done
    python3 scripts/gen_hot_text_layout.py \\
        --grit target/hot-text/grit-dist-2 --grit target/hot-text/grit-release-2 \\
        --grit target/hot-text/grit-dist-4 --grit target/hot-text/grit-release-4

(Linking the `release` build against the fragments being replaced is fine: the profile
reads function names, not addresses.)

Requires valgrind (callgrind) and the system `git` to build the fixture.
"""

from __future__ import annotations

import argparse
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import time
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
# A local branch whose tip differs from `main` in NEAR_FILES files.
NEAR_BRANCH = "synth/near"
NEAR_FILES = 50

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
    # Past one page, `--json` streams its rows in batches.
    ("log-json", "clean", [["log", "--json", "--limit", "20"], ["log", "--json", "--limit", "100"]]),
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
    # Fewer files than `checkout.thresholdForParallelism`: the serial write path.
    ("switch-near", "clean", [["switch", NEAR_BRANCH], ["switch", "main"]]),
    ("stash", "dirty", [["stash"], ["stash", "pop"]]),
    # Last, so the Gray order splits the earlier workloads' runs as little as possible.
    ("pick", "untracked-cache", [["pick", "origin/topic"]]),
    ("revert", "untracked-cache", [["revert", "HEAD"]]),
    ("merge", "untracked-cache", [["merge", "origin/topic"]]),
]

CRATE_DISAMBIGUATOR = re.compile(r"Cs[0-9A-Za-z]+_")
BACKREF = re.compile(r"B[0-9A-Za-z]*_")
# The instantiating crate root that ends a generic's v0 symbol (`...Cs*_4grit`), once the
# disambiguator is wildcarded.
INSTANTIATING_CRATE = re.compile(r"Cs\*_([0-9]+)([A-Za-z_][A-Za-z0-9_]*)$")
# Numbered local copies (`name.1234`, `name.llvm.5678`) are renumbered by every build.
LOCAL_SUFFIX = re.compile(r"(\.llvm)?(\.[0-9]+)+$")
CALLGRIND_FN = re.compile(r"^c?fn=\(\d+\) (.+)$")
# Bytes the kernel maps per file-backed page fault (`fault_around_bytes`).
FAULT_AROUND = 64 * 1024


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
    packed-refs, `main` tracks `origin/main`, and local branches exist to switch to: one at
    the first commit and `NEAR_BRANCH`, one loose commit ahead of `main`.
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
    # A topic forked before the last round that edits the first line of every fifth file:
    # picking, reverting or merging it is a clean three-way merge over thousands of files,
    # some of them merged line by line against the last round's appended line.
    git(source, "checkout", "-q", "-b", "topic", "HEAD~1")
    for path in sorted(source.glob("data/*/*.txt"))[50::5]:
        path.write_text("topic\n" + path.read_text().split("\n", 1)[1])
    git(source, "commit", "-q", "-a", "-m", "topic")
    git(source, "checkout", "-q", "main")
    git(source, "repack", "-adq")
    repo = root / "repo"
    git(root, "clone", "-q", "--no-hardlinks", str(source), str(repo))
    git(repo, "config", "core.untrackedCache", "false")
    git(repo, "config", "core.fsmonitor", "false")
    git(repo, "branch", "-q", "synth/b000001", "origin/synth/b000001")
    git(repo, "switch", "-q", "-c", NEAR_BRANCH)
    paths = sorted(repo.glob("data/*/*.txt"))
    for path in paths[:NEAR_FILES]:
        with path.open("a") as f:
            f.write("near\n")
    git(repo, "commit", "-q", "-a", "-m", "near")
    git(repo, "switch", "-q", "main")
    return repo, paths


def apply_shape(repo: Path, paths: list[Path], shape: str) -> None:
    """Back on `main` at `origin/main` with a clean tree, then `shape` applied.

    `untracked-cache` is a clean tree under the default `core.untrackedCache`, with an
    untracked cache in the index: grit adds one to a large repository's index under that
    default, as in the census's pick and merge fixtures, and every later index read and
    write carries it. Every other shape turns the untracked cache off, as the census's
    status fixtures do.
    """
    if shape == "untracked-cache":
        subprocess.run(
            ["git", "config", "--unset", "core.untrackedCache"],
            cwd=repo,
            env=run_env(isolated=False),
            check=False,
        )
    else:
        git(repo, "config", "core.untrackedCache", "false")
        git(repo, "update-index", "--no-untracked-cache")
    git(repo, "checkout", "-q", "-f", "main")
    git(repo, "reset", "-q", "--hard", "origin/main")
    git(repo, "clean", "-fdq")
    git(repo, "stash", "clear")
    if shape == "untracked-cache":
        git(repo, "-c", "core.untrackedCache=true", "update-index", "--untracked-cache")
        git(repo, "-c", "core.untrackedCache=true", "status")
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


def defined_text_symbols(grit: Path) -> tuple[dict[str, list[str]], dict[str, tuple[int, int]]]:
    """Every function symbol, mapped to all the function symbols at its address, and each
    symbol's body as (address, size in bytes).

    LLVM merges functions whose code is identical into one body and keeps the other names
    as aliases of it. The body's input section is named after one of them, while callgrind
    reports any one name for the address, so a function only lands in the hot section if
    the list names every alias.
    """
    out = subprocess.run(
        ["nm", "--defined-only", "--print-size", str(grit)],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    by_address: dict[str, list[str]] = {}
    bodies: dict[str, tuple[int, int]] = {}
    for line in out.splitlines():
        parts = line.split(" ", 3)
        if len(parts) == 3:
            parts.insert(1, "0")
        if len(parts) == 4 and parts[2] in "tTwW":
            by_address.setdefault(parts[0], []).append(parts[3])
            bodies[parts[3]] = (int(parts[0], 16), int(parts[1], 16))
    aliases = {name: names for names in by_address.values() for name in names}
    return aliases, bodies


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
    """Input-section globs for `symbol`; `#[cold]` functions live in `.text.unlikely.*`.

    Whether a function carries a local-copy suffix depends on the build, not the function:
    ThinLTO in the `release` profile renames functions it shares between codegen units to
    `name.llvm.<n>`, while the single-unit `dist` build profiled here keeps most bare. Each
    function therefore matches both with and without a suffix.
    """
    pattern = CRATE_DISAMBIGUATOR.sub("Cs*_", symbol)
    pattern = BACKREF.sub("B*_", pattern)
    suffix = LOCAL_SUFFIX.search(pattern)
    if suffix:
        pattern = pattern[: suffix.start()]
    crate = INSTANTIATING_CRATE.search(pattern)
    if crate and int(crate.group(1)) == len(crate.group(2)):
        pattern = pattern[: crate.start()] + "Cs*_*"
    names = [pattern] if pattern.endswith("*") else [pattern, f"{pattern}.*"]
    return " ".join(f"{section}.{name}" for section in (".text", ".text.unlikely") for name in names)


LIB_HEADER = [
    " * grit-lib's hot code path (library, runtime and dependency functions its common",
    " * operations run) in one output section ahead of .text, grouped by the set of",
    " * workloads that run them, the groups in Gray code order over the workloads, so an",
    " * operation faults in a few contiguous code windows instead of scattered ones.",
    " * Linked through grit-lib's `hot-text-layout` feature (grit-lib/build.rs). Patterns",
    " * that stop matching are harmless: the function stays in .text. */",
]
CLI_HEADER = [
    " * The rest of the `grit` binary's hot code path (the CLI, clap, and library generics",
    " * the CLI instantiates) in one output section ahead of .text, ordered by how many",
    " * workloads share them, ties in Gray code order over the workloads that run them.",
    " * grit-lib/hot-text.ld holds the library's own functions; this fragment is linked",
    " * into the `grit` binary only (grit-cli/build.rs). Patterns that stop matching are",
    " * harmless: the function stays in .text. */",
]


def render(section: str, header: list[str], patterns: list[str]) -> str:
    lines = ["/* Generated by scripts/gen_hot_text_layout.py; do not edit by hand.", " *"]
    lines += header
    lines += ["SECTIONS", "{", f"  {section} : {{"]
    lines += [f"    *({p})" for p in patterns]
    lines += ["  }", "}", "INSERT BEFORE .text;", ""]
    return "\n".join(lines)


def ordered(
    first_seen: dict[str, None], workloads: list[set[str]], sharing_first: bool
) -> list[str]:
    """Patterns in reflected binary order of the workloads that run them, ties first-seen.

    A pattern's key reads its membership in each workload, earliest workload first, and
    inverts every digit after one the pattern is a member of. Sorting by that key is the
    reflected Gray code order with members ahead of non-members: the first workload's
    patterns lead, and among patterns that agree on every earlier workload, a later
    workload's members are contiguous. With `sharing_first`, patterns run by more workloads
    come first and the Gray order only breaks ties between equally shared ones.
    """
    order = {p: i for i, p in enumerate(first_seen)}

    def key(pattern: str) -> tuple[int, tuple[int, ...], int]:
        digits = []
        flip = 0
        sharing = 0
        for seen in workloads:
            member = int(pattern in seen)
            digits.append(1 - (member ^ flip))
            flip ^= member
            sharing += member
        return -sharing if sharing_first else 0, tuple(digits), order[pattern]

    return sorted(first_seen, key=key)


def searched(
    patterns: list[str],
    builds: list[tuple[list[set[str]], dict[str, int]]],
    iterations: int,
) -> list[str]:
    """Refine `patterns` (in Gray code order) to touch fewer fault-around windows.

    Patterns that every build runs in the same set of workloads are always resident
    together, so they move as one group, in their given order. `builds` holds each
    build's per-workload pattern sets and pattern code sizes. The cost of an order is the
    number of 64 KiB windows each workload's code touches, summed over workloads and builds
    and over section starts a quarter window apart, so it does not hinge on where the
    section happens to begin. A seeded random walk moves one group to another place and
    keeps the move unless the total rises or some workload's own count exceeds what the
    starting order gave it: deterministic, and no workload is worse off than in the Gray
    code order.
    """
    groups: dict[tuple[tuple[int, ...], ...], list[str]] = {}
    for p in patterns:
        key = tuple(tuple(int(p in seen) for seen in sets) for sets, _ in builds)
        groups.setdefault(key, []).append(p)
    keys = list(groups)
    sizes = [
        [sum((sizes.get(p, 0) + 15) & ~15 for p in groups[k]) for k in keys]
        for _, sizes in builds
    ]
    members = [
        [[w for w, member in enumerate(k[b]) if member] for k in keys] for b in range(len(builds))
    ]
    offsets = range(0, FAULT_AROUND, FAULT_AROUND // 4)

    workloads = len(builds[0][0])

    def cost(order: list[int]) -> list[int]:
        per_workload = [0] * workloads
        for b in range(len(builds)):
            for start in offsets:
                windows: list[set[int]] = [set() for _ in range(workloads)]
                at = start
                for g in order:
                    size = sizes[b][g]
                    if size:
                        span = range(at // FAULT_AROUND, (at + size - 1) // FAULT_AROUND + 1)
                        for w in members[b][g]:
                            windows[w].update(span)
                    at += size
                for w, touched in enumerate(windows):
                    per_workload[w] += len(touched)
        return per_workload

    rng = random.Random(0)
    order = list(range(len(keys)))
    ceiling = cost(order)
    best = sum(ceiling)
    for _ in range(iterations):
        i, j = rng.randrange(len(order)), rng.randrange(len(order))
        if i == j:
            continue
        candidate = order[:i] + order[i + 1 :]
        candidate.insert(j, order[i])
        c = cost(candidate)
        if sum(c) <= best and all(x <= limit for x, limit in zip(c, ceiling)):
            order, best = candidate, sum(c)
    return [p for g in order for p in groups[keys[g]]]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument(
        "--grit",
        type=Path,
        action="append",
        required=True,
        help="unstripped grit binary; repeat to profile several builds and merge their lists",
    )
    parser.add_argument("--lib-output", type=Path, default=DEFAULT_LIB_OUTPUT)
    parser.add_argument("--cli-output", type=Path, default=DEFAULT_CLI_OUTPUT)
    # Large enough that index parsing and the work-tree scans take their threaded paths.
    parser.add_argument("--files", type=int, default=10000, help="tracked files in the fixture")
    parser.add_argument(
        "--search-iterations",
        type=int,
        default=60000,
        help="moves the layout search tries per section (0 keeps the Gray code order)",
    )
    args = parser.parse_args()

    if shutil.which("valgrind") is None:
        print("valgrind is required", file=sys.stderr)
        return 1
    builds = []
    crates = library_crates()
    for grit in (path.resolve() for path in args.grit):
        symbols, bodies = defined_text_symbols(grit)
        if not any(s.startswith("_R") for s in symbols):
            print(f"{grit} has no Rust symbols; build it with strip = false", file=sys.stderr)
            return 1
        builds.append((grit, symbols, bodies, library_symbols(set(symbols), crates)))

    # Dict order is first-seen order; `seen_by_workload` holds the patterns each workload
    # ran, in any of the builds. `build_profiles` keeps each build's own per-workload sets
    # and the code size of the bodies each pattern matched there, for the layout search.
    lib_seen: dict[str, None] = {}
    cli_seen: dict[str, None] = {}
    seen_by_workload: dict[str, set[str]] = {name: set() for name, _, _ in WORKLOADS}
    build_profiles: list[tuple[list[set[str]], dict[str, int]]] = []
    with tempfile.TemporaryDirectory(prefix="grit-hot-text-") as tmp:
        root = Path(tmp)
        repo, paths = build_fixture(root, args.files)
        for b, (grit, symbols, bodies, library) in enumerate(builds):
            build_sets: list[set[str]] = []
            pattern_bodies: dict[str, dict[int, int]] = {}
            for name, shape, steps in WORKLOADS:
                apply_shape(repo, paths, shape)
                seen = seen_by_workload[name]
                build_seen: set[str] = set()
                for i, argv in enumerate(steps):
                    # The first step runs with the census's isolated config, later ones with
                    # the usual config cascade.
                    env = run_env(isolated=i == 0)
                    out = root / f"{b}-{name}-{i}.callgrind"
                    for executed in executed_functions(grit, repo, argv, out, env):
                        for n in symbols.get(executed, []):
                            p = section_pattern(n)
                            (lib_seen if n in library else cli_seen).setdefault(p, None)
                            seen.add(p)
                            build_seen.add(p)
                            address, size = bodies[n]
                            pattern_bodies.setdefault(p, {})[address] = size
                build_sets.append(build_seen)
                print(
                    f"{grit} {name}: {len(seen)} functions, {len(lib_seen)} "
                    f"library and {len(cli_seen)} CLI total",
                    file=sys.stderr,
                )
            sizes = {p: sum(found.values()) for p, found in pattern_bodies.items()}
            build_profiles.append((build_sets, sizes))
    workload_sets = [seen_by_workload[name] for name, _, _ in WORKLOADS]

    # Wildcarded patterns can coincide; the library's fragment keeps a shared one.
    cli_seen = {p: None for p in cli_seen if p not in lib_seen}
    # Every command runs the CLI's start-up and argument parsing first, so its shared core
    # stays dense at the front of that section; the library's code differs most by command.
    for section, header, seen_patterns, output, sharing_first in [
        (".text.grit_hot", LIB_HEADER, lib_seen, args.lib_output, False),
        (".text.grit_cli_hot", CLI_HEADER, cli_seen, args.cli_output, True),
    ]:
        patterns = ordered(seen_patterns, workload_sets, sharing_first)
        patterns = searched(patterns, build_profiles, args.search_iterations)
        output.write_text(render(section, header, patterns))
        print(f"wrote {len(patterns)} patterns to {output}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
