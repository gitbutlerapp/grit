#!/bin/bash
set -eu
REPO=/home/hasi/grit
BIN=$REPO/target/release/grit
RESULTS=/tmp/grit-test-results
rm -rf "$RESULTS"
mkdir -p "$RESULTS"

run_test() {
  local f="$1"
  local name=$(basename "$f" .sh)
  local out="$RESULTS/$name.out"
  local trash="/tmp/grit-trash-$name"
  rm -rf "$trash"
  mkdir -p "$trash"
  (
    cd "$REPO/tests"
    GUST_BIN="$BIN" TRASH_DIRECTORY="$trash" timeout 30 sh "./$f" > "$out" 2>&1
  ) || true
  rm -rf "$trash"
}

export REPO BIN RESULTS
export -f run_test

find "$REPO/tests" -maxdepth 1 -name 't[0-9]*.sh' -printf '%f\n' | sort | \
  xargs -P 16 -I{} bash -c 'run_test "$@"' _ {}

# Aggregate
total_tests=0
total_pass=0
total_fail=0
total_skip=0
total_files=0
pass_files=0
fail_files=0
error_files=0

for out in "$RESULTS"/*.out; do
  total_files=$((total_files + 1))
  line=$(grep '^# Tests:' "$out" 2>/dev/null | tail -1) || true
  if [ -z "$line" ]; then
    error_files=$((error_files + 1))
    continue
  fi
  t=$(echo "$line" | sed 's/.*Tests: *\([0-9]*\).*/\1/')
  p=$(echo "$line" | sed 's/.*Pass: *\([0-9]*\).*/\1/')
  f=$(echo "$line" | sed 's/.*Fail: *\([0-9]*\).*/\1/')
  s=$(echo "$line" | sed 's/.*Skip: *\([0-9]*\).*/\1/')
  total_tests=$((total_tests + t))
  total_pass=$((total_pass + p))
  total_fail=$((total_fail + f))
  total_skip=$((total_skip + s))
  if [ "$f" -eq 0 ]; then
    pass_files=$((pass_files + 1))
  else
    fail_files=$((fail_files + 1))
  fi
done

echo "=== RESULTS ==="
echo "Files: $total_files (pass: $pass_files, fail: $fail_files, error/timeout: $error_files)"
echo "Tests: $total_tests (pass: $total_pass, fail: $total_fail, skip: $total_skip)"
pct=$(echo "scale=1; $total_pass * 100 / $total_tests" | bc)
echo "Pass rate: $pct%"
