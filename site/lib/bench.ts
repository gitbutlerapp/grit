/**
 * Benchmark tables for the benchmarks page, built from the committed grit-bench
 * JSON baselines listed under `[benchmarks].baseline` in `content/docs/site.toml`.
 * Refreshing a baseline file updates the page on the next deploy.
 */
import fs from "node:fs";
import path from "node:path";
import { readRepoFile, REPO_ROOT } from "./repo";

export const BENCHMARK_MARKER = "<!-- grit:benchmark-tables -->";
export const DAILY_MATRIX_MARKER = "<!-- grit:daily-matrix -->";
const SCHEMA_VERSION = 1;
export const DAILY_BENCH_SCHEMA = "grit-daily-bench/1" as const;
const SLOW_RATIO = 2.0;
const DAILY_SLOW_RATIO = 2.0;
const DAILY_MEMORY_SLOW_RATIO = 1.5;

interface Scenario {
  id: string;
  group: string;
  fixture: string;
  gitMean: number | null;
  gritMean: number;
  gritStddev: number;
  ratio: number | null;
}

interface Bundle {
  timestamp: string;
  gitVersion: string;
  gritVersion: string;
  cpu: string;
  os: string;
  scenarios: Scenario[];
}

function fail(message: string): never {
  throw new Error(`benchmark baseline: ${message}`);
}

function ms(block: Record<string, unknown>, label: string, field: string): number {
  if (!(field in block)) fail(`${label} missing ${field}`);
  const value = Number(block[field]);
  if (Number.isNaN(value)) fail(`${label} invalid ${field}: ${String(block[field])}`);
  return value;
}

function timingMs(block: Record<string, unknown>, label: string): number {
  if ("mean_ms" in block) return ms(block, label, "mean_ms");
  if ("median_ms" in block) return ms(block, label, "median_ms");
  fail(`${label} missing mean_ms or median_ms`);
}

function timingStddev(block: Record<string, unknown>, label: string): number {
  if ("stddev_ms" in block) return ms(block, label, "stddev_ms");
  return 0;
}

function isCampaignBaseline(rel: string): boolean {
  return rel.endsWith("campaign-before.json") || rel.endsWith("campaign-after.json");
}

function isRevisionSelectionRefresh(rel: string): boolean {
  return rel.endsWith("revision-selection.json");
}

/** The hash suite re-runs other suites' scenarios (e.g. `fsck-medium`) for context; their own baselines win. */
function isHashSuiteBaseline(rel: string): boolean {
  return rel.endsWith("hash-before.json") || rel.endsWith("hash-after.json");
}

function isConflictsBaseline(rel: string): boolean {
  return rel.endsWith("conflicts.json");
}

function isHotPathsBaseline(rel: string): boolean {
  return rel.endsWith("hot-paths-after.json");
}

function isDashboardBaseline(rel: string): boolean {
  return rel.startsWith("grit-utils/baselines/dashboard/");
}

function isDashboardPrimary(rel: string): boolean {
  return rel.endsWith("dashboard/current.json");
}

function readBaselineJson(rel: string): Record<string, unknown> {
  const file = path.resolve(REPO_ROOT, rel);
  if (!fs.existsSync(file)) fail(`missing ${rel}`);
  return JSON.parse(fs.readFileSync(file, "utf8")) as Record<string, unknown>;
}

function isDailyBenchReport(report: Record<string, unknown>): boolean {
  return report.schema === DAILY_BENCH_SCHEMA;
}

/** True when `rel` points at a `grit-daily-bench/1` census report (not hyperfine v1). */
function isDailyBenchBaseline(rel: string): boolean {
  return isDailyBenchReport(readBaselineJson(rel));
}

export interface DailyToolRun {
  median_wall_ms: number;
  peak_rss_bytes: number;
  runs?: number;
}

export interface DailyScenarioResult {
  id: string;
  area: string;
  headline?: boolean;
  name?: string;
  description: string;
  git: DailyToolRun;
  grit: DailyToolRun;
  ratio: number;
  pending?: boolean;
}

export interface DailyBenchReport {
  schema: typeof DAILY_BENCH_SCHEMA;
  generated_at: string;
  machine: Record<string, unknown>;
  tools: Record<string, unknown>;
  scenarios: DailyScenarioResult[];
}

export interface MatrixScenarioRow {
  id: string;
  notes?: string;
}

export interface MatrixAreaSection {
  area: string;
  scenarios: MatrixScenarioRow[];
}

function loadBundle(baselines: string[]): Bundle {
  const scenarios: Scenario[] = [];
  const seen = new Map<string, { index: number; rel: string }>();
  let primary: Record<string, any> | undefined;
  for (const rel of baselines) {
    const report = readBaselineJson(rel);
    if (isDailyBenchReport(report)) continue;
    if (report.schema_version !== SCHEMA_VERSION) {
      fail(`${rel} has unsupported schema_version ${String(report.schema_version)}`);
    }
    for (const key of ["timestamp", "machine", "tools", "scenarios"]) {
      if (!(key in report)) fail(`${rel} missing ${key}`);
    }
    if (
      !primary ||
      rel.endsWith("campaign-after.json") ||
      rel.endsWith("dashboard/current.json")
    ) {
      primary = report;
    }
    (report.scenarios as Record<string, any>[]).forEach((raw, i) => {
      const label = `${rel} scenario[${i}] (${raw.id ?? "?"})`;
      for (const key of ["id", "group", "fixture", "grit"]) if (!(key in raw)) fail(`${label} missing ${key}`);
      const gritOnly = (raw.driver ?? "cli") === "criterion";
      if (!gritOnly && !raw.git) fail(`${label} missing git`);
      const gitMean = raw.git ? timingMs(raw.git, `${label} git`) : null;
      const gritMean = timingMs(raw.grit, `${label} grit`);
      const id = String(raw.id);
      const row: Scenario = {
        id,
        group: String(raw.group),
        fixture: String(raw.fixture),
        gitMean,
        gritMean,
        gritStddev: timingStddev(raw.grit, `${label} grit`),
        ratio: gitMean ? gritMean / gitMean : null,
      };
      if (seen.has(id)) {
        const prev = seen.get(id)!;
        if (isCampaignBaseline(rel) && isCampaignBaseline(prev.rel)) {
          scenarios[prev.index] = row;
          return;
        }
        if (isRevisionSelectionRefresh(rel)) {
          scenarios[prev.index] = row;
          seen.set(id, { index: prev.index, rel });
          return;
        }
        if (isRevisionSelectionRefresh(prev.rel)) {
          return;
        }
        if (isHashSuiteBaseline(rel) && isHashSuiteBaseline(prev.rel)) {
          if (rel.endsWith("hash-after.json") && prev.rel.endsWith("hash-before.json")) {
            scenarios[prev.index] = row;
            seen.set(id, { index: prev.index, rel });
          }
          return;
        }
        if (isHashSuiteBaseline(rel) && !isHashSuiteBaseline(prev.rel)) {
          return;
        }
        if (isHashSuiteBaseline(prev.rel) && !isHashSuiteBaseline(rel)) {
          scenarios[prev.index] = row;
          seen.set(id, { index: prev.index, rel });
          return;
        }
        if (isDashboardBaseline(rel) && isDashboardBaseline(prev.rel)) {
          scenarios[prev.index] = row;
          return;
        }
        if (isDashboardPrimary(rel)) {
          scenarios[prev.index] = row;
          seen.set(id, { index: prev.index, rel });
          return;
        }
        if (isHotPathsBaseline(rel) && isConflictsBaseline(prev.rel)) {
          scenarios[prev.index] = row;
          seen.set(id, { index: prev.index, rel });
          return;
        }
        if (isConflictsBaseline(rel) && isHotPathsBaseline(prev.rel)) {
          return;
        }
        fail(`duplicate scenario id ${id}`);
      }
      seen.set(id, { index: scenarios.length, rel });
      scenarios.push(row);
    });
  }
  if (!primary) fail("no baselines listed");
  scenarios.sort((a, b) => a.group.localeCompare(b.group) || a.fixture.localeCompare(b.fixture) || a.id.localeCompare(b.id));
  const { machine, tools } = primary;
  return {
    timestamp: String(primary.timestamp),
    gitVersion: String(tools.git ?? "?"),
    gritVersion: String(tools.grit ?? "?"),
    cpu: String(machine.cpu_model ?? "?"),
    os: `${machine.os ?? "?"} (${machine.kernel ?? "?"})`,
    scenarios,
  };
}

function formatMs(value: number): string {
  if (value >= 100) return Math.round(value).toLocaleString("en-US");
  if (value >= 10) return value.toFixed(1);
  return value.toFixed(2);
}

function formatRatio(ratio: number | null): string {
  if (ratio === null) return "—";
  const text = `${ratio.toFixed(2)}×`;
  // Bold marks scenarios where grit is more than twice as slow as git.
  return ratio > SLOW_RATIO ? `**${text}**` : text;
}

function median(values: number[]): number {
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

/** The benchmark tables as Markdown, for the page and its Markdown twin. */
export function benchmarkMarkdown(baselines: string[]): string {
  const bundle = loadBundle(baselines);
  const groups = new Map<string, Scenario[]>();
  for (const s of bundle.scenarios) groups.set(s.group, [...(groups.get(s.group) ?? []), s]);
  const names = [...groups.keys()].sort();
  const out = [
    "| | |",
    "| --- | --- |",
    `| Git | ${bundle.gitVersion} |`,
    `| Grit | ${bundle.gritVersion} |`,
    `| CPU | ${bundle.cpu} |`,
    `| OS | ${bundle.os} |`,
    `| Recorded | ${bundle.timestamp.replace("T", " ").replace(/Z$/, "")} |`,
    "",
    "## Summary by operation",
    "",
    "| Operation | Scenarios | Median Grit / Git | Worst Grit / Git |",
    "| --- | ---: | ---: | ---: |",
  ];
  for (const name of names) {
    const items = groups.get(name)!;
    const ratios = items.map((s) => s.ratio).filter((r): r is number => r !== null);
    const med = ratios.length ? formatRatio(median(ratios)) : "—";
    const worst = ratios.length ? formatRatio(Math.max(...ratios)) : "—";
    out.push(`| ${name} | ${items.length} | ${med} | ${worst} |`);
  }
  for (const name of names) {
    out.push(
      "",
      `### ${name}`,
      "",
      "| Scenario | Fixture | Git mean (ms) | Grit mean (ms) | Grit / Git | Spread |",
      "| --- | --- | ---: | ---: | ---: | --- |",
    );
    for (const s of groups.get(name)!) {
      out.push(
        `| \`${s.id}\` | ${s.fixture} | ${s.gitMean === null ? "—" : formatMs(s.gitMean)} | ${formatMs(s.gritMean)} | ${formatRatio(s.ratio)} | ±${formatMs(s.gritStddev)} ms |`,
      );
    }
  }
  return out.join("\n") + "\n";
}

/** Repo-root-relative path to the committed daily census report from `site.toml` baselines. */
export function findDailyBaseline(baselines: string[]): string {
  const daily = baselines.filter(isDailyBenchBaseline);
  if (!daily.length) fail("no grit-daily-bench baseline listed in site.toml");
  const preferred = daily.find((rel) => rel.endsWith("/daily/latest.json"));
  return preferred ?? daily[0]!;
}

function readDailyReport(rel: string): DailyBenchReport {
  const file = path.resolve(REPO_ROOT, rel);
  if (!fs.existsSync(file)) fail(`missing ${rel}`);
  const report = JSON.parse(fs.readFileSync(file, "utf8")) as DailyBenchReport;
  if (report.schema !== DAILY_BENCH_SCHEMA) {
    fail(`${rel} has unsupported schema ${String((report as { schema?: unknown }).schema)}`);
  }
  if (!Array.isArray(report.scenarios)) fail(`${rel} missing scenarios`);
  return report;
}

/** Load a `grit-daily-bench/1` report (typed). */
export function loadDailyReport(rel: string): DailyBenchReport {
  return readDailyReport(rel);
}

/** Parse functional areas and scenario ids from `grit-utils/MATRIX.md`. */
export function parseMatrixAreas(markdown: string): MatrixAreaSection[] {
  const sections: MatrixAreaSection[] = [];
  let current: MatrixAreaSection | undefined;
  for (const line of markdown.split("\n")) {
    const heading = /^## ([^#].*)$/.exec(line.trim());
    if (heading) {
      const title = heading[1]!.trim();
      if (title === "Exemptions" || title === "Gaps" || title.startsWith("How to read")) {
        current = undefined;
        continue;
      }
      current = { area: title, scenarios: [] };
      sections.push(current);
      continue;
    }
    if (!current) continue;
    const trimmed = line.trim();
    if (!trimmed.startsWith("|") || trimmed.startsWith("| ---")) continue;
    const cells = trimmed
      .slice(1)
      .split("|")
      .map((c) => c.trim());
    const idCell = cells[0] ?? "";
    const inner = idCell.startsWith("`") && idCell.endsWith("`") ? idCell.slice(1, -1) : "";
    if (inner.includes("/") && inner.includes("@")) {
      current.scenarios.push({ id: inner, notes: cells[1] });
    }
  }
  return sections;
}

function formatRss(bytes: number): string {
  if (bytes <= 0) return "—";
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  return `${Math.round(bytes / 1024)} KiB`;
}

function emphasizeRatio(ratio: number | null, slow: number): string {
  if (ratio === null || !Number.isFinite(ratio) || ratio <= 0) return "—";
  const text = `${ratio.toFixed(2)}×`;
  return ratio > slow ? `**${text}**` : text;
}

function isMeasuredDailyResult(result: DailyScenarioResult): boolean {
  if (result.pending) return false;
  return result.ratio > 0 && result.git.median_wall_ms > 0 && result.grit.median_wall_ms > 0;
}

function scenarioExemption(
  row: MatrixScenarioRow,
  result: DailyScenarioResult | undefined,
): string | undefined {
  if (result?.pending) return "Pending (not timed)";
  if (result && !isMeasuredDailyResult(result)) {
    return "Not in committed daily baseline";
  }
  if (!result) {
    if (row.notes?.includes("GAP:") || row.notes?.toLowerCase().includes("pending")) {
      return row.notes;
    }
    return "Not in committed daily baseline";
  }
  return undefined;
}

/** Fail when a MATRIX.md row lacks both measured ratios and an exemption note. */
export function assertDailyMatrixCoverage(areas: MatrixAreaSection[], report: DailyBenchReport): void {
  const byId = new Map(report.scenarios.map((s) => [s.id, s]));
  const problems: string[] = [];
  for (const section of areas) {
    for (const row of section.scenarios) {
      const result = byId.get(row.id);
      const exemption = scenarioExemption(row, result);
      if (exemption) continue;
      if (!result || !isMeasuredDailyResult(result)) problems.push(row.id);
    }
  }
  if (problems.length) {
    fail(`daily matrix rows without ratio or exemption: ${problems.join(", ")}`);
  }
}

/** Daily census table grouped by MATRIX.md functional areas. */
export function dailyMatrixMarkdown(dailyRel: string, matrixRel = "grit-utils/MATRIX.md"): string {
  const report = readDailyReport(dailyRel);
  const matrixMd = readRepoFile(matrixRel);
  const areas = parseMatrixAreas(matrixMd);
  assertDailyMatrixCoverage(areas, report);
  const byId = new Map(report.scenarios.map((s) => [s.id, s]));
  const gitVersion = String(report.tools.git ?? "?");
  const gritVersion = String(report.tools.grit ?? "?");
  const cpu = String(report.machine.cpu_model ?? "?");
  const os = `${String(report.machine.os ?? "?")} (${String(report.machine.kernel ?? "?")})`;
  const recorded = report.generated_at.replace("T", " ").replace(/Z$/, "");

  const out = [
    "| | |",
    "| --- | --- |",
    `| Git | ${gitVersion} |`,
    `| Grit | ${gritVersion} |`,
    `| CPU | ${cpu} |`,
    `| OS | ${os} |`,
    `| Recorded | ${recorded} |`,
    "",
    "## Summary by area",
    "",
    "| Area | Scenarios | Median Grit / Git (time) | Worst Grit / Git (time) | Median memory ratio |",
    "| --- | ---: | ---: | ---: | ---: |",
  ];

  for (const section of areas) {
    const timeRatios: number[] = [];
    const memRatios: number[] = [];
    for (const row of section.scenarios) {
      const result = byId.get(row.id);
      const exemption = scenarioExemption(row, result);
      if (exemption || !result || result.pending) continue;
      if (result.ratio > 0) timeRatios.push(result.ratio);
      const gitRss = result.git.peak_rss_bytes;
      const gritRss = result.grit.peak_rss_bytes;
      if (gitRss > 0 && gritRss > 0) memRatios.push(gritRss / gitRss);
    }
    const medTime = timeRatios.length ? emphasizeRatio(median(timeRatios), DAILY_SLOW_RATIO) : "—";
    const worstTime = timeRatios.length ? emphasizeRatio(Math.max(...timeRatios), DAILY_SLOW_RATIO) : "—";
    const medMem = memRatios.length ? emphasizeRatio(median(memRatios), DAILY_MEMORY_SLOW_RATIO) : "—";
    out.push(`| ${section.area} | ${section.scenarios.length} | ${medTime} | ${worstTime} | ${medMem} |`);
  }

  for (const section of areas) {
    out.push(
      "",
      `### ${section.area}`,
      "",
      "| Scenario | Git median (ms) | Grit median (ms) | Time | Git peak RSS | Grit peak RSS | Memory | Notes |",
      "| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
    );
    for (const row of section.scenarios) {
      const result = byId.get(row.id);
      const exemption = scenarioExemption(row, result);
      const headline = result?.headline === true;
      const idCell = headline ? `**${row.id}**` : `\`${row.id}\``;
      if (exemption) {
        out.push(`| ${idCell} | — | — | — | — | — | — | ${exemption} |`);
        continue;
      }
      const gitMs = result!.git.median_wall_ms;
      const gritMs = result!.grit.median_wall_ms;
      const memRatio =
        result!.git.peak_rss_bytes > 0 && result!.grit.peak_rss_bytes > 0
          ? result!.grit.peak_rss_bytes / result!.git.peak_rss_bytes
          : null;
      out.push(
        `| ${idCell} | ${formatMs(gitMs)} | ${formatMs(gritMs)} | ${emphasizeRatio(result!.ratio, DAILY_SLOW_RATIO)} | ${formatRss(result!.git.peak_rss_bytes)} | ${formatRss(result!.grit.peak_rss_bytes)} | ${emphasizeRatio(memRatio, DAILY_MEMORY_SLOW_RATIO)} | |`,
      );
    }
  }
  return out.join("\n") + "\n";
}
