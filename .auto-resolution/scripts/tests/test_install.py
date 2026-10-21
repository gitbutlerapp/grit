"""Regression tests for grit-scm.com shell install scripts."""

from __future__ import annotations

import os
import subprocess
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]

INSTALL_CLOSING_SNIPPET = """
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo ""
    echo "Note: $INSTALL_DIR is not on your PATH."
    echo "Add it with:  export PATH=\\"$INSTALL_DIR:\\$PATH\\""
    ;;
esac
echo "$("$INSTALL_DIR/$BIN" --version)"
"""


class InstallScriptTests(unittest.TestCase):
    def test_stable_install_script_uses_install_dir_for_version(self) -> None:
        text = (REPO_ROOT / "site/public/install").read_text(encoding="utf-8")
        self.assertIn('"$INSTALL_DIR/$BIN" --version', text)
        self.assertNotIn("$(grit --version)", text)
        self.assertNotIn("command -v grit", text)

    def test_nightly_install_script_uses_install_dir_for_version(self) -> None:
        text = (REPO_ROOT / "site/public/install-nightly").read_text(encoding="utf-8")
        self.assertIn('"$INSTALL_DIR/$BIN" --version', text)
        self.assertNotIn("$(grit --version)", text)
        self.assertNotIn("command -v grit", text)

    def test_closing_block_reports_installed_binary_not_path_grit(self) -> None:
        with self.subTest("custom INSTALL_DIR not on PATH"):
            stdout = self._run_closing_block(
                path_grit_version="grit 0.5.0",
                install_grit_version="grit 0.5.7",
                install_dir_on_path=False,
            )
            self.assertIn("grit 0.5.7", stdout)
            self.assertNotIn("grit 0.5.0", stdout)
            self.assertIn("not on your PATH", stdout)

        with self.subTest("INSTALL_DIR on PATH but older grit is earlier"):
            stdout = self._run_closing_block(
                path_grit_version="grit 0.5.0",
                install_grit_version="grit 0.5.7",
                install_dir_on_path=True,
            )
            self.assertIn("grit 0.5.7", stdout)
            self.assertNotIn("grit 0.5.0", stdout)
            self.assertNotIn("not on your PATH", stdout)

    def _run_closing_block(
        self,
        *,
        path_grit_version: str,
        install_grit_version: str,
        install_dir_on_path: bool,
    ) -> str:
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            path_bin = tmp_path / "path_bin"
            install_dir = tmp_path / "install_bin"
            path_bin.mkdir()
            install_dir.mkdir()

            def write_grit(dest: Path, version_line: str) -> None:
                dest.write_text(f"#!/bin/sh\necho '{version_line}'\n", encoding="utf-8")
                dest.chmod(0o755)

            write_grit(path_bin / "grit", path_grit_version)
            write_grit(install_dir / "grit", install_grit_version)

            path_entries = [str(path_bin)]
            if install_dir_on_path:
                path_entries.append(str(install_dir))
            env = os.environ.copy()
            env["PATH"] = os.pathsep.join(path_entries)
            env["INSTALL_DIR"] = str(install_dir)
            env["BIN"] = "grit"

            proc = subprocess.run(
                ["/bin/sh", "-c", INSTALL_CLOSING_SNIPPET],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            return proc.stdout


if __name__ == "__main__":
    unittest.main()
