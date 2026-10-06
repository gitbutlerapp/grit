#!/usr/bin/env bash
#
# bench/run-everyday.sh — Scale-parameterized git-vs-grit benchmarks for the
# commands people actually run every day / every week, across repo scales
# (number of files, number of commits, tree shape).
#
# Complements bench/run.sh (which covers plumbing at a single scale). Results are
# written as bench/results/<cmd>@<scale>.json so report.py groups a command's
# behaviour across scales.
#
# Usage:
#   bash bench/run-everyday.sh                 # full sweep (slow: generates big repos)
#   bash bench/run-everyday.sh --scales S,M    # only small+medium
#   bash bench/run-everyday.sh status add diff # only these commands, all scales
#
set -uo pipefail   # NOT -e: a failed state-mutation must not kill the whole sweep

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GRIT="$REPO_ROOT/target/release/grit-git"
GIT="$(command -v git)"
RESULTS_DIR="$REPO_ROOT/bench/results"
SCRATCH="${BENCH_SCRATCH:-/tmp/grit-bench-everyday}"
WARMUP="${BENCH_WARMUP:-2}"
MIN_RUNS="${BENCH_MIN_RUNS:-10}"

[[ -x "$GRIT" ]] || { echo "Building grit (release)..."; (cd "$REPO_ROOT" && cargo build --release -q -p grit-git); }
command -v hyperfine >/dev/null || { echo "ERROR: hyperfine not found (cargo install hyperfine)"; exit 1; }
mkdir -p "$RESULTS_DIR"
GIT="$GIT -c user.email=b@b -c user.name=bench -c init.defaultBranch=main -c commit.gpgsign=false"

# ── Scale profiles: NAME = FILES DIRS COMMITS ──────────────────────────────
# Files dimension stresses index/worktree/tree ops; commits dimension stresses
# history walks; the WIDE profile is a flat single dir, DEEP is nested.
declare -A SCALE_SPEC=(
  [S]="100 8 100"        # small everyday repo
  [M]="1000 20 500"      # medium
  [L]="10000 100 1000"   # large working tree
  [H]="500 10 4000"      # deep history (few files, many commits)
)
SCALE_ORDER=(S M L H)

gen_repo() {
  # gen_repo <dir> <files> <dirs> <commits>
  local dir="$1" files="$2" dirs="$3" commits="$4"
  [[ -d "$dir/.git" ]] && return 0   # reuse if already generated this run
  rm -rf "$dir"; mkdir -p "$dir"
  ( cd "$dir"
    $GIT init -q
    # Fast bulk file creation: distribute <files> across <dirs>.
    local per=$(( (files + dirs - 1) / dirs )) made=0
    for d in $(seq 1 "$dirs"); do
      mkdir -p "d$d"
      for f in $(seq 1 "$per"); do
        made=$((made+1)); [[ $made -gt $files ]] && break
        printf 'content %s/%s\nline two\nline three\n' "$d" "$f" > "d$d/f$f.txt"
      done
    done
    $GIT add -A >/dev/null; $GIT commit -qm "initial $files files"
    # History: each commit touches a handful of files (bounded work per commit).
    for c in $(seq 2 "$commits"); do
      for k in 1 2 3 4; do
        local dd=$(( (c * k) % dirs + 1 )) ff=$(( (c + k) % per + 1 ))
        [[ -f "d$dd/f$ff.txt" ]] && printf 'edit %s\n' "$c" >> "d$dd/f$ff.txt"
      done
      $GIT commit -qam "commit $c" >/dev/null 2>&1 || $GIT commit -q --allow-empty -m "commit $c" >/dev/null
    done
    # A divergent branch (10 commits back) for merge/rebase/cherry-pick benches.
    $GIT branch bench-alt "HEAD~$(( commits>15 ? 10 : 1 ))" 2>/dev/null || true
  )
}

bench() {
  # bench <result-name> [hyperfine-args...] then the two commands
  local name="$1"; shift
  echo "  ▶ $name"
  hyperfine --warmup "$WARMUP" --min-runs "$MIN_RUNS" --style basic \
    --export-json "$RESULTS_DIR/$name.json" "$@" || echo "    (skipped: $name)"
}

# ── Per-command benches (each takes the repo dir R) ────────────────────────
# Everyday: status add commit log diff checkout switch restore branch show grep blame
# Weekly:   merge rebase stash cherry-pick reset shortlog clone-local
reset_repo() {
  # Restore the repo to a pristine main checkout (cheap; runs before each command).
  local R="$1"
  $GIT -C "$R" checkout -q main 2>/dev/null || true
  $GIT -C "$R" reset -q --hard main 2>/dev/null || true
  $GIT -C "$R" clean -fdq 2>/dev/null || true
}

run_cmds_for_scale() {
  local scale="$1" R="$2"
  local Gc="$GIT -C $R" Rc="$GRIT -C $R"

  for cmd in "${CMDS[@]}"; do
    reset_repo "$R"
    case "$cmd" in
    status)
      printf 'dirty\n' >> "$R/d1/f1.txt"; : > "$R/untracked_$RANDOM.txt"
      bench "status@$scale" "$Gc status --porcelain=v2" "$Rc status --porcelain=v2"
      bench "status-long@$scale" "$Gc status" "$Rc status" ;;
    add)
      bench "add@$scale" --prepare "$Gc reset -q; $Gc checkout -q -- . 2>/dev/null; for i in \$(seq 1 200); do echo x >> $R/d1/f\$i.txt 2>/dev/null; done; true" \
        "$Gc add -A" "$Rc add -A" ;;
    commit)
      bench "commit@$scale" --prepare "echo c\$RANDOM >> $R/d1/f1.txt; $Gc add d1/f1.txt" -i \
        "$Gc commit -q -m b" "$Rc commit -q -m b" ;;
    log)
      bench "log-oneline@$scale" "$Gc log --oneline" "$Rc log --oneline"
      bench "log-patch@$scale" "$Gc log -p -n 200" "$Rc log -p -n 200"
      bench "log-stat@$scale" "$Gc log --stat -n 200" "$Rc log --stat -n 200" ;;
    diff)
      for d in 1 2 3; do for f in 1 5 10; do echo mod >> "$R/d$d/f$f.txt" 2>/dev/null; done; done
      bench "diff@$scale" "$Gc diff" "$Rc diff"
      $Gc add -A 2>/dev/null
      bench "diff-staged@$scale" "$Gc diff --staged" "$Rc diff --staged"
      $Gc reset -q 2>/dev/null; $Gc checkout -q -- . 2>/dev/null ;;
    checkout)
      bench "checkout@$scale" --prepare "$Gc checkout -q main 2>/dev/null; true" -i \
        "$Gc checkout -q bench-alt" "$Rc checkout -q bench-alt"
      $Gc checkout -q main 2>/dev/null ;;
    restore)
      bench "restore@$scale" --prepare "echo z >> $R/d1/f1.txt" \
        "$Gc restore d1/f1.txt" "$Rc restore d1/f1.txt" ;;
    branch)
      bench "branch-list@$scale" "$Gc branch --list" "$Rc branch --list" ;;
    show)
      bench "show@$scale" "$Gc show HEAD" "$Rc show HEAD" ;;
    grep)
      bench "grep@$scale" "$Gc grep -n 'line two'" "$Rc grep -n 'line two'" ;;
    blame)
      bench "blame@$scale" "$Gc blame d1/f1.txt" "$Rc blame d1/f1.txt" ;;
    shortlog)
      bench "shortlog@$scale" "$Gc shortlog -ns" "$Rc shortlog -ns" ;;
    merge)
      bench "merge@$scale" --prepare "$Gc checkout -q main 2>/dev/null; $Gc reset -q --hard main 2>/dev/null; $Gc branch -f bench-merge HEAD~10 2>/dev/null; true" -i \
        "$Gc merge -q --no-edit bench-alt 2>/dev/null || true" "$Rc merge -q --no-edit bench-alt 2>/dev/null || true" ;;
    rebase)
      bench "rebase@$scale" --prepare "$Gc checkout -q -B bench-rb bench-alt 2>/dev/null; true" -i \
        "$Gc rebase -q main 2>/dev/null || $Gc rebase --abort 2>/dev/null; true" "$Rc rebase -q main 2>/dev/null || $Rc rebase --abort 2>/dev/null; true"
      $Gc checkout -q main 2>/dev/null ;;
    cherry-pick)
      bench "cherry-pick@$scale" --prepare "$Gc checkout -q -B bench-cp main 2>/dev/null; true" -i \
        "$Gc cherry-pick -n bench-alt 2>/dev/null; $Gc reset -q --hard 2>/dev/null; true" \
        "$Rc cherry-pick -n bench-alt 2>/dev/null; $Gc reset -q --hard 2>/dev/null; true" ;;
    reset)
      bench "reset@$scale" --prepare "$Gc reset -q --mixed HEAD 2>/dev/null; true" -i \
        "$Gc reset -q --mixed HEAD~1" "$Rc reset -q --mixed HEAD~1" ;;
    stash)
      bench "stash@$scale" --prepare "echo s\$RANDOM >> $R/d1/f1.txt" -i \
        "$Gc stash -q && $Gc stash pop -q" "$Rc stash -q && $Rc stash pop -q" ;;
    ls-files)
      bench "ls-files@$scale" "$Gc ls-files" "$Rc ls-files" ;;
    write-tree)
      bench "write-tree@$scale" "$Gc write-tree" "$Rc write-tree" ;;
    clone-local)
      bench "clone-local@$scale" --prepare "rm -rf $SCRATCH/clone-dst" \
        "$GIT clone -q $R $SCRATCH/clone-dst" "$GRIT clone -q $R $SCRATCH/clone-dst" ;;
  esac; done
}

# ── Driver ─────────────────────────────────────────────────────────────────
SCALES=("${SCALE_ORDER[@]}")
CMDS=(status add commit log diff checkout restore branch show grep blame shortlog
      merge rebase cherry-pick reset stash ls-files write-tree clone-local)
while [[ $# -gt 0 ]]; do
  case "$1" in
    --scales) IFS=',' read -ra SCALES <<< "$2"; shift 2 ;;
    --*) shift ;;
    *) CMDS=("$@"); break ;;
  esac
done

echo "grit: $GRIT"; echo "git:  $($GIT version)"; echo "scales: ${SCALES[*]}"; echo "commands: ${CMDS[*]}"; echo
mkdir -p "$SCRATCH"
for scale in "${SCALES[@]}"; do
  read -r F D C <<< "${SCALE_SPEC[$scale]}"
  echo "═══ scale $scale: $F files / $D dirs / $C commits ═══"
  R="$SCRATCH/repo-$scale"
  echo "  generating repo (one-time)..."; gen_repo "$R" "$F" "$D" "$C"
  run_cmds_for_scale "$scale" "$R"
  echo
done

echo "Generating report..."; python3 "$REPO_ROOT/bench/report.py" || true
echo "Done. Results in bench/results/<cmd>@<scale>.json ; report at docs/bench.html"
