#!/usr/bin/env python3
"""Gate: every public grit-lib function/method has at least one execution under lib, tests, and doc tests."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path

from public_api import parse_pins

ROOT = Path(__file__).resolve().parents[1]
GRIT_LIB = ROOT / "grit-lib"
DEFAULT_RUSTDOC_JSON = ROOT / "target" / "doc" / "grit_lib.json"
DEFAULT_ALLOWLIST = GRIT_LIB / "public-fn-untested.txt"
LLVM_COV_TARGET = ROOT / "target" / "llvm-cov-target"
DEFAULT_GRIT_BIN = LLVM_COV_TARGET / "debug" / ("grit.exe" if os.name == "nt" else "grit")

_GENERIC_RE = re.compile(r"<[^<>]*(?:<[^<>]*>[^<>]*)*>")
_CLOSURE_RE = re.compile(r"\{closure#\d+\}(?:::\{closure#\d+\})*")
CANONICAL_MAP_PATH = GRIT_LIB / "canonical-modules.toml"


@lru_cache(maxsize=1)
def canonical_to_rustdoc_prefixes() -> tuple[tuple[str, str], ...]:
    """Map llvm-cov canonical paths back to flat rustdoc path prefixes."""
    if not CANONICAL_MAP_PATH.is_file():
        return ()
    data = tomllib.loads(CANONICAL_MAP_PATH.read_text(encoding="utf-8"))
    mapping = data.get("map", {})
    aliases: list[tuple[str, str]] = []
    for flat, canonical in mapping.items():
        flat = str(flat)
        canonical = str(canonical)
        if canonical in {flat, "internal"}:
            continue
        canon_prefix = "grit_lib::" + canonical.replace(".", "::") + "::"
        flat_prefix = f"grit_lib::{flat}::"
        aliases.append((canon_prefix, flat_prefix))
    aliases.sort(key=lambda pair: len(pair[0]), reverse=True)
    return tuple(aliases)


@dataclass(frozen=True)
class PublicFn:
    """One public function or method in grit-lib from rustdoc JSON."""

    path: str


def rustdoc_json_command(*, nightly: str, output: Path) -> list[str]:
    """Command argv to emit rustdoc JSON for grit-lib."""
    return [
        "cargo",
        f"+nightly-{nightly}",
        "doc",
        "-p",
        "grit-lib",
        "--all-features",
        "--no-deps",
        "-Z",
        "unstable-options",
        "--output-format",
        "json",
    ]


def ensure_rustdoc_json(
    *,
    nightly: str,
    output: Path,
    env: dict[str, str] | None = None,
) -> None:
    """Build rustdoc JSON if missing or stale relative to grit-lib sources."""
    src_root = GRIT_LIB / "src"
    newest_src = max(p.stat().st_mtime for p in src_root.rglob("*.rs"))
    if output.is_file() and output.stat().st_mtime >= newest_src:
        return
    output.parent.mkdir(parents=True, exist_ok=True)
    run_env = dict(os.environ)
    run_env["RUSTDOCFLAGS"] = ""
    if env:
        run_env.update(env)
    cmd = rustdoc_json_command(nightly=nightly, output=output)
    result = subprocess.run(
        cmd,
        cwd=ROOT,
        env=run_env,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stdout)
        sys.stderr.write(result.stderr)
        raise SystemExit(result.returncode)
    generated = ROOT / "target" / "doc" / "grit_lib.json"
    if not generated.is_file():
        msg = f"expected rustdoc JSON at {generated}"
        raise SystemExit(msg)
    if generated.resolve() != output.resolve():
        output.write_bytes(generated.read_bytes())


def parse_public_functions(rustdoc_path: Path) -> list[PublicFn]:
    """Collect public function paths from rustdoc JSON (crate_id 0, kind function)."""
    payload = json.loads(rustdoc_path.read_text(encoding="utf-8"))
    index = payload.get("index", {})
    paths = payload.get("paths", {})
    out: list[PublicFn] = []
    for pid, meta in paths.items():
        if meta.get("crate_id") != 0:
            continue
        if meta.get("kind") != "function":
            continue
        path_parts = meta.get("path") or []
        if not path_parts or path_parts[0] != "grit_lib":
            continue
        item = index.get(str(pid)) or index.get(int(pid))  # type: ignore[arg-type]
        if item is not None and item.get("visibility") != "public":
            continue
        out.append(PublicFn(path="::".join(path_parts)))
    out.sort(key=lambda f: f.path)
    return out


def normalize_rust_path(name: str) -> str:
    """Normalize a demangled Rust path for comparison with rustdoc paths."""
    trimmed = name.strip()
    if trimmed.startswith("(") and trimmed.endswith(")"):
        trimmed = trimmed[1:-1]
    if trimmed.startswith("<") and ">::" in trimmed:
        type_part, rest = trimmed.split(">::", 1)
        trimmed = f"{type_part[1:]}::{rest}"
    for canon_prefix, flat_prefix in canonical_to_rustdoc_prefixes():
        if trimmed.startswith(canon_prefix):
            trimmed = flat_prefix + trimmed[len(canon_prefix) :]
            break
    if "::<" in trimmed:
        trimmed = trimmed.split("::<", 1)[0]
    trimmed = _CLOSURE_RE.sub("", trimmed)
    trimmed = _GENERIC_RE.sub("", trimmed)
    while ":::" in trimmed:
        trimmed = trimmed.replace(":::", "::")
    trimmed = re.sub(r"::+", "::", trimmed)
    return trimmed.strip(":")


def demangle_names(mangled: list[str]) -> list[str]:
    """Demangle rustc symbol names via rustfilt (one name per line)."""
    if not mangled:
        return []
    proc = subprocess.run(
        ["rustfilt"],
        input="\n".join(mangled),
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        raise SystemExit(proc.returncode)
    lines = proc.stdout.splitlines()
    if len(lines) != len(mangled):
        msg = f"rustfilt returned {len(lines)} lines for {len(mangled)} inputs"
        raise SystemExit(msg)
    return lines


def function_execution_counts(cov_paths: list[Path]) -> dict[str, int]:
    """Merge execution counts from one or more cargo llvm-cov JSON exports."""
    counts: dict[str, int] = {}
    for path in cov_paths:
        if not path.is_file():
            msg = f"missing llvm-cov JSON: {path}"
            raise SystemExit(msg)
        payload = json.loads(path.read_text(encoding="utf-8"))
        for entry in payload.get("data", []):
            for func in entry.get("functions", []):
                mangled = func.get("name")
                if not isinstance(mangled, str):
                    continue
                hit = int(func.get("count", 0) or 0)
                if hit <= 0:
                    continue
                existing = counts.get(mangled, 0)
                if hit > existing:
                    counts[mangled] = hit
    demangled = demangle_names(list(counts))
    by_path: dict[str, int] = {}
    for mangled, path in zip(counts, demangled, strict=True):
        norm = normalize_rust_path(path)
        if not norm.startswith("grit_lib::"):
            continue
        by_path[norm] = max(by_path.get(norm, 0), counts[mangled])
    return by_path


def load_allowlist(path: Path) -> list[str]:
    """Read allowlisted public paths (one per line, # comments allowed)."""
    if not path.is_file():
        return []
    lines: list[str] = []
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.split("#", 1)[0].strip()
        if line:
            lines.append(line)
    return lines


def write_allowlist(path: Path, entries: list[str]) -> None:
    """Write sorted allowlist paths."""
    path.parent.mkdir(parents=True, exist_ok=True)
    body = "\n".join(sorted(set(entries))) + ("\n" if entries else "")
    path.write_text(body, encoding="utf-8")


def check(
    public_fns: list[PublicFn],
    executions: dict[str, int],
    allowlist: list[str],
) -> tuple[list[str], list[str]]:
    """Return (table_lines, failure_messages)."""
    public_paths = {f.path for f in public_fns}
    allow = set(allowlist)
    failures: list[str] = []
    table: list[str] = []

    stale_allow = sorted(p for p in allow if executions.get(p, 0) > 0)
    if stale_allow:
        failures.append(
            "allowlist entries now have coverage (remove from "
            f"{DEFAULT_ALLOWLIST.relative_to(ROOT)}): "
            + ", ".join(stale_allow[:20])
            + (" …" if len(stale_allow) > 20 else "")
        )

    unknown_allow = sorted(p for p in allow if p not in public_paths)
    if unknown_allow:
        failures.append(
            "allowlist paths are not public functions anymore: "
            + ", ".join(unknown_allow[:20])
            + (" …" if len(unknown_allow) > 20 else "")
        )

    missing = sorted(
        f.path for f in public_fns if executions.get(f.path, 0) <= 0 and f.path not in allow
    )
    covered = len(public_paths) - len(missing) - len(stale_allow)
    table.append(
        f"public functions: {len(public_paths)} total, "
        f"{covered} covered, {len(missing)} missing, {len(allow)} allowlisted"
    )
    if missing:
        failures.append(
            f"{len(missing)} public functions have zero executions "
            f"(add tests or allowlist): "
            + ", ".join(missing[:25])
            + (" …" if len(missing) > 25 else "")
        )
    return table, failures


def run_llvm_cov_json(
    *,
    grit_bin: Path,
    output: Path,
    doc: bool,
    features: str,
) -> None:
    """Run cargo llvm-cov and write JSON (full export, not summary-only)."""
    env = os.environ.copy()
    env["GRIT_BIN"] = str(grit_bin)
    scope = ["--doc"] if doc else ["--lib", "--tests"]
    cmd = [
        "cargo",
        "llvm-cov",
        *scope,
        "-p",
        "grit-lib",
        "--features",
        features,
        "--json",
        "--output-path",
        str(output),
        "--",
        "--test-threads=1",
    ]
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


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--rustdoc-json",
        type=Path,
        default=DEFAULT_RUSTDOC_JSON,
        help="Path to grit_lib.json from rustdoc",
    )
    parser.add_argument(
        "--cov-json",
        type=Path,
        action="append",
        dest="cov_jsons",
        help="llvm-cov JSON export (repeat to merge lib/tests and doc)",
    )
    parser.add_argument(
        "--allowlist",
        type=Path,
        default=DEFAULT_ALLOWLIST,
        help="Public paths allowed to have zero executions (must shrink)",
    )
    parser.add_argument(
        "--write-allowlist",
        action="store_true",
        help="Write all currently-unexecuted public paths to the allowlist file",
    )
    parser.add_argument(
        "--print-missing",
        action="store_true",
        help="Print unexecuted public paths (respecting allowlist) to stdout",
    )
    parser.add_argument(
        "--run-coverage",
        action="store_true",
        help="Run llvm-cov (lib+tests and doc) before checking",
    )
    parser.add_argument(
        "--grit-bin",
        type=Path,
        default=Path(os.environ.get("GRIT_BIN", DEFAULT_GRIT_BIN)),
    )
    parser.add_argument(
        "--features",
        default=os.environ.get("LLVM_COV_FEATURES", "http-ureq"),
    )
    args = parser.parse_args(argv)

    pins = parse_pins()
    ensure_rustdoc_json(nightly=pins["nightly_date"], output=args.rustdoc_json)

    cov_jsons = list(args.cov_jsons or [])
    if args.run_coverage:
        lib_out = ROOT / "target" / "llvm-cov" / "functions-lib-tests.json"
        doc_out = ROOT / "target" / "llvm-cov" / "functions-doc.json"
        if not args.grit_bin.is_file():
            sys.stderr.write(
                f"grit binary not found at {args.grit_bin}; build grit-cli into "
                f"{LLVM_COV_TARGET} first (see `make coverage`).\n"
            )
            return 2
        run_llvm_cov_json(
            grit_bin=args.grit_bin,
            output=lib_out,
            doc=False,
            features=args.features,
        )
        run_llvm_cov_json(
            grit_bin=args.grit_bin,
            output=doc_out,
            doc=True,
            features=args.features,
        )
        cov_jsons = [lib_out, doc_out]

    if not cov_jsons:
        default = ROOT / "target" / "llvm-cov" / "summary.json"
        if default.is_file():
            cov_jsons = [default]
        else:
            parser.error("pass --cov-json, --run-coverage, or run `make coverage` first")

    public_fns = parse_public_functions(args.rustdoc_json)
    executions = function_execution_counts(cov_jsons)
    allowlist = load_allowlist(args.allowlist)

    if args.write_allowlist:
        missing = sorted(
            f.path for f in public_fns if executions.get(f.path, 0) <= 0
        )
        write_allowlist(args.allowlist, missing)
        print(f"Wrote {len(missing)} entries to {args.allowlist.relative_to(ROOT)}")

    table, failures = check(public_fns, executions, allowlist)
    for line in table:
        print(line)

    if args.print_missing:
        allow = set(allowlist)
        missing = sorted(
            f.path
            for f in public_fns
            if executions.get(f.path, 0) <= 0 and f.path not in allow
        )
        for path in missing:
            print(path)

    if failures:
        sys.stderr.write("\npublic-fn coverage gate failed:\n")
        for msg in failures:
            sys.stderr.write(f"  - {msg}\n")
        return 1
    return 0


__all__ = [
    "PublicFn",
    "check",
    "demangle_names",
    "ensure_rustdoc_json",
    "function_execution_counts",
    "load_allowlist",
    "normalize_rust_path",
    "parse_public_functions",
    "write_allowlist",
]


if __name__ == "__main__":
    raise SystemExit(main())
