import { beforeAll, describe, expect, it } from "vitest";
import { resolveRustdocPath, rustdocAvailable, unresolvedGritLibPaths, unresolvedRustdocLinks } from "../lib/rustdoc";

beforeAll(() => {
  if (!rustdocAvailable()) {
    throw new Error("grit-lib rustdoc missing; run: cargo doc -p grit-lib --no-deps");
  }
});

describe("resolveRustdocPath associated items", () => {
  it("links enum variants", () => {
    expect(resolveRustdocPath("grit_lib::revwalk::history::HistoryError::MergeCommitInReplay")).toBe(
      "revwalk/history/enum.HistoryError.html#variant.MergeCommitInReplay",
    );
  });

  it("links struct fields", () => {
    expect(resolveRustdocPath("grit_lib::revwalk::history::AbsorbOptions::base")).toBe(
      "revwalk/history/struct.AbsorbOptions.html#structfield.base",
    );
  });

  it("links methods (not struct fields)", () => {
    expect(resolveRustdocPath("grit_lib::revwalk::history::RewritePlan::ref_update_lines")).toBe(
      "revwalk/history/struct.RewritePlan.html#method.ref_update_lines",
    );
  });

  it("follows re-exports of items that are public elsewhere", () => {
    expect(resolveRustdocPath("grit_lib::config::ConfigHandle")).toBe("config/handle/struct.ConfigHandle.html");
    expect(resolveRustdocPath("grit_lib::porcelain::branch::DeleteMode::Safe")).toBe(
      "porcelain/branch_error/enum.DeleteMode.html#variant.Safe",
    );
  });

  it("rejects paths through private modules", () => {
    expect(() => resolveRustdocPath("grit_lib::serve::ServeOptions")).toThrow(/module not found/);
    expect(resolveRustdocPath("grit_lib::remote::serve::ServeOptions")).toBe("remote/serve/struct.ServeOptions.html");
  });

  it("rejects unknown associated items", () => {
    expect(() =>
      resolveRustdocPath("grit_lib::revwalk::history::RewritePlan::definitely_missing"),
    ).toThrow(/associated item not found/);
    const bad = unresolvedRustdocLinks(
      "[x](rustdoc:grit_lib::revwalk::history::RewritePlan::definitely_missing)",
    );
    expect(bad.length).toBe(1);
    expect(bad[0]).toMatch(/definitely_missing/);
  });
});

describe("unresolvedGritLibPaths", () => {
  it("accepts public paths in prose, code and use groups", () => {
    const markdown = [
      "Serve with `grit_lib::remote::serve::upload_pack` and [`Odb`](rustdoc:grit_lib::objects::odb::Odb).",
      "```rust",
      "use grit_lib::objects::{HashAlgo, ObjectKind as Kind, hash::hash_object};",
      "use grit_lib::config::ConfigHandle;",
      "```",
      "[docs](https://docs.rs/grit-lib/latest/grit_lib/remote/serve/struct.ServeOptions.html#method.default)",
    ].join("\n");
    expect(unresolvedGritLibPaths(markdown)).toEqual([]);
  });

  it("reports private and missing paths", () => {
    const markdown = [
      "Accepts `grit_lib::transfer::FetchOptions`.",
      "```rust",
      "use grit_lib::remote::serve::{ServeOptions, NoSuchThing};",
      "```",
      "[old](https://docs.rs/grit-lib/latest/grit_lib/serve/struct.ServeOptions.html)",
    ].join("\n");
    const bad = unresolvedGritLibPaths(markdown);
    expect(bad).toHaveLength(3);
    expect(bad[0]).toMatch(/^grit_lib::remote::serve::NoSuchThing: /);
    expect(bad[1]).toMatch(/^grit_lib::transfer::FetchOptions: /);
    expect(bad[2]).toMatch(/grit_lib\/serve\/struct\.ServeOptions\.html: no public rustdoc page/);
  });
});
