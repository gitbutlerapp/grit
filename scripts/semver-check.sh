#!/usr/bin/env bash
# Compare grit-lib's public API against a semver baseline git tag (cargo-semver-checks).
#
# Pin cargo-semver-checks here so CI and local `make semver-check` stay aligned.
# The tool builds baseline rustdoc in its own target tree under target/semver-checks/;
# that is not a grit worktree build.

set -euo pipefail

CARGO_SEMVER_CHECKS_VERSION="0.51.0"

REPO="$(cd "$(dirname "$0")/.." && pwd)"

usage() {
	cat <<EOF
Usage: $(basename "$0") <check|ensure-deps>

  check        Run cargo semver-checks (baseline: SEMVER_BASELINE, api-ratchet semver_baseline, else latest v* tag)
  ensure-deps  Install pinned cargo-semver-checks (for CI and local runs)
EOF
}

semver_checks_installed_version() {
	local raw=""
	if ! raw="$(cargo semver-checks --version 2>/dev/null)"; then
		printf ''
		return 0
	fi
	# "cargo-semver-checks 0.51.0"
	printf '%s\n' "${raw}" | awk '{print $2}'
}

ensure_cargo_semver_checks() {
	local installed=""
	installed="$(semver_checks_installed_version)"
	if [[ "${installed}" != "${CARGO_SEMVER_CHECKS_VERSION}" ]]; then
		echo "Installing cargo-semver-checks ${CARGO_SEMVER_CHECKS_VERSION} (installed: ${installed:-none})..."
		cargo install cargo-semver-checks --version "${CARGO_SEMVER_CHECKS_VERSION}" --locked --force
	fi
	installed="$(semver_checks_installed_version)"
	if [[ "${installed}" != "${CARGO_SEMVER_CHECKS_VERSION}" ]]; then
		echo "error: cargo-semver-checks ${CARGO_SEMVER_CHECKS_VERSION} required, got ${installed:-none}" >&2
		exit 1
	fi
}

read_ratchet_semver_baseline() {
	local ratchet="${REPO}/grit-lib/api-ratchet.toml"
	local line=""
	[[ -f "${ratchet}" ]] || return 1
	line="$(grep -E '^semver_baseline[[:space:]]*=' "${ratchet}" 2>/dev/null | head -1)" || return 1
	[[ -n "${line}" ]] || return 1
	printf '%s\n' "${line}" | sed -n 's/.*=[[:space:]]*"\([^"]*\)".*/\1/p'
}

baseline_rev() {
	if [[ -n "${SEMVER_BASELINE:-}" ]]; then
		printf '%s\n' "${SEMVER_BASELINE}"
		return 0
	fi
	local pinned=""
	pinned="$(read_ratchet_semver_baseline || true)"
	if [[ -n "${pinned}" ]]; then
		printf '%s\n' "${pinned}"
		return 0
	fi
	git -C "${REPO}" describe --tags --abbrev=0 --match 'v*'
}

write_step_summary() {
	local rev="$1"
	local status="$2"
	local log_path="$3"
	if [[ -z "${GITHUB_STEP_SUMMARY:-}" ]]; then
		return 0
	fi
	{
		echo "## semver-check"
		echo
		if [[ "${status}" -eq 0 ]]; then
			echo "Passed against baseline \`${rev}\`."
		else
			echo "Reported breaking API changes against baseline \`${rev}\`."
			echo
			echo 'Informational until **0.6.0** (CI \`continue-on-error: true\`). At 0.6.0, remove that flag so this job blocks merges.'
		fi
		echo
		echo '```text'
		cat "${log_path}"
		echo '```'
	} >>"${GITHUB_STEP_SUMMARY}"
}

run_check() {
	ensure_cargo_semver_checks
	local rev
	rev="$(baseline_rev)"
	echo "Semver baseline: ${rev}"
	cd "${REPO}"
	local log
	log="$(mktemp)"
	trap "rm -f '${log}'" EXIT
	set +e
	# Uses the workspace toolchain (rust-toolchain.toml). cargo-semver-checks 0.51.0
	# supports rustdoc JSON from Rust 1.99.0; no pinned nightly override needed.
	cargo semver-checks check-release -p grit-lib \
		--baseline-rev "${rev}" \
		--features http-ureq \
		2>&1 | tee "${log}"
	local status="${PIPESTATUS[0]}"
	set -e
	write_step_summary "${rev}" "${status}" "${log}"
	if [[ "${status}" -eq 0 ]]; then
		echo "semver-check passed (baseline ${rev})."
	else
		echo "semver-check reported breaking changes (baseline ${rev})." >&2
		exit "${status}"
	fi
}

cmd="${1:-}"
case "${cmd}" in
ensure-deps)
	ensure_cargo_semver_checks
	cargo semver-checks --version
	;;
check)
	run_check
	;;
-h | --help | help)
	usage
	;;
*)
	usage >&2
	exit 2
	;;
esac
