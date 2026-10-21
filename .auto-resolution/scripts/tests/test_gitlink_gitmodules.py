"""Tests for scripts/check_gitlink_gitmodules.py."""

from __future__ import annotations

import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def _run_script() -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "scripts/check_gitlink_gitmodules.py"],
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
    )


class GitlinkGitmodulesScriptTests(unittest.TestCase):
    def test_no_orphan_gitlinks_on_main_tree(self) -> None:
        result = _run_script()
        self.assertEqual(result.returncode, 0, result.stderr or result.stdout)
