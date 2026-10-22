#!/usr/bin/env bash
# Run two consecutive full `grit-bench daily` captures and verify ±10% stability.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
export PATH="/usr/local/cargo/bin:${PATH:-}"
# Full-size @repo fixtures only; do not silently substitute smoke tiers under matrix ids.
unset GRIT_BENCH_DAILY_LOW_MEMORY

BENCH="./target/release/grit-bench"
RUN1="/tmp/daily-production-run1.json"
RUN2="/tmp/daily-production-run2.json"
BASELINE_DIR="grit-utils/baselines/daily"
MAX_COMPARE_ATTEMPTS="${GRIT_BENCH_DAILY_COMPARE_ATTEMPTS:-8}"
TOLERANCE="${GRIT_BENCH_DAILY_TOLERANCE:-0.10}"

BASELINE_GRIT="${GRIT_BENCH_BASELINE_GRIT:-}"
if [[ -z "${BASELINE_GRIT}" ]]; then
  echo "Set GRIT_BENCH_BASELINE_GRIT to the latest release grit binary" >&2
  exit 1
fi

mkdir -p "${BASELINE_DIR}"

mapfile -t DAILY_IDS < <("$BENCH" daily list-ids)

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
      capture: (
        [.[].capture? // empty]
        | if length == 0 then null else .[0] end
      ),
      scenarios: (map(.scenarios) | add | unique_by(.id) | sort_by(.id))
    }
  ' "$@" >"$out"
}

run_daily_merged() {
  local label=$1
  local out=$2
  local tmpdir start end id part area t0 t1 elapsed area_file
  tmpdir=$(mktemp -d)
  area_file=$(mktemp)
  start=$(date +%s)
  echo "${label} started $(date -Is) (sequential --only ${#DAILY_IDS[@]} scenarios)"
  for id in "${DAILY_IDS[@]}"; do
    echo "  scenario=${id} …"
    local attempt=1
    t0=$(date +%s.%N)
    while ! "$BENCH" daily --offline --only "$id" --baseline-grit "${BASELINE_GRIT}" \
      --output "${tmpdir}/${id//\//@-}.json"; do
      attempt=$((attempt + 1))
      if [[ "$attempt" -gt 5 ]]; then
        echo "scenario ${id} failed after 5 attempts" >&2
        return 1
      fi
      echo "  retry ${id} (attempt ${attempt}/5)…" >&2
    done
    t1=$(date +%s.%N)
    elapsed=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.3f", b - a }')
    part="${tmpdir}/${id//\//@-}.json"
    area=$(jq -r '.scenarios[0].area // empty' "$part")
    if [[ -n "$area" ]]; then
      echo "${area} ${elapsed}" >>"$area_file"
    fi
  done
  merge_daily_parts "$out" "${tmpdir}"/*.json
  rm -rf "${tmpdir}"
  end=$(date +%s)
  total_wall=$((end - start))
  cores=$(jq -c '.machine.logical_cores' "$out")
  if [[ -s "$area_file" ]]; then
    areas_json=$(
      awk '{a[$1]+=$2} END {for (k in a) print k, a[k]}' "$area_file" \
        | jq -R -s '
            split("\n") | map(select(length > 0))
            | map(split(" "))
            | map({key: .[0], value: (.[1] | tonumber)})
            | from_entries
          '
    )
    jq --argjson areas "$areas_json" --argjson total "$total_wall" --argjson cores "$cores" '
      .capture = (
        (.capture // {})
        | .fixture_policy = (
            .fixture_policy
            // "full-size fixture cache tiers for every @repo id in the daily matrix"
          )
        | .area_wall_seconds = $areas
        | .total_wall_seconds = $total
        | .logical_cores = $cores
      )
    ' "$out" >"${out}.tmp" && mv "${out}.tmp" "$out"
  else
    jq --argjson total "$total_wall" --argjson cores "$cores" '
      .capture = (
        (.capture // {})
        | .fixture_policy = (
            .fixture_policy
            // "full-size fixture cache tiers for every @repo id in the daily matrix"
          )
        | .total_wall_seconds = $total
        | .logical_cores = $cores
      )
    ' "$out" >"${out}.tmp" && mv "${out}.tmp" "$out"
  fi
  rm -f "$area_file"
  echo "${label} wall_seconds=${total_wall}"
}

attempt=1
while true; do
  echo "=== daily capture attempt ${attempt}/${MAX_COMPARE_ATTEMPTS} $(date -Is) ==="
  echo "Run 1 → ${RUN1}"
  run_daily_merged "daily run 1" "${RUN1}" 2>&1 | tee "${BASELINE_DIR}/first-run.log"

  echo "Run 2 → ${RUN2}"
  run_daily_merged "daily run 2" "${RUN2}" 2>&1 | tee "${BASELINE_DIR}/second-run.log"

  pair_dir="${BASELINE_DIR}/pairs"
  mkdir -p "${pair_dir}"
  cp "${RUN1}" "${pair_dir}/attempt-${attempt}-run1.json"
  cp "${RUN2}" "${pair_dir}/attempt-${attempt}-run2.json"

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
