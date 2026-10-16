"""Pins and snapshot comparison for scripts/public-api.sh."""
from __future__ import annotations

import difflib
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PUBLIC_API_SCRIPT = Path(__file__).resolve().parent / "public-api.sh"
SNAPSHOT_PATH = ROOT / "grit-lib" / "public-api.txt"


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
    import sys

    args = list(sys.argv[1:] if argv is None else argv)
    if len(args) == 1 and args[0] in {"--print-pins", "print-pins"}:
        for key, value in parse_pins().items():
            print(f"{key}={value}")
        return 0
    if len(args) == 3 and args[0] == "check":
        ok, _ = compare_snapshot_files(Path(args[1]), Path(args[2]))
        return 0 if ok else 1
    if len(args) == 3 and args[0] == "diff":
        _, diff = compare_snapshot_files(Path(args[1]), Path(args[2]))
        sys.stdout.write(diff)
        return 0
    print("usage: public_api.py check|diff <expected> <actual>", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
