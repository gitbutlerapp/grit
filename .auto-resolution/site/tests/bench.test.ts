import { describe, expect, it } from "vitest";
import { benchmarkMarkdown } from "../lib/bench";

describe("benchmark baselines", () => {
  it("prefers hash-after over hash-before for duplicate scenario ids", () => {
    const md = benchmarkMarkdown([
      "grit-utils/baselines/hash-before.json",
      "grit-utils/baselines/hash-after.json",
    ]);
    expect(md).toContain("hot-path-100k-repacked");
    expect(md).not.toContain("/tmp/grit-bench-odb-cache/git.git");
    const hashRow = md
      .split("\n")
      .find((line) => line.includes("`hash-object-256mib`"));
    expect(hashRow).toBeDefined();
    expect(hashRow).toMatch(/0\.6\d×/);
  });
});
