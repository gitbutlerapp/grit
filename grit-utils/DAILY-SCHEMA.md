# Daily benchmark report schema (`grit-daily-bench/1`)

Machine-readable output of `grit-bench daily`. The factory dashboard at [maint.grit-scm.com/cooking/bench](https://maint.grit-scm.com/cooking/bench) consumes reports with this tag.

Scenario ids are stable: `<command>/<case>@<repo>`. Peak RSS is the **median** peak over timed runs for each tool (bytes, from child process resource usage). Using the median keeps consecutive-run memory ratio checks stable on noisy remote scenarios.

## TypeScript shape (factory `daily-bench.ts`)

```typescript
export const DAILY_BENCH_SCHEMA = "grit-daily-bench/1" as const;

export interface DailyBenchReport {
  schema: typeof DAILY_BENCH_SCHEMA;
  generated_at: string; // RFC3339 UTC
  machine: MachineInfo;
  tools: DailyToolVersions;
  /** Present when capture used non-default fixture policy (for example low-memory smoke specials). */
  capture?: DailyCaptureInfo;
  scenarios: DailyScenarioResult[];
}

export interface DailyCaptureInfo {
  fixture_policy: string;
  /** Wall seconds per functional area when merged from per-scenario `--only` captures. */
  area_wall_seconds?: Record<string, number>;
  /** Wall seconds for the full merged capture run. */
  total_wall_seconds?: number;
}

export interface MachineInfo {
  cpu_model: string;
  physical_cores: number;
  logical_cores: number;
  ram_bytes: number;
  os: string;
  kernel: string;
  scratch_filesystem: string;
  rustc_version: string;
  cargo_profile: string;
}

export interface DailyToolVersions {
  git: string;
  grit: string;
  grit_commit?: string; // GRIT_BENCH_REPORT_GRIT_COMMIT, else the grit-bench source commit
  baseline_grit?: string;
  baseline_grit_commit?: string;
}

export interface DailyScenarioResult {
  id: string; // `<command>/<case>@<repo>`
  area: Area;
  headline?: boolean;
  name?: string;
  description: string;
  git: DailyToolRun;
  grit: DailyToolRun;
  ratio: number; // grit.median_wall_ms / git.median_wall_ms
  baseline_grit?: DailyToolRun;
  baseline_grit_ratio?: number;
  pending?: boolean;
}

export interface DailyToolRun {
  median_wall_ms: number;
  peak_rss_bytes: number;
  runs?: number;
}

export type Area =
  | "odb"
  | "packs"
  | "refs"
  | "index"
  | "worktree"
  | "diff"
  | "revwalk"
  | "blame"
  | "branching"
  | "remotes"
  | "maintenance"
  | "config"
  | "ignore"
  | "hooks"
  | "notes"
  | "bundles"
  | "worktrees";

export type RepoShapeId =
  | "small"
  | "synth-10k"
  | "synth-100k"
  | "git-git"
  | "many-refs"
  | "many-refs-loose"
  | "big-blobs"
  | "deep-tree";
```

## Headline scenarios (cooking page)

| id | name |
| --- | --- |
| `status/clean@synth-10k` | status-clean |
| `status/dirty@synth-10k` | status-dirty |
| `add/modified@synth-10k` | add |
| `commit/small@synth-10k` | commit |
| `diff/worktree@synth-10k` | diff |
| `log/oneline-100@git-git` | log |
| `log/json-100@git-git` | log |
| `show/head@git-git` | show |
| `blame/file@git-git` | blame |
| `switch/branch@synth-10k` | switch |
| `branch/list@many-refs` | branch |
| `merge/clean@synth-10k` | merge |
| `pick/one@synth-10k` | pick |
| `stash/push-pop@synth-10k` | stash |
| `fetch/incremental@git-git` | fetch |
| `pull/fast-forward@git-git` | pull |
| `pull/rebase@synth-10k` | pull |
| `push/incremental@git-git` | push |
| `push/force-with-lease@git-git` | push |
| `clone/local@synth-10k` | clone |

## Validation

```bash
grit-bench daily validate path/to/report.json
```

Checks: schema tag, parseable unique scenario ids, non-zero `peak_rss_bytes` on git and grit (and on `baseline_grit` when present).

Rust types live in `grit-utils/src/daily/`.
