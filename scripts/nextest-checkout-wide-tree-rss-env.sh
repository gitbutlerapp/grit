#!/usr/bin/env bash
# Builds release grit and exports GRIT_BIN for grit-lib checkout_wide_tree_rss (issue #2280).
set -euo pipefail
if [[ -z "${NEXTEST_ENV:-}" ]]; then
	echo "nextest-checkout-wide-tree-rss-env: NEXTEST_ENV unset" >&2
	exit 1
fi
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"
cargo build --release -p grit-cli -q
grit_bin="$repo_root/target/release/grit"
if [[ ! -f "$grit_bin" ]]; then
	echo "nextest-checkout-wide-tree-rss-env: missing $grit_bin" >&2
	exit 1
fi
printf 'GRIT_BIN=%s\n' "$grit_bin" >>"$NEXTEST_ENV"
