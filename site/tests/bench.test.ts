import fs from "node:fs";
import path from "node:path";
import { parse as parseToml } from "smol-toml";
import { describe, expect, it } from "vitest";
import {
  assertDailyMatrixCoverage,
  benchmarkMarkdown,
  dailyMatrixMarkdown,
  findDailyBaseline,
  loadDailyReport,
  parseMatrixAreas,
} from "../lib/bench";
import { readRepoFile, REPO_ROOT } from "../lib/repo";

function siteTomlBaselines(): string[] {
  const raw = parseToml(fs.readFileSync(path.join(REPO_ROOT, "content/docs/site.toml"), "utf8")) as {
    benchmarks?: { baseline?: string[] };
  };
  const baselines = raw.benchmarks?.baseline;
  if (!baselines?.length) throw new Error("site.toml [benchmarks].baseline is empty");
  return baselines;
}

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

  it("ignores grit-daily-bench baselines when building hyperfine tables", () => {
    const md = benchmarkMarkdown([
      "grit-utils/baselines/hot-paths-after.json",
      "grit-utils/baselines/daily/latest.json",
    ]);
    expect(md).toContain("merge-10000");
    expect(md).not.toContain("status/clean@synth-10k");
  });

  it("builds hyperfine tables from every site.toml baseline", () => {
    const baselines = siteTomlBaselines();
    const md = benchmarkMarkdown(baselines);
    expect(md).toContain("## Summary by operation");
    expect(md).not.toContain("status/clean@synth-10k");
    expect(findDailyBaseline(baselines)).toBe("grit-utils/baselines/daily/latest.json");
  });
});

describe("daily matrix", () => {
  it("loads grit-daily-bench/1 reports", () => {
    const report = loadDailyReport("site/tests/fixtures/daily-matrix-report.json");
    expect(report.schema).toBe("grit-daily-bench/1");
    expect(report.scenarios).toHaveLength(2);
  });

  it("renders headline, pending, and baseline exemptions", () => {
    const matrixMd = readRepoFile("site/tests/fixtures/daily-matrix-mini.md");
    const areas = parseMatrixAreas(matrixMd);
    const report = loadDailyReport("site/tests/fixtures/daily-matrix-report.json");
    assertDailyMatrixCoverage(areas, report);
    const md = dailyMatrixMarkdown(
      "site/tests/fixtures/daily-matrix-report.json",
      "site/tests/fixtures/daily-matrix-mini.md",
    );
    expect(md).toContain("**status/clean@synth-10k**");
    expect(md).toContain("2.00×");
    expect(md).toContain("Pending (not timed)");
    expect(md).toContain("Not in committed daily baseline");
  });

  it("covers every MATRIX.md scenario against the committed daily baseline", () => {
    const matrixMd = readRepoFile("grit-utils/MATRIX.md");
    const areas = parseMatrixAreas(matrixMd);
    const report = loadDailyReport("grit-utils/baselines/daily/latest.json");
    assertDailyMatrixCoverage(areas, report);
    const md = dailyMatrixMarkdown("grit-utils/baselines/daily/latest.json");
    expect(md).toContain("### worktree");
    expect(md).toContain("Summary by area");
  });

  it("finds the daily baseline path from site.toml entries", () => {
    expect(findDailyBaseline(["grit-utils/baselines/daily/latest.json", "grit-utils/baselines/hash-after.json"])).toBe(
      "grit-utils/baselines/daily/latest.json",
    );
  });
});
