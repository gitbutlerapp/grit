"""Unit tests for scripts/public_api.py and public-api.sh pins."""
from __future__ import annotations

import os
import stat
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import public_api as pa  # noqa: E402

PUBLIC_API_SH = ROOT / "scripts" / "public-api.sh"
PIN_VERSION = pa.parse_pins()["cargo_public_api_version"]


def _write_executable(path: Path, content: str) -> None:
    path.write_text(textwrap.dedent(content), encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def _mock_cargo_public_api_layout(
    tmp: Path,
    *,
    installed_version: str,
    pin_version: str = PIN_VERSION,
) -> tuple[Path, Path]:
    """Return (bindir, install_log) with fake cargo + cargo-public-api for wrapper tests."""
    bindir = tmp / "bin"
    bindir.mkdir()
    install_log = tmp / "cargo-install.log"
    tool_path = bindir / "cargo-public-api"

    _write_executable(
        tool_path,
        f"""\
        #!/usr/bin/env bash
        set -euo pipefail
        if [[ "${{1:-}}" == "--version" ]]; then
          echo "cargo-public-api {installed_version}"
          exit 0
        fi
        echo "unexpected cargo-public-api args: $*" >&2
        exit 99
        """,
    )

    _write_executable(
        bindir / "cargo",
        f"""\
        #!/usr/bin/env bash
        set -euo pipefail
        if [[ "${{1:-}}" == "public-api" && "${{2:-}}" == "--version" ]]; then
          exec "{tool_path}" --version
        fi
        if [[ "${{1:-}}" == "install" ]]; then
          echo "$*" >> "{install_log}"
          cat > "{tool_path}" <<'EOF'
        #!/usr/bin/env bash
        set -euo pipefail
        if [[ "${{1:-}}" == "--version" ]]; then
          echo "cargo-public-api {pin_version}"
          exit 0
        fi
        echo "unexpected cargo-public-api args: $*" >&2
        exit 99
        EOF
          chmod +x "{tool_path}"
          exit 0
        fi
        echo "unexpected cargo args: $*" >&2
        exit 98
        """,
    )
    return bindir, install_log


class PublicApiScriptTests(unittest.TestCase):
    def test_parse_pins_matches_public_api_sh(self) -> None:
        pins = pa.parse_pins()
        self.assertEqual(pins["nightly_date"], "2026-10-01")
        self.assertEqual(pins["cargo_public_api_version"], "0.52.0")

    def test_compare_snapshot_files_match(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            left = Path(tmp) / "left.txt"
            right = Path(tmp) / "right.txt"
            left.write_text("pub mod grit_lib\n", encoding="utf-8")
            right.write_text("pub mod grit_lib\n", encoding="utf-8")
            ok, diff = pa.compare_snapshot_files(left, right)
            self.assertTrue(ok)
            self.assertEqual(diff, "")

    def test_compare_snapshot_files_differs(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            left = Path(tmp) / "left.txt"
            right = Path(tmp) / "right.txt"
            left.write_text("pub mod grit_lib\n", encoding="utf-8")
            right.write_text("pub mod grit_lib\npub fn dummy()\n", encoding="utf-8")
            ok, diff = pa.compare_snapshot_files(left, right)
            self.assertFalse(ok)
            self.assertIn("dummy", diff)

    def test_parse_cargo_public_api_version_output(self) -> None:
        self.assertEqual(
            pa.parse_cargo_public_api_version_output("cargo-public-api 0.52.0\n"),
            "0.52.0",
        )
        self.assertIsNone(pa.parse_cargo_public_api_version_output("unexpected"))

    def test_wrapper_accepts_matching_cargo_public_api_version(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            bindir, install_log = _mock_cargo_public_api_layout(
                Path(tmp), installed_version=PIN_VERSION
            )
            env = os.environ.copy()
            env["PATH"] = f"{bindir}{os.pathsep}{env['PATH']}"
            proc = subprocess.run(
                [str(PUBLIC_API_SH), "ensure-cargo-public-api"],
                cwd=ROOT,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertIn(f"cargo-public-api {PIN_VERSION}", proc.stdout)
            self.assertFalse(install_log.exists())

    def test_wrapper_replaces_mismatched_cargo_public_api_version(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            bindir, install_log = _mock_cargo_public_api_layout(
                Path(tmp), installed_version="999.0.0"
            )
            env = os.environ.copy()
            env["PATH"] = f"{bindir}{os.pathsep}{env['PATH']}"
            proc = subprocess.run(
                [str(PUBLIC_API_SH), "ensure-cargo-public-api"],
                cwd=ROOT,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertIn(f"cargo-public-api {PIN_VERSION}", proc.stdout)
            self.assertTrue(install_log.exists())
            logged = install_log.read_text(encoding="utf-8")
            self.assertIn("cargo-public-api", logged)
            self.assertIn(PIN_VERSION, logged)


if __name__ == "__main__":
    unittest.main()
