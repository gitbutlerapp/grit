import { describe, expect, it } from "vitest";
import { findPage, loadSite } from "../lib/docs";
import { bundle, bundleUrl, CLI_BUNDLE, LIB_BUNDLE, llmsFullTxt, llmsTxt, pageTwin } from "../lib/llms";

describe("agent-facing Markdown", () => {
  it("follows the llms.txt structure and lists every page", () => {
    const site = loadSite();
    const llms = llmsTxt(site);
    const lines = llms.split("\n");
    expect(lines[0]).toBe("# Grit");
    expect(lines.slice(0, 10).some((l) => l.startsWith("> "))).toBe(true);
    for (const section of site.sections) expect(llms).toContain(`## ${section.title}`);
    expect(llms).toContain("## Optional");
    expect(llms).toContain(bundleUrl(LIB_BUNDLE));
    for (const line of lines.filter((l) => l.startsWith("- ["))) expect(line).toMatch(/^- \[.+?\]\(.+?\)(: .+)?$/);
    for (const page of site.pages) {
      const url = page.slug === "index" ? "https://grit-scm.com/docs/index.md" : `https://grit-scm.com/docs/${page.slug}/index.md`;
      expect(llms).toContain(url);
    }
  });

  it("puts every twin in llms-full.txt in reading order", () => {
    const site = loadSite();
    const full = llmsFullTxt(site);
    const headings = [...full.matchAll(/^# (https:\/\/grit-scm\.com\/docs\/\S+)$/gm)].map((m) => m[1]);
    expect(headings).toHaveLength(site.pages.length);
  });

  it("splits the bundles into the CLI and library halves", () => {
    const cli = bundle(false);
    const lib = bundle(true);
    expect(cli).toContain("# https://grit-scm.com/docs/commit/index.md");
    expect(cli).toContain("# https://grit-scm.com/docs/tutorial/index.md");
    expect(cli).not.toContain("# https://grit-scm.com/docs/library/refs/index.md");
    expect(lib).toContain("# https://grit-scm.com/docs/library/refs/index.md");
    expect(lib).toContain("# https://grit-scm.com/docs/library-quickstart/index.md");
    expect(lib).not.toContain("# https://grit-scm.com/docs/commit/index.md");
  });

  it("adds the agents section and command table to the overview twin", () => {
    const site = loadSite();
    const index = pageTwin(findPage(site, "index")!, site);
    expect(index).toContain("## For agents");
    expect(index).toContain(bundleUrl(CLI_BUNDLE));
    expect(index).toContain("| Command | Summary |");
    expect(index).toContain("](https://grit-scm.com/docs/status/index.md)");
  });
});
