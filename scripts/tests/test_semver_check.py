"""Unit tests for scripts/semver-check.sh baseline resolution."""

from __future__ import annotations

import os
import shutil
import stat
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SEMVER_CHECK_SH = ROOT / "scripts" / "semver-check.sh"


def _run_script(repo: Path, subcmd: str, *, env: dict[str, str] | None = None) -> str:
    script = repo / "scripts" / "semver-check.sh"
    assert script.is_file()
    run_env = {**os.environ, **(env or {})}
    out = subprocess.check_output(
        [str(script), subcmd],
        cwd=repo,
        env=run_env,
        text=True,
    )
    return out.strip()


def _write_executable(path: Path, content: str) -> None:
    path.write_text(textwrap.dedent(content), encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


def _init_repo(repo: Path, *, pin: str, workspace_version: str = "0.5.32") -> None:
    (repo / "grit-lib").mkdir(parents=True)
    (repo / "grit-lib" / "api-ratchet.toml").write_text(
        textwrap.dedent(
            f"""
            [meta]
            semver_baseline = "{pin}"
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )
    (repo / "Cargo.toml").write_text(
        textwrap.dedent(
            f"""
            [workspace.package]
            version = "{workspace_version}"
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )
    scripts = repo / "scripts"
    scripts.mkdir(parents=True)
    shutil.copy2(SEMVER_CHECK_SH, scripts / "semver-check.sh")
    os.chmod(scripts / "semver-check.sh", SEMVER_CHECK_SH.stat().st_mode)

    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    subprocess.run(["git", "config", "user.email", "test@example.com"], cwd=repo, check=True)
    subprocess.run(["git", "config", "user.name", "Test"], cwd=repo, check=True)
    subprocess.run(["git", "add", "-A"], cwd=repo, check=True)
    subprocess.run(["git", "commit", "-qm", "init"], cwd=repo, check=True)


def _tag(repo: Path, name: str) -> None:
    subprocess.run(["git", "tag", name], cwd=repo, check=True)


class SemverCheckBaselineTests(unittest.TestCase):
    def test_baseline_pin_with_no_tags(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            pin = "pinned-deadbeef"
            _init_repo(repo, pin=pin)
            self.assertEqual(_run_script(repo, "baseline"), pin)

    def test_baseline_pin_with_only_v0531(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            pin = "pinned-deadbeef"
            _init_repo(repo, pin=pin)
            _tag(repo, "v0.5.31")
            self.assertEqual(_run_script(repo, "baseline"), pin)

    def test_baseline_v060_when_reachable(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            pin = "pinned-deadbeef"
            _init_repo(repo, pin=pin)
            _tag(repo, "v0.6.0")
            self.assertEqual(_run_script(repo, "baseline"), "v0.6.0")

    def test_baseline_latest_v06_with_multiple_tags(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            pin = "pinned-deadbeef"
            _init_repo(repo, pin=pin)
            _tag(repo, "v0.6.0")
            _tag(repo, "v0.6.2")
            self.assertEqual(_run_script(repo, "baseline"), "v0.6.2")

    def test_semver_baseline_env_wins(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            pin = "pinned-deadbeef"
            _init_repo(repo, pin=pin)
            _tag(repo, "v0.6.2")
            self.assertEqual(
                _run_script(repo, "baseline", env={"SEMVER_BASELINE": "custom-ref"}),
                "custom-ref",
            )

    def test_check_cmd_passes_release_type_minor_for_v06(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            pin = "pinned-deadbeef"
            _init_repo(repo, pin=pin, workspace_version="0.6.1")
            _tag(repo, "v0.6.0")
            cmd = _run_script(repo, "check-cmd")
            self.assertIn("--release-type", cmd)
            self.assertIn("minor", cmd)
            self.assertIn("v0.6.0", cmd)

    def test_baseline_pin_with_only_prerelease_v060_rc(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            pin = "pinned-deadbeef"
            _init_repo(repo, pin=pin)
            _tag(repo, "v0.6.0-rc.1")
            self.assertEqual(_run_script(repo, "baseline"), pin)

    def test_baseline_stable_v060_when_rc_also_present(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            pin = "pinned-deadbeef"
            _init_repo(repo, pin=pin)
            _tag(repo, "v0.6.0")
            _tag(repo, "v0.6.0-rc.1")
            self.assertEqual(_run_script(repo, "baseline"), "v0.6.0")

    def test_check_cmd_no_release_type_minor_with_only_rc(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            pin = "pinned-deadbeef"
            _init_repo(repo, pin=pin, workspace_version="0.6.1")
            _tag(repo, "v0.6.0-rc.1")
            cmd = _run_script(repo, "check-cmd")
            self.assertNotIn("--release-type", cmd)
            self.assertIn(pin, cmd)


if __name__ == "__main__":
    unittest.main()
