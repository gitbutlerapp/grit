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
run_stage "check (default features)" bash -c 'cargo check -p grit-lib && cargo check -p grit-cli'
run_stage "clippy" cargo clippy --workspace -- -D warnings
run_stage "rustdoc" env RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
run_stage "public module doctests" cargo test -p grit-lib --test public_module_doctests
PUBLIC_API_NIGHTLY_DATE="$(grep 'PUBLIC_API_NIGHTLY_DATE=' scripts/public-api.sh | cut -d'"' -f2)"
if rustup toolchain list | grep -q "^nightly-${PUBLIC_API_NIGHTLY_DATE}"; then
	run_stage "api-ratchet" make api-ratchet-check
else
	echo ""
	echo "==> api-ratchet skipped (pinned nightly-${PUBLIC_API_NIGHTLY_DATE} not installed)"
fi
if git diff --name-only origin/main...HEAD 2>/dev/null | grep -qE '^grit-lib/'; then
	SEMVER_CHECKS_VERSION="$(grep 'CARGO_SEMVER_CHECKS_VERSION=' scripts/semver-check.sh | cut -d'"' -f2)"
	if command -v cargo >/dev/null 2>&1 && cargo semver-checks --version 2>/dev/null | grep -qF "${SEMVER_CHECKS_VERSION}"; then
		run_stage "semver-check" make semver-check
	else
		echo ""
		echo "==> semver-check skipped (cargo-semver-checks ${SEMVER_CHECKS_VERSION} not installed)"
	fi
else
	echo ""
	echo "==> semver-check skipped (no grit-lib/ changes vs origin/main)"
fi
WORKSPACE_TEST_FEATURES=(--features grit-lib/test-support)
if cargo nextest --version >/dev/null 2>&1; then
	run_stage "test" cargo nextest run --workspace "${WORKSPACE_TEST_FEATURES[@]}"
	run_stage "doctests" cargo test --workspace --doc "${WORKSPACE_TEST_FEATURES[@]}"
else
	run_stage "test" cargo test --workspace "${WORKSPACE_TEST_FEATURES[@]}"
fi

run_stage "cli-layer-ratchet" make cli-layer-ratchet-check

echo ""
echo "Gate passed."
