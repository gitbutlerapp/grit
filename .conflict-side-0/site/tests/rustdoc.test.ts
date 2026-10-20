import { beforeAll, describe, expect, it } from "vitest";
import { resolveRustdocPath, rustdocAvailable, unresolvedRustdocLinks } from "../lib/rustdoc";

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
