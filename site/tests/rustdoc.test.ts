import { beforeAll, describe, expect, it } from "vitest";
import {
  enumVariantRows,
  expandVariantTables,
  resolveRustdocPath,
  rustdocAvailable,
  unresolvedGritLibPaths,
  unresolvedRustdocLinks,
} from "../lib/rustdoc";

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
    expect(resolveRustdocPath("grit_lib::revwalk::history::RewritePlan::rewritten_pairs")).toBe(
      "revwalk/history/struct.RewritePlan.html#method.rewritten_pairs",
    );
  });

  it("follows re-exports of items that are public elsewhere", () => {
    expect(resolveRustdocPath("grit_lib::config::Config")).toBe("config/struct.Config.html");
    expect(resolveRustdocPath("grit_lib::porcelain::branch::DeleteMode::Safe")).toBe(
      "porcelain/branch/enum.DeleteMode.html#variant.Safe",
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

describe("variant tables", () => {
  it("lists every variant with its snake_case name and rustdoc", () => {
    const rows = enumVariantRows("grit_lib::repo::error::FilesOnlyOperation");
    const gc = rows.find((r) => r.variant === "Gc")!;
    expect(gc.name).toBe("gc");
    expect(gc.url).toMatch(/repo\/error\/enum\.FilesOnlyOperation\.html#variant\.Gc$/);
    expect(gc.doc).toContain("[`Odb::gc`](https://docs.rs/grit-lib/latest/grit_lib/objects/odb/struct.Odb.html#method.gc)");
    expect(rows.find((r) => r.variant === "CommitGraphWrite")!.name).toBe("commit_graph_write");
  });

  it("expands the marker into a Markdown table", () => {
    const table = expandVariantTables("<!-- grit:variant-table grit_lib::repo::error::FilesOnlyOperation -->");
    expect(table.split("\n")[0]).toBe("| Name | Variant | Description |");
    expect(table).toContain("| `partial_fetch` | [`PartialFetch`](");
  });

  it("rejects paths that are not enums", () => {
    expect(() => enumVariantRows("grit_lib::objects::odb::Odb")).toThrow(/not an enum/);
  });
});

describe("unresolvedGritLibPaths", () => {
  it("accepts public paths in prose, code and use groups", () => {
    const markdown = [
      "Serve with `grit_lib::remote::serve::upload_pack` and [`Odb`](rustdoc:grit_lib::objects::odb::Odb).",
      "```rust",
      "use grit_lib::objects::{HashAlgo, ObjectKind as Kind, hash::hash_object};",
      "use grit_lib::config::Config;",
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
