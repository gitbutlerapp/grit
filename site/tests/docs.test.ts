import fs from "node:fs";
import path from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { findPage, hrefTo, isLibrarySlug, librarySlugs, linkTargetSlug, loadSite, resetSiteCache } from "../lib/docs";
import { REPO_ROOT } from "../lib/repo";
import { slugify } from "../lib/text";
import { withContentCopy } from "./helpers";

let cleanup: (() => void) | undefined;
afterEach(() => {
  cleanup?.();
  cleanup = undefined;
  resetSiteCache();
});

describe("docs content model", () => {
  it("has a page for every command source, at /docs/<command>/", () => {
    const site = loadSite();
    const sources = fs.readdirSync(path.join(REPO_ROOT, "content/docs/commands")).filter((n) => n.endsWith(".md") && n !== "README.md");
    expect(site.commandPages.map((p) => p.slug).sort()).toEqual(sources.map((n) => n.slice(0, -3)).sort());
  });

  it("orders commands by manifest group, then order", () => {
    const site = loadSite();
    const groups = site.commandPages.map((p) => site.commandGroups.indexOf(p.group));
    expect(groups).toEqual([...groups].sort((a, b) => a - b));
  });

  it("puts the library overview first and the quick start in the library half", () => {
    const site = loadSite();
    expect(librarySlugs(site)[0]).toBe("library");
    expect(isLibrarySlug("library-quickstart")).toBe(true);
    expect(isLibrarySlug("commit")).toBe(false);
  });

  it("computes relative hrefs between pages", () => {
    expect(hrefTo("index", "commit")).toBe("commit/");
    expect(hrefTo("commit", "index")).toBe("../");
    expect(hrefTo("library/refs", "commit")).toBe("../../commit/");
    expect(linkTargetSlug("library/refs", "../objects/")).toBe("library/objects");
    expect(linkTargetSlug("commit", "../add/")).toBe("add");
  });

  it("expands includes with a tag for the layout and plain fences in twins", () => {
    const page = findPage(loadSite(), "library/refs")!;
    expect(page.markdown).toContain("```rust include=grit-examples/src/bin/guide_refs.rs");
    expect(page.twin).toContain("```rust\n");
    expect(page.twin).not.toContain("include=");
  });

  it("makes twin links absolute", () => {
    const twin = findPage(loadSite(), "tutorial")!.twin;
    expect(twin).toContain("](https://grit-scm.com/docs/install/index.md)");
    expect(twin).not.toContain("](../install/)");
  });

  it("slugifies headings like the old generator", () => {
    expect(slugify("Your home base: grit status")).toBe("your-home-base-grit-status");
    expect(slugify("Pluggable ref storage (RefStore)")).toBe("pluggable-ref-storage-refstore");
    expect(slugify("--json")).toBe("json");
  });

  it("fails when site.toml lists a missing page", () => {
    cleanup = withContentCopy((dir) => fs.rmSync(path.join(dir, "install.md")));
    expect(() => loadSite()).toThrow(/missing page install.md/);
  });

  it("fails when a Markdown file isn't listed", () => {
    cleanup = withContentCopy((dir) => fs.writeFileSync(path.join(dir, "stray.md"), "---\ntitle: Stray\n---\n"));
    expect(() => loadSite()).toThrow(/not listed in site.toml: stray.md/);
  });

  it("fails when a code fence has no language", () => {
    cleanup = withContentCopy((dir) => fs.appendFileSync(path.join(dir, "install.md"), "\n```\nnope\n```\n"));
    expect(() => loadSite()).toThrow(/install.md:\d+: code fence missing language tag/);
  });

  it("fails when a command has an unknown group", () => {
    cleanup = withContentCopy((dir) => {
      const file = path.join(dir, "commands/commit.md");
      fs.writeFileSync(file, fs.readFileSync(file, "utf8").replace("group: Making changes", "group: Nope"));
    });
    expect(() => loadSite()).toThrow(/group "Nope" must be one of/);
  });
});
