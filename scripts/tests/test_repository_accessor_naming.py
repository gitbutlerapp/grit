"""Ratchet: Repository handle accessors match naming from public-api snapshot (task #7122)."""

from __future__ import annotations

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PUBLIC_API = ROOT / "grit-lib" / "public-api.txt"

LEGACY_PATTERNS = (
    "Repository::index_handle",
    "Repository::rev_walk",
    "Repository::refs(",
)

REQUIRED_SNIPPETS = (
    "Repository::ref_store",
    "Repository::index(",
    "Repository::revwalk(",
    # cargo-public-api includes the receiver lifetime in the path
    "grit_lib::refs::References<'repo>::store",
)


class RepositoryAccessorNamingTests(unittest.TestCase):
    def test_public_api_matches_repository_handle_naming(self) -> None:
        text = PUBLIC_API.read_text(encoding="utf-8")
        for legacy in LEGACY_PATTERNS:
            self.assertNotIn(
                legacy,
                text,
                f"{PUBLIC_API} must not contain legacy accessor {legacy!r}",
            )
        for required in REQUIRED_SNIPPETS:
            self.assertIn(
                required,
                text,
                f"{PUBLIC_API} should list {required!r}",
            )


if __name__ == "__main__":
    unittest.main()
