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

echo "Run 1 → ${RUN1}"
"$BENCH" campaign --warmup 0 --min-runs 1 --no-network --format json --output "$RUN1" --ledger

echo "Run 2 → ${RUN2}"
"$BENCH" campaign --warmup 0 --min-runs 1 --no-network --format json --output "$RUN2" --ledger

echo "Compare consecutive runs (±10% tolerance)"
"$BENCH" compare "$RUN1" "$RUN2" --tolerance 0.10 | tee "${BASELINE_DIR}/campaign-consecutive-compare.txt"

cp "$RUN1" "${BASELINE_DIR}/campaign-before.json"
cp "$RUN2" "${BASELINE_DIR}/campaign-after.json"
cp "${RUN1%.json}.ledger.md" "${BASELINE_DIR}/campaign-before.ledger.md" 2>/dev/null || true

echo "Committed baselines updated under ${BASELINE_DIR}/"
