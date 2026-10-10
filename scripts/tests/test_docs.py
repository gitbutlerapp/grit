"""Unit tests for the sectioned docs generator."""
from __future__ import annotations

import re
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import blog  # noqa: E402
import docs  # noqa: E402
import rustdoc_links  # noqa: E402


class DocsManifestTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        rustdoc_links.ensure_local_rustdoc(docs.DOC_ROOT, repo_root=ROOT)

    def setUp(self) -> None:
        self.content = Path(tempfile.mkdtemp(prefix="grit-docs-test-"))
        self.addCleanup(lambda: shutil.rmtree(self.content, ignore_errors=True))
        shutil.copytree(ROOT / "content" / "docs", self.content, dirs_exist_ok=True)

    def test_missing_manifest_page_fails(self) -> None:
        manifest = self.content / "site.toml"
        text = manifest.read_text(encoding="utf-8")
        manifest.write_text(
            text
            + """

[[section.page]]
file = "does-not-exist.md"
slug = "missing"
label = "Missing"
""",
            encoding="utf-8",
        )
        sections = docs.load_manifest_from(manifest, self.content)
        with self.assertRaises(SystemExit) as ctx:
            docs.validate_manifest(sections, content_dir=self.content)
        self.assertIn("missing page", str(ctx.exception))

    def test_unlisted_markdown_fails(self) -> None:
        (self.content / "orphan.md").write_text("---\ntitle: Orphan\n---\n\nHi.\n", encoding="utf-8")
        sections = docs.load_manifest_from(self.content / "site.toml", self.content)
        with self.assertRaises(SystemExit) as ctx:
            docs.validate_manifest(sections, content_dir=self.content)
        self.assertIn("not listed in site.toml", str(ctx.exception))

    def test_sidebar_contains_every_section(self) -> None:
        site = docs.load_site(content_dir=self.content)
        html_out = docs.sidebar(site, "index")
        for section in site.sections:
            self.assertIn(section.title, html_out)

    def test_sidebar_grit_lib_api_links_to_library_guide(self) -> None:
        site = docs.load_site(content_dir=self.content)
        for current in ("index", "status"):
            html_out = docs.sidebar(site, current)
            expected = docs.href_to(current, docs.LIBRARY_GUIDE_SLUG)
            self.assertIn(f'<a href="{expected}">grit-lib API</a>', html_out)
        # Library pages use the ink nav, headed by a link to the library overview.
        for current in ("library", "library/refs", "library-quickstart"):
            html_out = docs.sidebar(site, current)
            expected = docs.href_to(current, docs.LIBRARY_GUIDE_SLUG)
            self.assertIn(f'<a class="label" href="{expected}">use grit_lib;</a>', html_out)

    def test_command_page_puts_examples_in_ink_column(self) -> None:
        site = docs.load_site(content_dir=self.content)
        page = next(p for p in site.command_pages if p.slug == "commit")
        html_out = docs.render(page, site, is_index=False)
        main, aside = html_out.split('<aside class="col ink-col"', 1)
        self.assertIn('class="synopsis"', main)
        self.assertNotIn("<h2 id=\"examples\">", main)
        self.assertIn('<span class="p">$</span> <span class="f">grit</span> commit', aside)
        self.assertIn("--json output", aside)
        self.assertIn('class="see-also"', aside)

    def test_code_highlighting_marks_tokens_by_language(self) -> None:
        rust = docs.highlight_rust('#[derive(Debug)]\nfn main() { let s: &\'a str = "x"; println!("{s}"); Repo::open(1)?; }\n# hidden')
        for marked in ('<span class="a">#[derive(Debug)]</span>', '<span class="k">fn</span>', '<span class="f">main</span>',
                       '<span class="a">&#x27;a</span>', '<span class="s">&quot;x&quot;</span>', '<span class="f">println!</span>',
                       '<span class="t">Repo</span>', '<span class="n">1</span>'):
            self.assertIn(marked, rust)
        self.assertNotIn("hidden", rust)
        console = docs.highlight_console('$ grit log --json | head -3\nabc123 output')
        self.assertIn('<span class="f">grit</span>', console)
        self.assertIn('<span class="a">--json</span>', console)
        self.assertIn('<span class="f">head</span>', console)
        self.assertIn('<span class="out">abc123 output</span>', console)
        self.assertIn('<span class="k">&quot;a&quot;</span>: <span class="n">1</span>', docs.highlight_json('{"a": 1}'))

    def test_library_guide_puts_included_example_in_ink_column(self) -> None:
        site = docs.load_site(content_dir=self.content)
        page = next(p for p in site.pages if p.slug == "library/refs")
        html_out = docs.render(page, site, is_index=False)
        main, aside = html_out.split('<aside class="col ink-col lib"', 1)
        self.assertNotIn("rust:include=", html_out)
        self.assertIn("Example · guide_refs.rs", aside)
        self.assertIn(f"{docs.GITHUB_BLOB}/grit-examples/src/bin/guide_refs.rs", aside)
        self.assertNotIn("guide_refs.rs", main.split("<main", 1)[1].split("pager")[0])

    def test_section_bundles_hold_each_half(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-docs-out-") as tmp:
            out = Path(tmp)
            docs.generate(out, content_dir=self.content, llms_dir=out / "llms")
            cli = (out / docs.CLI_BUNDLE).read_text(encoding="utf-8")
            lib = (out / docs.LIB_BUNDLE).read_text(encoding="utf-8")
            self.assertIn(f"# {blog.SITE_URL}/docs/commit/index.md", cli)
            self.assertIn(f"# {blog.SITE_URL}/docs/tutorial/index.md", cli)
            self.assertNotIn(f"# {blog.SITE_URL}/docs/library/refs/index.md", cli)
            self.assertIn(f"# {blog.SITE_URL}/docs/library/refs/index.md", lib)
            self.assertIn(f"# {blog.SITE_URL}/docs/library-quickstart/index.md", lib)
            self.assertNotIn(f"# {blog.SITE_URL}/docs/commit/index.md", lib)
            index_md = (out / "index.md").read_text(encoding="utf-8")
            self.assertIn("## For agents", index_md)
            self.assertIn(docs.bundle_url(docs.CLI_BUNDLE), index_md)
            llms = (out / "llms" / "llms.txt").read_text(encoding="utf-8")
            self.assertIn(docs.bundle_url(docs.LIB_BUNDLE), llms)

    def test_command_urls_unchanged(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-docs-out-") as tmp:
            out = Path(tmp)
            docs.generate(out, content_dir=self.content, llms_dir=out / "llms")
            for name in ("status", "commit", "fetch", "upload-pack"):
                path = out / name / "index.html"
                self.assertTrue(path.is_file(), f"missing command page {name}")

    def test_rustdoc_links_resolve_function_and_enum_kinds(self) -> None:
        fn_md = docs.prepare_markdown_body("[f](rustdoc:grit_lib::objects::parse_tree)")
        self.assertIn("grit_lib/objects/fn.parse_tree.html", fn_md)
        self.assertNotIn("struct.parse_tree", fn_md)
        self.assertNotIn("grit_lib/grit_lib", fn_md)

        enum_md = docs.prepare_markdown_body("[e](rustdoc:grit_lib::error::Error)")
        self.assertIn("grit_lib/error/enum.Error.html", enum_md)

    def test_generate_builds_rustdoc_when_grit_lib_docs_missing(self) -> None:
        doc_root = docs.DOC_ROOT
        grit_lib = doc_root / "grit_lib"
        self.assertTrue(grit_lib.is_dir(), "setUpClass should have built rustdoc")

        backup_parent = Path(tempfile.mkdtemp(prefix="grit-rustdoc-backup-"))
        self.addCleanup(lambda: shutil.rmtree(backup_parent, ignore_errors=True))
        shutil.move(str(grit_lib), str(backup_parent / "grit_lib"))

        def restore() -> None:
            if not grit_lib.is_dir() and (backup_parent / "grit_lib").is_dir():
                shutil.move(str(backup_parent / "grit_lib"), str(grit_lib))

        self.addCleanup(restore)

        with tempfile.TemporaryDirectory(prefix="grit-docs-out-") as tmp:
            docs.generate(Path(tmp), content_dir=self.content, llms_dir=Path(tmp) / "llms")
            html = (Path(tmp) / "library" / "objects" / "index.html").read_text(encoding="utf-8")
            self.assertIn("grit_lib/objects/fn.parse_tree.html", html)
            self.assertNotIn("struct.parse_tree", html)

        restore()

    def test_every_fence_has_language(self) -> None:
        cases = (
            ("tutorial.md", lambda text: text.replace("```console\n", "```\n", 1)),
            ("commands/README.md", lambda text: text + "\n\n```\nignored\n```\n"),
        )
        for rel, mutate in cases:
            with self.subTest(page=rel):
                shutil.copytree(ROOT / "content" / "docs", self.content, dirs_exist_ok=True)
                page = self.content / rel
                page.write_text(mutate(page.read_text(encoding="utf-8")), encoding="utf-8")
                with self.assertRaises(SystemExit) as ctx:
                    docs.load_site(content_dir=self.content)
                self.assertIn("missing language tag", str(ctx.exception))
                self.assertIn(rel, str(ctx.exception))

    def test_every_page_has_markdown_twin(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-docs-out-") as tmp:
            out = Path(tmp)
            docs.generate(out, content_dir=self.content, llms_dir=out / "llms")
            html_files = list(out.rglob("index.html"))
            md_files = list(out.rglob("index.md"))
            self.assertEqual(len(md_files), len(html_files))
            for html_path in html_files:
                md_path = html_path.with_suffix(".md")
                self.assertTrue(md_path.is_file(), f"missing markdown twin for {html_path}")

    def test_markdown_twin_has_title_summary_and_no_html_chrome(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-docs-out-") as tmp:
            out = Path(tmp)
            docs.generate(out, content_dir=self.content, llms_dir=out / "llms")
            status_md = (out / "status" / "index.md").read_text(encoding="utf-8")
            self.assertTrue(status_md.startswith("# "))
            self.assertIn("\n> ", status_md)
            for marker in docs.HTML_CHROME_MARKERS:
                self.assertNotIn(marker, status_md)

    def test_markdown_twin_links_are_absolute(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-docs-out-") as tmp:
            out = Path(tmp)
            docs.generate(out, content_dir=self.content, llms_dir=out / "llms")
            tutorial_md = (out / "tutorial" / "index.md").read_text(encoding="utf-8")
            self.assertIn(f"]({blog.SITE_URL}/docs/install/index.md)", tutorial_md)
            self.assertNotIn("](../install/)", tutorial_md)

    def test_markdown_twin_expands_includes_and_rustdoc_links(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-docs-out-") as tmp:
            out = Path(tmp)
            docs.generate(out, content_dir=self.content, llms_dir=out / "llms")
            quickstart_md = (out / "library-quickstart" / "index.md").read_text(encoding="utf-8")
            self.assertIn("```rust", quickstart_md)
            self.assertIn("docs.rs/grit-lib", quickstart_md)
            objects_md = (out / "library" / "objects" / "index.md").read_text(encoding="utf-8")
            self.assertIn("grit_lib/objects/fn.parse_tree.html", objects_md)

    def test_html_head_links_markdown_alternate(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-docs-out-") as tmp:
            out = Path(tmp)
            docs.generate(out, content_dir=self.content, llms_dir=out / "llms")
            html_out = (out / "status" / "index.html").read_text(encoding="utf-8")
            self.assertIn('rel="alternate" type="text/markdown" href="index.md"', html_out)
            self.assertIn('href="index.md">Markdown</a>', html_out)

    def test_docs_index_markdown_includes_command_table(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-docs-out-") as tmp:
            out = Path(tmp)
            docs.generate(out, content_dir=self.content, llms_dir=out / "llms")
            index_md = (out / "index.md").read_text(encoding="utf-8")
            self.assertIn("## Commands", index_md)
            self.assertIn("| Command | Summary |", index_md)
            self.assertIn(f"]({blog.SITE_URL}/docs/status/index.md)", index_md)

    def test_llms_txt_optional_lists_blog_posts(self) -> None:
        site = docs.load_site(content_dir=self.content)
        llms = docs.render_llms_txt(site)
        optional_start = llms.index("## Optional")
        optional = llms[optional_start:]
        posts = blog.load_posts()
        self.assertGreater(len(posts), 0)
        positions: list[int] = []
        for post in posts:
            url = blog.post_markdown_url(post.slug)
            self.assertIn(url, optional)
            positions.append(optional.index(url))
        self.assertEqual(positions, sorted(positions))

    def test_llms_txt_lists_every_manifest_page(self) -> None:
        site = docs.load_site(content_dir=self.content)
        llms = docs.render_llms_txt(site)
        for page in site.pager_order:
            url = docs.markdown_canonical_url(page.slug)
            self.assertIn(url, llms, msg=f"missing llms link for {page.slug}")

    def test_llms_txt_follows_llmstxt_structure(self) -> None:
        site = docs.load_site(content_dir=self.content)
        llms = docs.render_llms_txt(site)
        lines = llms.splitlines()
        self.assertEqual(lines[0], "# Grit")
        self.assertTrue(any(line.startswith("> ") for line in lines[:10]))
        section_titles = [f"## {section.title}" for section in site.sections]
        section_titles.append("## Optional")
        for heading in section_titles:
            self.assertIn(heading, llms)
        link_lines = [line for line in lines if line.startswith("- [")]
        self.assertGreater(len(link_lines), 20)
        for line in link_lines:
            self.assertRegex(line, r"^- \[.+?\]\(.+?\)(: .+)?$")

    def test_llms_full_contains_every_twin_in_pager_order(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-docs-out-") as tmp:
            out = Path(tmp)
            site = docs.generate(out, content_dir=self.content, llms_dir=out / "llms")
            root = self.content
            manifest_path = root / "site.toml"
            listed = docs.collect_listed_sources_for_dir(
                docs.load_manifest_from(manifest_path, root),
                root,
            )
            full = docs.render_llms_full_txt(
                site,
                root=root,
                manifest_path=manifest_path,
                listed=listed,
            )
            chunks = re.split(r"^# https://grit-scm\.com/docs/.+$", full, flags=re.MULTILINE)
            self.assertEqual(len(chunks), len(site.pager_order) + 1)
            for page, chunk in zip(site.pager_order, chunks[1:], strict=True):
                twin_path = docs.markdown_output_path(out, page.slug)
                twin = twin_path.read_text(encoding="utf-8")
                self.assertEqual(chunk.strip(), twin.strip(), msg=page.slug)

    def test_check_detects_stale_llms_txt(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-docs-check-") as tmp:
            tmp_path = Path(tmp)
            committed = tmp_path / "committed"
            stale = tmp_path / "stale"
            docs.generate(tmp_path / "docs", content_dir=self.content, llms_dir=committed)
            shutil.copytree(committed, stale)
            path = stale / "llms.txt"
            path.write_text(path.read_text(encoding="utf-8") + "stale\n", encoding="utf-8")
            issues = docs.compare_llms_files(stale, committed)
            self.assertTrue(any("llms.txt" in line for line in issues))
            self.assertFalse(docs.compare_llms_files(committed, committed))

    def test_missing_include_file_fails(self) -> None:
        page = self.content / "library-quickstart.md"
        text = page.read_text(encoding="utf-8")
        page.write_text(
            text.replace(
                "grit-examples/src/bin/quickstart.rs",
                "grit-examples/src/bin/does-not-exist.rs",
            ),
            encoding="utf-8",
        )
        with self.assertRaises(SystemExit) as ctx:
            docs.load_site(content_dir=self.content)
        self.assertIn("include missing file", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
