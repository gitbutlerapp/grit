import fs from "node:fs";
import path from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { loadPosts } from "../lib/blog";
import { loadSite } from "../lib/docs";
import { validateGritLibPaths, validateLinks } from "../lib/validate";
import { withContentCopy } from "./helpers";

let cleanup: (() => void) | undefined;
afterEach(() => {
  cleanup?.();
  cleanup = undefined;
});

describe("link validation", () => {
  it("passes on the real content", () => {
    expect(validateLinks(loadSite(), loadPosts())).toEqual([]);
  });

  it("reports links to missing pages and anchors", () => {
    cleanup = withContentCopy((dir) =>
      fs.appendFileSync(path.join(dir, "install.md"), "\n\nSee [nowhere](../nowhere/) and [bad anchor](../commit/#nope).\n"),
    );
    const problems = validateLinks(loadSite(), loadPosts());
    expect(problems.some((p) => p.includes('no docs page "nowhere"'))).toBe(true);
    expect(problems.some((p) => p.includes("no #nope on commit"))).toBe(true);
  });
});

describe("grit_lib path validation", () => {
  it("passes on the real content", () => {
    expect(validateGritLibPaths(loadSite())).toEqual([]);
  });

  it("reports paths through private modules", () => {
    cleanup = withContentCopy((dir) =>
      fs.appendFileSync(path.join(dir, "library", "embedding.md"), "\n\nServe with `grit_lib::serve::upload_pack`.\n"),
    );
    const problems = validateGritLibPaths(loadSite());
    expect(problems).toHaveLength(1);
    expect(problems[0]).toMatch(/^content\/docs page library\/embedding: grit_lib::serve::upload_pack: /);
  });
});
