#!/bin/bash
# Benchmark grit's clone / commit / push workflows against fixed fixtures.
#
# Usage:
#   scripts/bench-ccp.sh fixtures            # build fixture repos (one-time)
#   scripts/bench-ccp.sh run [BIN] [N]       # run all scenarios, median of N (default 5)
#   scripts/bench-ccp.sh run-one NAME [BIN] [N]
#
# BIN defaults to target/release/grit-git. Pass /usr/bin/git to get a
# reference baseline. All work happens under $BENCH_ROOT (default
# /tmp/grit-bench); the source tree is never touched.
set -u
BENCH_ROOT=${BENCH_ROOT:-/tmp/grit-bench}
FIX=$BENCH_ROOT/fixtures
WORK=$BENCH_ROOT/work
DEFAULT_BIN=$(cd "$(dirname "$0")/.." && pwd)/target/release/grit-git
export GIT_AUTHOR_NAME=bench GIT_AUTHOR_EMAIL=bench@example.com
export GIT_COMMITTER_NAME=bench GIT_COMMITTER_EMAIL=bench@example.com
export GIT_CONFIG_NOSYSTEM=1 HOME=$BENCH_ROOT/home

now_ms() { date +%s%3N; }

build_fixtures() {
  local g=${1:-$DEFAULT_BIN}
  rm -rf "$FIX" "$BENCH_ROOT/home"
  mkdir -p "$FIX" "$BENCH_ROOT/home"

  # many: 10k small files over 100 dirs, 5 commits, packed.
  local d=$FIX/many
  "$g" init -q "$d"
  (
    cd "$d"
    for batch in 0 1 2 3 4; do
      for dir in $(seq 0 19); do
        mkdir -p "d$((batch * 20 + dir))"
        for f in $(seq 0 99); do
          printf 'content %s %s %s\n' "$batch" "$dir" "$f" > "d$((batch * 20 + dir))/f$f.txt"
        done
      done
      "$g" add . && "$g" commit -q -m "batch $batch"
    done
    "$g" repack -adq 2>/dev/null || "$g" repack -ad
  )

  # many-loose: same tree shape, single commit, objects left loose.
  d=$FIX/many-loose
  "$g" init -q "$d"
  (
    cd "$d"
    for dir in $(seq 0 99); do
      mkdir -p "d$dir"
      for f in $(seq 0 99); do
        printf 'loose %s %s\n' "$dir" "$f" > "d$dir/f$f.txt"
      done
    done
    "$g" add . && "$g" commit -q -m loose
  )

  # hist: 2000 commits appending to a rotating set of 10 files, packed.
  d=$FIX/hist
  "$g" init -q "$d"
  (
    cd "$d"
    for i in $(seq 1 2000); do
      echo "line $i" >> "file$((i % 10)).txt"
      "$g" add "file$((i % 10)).txt"
      "$g" commit -q -m "c$i"
    done
    "$g" repack -adq 2>/dev/null || "$g" repack -ad
  )

  # large: three ~40MB blobs (base64 of urandom: incompressible-ish), packed.
  d=$FIX/large
  "$g" init -q "$d"
  (
    cd "$d"
    for i in 1 2 3; do
      head -c 30000000 /dev/urandom | base64 > "big$i.bin"
    done
    "$g" add . && "$g" commit -q -m large
    "$g" repack -adq 2>/dev/null || "$g" repack -ad
  )

  # plain-10k: bare directory tree (no repo) used to seed add/commit runs.
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
  echo "fixtures built under $FIX"
}

# Each scenario defines: prep (untimed), cmd (timed), post (untimed check).
prep() {
  rm -rf "$WORK"
  mkdir -p "$WORK"
  cd "$WORK"
  case $1 in
    clone-many|clone-hist|clone-large|clone-file-many|clone-bare-many|clone-many-loose) : ;;
    commit-touch-many)
      "$G" clone -q "$FIX/many" w
      cd w && echo tweak >> d0/f0.txt ;;
    add-10k)
      "$G" init -q w
      cp -r "$FIX/plain-10k/." w/
      cd w ;;
    commit-10k)
      "$G" init -q w
      cp -r "$FIX/plain-10k/." w/
      cd w && "$G" add -A ;;
    push-hist)
      "$G" clone -q "$FIX/hist" w
      "$G" init -q --bare r.git
      cd w && "$G" remote add dst ../r.git ;;
    push-incr)
      "$G" clone -q "$FIX/hist" w
      "$G" clone -q --bare "$FIX/hist" r.git
      cd w && "$G" remote add dst ../r.git
      echo extra >> file0.txt && "$G" commit -q -am extra ;;
    push-large)
      "$G" clone -q "$FIX/large" w
      "$G" init -q --bare r.git
      cd w && "$G" remote add dst ../r.git ;;
    *) echo "unknown scenario $1" >&2; return 1 ;;
  esac
}

run_cmd() {
  case $1 in
    clone-many)       "$G" clone -q "$FIX/many" c ;;
    clone-many-loose) "$G" clone -q "$FIX/many-loose" c ;;
    clone-hist)       "$G" clone -q "$FIX/hist" c ;;
    clone-large)      "$G" clone -q "$FIX/large" c ;;
    clone-file-many)  "$G" clone -q "file://$FIX/many" c ;;
    clone-bare-many)  "$G" clone -q --bare "$FIX/many" c.git ;;
    commit-touch-many) "$G" commit -q -am tweak ;;
    add-10k)          "$G" add -A ;;
    commit-10k)       "$G" commit -q -m init ;;
    push-hist)        "$G" push -q dst master ;;
    push-incr)        "$G" push -q dst master ;;
    push-large)       "$G" push -q dst master ;;
  esac
}

SCENARIOS="clone-many clone-many-loose clone-hist clone-large clone-file-many clone-bare-many commit-touch-many add-10k commit-10k push-hist push-incr push-large"

run_one() {
  local name=$1 n=${2:-5} times=() t0 t1 rc
  for _ in $(seq 1 "$n"); do
    prep "$name" > /dev/null 2>&1 || { echo "$name PREP-FAIL"; return 1; }
    t0=$(now_ms)
    run_cmd "$name" > /dev/null 2>&1
    rc=$?
    t1=$(now_ms)
    [ $rc -ne 0 ] && { echo "$name FAIL rc=$rc"; return 1; }
    times+=($((t1 - t0)))
  done
  local sorted median
  sorted=$(printf '%s\n' "${times[@]}" | sort -n)
  median=$(printf '%s\n' "$sorted" | awk "NR==$(((n + 1) / 2))")
  printf '%s\t%s\t[%s]\n' "$name" "$median" "$(printf '%s\n' "$sorted" | paste -sd,)"
}

case ${1:-run} in
  fixtures) build_fixtures "${2:-$DEFAULT_BIN}" ;;
  run)
    G=${2:-$DEFAULT_BIN}; n=${3:-5}
    echo -e "scenario\tmedian_ms\truns"
    for s in $SCENARIOS; do run_one "$s" "$n"; done ;;
  run-one)
    G=${3:-$DEFAULT_BIN}
    run_one "$2" "${4:-5}" ;;
  *) echo "usage: $0 fixtures|run [BIN] [N]|run-one NAME [BIN] [N]" >&2; exit 1 ;;
esac
