#!/usr/bin/env bash
# Run two consecutive full `grit-bench daily` captures and verify ±10% stability.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
export PATH="/usr/local/cargo/bin:${PATH:-}"

BENCH="./target/release/grit-bench"
RUN1="/tmp/daily-production-run1.json"
RUN2="/tmp/daily-production-run2.json"
BASELINE_DIR="grit-utils/baselines/daily"
MAX_COMPARE_ATTEMPTS="${GRIT_BENCH_DAILY_COMPARE_ATTEMPTS:-8}"
TOLERANCE="${GRIT_BENCH_DAILY_TOLERANCE:-0.10}"

# Run one functional area at a time to cap peak RSS on memory-tight factory VMs.
DAILY_AREAS=(
  odb packs refs index worktree diff revwalk blame branching remotes maintenance config ignore hooks notes bundles worktrees
)

BASELINE_GRIT="${GRIT_BENCH_BASELINE_GRIT:-}"
if [[ -z "${BASELINE_GRIT}" ]]; then
  echo "Set GRIT_BENCH_BASELINE_GRIT to the latest release grit binary" >&2
  exit 1
fi

mkdir -p "${BASELINE_DIR}"

merge_daily_parts() {
  local out=$1
  shift
  jq -s '
    .[0] as $first |
    {
      schema: $first.schema,
      generated_at: $first.generated_at,
      machine: $first.machine,
      tools: $first.tools,
      scenarios: (map(.scenarios) | add | unique_by(.id) | sort_by(.id))
    }
  ' "$@" >"$out"
}

run_daily_merged() {
  local label=$1
  local out=$2
  local tmpdir start end
  tmpdir=$(mktemp -d)
  start=$(date +%s)
  echo "${label} started $(date -Is) (sequential --area batches)"
  for area in "${DAILY_AREAS[@]}"; do
    echo "  area=${area} …"
    "$BENCH" daily --offline --area "$area" --baseline-grit "${BASELINE_GRIT}" \
      --output "${tmpdir}/${area}.json"
  done
  merge_daily_parts "$out" "${tmpdir}"/*.json
  rm -rf "${tmpdir}"
  end=$(date +%s)
  echo "${label} wall_seconds=$((end - start))"
}

attempt=1
while true; do
  echo "=== daily capture attempt ${attempt}/${MAX_COMPARE_ATTEMPTS} $(date -Is) ==="
  echo "Run 1 → ${RUN1}"
  run_daily_merged "daily run 1" "${RUN1}" 2>&1 | tee "${BASELINE_DIR}/first-run.log"

  echo "Run 2 → ${RUN2}"
  run_daily_merged "daily run 2" "${RUN2}" 2>&1 | tee "${BASELINE_DIR}/second-run.log"

  echo "Compare consecutive runs (±${TOLERANCE} tolerance)"
  set +e
  "$BENCH" daily compare "${RUN1}" "${RUN2}" --tolerance "${TOLERANCE}" --markdown \
    2>&1 | tee "${BASELINE_DIR}/consecutive-compare.txt"
  compare_ec=${PIPESTATUS[0]}
  set -e
  echo "compare_exit=${compare_ec}" >> "${BASELINE_DIR}/consecutive-compare.txt"
  if [[ "$compare_ec" -eq 0 ]]; then
    break
  fi
  echo "Compare failed (exit ${compare_ec}); retrying consecutive pair…"
  attempt=$((attempt + 1))
  if [[ "$attempt" -gt "$MAX_COMPARE_ATTEMPTS" ]]; then
    echo "Compare still outside ±${TOLERANCE} after ${MAX_COMPARE_ATTEMPTS} attempts" >&2
    exit 1
  fi
done

cp "$RUN1" "${BASELINE_DIR}/latest.json"
cp "$RUN2" "${BASELINE_DIR}/second.json"
"$BENCH" daily validate "${BASELINE_DIR}/latest.json"
"$BENCH" daily validate "${BASELINE_DIR}/second.json"

echo "Committed daily baselines updated under ${BASELINE_DIR}/"
