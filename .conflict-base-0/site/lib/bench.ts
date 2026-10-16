/**
 * Benchmark tables for the benchmarks page, built from the committed grit-bench
 * JSON baselines listed under `[benchmarks].baseline` in `content/docs/site.toml`.
 * Refreshing a baseline file updates the page on the next deploy.
 */
import fs from "node:fs";
import path from "node:path";
import { REPO_ROOT } from "./repo";

export const BENCHMARK_MARKER = "<!-- grit:benchmark-tables -->";
const SCHEMA_VERSION = 1;
const SLOW_RATIO = 2.0;

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

function loadBundle(baselines: string[]): Bundle {
  const scenarios: Scenario[] = [];
  const seen = new Set<string>();
  let primary: Record<string, any> | undefined;
  for (const rel of baselines) {
    const file = path.resolve(REPO_ROOT, rel);
    if (!fs.existsSync(file)) fail(`missing ${rel}`);
    const report = JSON.parse(fs.readFileSync(file, "utf8"));
    if (report.schema_version !== SCHEMA_VERSION) fail(`${rel} has unsupported schema_version ${report.schema_version}`);
    for (const key of ["timestamp", "machine", "tools", "scenarios"]) {
      if (!(key in report)) fail(`${rel} missing ${key}`);
    }
    primary ??= report;
    (report.scenarios as Record<string, any>[]).forEach((raw, i) => {
      const label = `${rel} scenario[${i}] (${raw.id ?? "?"})`;
      for (const key of ["id", "group", "fixture", "grit"]) if (!(key in raw)) fail(`${label} missing ${key}`);
      const gritOnly = (raw.driver ?? "cli") === "criterion";
      if (!gritOnly && !raw.git) fail(`${label} missing git`);
      const gitMean = raw.git ? ms(raw.git, `${label} git`, "mean_ms") : null;
      const gritMean = ms(raw.grit, `${label} grit`, "mean_ms");
      const id = String(raw.id);
      if (seen.has(id)) fail(`duplicate scenario id ${id}`);
      seen.add(id);
      scenarios.push({
        id,
        group: String(raw.group),
        fixture: String(raw.fixture),
        gitMean,
        gritMean,
        gritStddev: ms(raw.grit, `${label} grit`, "stddev_ms"),
        ratio: gitMean ? gritMean / gitMean : null,
      });
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
