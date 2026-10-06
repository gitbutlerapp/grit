#!/usr/bin/env bash
#
# bench/run-ccp.sh — Clone / commit / push scenario benchmarks on fixed fixtures.
#
# Twelve timed scenarios (median of N runs via hyperfine), derived from
# scripts/bench-ccp.sh (commit 5e851125e). Fixtures are built with system git
# and core.multiPackIndex=false so C git 2.43 can read packs (grit may write
# MIDX v2 that older git cannot use as a reference).
#
# Usage:
#   bash bench/run-ccp.sh fixtures              # one-time fixture build under /tmp
#   bash bench/run-ccp.sh run                   # all scenarios → bench/results/ccp-*.json
#   bash bench/run-ccp.sh run clone-many        # one scenario
#   bash bench/run-ccp.sh run --runs 3          # fewer iterations (faster)
#
# Environment:
#   BENCH_ROOT          scratch root (default /tmp/grit-bench-ccp)
#   BENCH_RUNS          hyperfine exact runs per command (default 5)
#   BENCH_WARMUP        hyperfine warmup (default 1)
#   BENCH_TOOLS         comma list: git,grit-git,grit (default all three)
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RESULTS_DIR="$REPO_ROOT/bench/results"
BENCH_ROOT="${BENCH_ROOT:-/tmp/grit-bench-ccp}"
FIX="$BENCH_ROOT/fixtures"
WORK="$BENCH_ROOT/work"
SETUP_HOME="$BENCH_ROOT/home"

GIT_BIN="$(command -v git)"
GRIT_GIT="$REPO_ROOT/target/release/grit-git"
GRIT_CLI="$REPO_ROOT/target/release/grit"
WARMUP="${BENCH_WARMUP:-1}"
RUNS="${BENCH_RUNS:-5}"
TOOLS="${BENCH_TOOLS:-git,grit-git,grit}"
# Scenarios written in the current `run` invocation (for summary aggregation).
declare -a CCP_RAN_SCENARIOS=()

export GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=bench GIT_AUTHOR_EMAIL=bench@example.com
export GIT_COMMITTER_NAME=bench GIT_COMMITTER_EMAIL=bench@example.com
export HOME="$SETUP_HOME"

# System git for fixture construction and untimed prep.
SETUP_GIT=(
  "$GIT_BIN"
  -c user.name=bench
  -c user.email=bench@example.com
  -c init.defaultBranch=main
  -c commit.gpgsign=false
  -c core.multiPackIndex=false
  -c gc.auto=0
)

SCENARIOS=(
  clone-many
  clone-many-loose
  clone-hist
  clone-large
  clone-file-many
  clone-bare-many
  commit-touch-many
  add-10k
  commit-10k
  push-hist
  push-incr
  push-large
)

ensure_bins() {
  local need_grit=0
  [[ "$TOOLS" == *grit-git* ]] && need_grit=1
  [[ "$TOOLS" == *grit* ]] && need_grit=1
  if (( need_grit )); then
    if [[ ! -x "$GRIT_GIT" ]] || [[ ! -x "$GRIT_CLI" ]]; then
      echo "Building grit-git and grit-cli (release)..."
      (cd "$REPO_ROOT" && cargo build --release -q -p grit-git -p grit-cli)
    fi
  fi
  command -v hyperfine >/dev/null 2>&1 || {
    echo "ERROR: hyperfine not found (cargo install hyperfine)" >&2
    exit 1
  }
}

tool_cmd() {
  local tool=$1
  shift
  case "$tool" in
    git)       "${SETUP_GIT[@]}" "$@" ;;
    grit-git)  "$GRIT_GIT" -c commit.gpgsign=false "$@" ;;
    grit)      "$GRIT_CLI" "$@" ;;
    *)         echo "unknown tool $tool" >&2; return 1 ;;
  esac
}

build_fixtures() {
  rm -rf "$FIX" "$SETUP_HOME"
  mkdir -p "$FIX" "$SETUP_HOME"

  echo "Building fixtures with system git (core.multiPackIndex=false)..."

  local d=$FIX/many
  "${SETUP_GIT[@]}" init -q "$d"
  (
    cd "$d"
    for batch in 0 1 2 3 4; do
      for dir in $(seq 0 19); do
        mkdir -p "d$((batch * 20 + dir))"
        for f in $(seq 0 99); do
          printf 'content %s %s %s\n' "$batch" "$dir" "$f" > "d$((batch * 20 + dir))/f$f.txt"
        done
      done
      "${SETUP_GIT[@]}" add . && "${SETUP_GIT[@]}" commit -q -m "batch $batch"
    done
    "${SETUP_GIT[@]}" repack -ad
  )

  d=$FIX/many-loose
  "${SETUP_GIT[@]}" init -q "$d"
  (
    cd "$d"
    for dir in $(seq 0 99); do
      mkdir -p "d$dir"
      for f in $(seq 0 99); do
        printf 'loose %s %s\n' "$dir" "$f" > "d$dir/f$f.txt"
      done
    done
    "${SETUP_GIT[@]}" add . && "${SETUP_GIT[@]}" commit -q -m loose
  )
  verify_many_loose_fixture

  d=$FIX/hist
  "${SETUP_GIT[@]}" init -q "$d"
  (
    cd "$d"
    for i in $(seq 1 2000); do
      echo "line $i" >> "file$((i % 10)).txt"
      "${SETUP_GIT[@]}" add "file$((i % 10)).txt"
      "${SETUP_GIT[@]}" commit -q -m "c$i"
    done
    "${SETUP_GIT[@]}" repack -ad
  )

  d=$FIX/large
  "${SETUP_GIT[@]}" init -q "$d"
  (
    cd "$d"
    for i in 1 2 3; do
      head -c 30000000 /dev/urandom | base64 > "big$i.bin"
    done
    "${SETUP_GIT[@]}" add . && "${SETUP_GIT[@]}" commit -q -m large
    "${SETUP_GIT[@]}" repack -ad
  )

  d=$FIX/plain-10k
  mkdir -p "$d"
  (
    cd "$d"
    for dir in $(seq 0 99); do
      mkdir -p "d$dir"
      for f in $(seq 0 99); do
        printf 'plain %s %s\n' "$dir" "$f" > "d$dir/f$f.txt"
      done
    done
  )

  echo "Fixtures ready under $FIX"
}

verify_many_loose_fixture() {
  local in_pack loose
  in_pack=$("${SETUP_GIT[@]}" -C "$FIX/many-loose" count-objects -v | awk '/^in-pack:/ {print $2}')
  loose=$("${SETUP_GIT[@]}" -C "$FIX/many-loose" count-objects -v | awk '/^count:/ {print $2}')
  if [[ "${in_pack:-x}" != "0" ]]; then
    echo "ERROR: many-loose fixture is packed (in-pack=$in_pack); need gc.auto=0 during build" >&2
    exit 1
  fi
  if [[ "${loose:-0}" -lt 10000 ]]; then
    echo "ERROR: many-loose fixture has too few loose objects (count=$loose)" >&2
    exit 1
  fi
  echo "many-loose verified: count=$loose in-pack=$in_pack"
}

do_prep() {
  local scenario=${CCP_SCENARIO:?}
  rm -rf "$WORK"
  mkdir -p "$WORK" "$SETUP_HOME"
  cd "$WORK"
  case "$scenario" in
    clone-many|clone-hist|clone-large|clone-file-many|clone-bare-many|clone-many-loose) ;;
    commit-touch-many)
      "${SETUP_GIT[@]}" clone -q "$FIX/many" w
      cd w && echo tweak >> d0/f0.txt ;;
    add-10k)
      "${SETUP_GIT[@]}" init -q w
      cp -r "$FIX/plain-10k/." w/
      cd w ;;
    commit-10k)
      "${SETUP_GIT[@]}" init -q w
      cp -r "$FIX/plain-10k/." w/
      cd w && "${SETUP_GIT[@]}" add -A ;;
    push-hist)
      "${SETUP_GIT[@]}" clone -q "$FIX/hist" w
      "${SETUP_GIT[@]}" init -q --bare r.git
      (
        cd w
        "${SETUP_GIT[@]}" remote remove origin 2>/dev/null || true
        "${SETUP_GIT[@]}" remote add origin ../r.git
      ) ;;
    push-incr)
      "${SETUP_GIT[@]}" clone -q "$FIX/hist" w
      "${SETUP_GIT[@]}" clone -q --bare "$FIX/hist" r.git
      (
        cd w
        "${SETUP_GIT[@]}" remote remove origin 2>/dev/null || true
        "${SETUP_GIT[@]}" remote add origin ../r.git
        echo extra >> file0.txt && "${SETUP_GIT[@]}" commit -q -am extra
      ) ;;
    push-large)
      "${SETUP_GIT[@]}" clone -q "$FIX/large" w
      "${SETUP_GIT[@]}" init -q --bare r.git
      (
        cd w
        "${SETUP_GIT[@]}" remote remove origin 2>/dev/null || true
        "${SETUP_GIT[@]}" remote add origin ../r.git
      ) ;;
    *)
      echo "unknown scenario $scenario" >&2
      return 1 ;;
  esac
}

quiet() {
  # grit-cli has no -q; silence output for timing instead.
  "$@" >/dev/null 2>&1
}

clone_to() {
  local tool=$1 src=$2 dest=$3
  case "$tool" in
    grit) quiet tool_cmd "$tool" clone "$src" "$dest" ;;
    *) quiet tool_cmd "$tool" clone -q "$src" "$dest" ;;
  esac
}

commit_msg() {
  local tool=$1 msg=$2
  case "$tool" in
    grit) quiet tool_cmd "$tool" commit -a -m "$msg" ;;
    *) quiet tool_cmd "$tool" commit -q -am "$msg" ;;
  esac
}

push_origin_main() {
  local tool=$1
  case "$tool" in
    grit) quiet tool_cmd "$tool" push ;;
    *) quiet tool_cmd "$tool" push -q origin main ;;
  esac
}

do_timed() {
  local scenario=${CCP_SCENARIO:?} tool=${CCP_TOOL:?}
  cd "$WORK"
  case "$scenario" in
    clone-many)       clone_to "$tool" "$FIX/many" c ;;
    clone-many-loose) clone_to "$tool" "$FIX/many-loose" c ;;
    clone-hist)       clone_to "$tool" "$FIX/hist" c ;;
    clone-large)      clone_to "$tool" "$FIX/large" c ;;
    clone-file-many)  clone_to "$tool" "file://$FIX/many" c ;;
    clone-bare-many)
      case "$tool" in
        grit) quiet tool_cmd "$tool" clone --bare "$FIX/many" c.git ;;
        *) quiet tool_cmd "$tool" clone -q --bare "$FIX/many" c.git ;;
      esac ;;
    commit-touch-many) cd w && commit_msg "$tool" tweak ;;
    add-10k)
      cd w
      case "$tool" in
        grit) quiet tool_cmd "$tool" add ;;
        *) quiet tool_cmd "$tool" add -A ;;
      esac ;;
    commit-10k)
      cd w
      case "$tool" in
        grit) quiet tool_cmd "$tool" commit -m init ;;
        *) quiet tool_cmd "$tool" commit -q -m init ;;
      esac ;;
    push-hist|push-incr|push-large)
      cd w && push_origin_main "$tool" ;;
  esac
}

scenario_supported() {
  local scenario=$1 tool=$2
  case "$tool" in
    grit)
      # grit-cli has no bare clone yet.
      case "$scenario" in
        clone-bare-many) return 1 ;;
      esac
      ;;
  esac
  return 0
}

run_scenario() {
  local scenario=$1
  local out="$RESULTS_DIR/ccp-$scenario.json"
  local -a hf_args=()
  local tool prep timed

  IFS=',' read -ra tool_list <<< "$TOOLS"
  for tool in "${tool_list[@]}"; do
    scenario_supported "$scenario" "$tool" || continue
    timed="env CCP_SCENARIO=$scenario CCP_TOOL=$tool BENCH_ROOT=$BENCH_ROOT bash $REPO_ROOT/bench/run-ccp.sh timed"
    hf_args+=(--command-name "$tool" "$timed")
  done

  if ((${#hf_args[@]} == 0)); then
    echo "  (skip $scenario: no tools)"
    return 0
  fi

  prep="env CCP_SCENARIO=$scenario BENCH_ROOT=$BENCH_ROOT bash $REPO_ROOT/bench/run-ccp.sh prep"

  echo "  ▶ ccp-$scenario"
  hyperfine --warmup "$WARMUP" --runs "$RUNS" --style basic \
    --export-json "$out" --prepare "$prep" \
    "${hf_args[@]}"
  CCP_RAN_SCENARIOS+=("$scenario")
}

write_summary() {
  local summary="$RESULTS_DIR/ccp-baseline.json"
  local ran_csv
  ran_csv=$(IFS=,; echo "${CCP_RAN_SCENARIOS[*]}")
  python3 - <<'PY' "$RESULTS_DIR" "$summary" "$TOOLS" "$RUNS" "$GIT_BIN" "$GRIT_GIT" "$GRIT_CLI" "$ran_csv"
import json, os, subprocess, sys
from datetime import datetime, timezone

results_dir, out_path, tools, runs, git_bin, grit_git, grit_cli, ran_csv = sys.argv[1:9]
scenario_names = [s for s in ran_csv.split(",") if s]
scenarios = {}
for name in scenario_names:
    base = f"ccp-{name}.json"
    path = os.path.join(results_dir, base)
    if not os.path.isfile(path):
        continue
    try:
        with open(path) as f:
            data = json.load(f)
    except json.JSONDecodeError:
        continue
    scenarios[name] = {
        "file": base,
        "results": [
            {
                "command": r.get("command", ""),
                "median_sec": r.get("median"),
                "mean_sec": r.get("mean"),
            }
            for r in data.get("results", [])
        ],
    }

doc = {
    "kind": "grit-bench-ccp-baseline",
    "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "runs": int(runs),
    "tools": tools.split(","),
    "git_version": subprocess.check_output([git_bin, "version"], text=True).strip(),
    "binaries": {"git": git_bin, "grit-git": grit_git, "grit-cli": grit_cli},
    "scenarios": scenarios,
}
with open(out_path, "w") as f:
    json.dump(doc, f, indent=2)
    f.write("\n")
print(f"Wrote {out_path}")
PY
}

run_all() {
  local -a only=()
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --runs) RUNS="$2"; shift 2 ;;
      --tools) TOOLS="$2"; shift 2 ;;
      --warmup) WARMUP="$2"; shift 2 ;;
      --*) echo "unknown option $1" >&2; exit 1 ;;
      *) only+=("$1"); shift ;;
    esac
  done

  ensure_bins
  [[ -d "$FIX/many/.git" ]] || {
    echo "Fixtures missing; run: bash bench/run-ccp.sh fixtures" >&2
    exit 1
  }
  mkdir -p "$RESULTS_DIR" "$WORK" "$SETUP_HOME"

  echo "bench-ccp: fixtures=$FIX results=$RESULTS_DIR runs=$RUNS tools=$TOOLS"
  echo "git: $("$GIT_BIN" version)"
  [[ -x "$GRIT_GIT" ]] && echo "grit-git: $GRIT_GIT"
  [[ -x "$GRIT_CLI" ]] && echo "grit-cli: $GRIT_CLI"
  echo

  local -a list=("${SCENARIOS[@]}")
  if ((${#only[@]})); then list=("${only[@]}"); fi

  for s in "${list[@]}"; do
    run_scenario "$s"
  done

  write_summary
  echo "Done. Per-scenario hyperfine JSON: bench/results/ccp-<scenario>.json"
}

case ${1:-run} in
  fixtures) build_fixtures ;;
  prep) do_prep ;;
  timed) do_timed ;;
  run) shift; run_all "$@" ;;
  *)
    echo "Usage: $0 fixtures | prep | timed | run [--runs N] [--tools list] [scenario...]" >&2
    exit 1
    ;;
esac
