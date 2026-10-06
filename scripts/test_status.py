#!/usr/bin/env python3
"""Per-test status store: one TOML file per harness test file.

Layout: ``data/tests/<group>/<stem>.toml`` where ``group`` is the upstream Git
test family (``t0``–``t9``, from the first digit of the numeric prefix after
``t``) and ``stem`` is the harness file name without ``.sh``. ``file`` and
``group`` are derived from the path and never stored in the file itself.

Keys (flat scalars, written in this order):

    in_scope = "yes"          # "yes" or "skip"
    tests_total = 92
    passed_last = 92
    failing = 0
    fully_passing = true
    status = "ok"             # "ok", "error", "timeout", or free text
    expect_failure = 8

Python 3.11+ reads TOML via ``tomllib``; the stdlib has no writer, so a tiny
serializer for this flat schema lives here. Writes are atomic (temp file +
``os.replace`` in the same directory) so concurrent family runs and dashboard
reads never see a half-written file.
"""

from __future__ import annotations

import os
import tempfile
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TESTS_DATA = REPO / "data" / "tests"

FIELDS = [
    "in_scope",
    "tests_total",
    "passed_last",
    "failing",
    "fully_passing",
    "status",
    "expect_failure",
]

_DEFAULTS = {
    "in_scope": "yes",
    "tests_total": 0,
    "passed_last": 0,
    "failing": 0,
    "fully_passing": False,
    "status": "",
    "expect_failure": 0,
}


def group_from_stem(stem: str) -> str:
    """Harness group from a test file stem (no ``.sh``).

    Uses the first digit of the numeric prefix after ``t``, matching upstream
    Git test family numbering (``tNNNN-…`` → family ``N``'s first digit).
    """
    if not stem.startswith("t"):
        return "t?"
    rest = stem[1:]
    i = 0
    while i < len(rest) and rest[i].isdigit():
        i += 1
    digits = rest[:i]
    if len(digits) >= 1:
        return f"t{digits[0]}"
    return "t?"


def path_for(stem: str, group: str | None = None, data_dir: Path | None = None) -> Path:
    base = data_dir if data_dir is not None else TESTS_DATA
    return base / (group or group_from_stem(stem)) / f"{stem}.toml"


def _coerce(raw: dict) -> dict:
    """Apply defaults and native types so readers never KeyError/TypeError."""
    out = {}
    out["in_scope"] = str(raw.get("in_scope", _DEFAULTS["in_scope"]))
    for key in ("tests_total", "passed_last", "failing", "expect_failure"):
        try:
            out[key] = int(raw.get(key, _DEFAULTS[key]))
        except (TypeError, ValueError):
            out[key] = 0
    out["fully_passing"] = bool(raw.get("fully_passing", _DEFAULTS["fully_passing"]))
    out["status"] = str(raw.get("status", _DEFAULTS["status"]))
    return out


def _read(path: Path) -> dict | None:
    try:
        with path.open("rb") as f:
            raw = tomllib.load(f)
    except (OSError, tomllib.TOMLDecodeError):
        return None
    return _coerce(raw)


def load_all(data_dir: Path | None = None) -> dict[str, dict]:
    """All test statuses keyed by stem; values carry derived ``file``/``group``."""
    base = data_dir if data_dir is not None else TESTS_DATA
    rows: dict[str, dict] = {}
    if not base.is_dir():
        return rows
    for path in sorted(base.glob("*/*.toml")):
        fields = _read(path)
        if fields is None:
            continue
        fields["file"] = path.stem
        fields["group"] = path.parent.name
        rows[path.stem] = fields
    return rows


def load_one(stem: str, group: str | None = None, data_dir: Path | None = None) -> dict | None:
    path = path_for(stem, group, data_dir)
    fields = _read(path)
    if fields is None:
        return None
    fields["file"] = stem
    fields["group"] = path.parent.name
    return fields


def _toml_str(s: str) -> str:
    out = s.replace("\\", "\\\\").replace('"', '\\"')
    out = out.replace("\n", "\\n").replace("\r", "\\r").replace("\t", "\\t")
    return f'"{out}"'


def _serialize(fields: dict) -> str:
    lines = []
    for key in FIELDS:
        value = fields[key]
        if isinstance(value, bool):
            rendered = "true" if value else "false"
        elif isinstance(value, int):
            rendered = str(value)
        else:
            rendered = _toml_str(str(value))
        lines.append(f"{key} = {rendered}")
    return "\n".join(lines) + "\n"


def save(stem: str, group: str, fields: dict, data_dir: Path | None = None) -> Path:
    """Atomically write one test's status TOML; returns the path written."""
    path = path_for(stem, group, data_dir)
    path.parent.mkdir(parents=True, exist_ok=True)
    content = _serialize(_coerce(fields))
    # Skip identical rewrites: the catalog re-saves every test on each run and
    # most are unchanged (avoids mtime churn across ~1600 files).
    try:
        if path.read_text(encoding="utf-8") == content:
            return path
    except OSError:
        pass
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{stem}.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(content)
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise
    return path


def prune(keep_stems: set[str], data_dir: Path | None = None) -> list[str]:
    """Remove TOMLs for tests no longer in tests/; returns removed stems."""
    base = data_dir if data_dir is not None else TESTS_DATA
    removed: list[str] = []
    if not base.is_dir():
        return removed
    for path in sorted(base.glob("*/*.toml")):
        if path.stem not in keep_stems:
            path.unlink()
            removed.append(path.stem)
    return removed
