#!/usr/bin/env bash
# Regression: bench_setup_env strips inherited GIT_* and leaves only explicit exports.
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=env.sh
source "$BENCH_DIR/env.sh"

export GIT_DIR=/tmp/evil.git
export GIT_WORK_TREE=/tmp/evil-tree
export GIT_CONFIG_COUNT=1
export GIT_CONFIG_KEY_0=commit.gpgsign
export GIT_CONFIG_VALUE_0=true
export GIT_AUTHOR_NAME=leak
export GIT_COMMITTER_EMAIL=leak@example.com

_bench_env_initialized=0
bench_setup_env

for bad in GIT_DIR GIT_WORK_TREE GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0 \
  GIT_AUTHOR_NAME GIT_COMMITTER_EMAIL; do
  if [[ -n "${!bad-}" ]]; then
    echo "FAIL: $bad still set after bench_setup_env" >&2
    exit 1
  fi
done

for want in GIT_CONFIG_NOSYSTEM GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM; do
  if [[ -z "${!want-}" ]]; then
    echo "FAIL: expected $want to be set" >&2
    exit 1
  fi
done

echo "OK: bench_setup_env hermetic GIT_* cleanup"
