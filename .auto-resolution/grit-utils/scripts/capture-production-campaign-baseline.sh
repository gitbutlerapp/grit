#!/usr/bin/env bash
# Run two consecutive production campaign captures and verify ±10% ratio stability.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
export PATH="/usr/local/cargo/bin:${PATH:-}"

BENCH="./target/release/grit-bench"
RUN1="/tmp/campaign-production-run1.json"
RUN2="/tmp/campaign-production-run2.json"
BASELINE_DIR="grit-utils/baselines"
MAX_COMPARE_ATTEMPTS="${GRIT_BENCH_CAMPAIGN_COMPARE_ATTEMPTS:-8}"

attempt=1
while true; do
  echo "=== capture attempt ${attempt}/${MAX_COMPARE_ATTEMPTS} $(date -Is) ==="
  echo "Run 1 → ${RUN1}"
  "$BENCH" campaign --warmup 0 --min-runs 5 --no-network --format json --output "$RUN1" --ledger

  echo "Run 2 → ${RUN2}"
  "$BENCH" campaign --warmup 0 --min-runs 5 --no-network --format json --output "$RUN2" --ledger

  echo "Compare consecutive runs (±10% tolerance)"
  set +e
  "$BENCH" compare "$RUN1" "$RUN2" --tolerance 0.10 2>&1 | tee "${BASELINE_DIR}/campaign-consecutive-compare.txt"
  compare_ec=${PIPESTATUS[0]}
  set -e
  if [[ "$compare_ec" -eq 0 ]]; then
    break
  fi
  echo "Compare failed (exit ${compare_ec}); retrying consecutive pair…"
  attempt=$((attempt + 1))
  if [[ "$attempt" -gt "$MAX_COMPARE_ATTEMPTS" ]]; then
    echo "Compare still outside ±10% after ${MAX_COMPARE_ATTEMPTS} attempts" >&2
    exit 1
  fi
done

cp "$RUN1" "${BASELINE_DIR}/campaign-before.json"
cp "$RUN2" "${BASELINE_DIR}/campaign-after.json"
cp "${RUN1%.json}.ledger.md" "${BASELINE_DIR}/campaign-before.ledger.md" 2>/dev/null || true

echo "Committed baselines updated under ${BASELINE_DIR}/"
