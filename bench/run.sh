#!/usr/bin/env bash
#
# bench/run.sh — Benchmark grit vs C git on identical operations.
#
# Usage:
#   bash bench/run.sh                  # run all benchmarks
#   bash bench/run.sh init cat-file    # run specific benchmarks
#
# Output:
#   bench/results.json   — raw hyperfine JSON
#   docs/bench.html      — rendered report
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GRIT="$REPO_ROOT/target/release/grit-git"
GIT="$(command -v git)"
BENCH_DIR="$REPO_ROOT/bench"
RESULTS_DIR="$BENCH_DIR/results"
SCRATCH="/tmp/grit-bench-scratch"

# ── Preflight ──────────────────────────────────────────────────────

if [[ ! -x "$GRIT" ]]; then
  echo "Building grit (release)..."
  (cd "$REPO_ROOT" && cargo build --release --quiet -p grit-git)
fi

command -v hyperfine >/dev/null 2>&1 || {
  echo "ERROR: hyperfine not found. Install with: cargo install hyperfine" >&2
  exit 1
}

mkdir -p "$RESULTS_DIR"

echo "grit: $GRIT"
echo "git:  $GIT ($($GIT version))"
echo ""

# ── Helpers ────────────────────────────────────────────────────────

WARMUP=2
MIN_RUNS=10

run_bench() {
  local name="$1"
  shift
  # remaining args: --prepare "..." "cmd1" "cmd2" ...
  echo "▶ $name"
  hyperfine \
    --warmup "$WARMUP" \
    --min-runs "$MIN_RUNS" \
    --export-json "$RESULTS_DIR/$name.json" \
    --style basic \
    "$@"
  echo ""
}

cleanup_scratch() {
  rm -rf "$SCRATCH"
}
trap cleanup_scratch EXIT

make_scratch_repo() {
  cleanup_scratch
  mkdir -p "$SCRATCH"
  (cd "$SCRATCH" && $GIT init -q)
}

make_large_scratch_repo() {
  # Creates a repo with ~1000 files across 10 dirs, 50 commits
  cleanup_scratch
  mkdir -p "$SCRATCH"
  (
    cd "$SCRATCH"
    $GIT init -q
    for d in $(seq 1 10); do
      mkdir -p "dir$d"
      for f in $(seq 1 100); do
        echo "file content $d/$f line 1" > "dir$d/file$f.txt"
        echo "file content $d/$f line 2" >> "dir$d/file$f.txt"
        echo "file content $d/$f line 3" >> "dir$d/file$f.txt"
      done
    done
    $GIT add -A && $GIT commit -q -m "initial: 1000 files"
    for c in $(seq 2 50); do
      # Modify ~20 random files per commit
      for f in $(shuf -i 1-100 -n 20); do
        d=$(( (RANDOM % 10) + 1 ))
        echo "change $c" >> "dir$d/file$f.txt"
      done
      $GIT add -A && $GIT commit -q -m "commit $c"
    done
  )
}

# ── Benchmark definitions ─────────────────────────────────────────

bench_init() {
  local tmp="$SCRATCH/init-target"
  run_bench "init" \
    --prepare "rm -rf $tmp" \
    "$GIT init -q $tmp" \
    "$GRIT init -q $tmp"
}

bench_hash_object() {
  make_scratch_repo
  # Create a 1MB file to hash
  dd if=/dev/urandom of="$SCRATCH/blob.bin" bs=1024 count=1024 2>/dev/null
  run_bench "hash-object" \
    "$GIT -C $SCRATCH hash-object $SCRATCH/blob.bin" \
    "$GRIT -C $SCRATCH hash-object $SCRATCH/blob.bin"
}

bench_cat_file() {
  make_scratch_repo
  echo "hello world" > "$SCRATCH/hello.txt"
  (cd "$SCRATCH" && $GIT add hello.txt && $GIT commit -q -m "add hello")
  local blob_sha
  blob_sha=$(cd "$SCRATCH" && $GIT rev-parse HEAD:hello.txt)
  run_bench "cat-file" \
    "$GIT -C $SCRATCH cat-file -p $blob_sha" \
    "$GRIT -C $SCRATCH cat-file -p $blob_sha"
}

bench_cat_file_batch() {
  make_scratch_repo
  # Create 100 blobs
  for i in $(seq 1 100); do
    echo "blob content $i" > "$SCRATCH/file$i.txt"
  done
  (cd "$SCRATCH" && $GIT add . && $GIT commit -q -m "100 files")
  # Build a list of all blob SHAs
  (cd "$SCRATCH" && $GIT rev-list --objects --all | head -200 | awk '{print $1}') > "$SCRATCH/shas.txt"
  run_bench "cat-file-batch" \
    "$GIT -C $SCRATCH cat-file --batch < $SCRATCH/shas.txt" \
    "$GRIT -C $SCRATCH cat-file --batch < $SCRATCH/shas.txt"
}

bench_status() {
  make_large_scratch_repo
  # Dirty some files
  echo "dirty" >> "$SCRATCH/dir1/file1.txt"
  echo "dirty" >> "$SCRATCH/dir5/file50.txt"
  echo "new file" > "$SCRATCH/untracked.txt"
  run_bench "status" \
    "$GIT -C $SCRATCH status --porcelain" \
    "$GRIT -C $SCRATCH status --porcelain"
}

bench_status_clean() {
  make_large_scratch_repo
  run_bench "status-clean" \
    "$GIT -C $SCRATCH status --porcelain" \
    "$GRIT -C $SCRATCH status --porcelain"
}

bench_log() {
  make_large_scratch_repo
  run_bench "log-oneline" \
    "$GIT -C $SCRATCH log --oneline" \
    "$GRIT -C $SCRATCH log --oneline"
}

bench_log_format() {
  make_large_scratch_repo
  run_bench "log-format" \
    "$GIT -C $SCRATCH log --format='%H %an %s'" \
    "$GRIT -C $SCRATCH log --format='%H %an %s'"
}

bench_rev_parse() {
  make_large_scratch_repo
  run_bench "rev-parse" \
    "$GIT -C $SCRATCH rev-parse HEAD" \
    "$GRIT -C $SCRATCH rev-parse HEAD"
}

bench_rev_list() {
  make_large_scratch_repo
  run_bench "rev-list" \
    "$GIT -C $SCRATCH rev-list HEAD" \
    "$GRIT -C $SCRATCH rev-list HEAD"
}

bench_ls_files() {
  make_large_scratch_repo
  run_bench "ls-files" \
    "$GIT -C $SCRATCH ls-files" \
    "$GRIT -C $SCRATCH ls-files"
}

bench_ls_tree() {
  make_large_scratch_repo
  run_bench "ls-tree" \
    "$GIT -C $SCRATCH ls-tree -r HEAD" \
    "$GRIT -C $SCRATCH ls-tree -r HEAD"
}

bench_diff_files() {
  make_large_scratch_repo
  # Modify several files
  for d in 1 3 5 7 9; do
    for f in 10 30 50 70 90; do
      echo "modification" >> "$SCRATCH/dir$d/file$f.txt"
    done
  done
  run_bench "diff-files" \
    "$GIT -C $SCRATCH diff-files" \
    "$GRIT -C $SCRATCH diff-files"
}

bench_diff() {
  make_large_scratch_repo
  for d in 1 3 5 7 9; do
    for f in 10 30 50 70 90; do
      echo "modification" >> "$SCRATCH/dir$d/file$f.txt"
    done
  done
  run_bench "diff" \
    "$GIT -C $SCRATCH diff" \
    "$GRIT -C $SCRATCH diff"
}

bench_add() {
  make_large_scratch_repo
  for d in 1 3 5 7 9; do
    for f in $(seq 1 50); do
      echo "new content" >> "$SCRATCH/dir$d/file$f.txt"
    done
  done
  run_bench "add" \
    --prepare "cd $SCRATCH && $GIT checkout -q -- ." \
    "$GIT -C $SCRATCH add -A" \
    "$GRIT -C $SCRATCH add -A"
}

bench_write_tree() {
  make_large_scratch_repo
  run_bench "write-tree" \
    "$GIT -C $SCRATCH write-tree" \
    "$GRIT -C $SCRATCH write-tree"
}

bench_branch() {
  make_large_scratch_repo
  # Create 100 branches
  for b in $(seq 1 100); do
    (cd "$SCRATCH" && $GIT branch "test-branch-$b")
  done
  run_bench "branch-list" \
    "$GIT -C $SCRATCH branch --list" \
    "$GRIT -C $SCRATCH branch --list"
}

bench_for_each_ref() {
  make_large_scratch_repo
  for b in $(seq 1 100); do
    (cd "$SCRATCH" && $GIT branch "bench-ref-$b")
  done
  for t in $(seq 1 50); do
    (cd "$SCRATCH" && $GIT tag "bench-tag-$t")
  done
  run_bench "for-each-ref" \
    "$GIT -C $SCRATCH for-each-ref" \
    "$GRIT -C $SCRATCH for-each-ref"
}

bench_config() {
  make_scratch_repo
  # Set 100 config entries
  for i in $(seq 1 100); do
    (cd "$SCRATCH" && $GIT config "bench.key$i" "value$i")
  done
  run_bench "config-list" \
    "$GIT -C $SCRATCH config --list" \
    "$GRIT -C $SCRATCH config --list"
}

bench_show_ref() {
  make_large_scratch_repo
  for b in $(seq 1 100); do
    (cd "$SCRATCH" && $GIT branch "showref-$b")
  done
  for t in $(seq 1 50); do
    (cd "$SCRATCH" && $GIT tag "showref-tag-$t")
  done
  run_bench "show-ref" \
    "$GIT -C $SCRATCH show-ref" \
    "$GRIT -C $SCRATCH show-ref"
}

bench_read_tree() {
  make_large_scratch_repo
  local tree_sha
  tree_sha=$(cd "$SCRATCH" && $GIT rev-parse HEAD^{tree})
  run_bench "read-tree" \
    "$GIT -C $SCRATCH read-tree $tree_sha" \
    "$GRIT -C $SCRATCH read-tree $tree_sha"
}

bench_commit() {
  make_large_scratch_repo
  run_bench "commit" \
    --prepare "cd $SCRATCH && echo change-\$RANDOM >> dir1/file1.txt && $GIT add dir1/file1.txt" \
    -i \
    "$GIT -C $SCRATCH commit -q -m benchcommit --allow-empty" \
    "$GRIT -C $SCRATCH commit -q -m benchcommit --allow-empty"
}

bench_tag() {
  make_large_scratch_repo
  run_bench "tag-create" \
    --prepare "$GIT -C $SCRATCH tag -d bench-tag 2>/dev/null; true" \
    -i \
    "$GIT -C $SCRATCH tag bench-tag" \
    "$GRIT -C $SCRATCH tag bench-tag"
}

bench_tag_list() {
  make_large_scratch_repo
  for t in $(seq 1 200); do
    (cd "$SCRATCH" && $GIT tag "list-tag-$t")
  done
  run_bench "tag-list" \
    "$GIT -C $SCRATCH tag --list" \
    "$GRIT -C $SCRATCH tag --list"
}

bench_diff_index() {
  make_large_scratch_repo
  for d in 1 3 5 7 9; do
    for f in 10 30 50 70 90; do
      echo "modification" >> "$SCRATCH/dir$d/file$f.txt"
    done
  done
  (cd "$SCRATCH" && $GIT add -A)
  local head_sha
  head_sha=$(cd "$SCRATCH" && $GIT rev-parse HEAD)
  run_bench "diff-index" \
    "$GIT -C $SCRATCH diff-index $head_sha" \
    "$GRIT -C $SCRATCH diff-index $head_sha"
}

bench_diff_tree() {
  make_large_scratch_repo
  local parent_sha head_sha
  parent_sha=$(cd "$SCRATCH" && $GIT rev-parse HEAD~5)
  head_sha=$(cd "$SCRATCH" && $GIT rev-parse HEAD)
  run_bench "diff-tree" \
    "$GIT -C $SCRATCH diff-tree $parent_sha $head_sha" \
    "$GRIT -C $SCRATCH diff-tree $parent_sha $head_sha"
}

bench_checkout() {
  make_large_scratch_repo
  (cd "$SCRATCH" && $GIT branch bench-alt HEAD~10)
  run_bench "checkout" \
    --prepare "$GIT -C $SCRATCH checkout -q main 2>/dev/null; true" \
    -i \
    "$GIT -C $SCRATCH checkout -q bench-alt" \
    "$GRIT -C $SCRATCH checkout -q bench-alt"
}

bench_reset() {
  make_large_scratch_repo
  local prev_sha
  prev_sha=$(cd "$SCRATCH" && $GIT rev-parse HEAD~1)
  run_bench "reset" \
    --prepare "$GIT -C $SCRATCH reset -q HEAD 2>/dev/null; true" \
    -i \
    "$GIT -C $SCRATCH reset -q --mixed $prev_sha" \
    "$GRIT -C $SCRATCH reset -q --mixed $prev_sha"
}

bench_rm() {
  make_large_scratch_repo
  run_bench "rm" \
    --prepare "$GIT -C $SCRATCH checkout -q -- . 2>/dev/null; $GIT -C $SCRATCH reset -q HEAD 2>/dev/null; true" \
    -i \
    "$GIT -C $SCRATCH rm -q --cached dir1/file1.txt" \
    "$GRIT -C $SCRATCH rm -q --cached dir1/file1.txt"
}

bench_mv() {
  make_large_scratch_repo
  run_bench "mv" \
    --prepare "$GIT -C $SCRATCH checkout -q -- . 2>/dev/null; $GIT -C $SCRATCH reset -q HEAD 2>/dev/null; true" \
    -i \
    "$GIT -C $SCRATCH mv dir1/file1.txt dir1/file1-renamed.txt" \
    "$GRIT -C $SCRATCH mv dir1/file1.txt dir1/file1-renamed.txt"
}

bench_check_ignore() {
  make_large_scratch_repo
  echo "*.log" > "$SCRATCH/.gitignore"
  echo "build/" >> "$SCRATCH/.gitignore"
  (cd "$SCRATCH" && $GIT add .gitignore && $GIT commit -q -m "add gitignore")
  run_bench "check-ignore" \
    "$GIT -C $SCRATCH check-ignore dir1/file1.log build/output.o src/main.rs" \
    "$GRIT -C $SCRATCH check-ignore dir1/file1.log build/output.o src/main.rs"
}

bench_symbolic_ref() {
  make_large_scratch_repo
  run_bench "symbolic-ref" \
    "$GIT -C $SCRATCH symbolic-ref HEAD" \
    "$GRIT -C $SCRATCH symbolic-ref HEAD"
}

bench_update_ref() {
  make_large_scratch_repo
  local head_sha
  head_sha=$(cd "$SCRATCH" && $GIT rev-parse HEAD)
  run_bench "update-ref" \
    --prepare "$GIT -C $SCRATCH update-ref -d refs/heads/bench-update 2>/dev/null; true" \
    "$GIT -C $SCRATCH update-ref refs/heads/bench-update $head_sha" \
    --prepare "$GIT -C $SCRATCH update-ref -d refs/heads/bench-update 2>/dev/null; true" \
    "$GRIT -C $SCRATCH update-ref refs/heads/bench-update $head_sha"
}

bench_merge_base() {
  make_large_scratch_repo
  local head_sha parent_sha
  head_sha=$(cd "$SCRATCH" && $GIT rev-parse HEAD)
  parent_sha=$(cd "$SCRATCH" && $GIT rev-parse HEAD~20)
  run_bench "merge-base" \
    "$GIT -C $SCRATCH merge-base $head_sha $parent_sha" \
    "$GRIT -C $SCRATCH merge-base $head_sha $parent_sha"
}

bench_show() {
  make_large_scratch_repo
  run_bench "show" \
    "$GIT -C $SCRATCH show HEAD" \
    "$GRIT -C $SCRATCH show HEAD"
}

bench_checkout_index() {
  make_large_scratch_repo
  run_bench "checkout-index" \
    --prepare "cd $SCRATCH && rm -f dir1/file1.txt" \
    "$GIT -C $SCRATCH checkout-index -f -- dir1/file1.txt" \
    --prepare "cd $SCRATCH && rm -f dir1/file1.txt" \
    "$GRIT -C $SCRATCH checkout-index -f -- dir1/file1.txt"
}

bench_update_index() {
  make_large_scratch_repo
  echo "new content" > "$SCRATCH/dir1/file1.txt"
  run_bench "update-index" \
    "$GIT -C $SCRATCH update-index --refresh" \
    "$GRIT -C $SCRATCH update-index --refresh"
}

bench_commit_tree() {
  make_large_scratch_repo
  local tree_sha
  tree_sha=$(cd "$SCRATCH" && $GIT rev-parse HEAD^{tree})
  run_bench "commit-tree" \
    "echo bench | $GIT -C $SCRATCH commit-tree $tree_sha" \
    "echo bench | $GRIT -C $SCRATCH commit-tree $tree_sha"
}

bench_name_rev() {
  make_large_scratch_repo
  for t in $(seq 1 50); do
    (cd "$SCRATCH" && $GIT tag "nrev-$t" HEAD~$((t % 50)))
  done
  local head_sha
  head_sha=$(cd "$SCRATCH" && $GIT rev-parse HEAD)
  run_bench "name-rev" \
    "$GIT -C $SCRATCH name-rev $head_sha" \
    "$GRIT -C $SCRATCH name-rev $head_sha"
}

bench_stripspace() {
  # Generate input with lots of trailing whitespace and blank lines
  printf '%s\n' "  hello  " "" "  world  " "" "" "  test  " > "$SCRATCH/strip-input.txt" 2>/dev/null || {
    make_scratch_repo
    printf '%s\n' "  hello  " "" "  world  " "" "" "  test  " > "$SCRATCH/strip-input.txt"
  }
  run_bench "stripspace" \
    "$GIT stripspace < $SCRATCH/strip-input.txt" \
    "$GRIT stripspace < $SCRATCH/strip-input.txt"
}

bench_count_objects() {
  make_large_scratch_repo
  run_bench "count-objects" \
    "$GIT -C $SCRATCH count-objects" \
    "$GRIT -C $SCRATCH count-objects"
}

# ── Run selected or all ───────────────────────────────────────────

ALL_BENCHES=(
  init hash-object cat-file cat-file-batch
  status status-clean
  log log-format rev-parse rev-list
  ls-files ls-tree
  diff-files diff diff-index diff-tree
  add write-tree commit
  tag tag-list branch for-each-ref
  config show-ref read-tree
  checkout checkout-index reset rm mv
  check-ignore symbolic-ref update-ref
  merge-base commit-tree name-rev
  show stripspace count-objects
)

if [[ $# -gt 0 ]]; then
  selected=("$@")
else
  selected=("${ALL_BENCHES[@]}")
fi

for bench in "${selected[@]}"; do
  fn="bench_${bench//-/_}"
  if declare -f "$fn" >/dev/null 2>&1; then
    $fn
  else
    echo "⚠ Unknown benchmark: $bench (skipping)"
  fi
done

# ── Generate report ───────────────────────────────────────────────

echo "Generating report..."
python3 "$BENCH_DIR/report.py"
echo "Done! Report at: docs/bench.html"
