#!/usr/bin/env bash
# Pre-integration gate: fmt, clippy (-D warnings), workspace tests.
# Stops at the first failing stage. Uses all CPU cores for this run only
# (overrides .cargo/config.toml jobs = 2 via CARGO_BUILD_JOBS).

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 2)}"

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
run_stage "public module doctests" cargo test -p grit-lib --test public_module_doctests
if cargo nextest --version >/dev/null 2>&1; then
	run_stage "test" cargo nextest run --workspace
	run_stage "doctests" cargo test --workspace --doc
else
	run_stage "test" cargo test --workspace
fi

echo ""
echo "Gate passed."
