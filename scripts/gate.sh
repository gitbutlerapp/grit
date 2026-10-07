#!/usr/bin/env bash
# Pre-integration gate: fmt, clippy (-D warnings), workspace tests.
# Stops at the first failing stage. Uses all CPU cores for this run only
# (overrides .cargo/config.toml jobs = 2 via CARGO_BUILD_JOBS).

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$(nproc)}"

run_stage() {
	local label="$1"
	shift
	echo ""
	echo "==> ${label}"
	local start=$SECONDS
	if "$@"; then
		local elapsed=$((SECONDS - start))
		printf '==> %s passed (%dm %ds)\n' "${label}" $((elapsed / 60)) $((elapsed % 60))
	else
		local status=$?
		local elapsed=$((SECONDS - start))
		printf '==> %s failed (%dm %ds)\n' "${label}" $((elapsed / 60)) $((elapsed % 60))
		exit "${status}"
	fi
}

echo "Pre-integration gate (CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS})"

run_stage "fmt" cargo fmt --all --check
run_stage "clippy" cargo clippy --workspace -- -D warnings
run_stage "rustdoc" env RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
run_stage "test" cargo test --workspace

echo ""
echo "Gate passed."
