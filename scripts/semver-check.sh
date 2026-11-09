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
Usage: $(basename "$0") <check|baseline|check-cmd|ensure-deps>

  baseline     Print the chosen baseline revision (tag or commit)
  check-cmd    Print the cargo semver-checks command line (no install or run)
  check        Run cargo semver-checks (see baseline)
  ensure-deps  Install pinned cargo-semver-checks (for CI and local runs)

Baseline resolution (first match wins): SEMVER_BASELINE; else latest reachable v0.6+
tag when one exists; else api-ratchet semver_baseline pin; else latest v* tag.
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

is_full_object_name() {
	local rev="$1"
	[[ "${rev}" =~ ^[0-9a-fA-F]{40}$ ]]
}

git_object_exists() {
	local rev="$1"
	git -C "${REPO}" rev-parse --verify --quiet "${rev}^{commit}" >/dev/null 2>&1
}

is_stable_v_tag() {
	local tag="$1"
	[[ "${tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]
}

version_ge_0_6_0() {
	local ver="$1"
	# ver is like 0.6.0 or 1.0.0 (no leading v); caller must ensure a stable tag shape
	awk -v v="${ver}" '
		BEGIN {
			split(v, p, ".");
			maj = p[1] + 0;
			min = p[2] + 0;
			if (maj > 0) exit 0;
			if (min >= 6) exit 0;
			exit 1;
		}
	'
}

latest_merged_v06_plus_tag() {
	local tag=""
	while IFS= read -r tag; do
		[[ -n "${tag}" ]] || continue
		is_stable_v_tag "${tag}" || continue
		local ver="${tag#v}"
		if version_ge_0_6_0 "${ver}"; then
			printf '%s\n' "${tag}"
			return 0
		fi
	done < <(git -C "${REPO}" tag --merged HEAD --list 'v*' --sort=-v:refname 2>/dev/null || true)
	return 1
}

baseline_rev() {
	if [[ -n "${SEMVER_BASELINE:-}" ]]; then
		printf '%s\n' "${SEMVER_BASELINE}"
		return 0
	fi
	local tag=""
	tag="$(latest_merged_v06_plus_tag || true)"
	if [[ -n "${tag}" ]]; then
		printf '%s\n' "${tag}"
		return 0
	fi
	local pinned=""
	pinned="$(read_ratchet_semver_baseline || true)"
	if [[ -n "${pinned}" ]]; then
		if ! is_full_object_name "${pinned}" || git_object_exists "${pinned}"; then
			printf '%s\n' "${pinned}"
			return 0
		fi
		echo "warning: semver_baseline ${pinned} is not in this repository; ignoring pin" >&2
	fi
	git -C "${REPO}" describe --tags --abbrev=0 --match 'v*'
}

workspace_version() {
	grep -E '^version[[:space:]]*=' "${REPO}/Cargo.toml" | head -1 | sed -E 's/.*=[[:space:]]*"([^"]+)".*/\1/'
}

crates_still_0_6_x() {
	local ver
	ver="$(workspace_version)"
	awk -v v="${ver}" '
		BEGIN {
			split(v, p, ".");
			exit (p[1] + 0 == 0 && p[2] + 0 == 6) ? 0 : 1;
		}
	'
}

baseline_is_v06_plus_tag() {
	local rev="$1"
	local tag=""
	tag="$(git -C "${REPO}" describe --tags --exact-match "${rev}" 2>/dev/null)" || return 1
	[[ -n "${tag}" ]] || return 1
	is_stable_v_tag "${tag}" || return 1
	local ver="${tag#v}"
	version_ge_0_6_0 "${ver}"
}

semver_check_release_args() {
	local rev="$1"
	local args=(
		check-release -p grit-lib
		--baseline-rev "${rev}"
		--features http-ureq
	)
	if baseline_is_v06_plus_tag "${rev}" && crates_still_0_6_x; then
		args+=(--release-type minor)
	fi
	printf '%s\n' "${args[@]}"
}

print_check_cmd() {
	local rev
	rev="$(baseline_rev)"
	local -a args
	mapfile -t args < <(semver_check_release_args "${rev}")
	printf 'cargo semver-checks'
	printf ' %q' "${args[@]}"
	printf '\n'
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
			echo 'Breaking changes block merges when the baseline is a **v0.6+** tag (additive-only within **0.6.x**). Before **v0.6.0** exists on the branch, the pin in \`api-ratchet.toml\` keeps the comparison stable.'
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
	local -a args
	mapfile -t args < <(semver_check_release_args "${rev}")
	set +e
	# Uses the workspace toolchain (rust-toolchain.toml). cargo-semver-checks 0.51.0
	# supports rustdoc JSON from Rust 1.99.0; no pinned nightly override needed.
	cargo semver-checks "${args[@]}" 2>&1 | tee "${log}"
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
baseline)
	baseline_rev
	;;
check-cmd)
	print_check_cmd
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
