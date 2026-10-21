#!/usr/bin/env bash
# Generate and check grit-lib's committed public API snapshot (cargo-public-api).
#
# Pin the nightly toolchain date and cargo-public-api crate version here so CI and
# local `make public-api` / `make public-api-check` stay aligned.

set -euo pipefail

PUBLIC_API_NIGHTLY_DATE="2026-10-01"
PUBLIC_API_TOOLCHAIN="nightly-${PUBLIC_API_NIGHTLY_DATE}"
CARGO_PUBLIC_API_VERSION="0.52.0"

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SNAPSHOT="${REPO}/grit-lib/public-api.txt"

usage() {
	cat <<EOF
Usage: $(basename "$0") <generate|check|ensure-deps>

  generate     Write ${SNAPSHOT#"$REPO"/} from cargo public-api
  check        Regenerate to a temp file and diff against the committed snapshot
  ensure-deps              Install pinned nightly + cargo-public-api (for tests)
  ensure-toolchain         Install pinned nightly only (rustdoc JSON / coverage gate)
  ensure-cargo-public-api  Install or replace cargo-public-api when version mismatches
EOF
}

ensure_toolchain() {
	if ! rustup toolchain list | grep -q "^${PUBLIC_API_TOOLCHAIN}"; then
		echo "Installing Rust toolchain ${PUBLIC_API_TOOLCHAIN}..."
		rustup toolchain install "${PUBLIC_API_TOOLCHAIN}" --profile minimal
	fi
}

cargo_public_api_installed_version() {
	local raw=""
	if ! raw="$(cargo public-api --version 2>/dev/null)"; then
		printf ''
		return 0
	fi
	python3 -c "import sys; sys.path.insert(0, '${REPO}/scripts'); from public_api import parse_cargo_public_api_version_output; print(parse_cargo_public_api_version_output(sys.argv[1]) or '')" \
		"${raw}"
}

ensure_cargo_public_api() {
	local installed=""
	installed="$(cargo_public_api_installed_version)"
	if [[ "${installed}" != "${CARGO_PUBLIC_API_VERSION}" ]]; then
		echo "Installing cargo-public-api ${CARGO_PUBLIC_API_VERSION} (installed: ${installed:-none})..."
		cargo install cargo-public-api --version "${CARGO_PUBLIC_API_VERSION}" --locked --force
	fi
	installed="$(cargo_public_api_installed_version)"
	if [[ "${installed}" != "${CARGO_PUBLIC_API_VERSION}" ]]; then
		echo "error: cargo-public-api ${CARGO_PUBLIC_API_VERSION} required, got ${installed:-none}" >&2
		exit 1
	fi
}

write_snapshot() {
	local dest="$1"
	ensure_toolchain
	ensure_cargo_public_api
	cd "${REPO}"
	# Document the supported embedder surface only (`http-ureq` optional). Internal
	# bench helpers (`unstable-bench-helpers`) stay out of the committed snapshot.
	cargo "+${PUBLIC_API_TOOLCHAIN}" public-api -p grit-lib --features http-ureq --simplified >"${dest}"
}

write_step_summary() {
	local diff_text="$1"
	if [[ -z "${GITHUB_STEP_SUMMARY:-}" ]]; then
		return 0
	fi
	{
		echo "## public-api check failed"
		echo
		echo 'The regenerated \`grit-lib\` public API does not match \`grit-lib/public-api.txt\`.'
		echo
		echo '```diff'
		echo "${diff_text}"
		echo '```'
		echo
		echo 'Regenerate with \`make public-api\` and commit \`grit-lib/public-api.txt\`.'
	} >>"${GITHUB_STEP_SUMMARY}"
}

cmd="${1:-}"
case "${cmd}" in
ensure-deps)
	ensure_toolchain
	ensure_cargo_public_api
	cargo public-api --version
	;;
ensure-toolchain)
	ensure_toolchain
	;;
ensure-cargo-public-api)
	ensure_cargo_public_api
	cargo public-api --version
	;;
generate)
	write_snapshot "${SNAPSHOT}"
	echo "Wrote ${SNAPSHOT#"${REPO}/"}"
	;;
check)
	tmp="$(mktemp)"
	trap 'rm -f "${tmp}"' EXIT
	write_snapshot "${tmp}"
	if python3 "${REPO}/scripts/public_api.py" check "${SNAPSHOT}" "${tmp}"; then
		echo "public-api snapshot matches."
	else
		diff_text="$(python3 "${REPO}/scripts/public_api.py" diff "${SNAPSHOT}" "${tmp}")"
		printf '%s\n' "${diff_text}"
		write_step_summary "${diff_text}"
		exit 1
	fi
	;;
-h | --help | help)
	usage
	;;
*)
	usage >&2
	exit 2
	;;
esac
