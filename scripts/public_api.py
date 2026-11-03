"""Pins, snapshot comparison, and public API test-item ratchet for scripts/public-api.sh."""
from __future__ import annotations

import difflib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PUBLIC_API_SCRIPT = Path(__file__).resolve().parent / "public-api.sh"
SNAPSHOT_PATH = ROOT / "grit-lib" / "public-api.txt"
ALLOWLIST_PATH = ROOT / "grit-lib" / "public-api-test-items.allow"
HIDDEN_OFFENDERS_PATH = ROOT / "grit-lib" / "public-api-test-items.hidden-offenders"
DEFAULT_RUSTDOC_JSON = ROOT / "target" / "doc" / "grit_lib.json"

TEST_ITEM_FORBIDDEN_WORDS = frozenset(
    {
        "test",
        "tests",
        "probe",
        "probes",
        "fixture",
        "fixtures",
        "bench",
        "benches",
    }
)


def parse_cargo_public_api_version_output(text: str) -> str | None:
    """Parse `cargo public-api --version` stdout (e.g. ``cargo-public-api 0.52.0``)."""
    match = re.match(r"cargo-public-api\s+(\S+)", text.strip())
    return match.group(1) if match else None


def parse_pins(script_path: Path | None = None) -> dict[str, str]:
    """Read pinned nightly date and cargo-public-api version from the shell wrapper."""
    path = script_path or PUBLIC_API_SCRIPT
    text = path.read_text(encoding="utf-8")
    nightly = re.search(r'PUBLIC_API_NIGHTLY_DATE="([^"]+)"', text)
    version = re.search(r'CARGO_PUBLIC_API_VERSION="([^"]+)"', text)
    if nightly is None or version is None:
        msg = f"missing pin constants in {path}"
        raise ValueError(msg)
    return {
        "nightly_date": nightly.group(1),
        "cargo_public_api_version": version.group(1),
    }


def unified_diff(
    expected: str,
    actual: str,
    *,
    fromfile: str = "grit-lib/public-api.txt",
    tofile: str = "regenerated",
) -> str:
    """Return a unified diff between two snapshot texts (empty when they match)."""
    lines = list(
        difflib.unified_diff(
            expected.splitlines(keepends=True),
            actual.splitlines(keepends=True),
            fromfile=fromfile,
            tofile=tofile,
        )
    )
    return "".join(lines)


def segment_words(segment: str) -> list[str]:
    """Split a Rust path segment on ``_`` and CamelCase into lowercase words."""
    words: list[str] = []
    for chunk in segment.split("_"):
        if not chunk:
            continue
        split = re.sub(r"([a-z0-9])([A-Z])", r"\1 \2", chunk)
        split = re.sub(r"([A-Z]+)([A-Z][a-z])", r"\1 \2", split)
        words.extend(part for part in split.split() if part)
    return [word.lower() for word in words]


def path_violates_test_item_rule(path: str) -> bool:
    """Return whether any path segment looks like a test/bench/fixture hook name."""
    for segment in path.split("::"):
        for word in segment_words(segment):
            if word in TEST_ITEM_FORBIDDEN_WORDS:
                return True
    return False


def normalize_grit_lib_path(raw: str) -> str:
    """Normalize one ``grit_lib::…`` fragment from a snapshot line."""
    tail = raw
    if "(" in tail:
        tail = re.sub(r"\(.*", "", tail)
    elif ": " in tail:
        tail = tail.rsplit(": ", 1)[0]
    tail = re.sub(r"\s:.*", "", tail)
    return tail.rstrip(":").strip()


def item_paths_from_snapshot_line(line: str) -> list[str]:
    """Extract every ``grit_lib::…`` path referenced on one snapshot line."""
    line = line.strip()
    if not line:
        return []
    paths: list[str] = []
    for match in re.finditer(r"grit_lib::[^\s(,)>-]+", line):
        paths.append(normalize_grit_lib_path(match.group(0)))
    return paths


def offending_paths_from_snapshot_text(text: str) -> set[str]:
    """Collect unique snapshot paths that violate the test-item word rule."""
    paths: set[str] = set()
    for line in text.splitlines():
        for path in item_paths_from_snapshot_line(line):
            if path_violates_test_item_rule(path):
                paths.add(path)
    return paths


def offending_paths_from_rustdoc_json(rustdoc_path: Path) -> set[str]:
    """Collect public rustdoc paths (including ``#[doc(hidden)]``) that violate the rule."""
    payload = json.loads(rustdoc_path.read_text(encoding="utf-8"))
    index = payload.get("index", {})
    paths_meta = payload.get("paths", {})
    out: set[str] = set()
    for _pid, meta in paths_meta.items():
        if meta.get("crate_id") != 0:
            continue
        parts = meta.get("path") or []
        if not parts or parts[0] != "grit_lib":
            continue
        item = index.get(str(_pid)) or index.get(int(_pid))  # type: ignore[arg-type]
        if item is not None and item.get("visibility") != "public":
            continue
        full = "::".join(parts)
        if path_violates_test_item_rule(full):
            out.add(full)
    return out


def ensure_rustdoc_json_with_hidden_items(*, nightly: str, output: Path) -> None:
    """Build rustdoc JSON documenting hidden items (cargo-public-api has no flag in 0.52)."""
    src_root = ROOT / "grit-lib" / "src"
    newest_src = max(p.stat().st_mtime for p in src_root.rglob("*.rs"))
    if output.is_file() and output.stat().st_mtime >= newest_src:
        return
    output.parent.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    env["RUSTDOCFLAGS"] = (
        "--document-hidden-items "
        "-A rustdoc::broken_intra_doc_links "
        "-A rustdoc::private_intra_doc_links"
    )
    cmd = [
        "cargo",
        f"+nightly-{nightly}",
        "doc",
        "-p",
        "grit-lib",
        "--features",
        "http-ureq",
        "--no-deps",
        "-Z",
        "unstable-options",
        "--output-format",
        "json",
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
    generated = ROOT / "target" / "doc" / "grit_lib.json"
    if not generated.is_file():
        msg = f"expected rustdoc JSON at {generated}"
        raise SystemExit(msg)
    if generated.resolve() != output.resolve():
        output.write_bytes(generated.read_bytes())


def check_test_items_allowlist() -> tuple[bool, str]:
    """Verify allowlist matches snapshot + hidden rustdoc offenders; list is shrink-only."""
    snapshot_text = SNAPSHOT_PATH.read_text(encoding="utf-8")
    snapshot_offenders = offending_paths_from_snapshot_text(snapshot_text)
    pins = parse_pins()
    ensure_rustdoc_json_with_hidden_items(
        nightly=pins["nightly_date"],
        output=DEFAULT_RUSTDOC_JSON,
    )
    offenders = set(snapshot_offenders)
    offenders |= offending_paths_from_rustdoc_json(DEFAULT_RUSTDOC_JSON)
    hidden_expected = sorted(offenders - snapshot_offenders)

    allow_raw = ALLOWLIST_PATH.read_text(encoding="utf-8")
    allow = {
        line.strip()
        for line in allow_raw.splitlines()
        if line.strip() and not line.strip().startswith("#")
    }
    allow_sorted = sorted(allow)
    file_lines = [
        line.strip()
        for line in allow_raw.splitlines()
        if line.strip() and not line.strip().startswith("#")
    ]
    if file_lines != allow_sorted:
        return False, "public-api-test-items.allow must be sorted (one path per line)"

    if HIDDEN_OFFENDERS_PATH.is_file():
        hidden_file = [
            line.strip()
            for line in HIDDEN_OFFENDERS_PATH.read_text(encoding="utf-8").splitlines()
            if line.strip() and not line.strip().startswith("#")
        ]
        if hidden_file != hidden_expected:
            return (
                False,
                "public-api-test-items.hidden-offenders is out of date "
                "(regenerate from allowlist minus snapshot offenders)",
            )
    elif hidden_expected:
        return False, "missing grit-lib/public-api-test-items.hidden-offenders"

    missing = sorted(offenders - allow)
    stale = sorted(allow - offenders)
    if missing or stale:
        parts: list[str] = []
        if missing:
            parts.append(
                "paths missing from allowlist:\n"
                + "\n".join(f"  {path}" for path in missing)
            )
        if stale:
            parts.append(
                "stale allowlist entries:\n" + "\n".join(f"  {path}" for path in stale)
            )
        return False, "\n".join(parts)
    return True, ""


def compare_snapshot_files(
    expected_path: Path,
    actual_path: Path,
) -> tuple[bool, str]:
    """Return whether the files match and a unified diff when they do not."""
    expected = expected_path.read_text(encoding="utf-8")
    actual = actual_path.read_text(encoding="utf-8")
    if expected == actual:
        return True, ""
    return False, unified_diff(expected, actual)


def main(argv: list[str] | None = None) -> int:
    """CLI helpers for scripts/public-api.sh (check exit code, diff text)."""
    args = list(sys.argv[1:] if argv is None else argv)
    if len(args) == 1 and args[0] in {"--print-pins", "print-pins"}:
        for key, value in parse_pins().items():
            print(f"{key}={value}")
        return 0
    if len(args) == 1 and args[0] == "check-test-items":
        ok, message = check_test_items_allowlist()
        if not ok:
            sys.stderr.write(message)
            if not message.endswith("\n"):
                sys.stderr.write("\n")
            return 1
        return 0
    if len(args) == 3 and args[0] == "check":
        ok, _ = compare_snapshot_files(Path(args[1]), Path(args[2]))
        return 0 if ok else 1
    if len(args) == 3 and args[0] == "diff":
        _, diff = compare_snapshot_files(Path(args[1]), Path(args[2]))
        sys.stdout.write(diff)
        return 0
    print(
        "usage: public_api.py check|diff|check-test-items …",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
