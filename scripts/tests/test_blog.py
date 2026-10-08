"""Unit tests for deterministic blog generation."""
from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import blog  # noqa: E402


class MarkdownFenceTest(unittest.TestCase):
    def test_language_tag_on_opening_fence_is_not_emitted(self) -> None:
        md = "```console\n$ grit status\nOn main\n```\n"
        html_out, _ = blog.markdown_to_html(md)
        self.assertIn("$ grit status", html_out)
        self.assertNotIn("console", html_out)


class BlogMarkdownTwinTest(unittest.TestCase):
    def test_every_post_has_markdown_twin(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-blog-out-") as tmp:
            out = Path(tmp)
            blog.generate(out)
            posts = blog.load_posts()
            for post in posts:
                md_path = out / post.slug / "index.md"
                self.assertTrue(md_path.is_file(), f"missing markdown twin for {post.slug}")

    def test_post_twin_header_has_title_and_date(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-blog-out-") as tmp:
            out = Path(tmp)
            blog.generate(out)
            post = blog.load_posts()[0]
            md = (out / post.slug / "index.md").read_text(encoding="utf-8")
            self.assertTrue(md.startswith(f"# {post.title}"))
            self.assertIn(f"**Date:** {post.published.isoformat()}", md)
            if post.summary:
                self.assertIn(f"> {post.summary}", md)

    def test_post_twin_links_are_absolute(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-blog-out-") as tmp:
            out = Path(tmp)
            blog.generate(out)
            md = (out / "goodbye-grit-git" / "index.md").read_text(encoding="utf-8")
            self.assertIn(
                f"]({blog.post_markdown_url('a-new-site-and-a-new-focus')})",
                md,
            )
            self.assertNotIn("](https://grit-scm.com/blog/a-new-site-and-a-new-focus/)", md)

    def test_post_html_head_links_markdown_alternate(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-blog-out-") as tmp:
            out = Path(tmp)
            blog.generate(out)
            post = blog.load_posts()[0]
            html_out = (out / post.slug / "index.html").read_text(encoding="utf-8")
            self.assertIn('rel="alternate" type="text/markdown" href="index.md"', html_out)


class BlogDeterminismTest(unittest.TestCase):
    def test_empty_feed_output_is_stable(self) -> None:
        first = blog.render_feed([])
        second = blog.render_feed([])
        self.assertEqual(first, second)
        self.assertIn("Thu, 01 Jan 1970 00:00:00 +0000", first)

    def test_load_posts_requires_date(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            content = Path(tmp)
            (content / "undated.md").write_text("---\ntitle: No date\n---\n\nHello.\n", encoding="utf-8")
            with mock.patch.object(blog, "CONTENT_DIR", content):
                with self.assertRaises(SystemExit):
                    blog.load_posts()


if __name__ == "__main__":
    unittest.main()
