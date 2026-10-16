import { renderToStaticMarkup } from "react-dom/server";
import { beforeAll, describe, expect, it } from "vitest";
import { CommandLayout, LibraryLayout, StepsLayout } from "../components/docs/Layouts";
import { findPage, loadSite } from "../lib/docs";
import { initHighlighter } from "../lib/highlight";

beforeAll(() => initHighlighter());

describe("docs layouts", () => {
  it("puts command examples, JSON and See also in the ink column", () => {
    const site = loadSite();
    const html = renderToStaticMarkup(<CommandLayout page={findPage(site, "commit")!} site={site} />);
    const [main, aside] = html.split('<aside class="col ink-col"');
    expect(main).toContain('class="synopsis"');
    expect(main).not.toContain('id="examples"');
    expect(aside).toContain('<span class="f">grit</span> commit');
    expect(aside).toContain("--json output");
    expect(aside).toContain('class="see-also"');
  });

  it("puts a library guide's included example in the ink column", () => {
    const site = loadSite();
    const html = renderToStaticMarkup(<>{LibraryLayout({ page: findPage(site, "library/refs")!, site })}</>);
    const [main, aside] = html.split('<aside class="col ink-col lib"');
    expect(aside).toContain("Example · guide_refs.rs");
    expect(aside).toContain("https://github.com/gitbutlerapp/grit/blob/main/grit-examples/src/bin/guide_refs.rs");
    expect(main).not.toContain("include=");
  });

  it("lays the tutorial out as numbered steps with code beside each", () => {
    const site = loadSite();
    const html = renderToStaticMarkup(<StepsLayout page={findPage(site, "tutorial")!} site={site} />);
    expect(html).toContain('<span class="num">01</span>');
    expect((html.match(/class="step-code"/g) ?? []).length).toBeGreaterThan(5);
  });
});
