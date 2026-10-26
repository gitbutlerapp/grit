#!/usr/bin/env python3
"""Classify tests as fast (default run) or full-only (system git, heavy fixtures).

A test belongs to the full run (`--features full-tests`) when it starts the system
`git`, directly or through a helper that does, or when it is a heavy scale or
memory test. Everything else runs by default.

Used by scripts/sync_integration_tests.py (one decision per integration test
file) and by `--mark-unit-tests`, which tags unit tests in `src/` with
`#[cfg_attr(not(feature = "full-tests"), ignore = ...)]`.

    python3 scripts/classify_tests.py            # print a summary
    python3 scripts/classify_tests.py --mark-unit-tests
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CRATES = ["grit-lib", "grit-cli", "grit-utils", "grit-examples", "grit-protocol", "grit-http-server", "grit-test-support"]

# Starting the system git binary.
GIT_SPAWN = re.compile(
    r'Command::new\(\s*"git"\s*\)'
    r'|Command::new\(\s*&?git_bin'
    r'|grit_test_support::(?:git|git_cmd|git_packed_history|git_dir_state_digest)\('
    r'|\bgit_cmd\('
)
# Heavy scale, memory or benchmark-style tests: slow even without git.
HEAVY_NAME = re.compile(r"(perf|memory|rss|campaign|bench|stress|_80k|_50k|large_index|coverage_floor)")
HEAVY_BODY = re.compile(r"heavy_disk_serial_guard\(")

FN = re.compile(r"\bfn\s+([a-z_][a-z0-9_]*)\s*(?:<[^>{]*>)?\s*\(")
# Names too generic to treat as git helpers when seen in another file.
GENERIC = {"run", "new", "setup", "repo", "open", "init", "main", "write", "read", "out", "cmd", "exec", "ok", "check"}


def functions(text: str) -> dict[str, str]:
    """Map each `fn name(` in `text` to its body (brace-matched), last one wins."""
    out: dict[str, str] = {}
    for m in FN.finditer(text):
        start = text.find("{", m.end())
        semi = text.find(";", m.end())
        if start < 0 or (0 <= semi < start):
            continue
        depth, i = 0, start
        while i < len(text):
            c = text[i]
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    break
            i += 1
        out[m.group(1)] = text[start : i + 1]
    return out


def git_functions(bodies: dict[str, str], known: set[str]) -> set[str]:
    """Names among `bodies` that spawn git or call one that does (fixpoint)."""
    found = {n for n, b in bodies.items() if GIT_SPAWN.search(b)}
    changed = True
    while changed:
        changed = False
        names = found | known
        pattern = re.compile(r"\b(" + "|".join(map(re.escape, sorted(names))) + r")\s*\(") if names else None
        for n, b in bodies.items():
            if n not in found and pattern and pattern.search(b):
                found.add(n)
                changed = True
    return found


def shared_git_helpers() -> set[str]:
    """Git-spawning functions in helpers shared across test files."""
    files = list((REPO / "grit-test-support/src").rglob("*.rs"))
    for crate in CRATES:
        tests = REPO / crate / "tests"
        if tests.is_dir():
            files += [p for d in tests.iterdir() if d.is_dir() for p in d.rglob("*.rs")]
    bodies: dict[str, str] = {}
    for p in files:
        bodies.update(functions(p.read_text()))
    return {n for n in git_functions(bodies, set()) if n not in GENERIC}


# Crates whose integration tests are end-to-end scenario and benchmark runs
# (grit-bench and its daily jobs): all of them belong to the full run.
FULL_CRATES = {"grit-utils"}


def file_is_full(path: Path, shared: set[str]) -> str | None:
    """Why `path` (an integration test file) belongs to the full run, or None."""
    if path.parent.parent.name in FULL_CRATES:
        return "scenario"
    text = path.read_text()
    if HEAVY_NAME.search(path.stem) or HEAVY_BODY.search(text):
        return "heavy"
    local = git_functions(functions(text), shared)
    names = local | shared
    if GIT_SPAWN.search(text):
        return "git"
    if names and re.search(r"\b(" + "|".join(map(re.escape, sorted(names))) + r")\s*\(", text):
        return "git"
    return None


IGNORE_REASON = "starts system git or is heavy; run with --features full-tests"


def ignore_attr(indent: str) -> str:
    """The full-tests ignore attribute, laid out the way rustfmt writes it."""
    return (
        f"{indent}#[cfg_attr(\n"
        f'{indent}    not(feature = "full-tests"),\n'
        f'{indent}    ignore = "{IGNORE_REASON}"\n'
        f"{indent})]"
    )
TEST_ATTR = re.compile(r"^(\s*)#\[test\]\s*$", re.M)


def mark_unit_tests(path: Path, shared: set[str]) -> int:
    """Tag each unit test in `path` that needs git (or is heavy); return how many."""
    text = path.read_text()
    if "#[test]" not in text:
        return 0
    bodies = functions(text)
    git_fns = git_functions(bodies, shared)
    names = git_fns | shared
    call = re.compile(r"\b(" + "|".join(map(re.escape, sorted(names))) + r")\s*\(") if names else None
    lines = text.split("\n")
    out, n, i = [], 0, 0
    while i < len(lines):
        line = lines[i]
        m = re.match(r"^(\s*)#\[test\]\s*$", line)
        if m:
            # Find the fn this attribute belongs to.
            j = i + 1
            while j < len(lines) and not FN.search(lines[j]):
                j += 1
            fm = FN.search(lines[j]) if j < len(lines) else None
            name = fm.group(1) if fm else None
            body = bodies.get(name, "") if name else ""
            already = "full-tests" in "\n".join(lines[max(0, i - 3) : j])
            needs = bool(GIT_SPAWN.search(body) or (call and call.search(body)) or HEAVY_BODY.search(body)
                         or (name and HEAVY_NAME.search(name)))
            if needs and not already:
                out.append(ignore_attr(m.group(1)))
                n += 1
        out.append(line)
        i += 1
    if n:
        path.write_text("\n".join(out))
    return n


def main(argv: list[str]) -> int:
    shared = shared_git_helpers()
    if "--mark-unit-tests" in argv:
        total = 0
        for crate in CRATES:
            for p in sorted((REPO / crate / "src").rglob("*.rs")):
                total += mark_unit_tests(p, shared)
        print(f"tagged {total} unit tests")
        return 0
    for crate in CRATES:
        tests = REPO / crate / "tests"
        if not tests.is_dir():
            continue
        files = sorted(p for p in tests.glob("*.rs") if p.name != "integration.rs")
        kinds = [file_is_full(p, shared) for p in files]
        print(f"{crate}: {len(files)} files, fast {kinds.count(None)}, git {kinds.count('git')}, heavy {kinds.count('heavy')}")
    print(f"shared git helpers: {len(shared)}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
