#!/usr/bin/env python3
"""Regression tests for ``run-tests.sh`` strict + ``--list`` target resolution."""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
RUN_TESTS = REPO / "scripts" / "run-tests.sh"
GRIT_BIN = REPO / "target" / "release" / "grit-git"


def _ensure_stub_grit() -> None:
    GRIT_BIN.parent.mkdir(parents=True, exist_ok=True)
    if not GRIT_BIN.exists():
        GRIT_BIN.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        GRIT_BIN.chmod(0o755)


def _run_strict_list(list_body: str, *, extra_args: list[str] | None = None) -> subprocess.CompletedProcess[str]:
    _ensure_stub_grit()
    with tempfile.TemporaryDirectory() as tmp:
        list_path = Path(tmp) / "list.txt"
        list_path.write_text(list_body, encoding="utf-8")
        data_dir = Path(tmp) / "data"
        cmd = [
            str(RUN_TESTS),
            "--strict",
            "--no-catalog",
            "--quiet",
            "--list",
            str(list_path),
            "--data-dir",
            str(data_dir),
        ]
        if extra_args:
            cmd.extend(extra_args)
        return subprocess.run(
            cmd,
            cwd=REPO,
            text=True,
            capture_output=True,
            env={**os.environ, "PATH": os.environ.get("PATH", "")},
        )


class StrictListResolution(unittest.TestCase):
    def _assert_fails_without_running(self, proc: subprocess.CompletedProcess[str]) -> None:
        self.assertNotEqual(proc.returncode, 0, msg=proc.stdout + proc.stderr)
        combined = proc.stdout + proc.stderr
        self.assertNotIn("Running ", combined)
        self.assertIn("strict mode", proc.stderr.lower())

    def test_empty_list_file_fails(self) -> None:
        proc = _run_strict_list("")
        self._assert_fails_without_running(proc)
        self.assertIn("no runnable entries", proc.stderr.lower())

    def test_comment_only_list_file_fails(self) -> None:
        proc = _run_strict_list("# CI smoke subset\n\n# t0000-basic.sh\n")
        self._assert_fails_without_running(proc)
        self.assertIn("no runnable entries", proc.stderr.lower())

    def test_unmatched_list_entry_fails(self) -> None:
        proc = _run_strict_list("t999999-no-such-harness-file.sh\n")
        self.assertNotEqual(proc.returncode, 0, msg=proc.stdout + proc.stderr)
        self.assertIn("strict mode", proc.stderr.lower())

    def test_all_unmatched_resolves_to_empty_and_fails(self) -> None:
        proc = _run_strict_list(
            "# comment only\n"
            "not-a-real-test.sh\n"
            "t888888-also-missing.sh\n"
        )
        self.assertNotEqual(proc.returncode, 0, msg=proc.stdout + proc.stderr)
        combined = proc.stderr.lower()
        self.assertIn("strict mode", combined)
        self.assertTrue(
            "zero runnable" in combined or "no test files matched" in combined,
            msg=proc.stderr,
        )

    def test_mixed_valid_and_invalid_list_fails_without_running(self) -> None:
        proc = _run_strict_list(
            "t0000-basic.sh\n"
            "t999999-no-such-harness-file.sh\n",
            extra_args=["--timeout", "1"],
        )
        self.assertNotEqual(proc.returncode, 0, msg=proc.stdout + proc.stderr)
        self.assertIn("strict mode", proc.stderr.lower())
        # Should fail during list resolution, not after executing t0000-basic.
        self.assertNotIn("Running 1 test file", proc.stderr + proc.stdout)


if __name__ == "__main__":
    unittest.main()
