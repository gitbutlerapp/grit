# bench/env.sh — Hermetic git/grit environment for fair hyperfine comparisons.
#
# Sourced by run.sh and run-everyday.sh. Both tools see the same HOME and global
# config (no system config, no runner gpgsign/signing program leakage).

_bench_env_initialized=0
_bench_home_owned=0

bench_setup_env() {
  if [[ "${_bench_env_initialized}" -eq 1 ]]; then
    return 0
  fi
  _bench_env_initialized=1

  # Drop every inherited GIT_*; only the exports below may remain.
  local _v
  while IFS= read -r _v; do
    unset "$_v"
  done < <(compgen -e | grep '^GIT_' || true)

  if [[ -z "${BENCH_HOME:-}" ]]; then
    BENCH_HOME="$(mktemp -d -t grit-bench-home.XXXXXX)"
    _bench_home_owned=1
  fi
  BENCH_GITCONFIG="${BENCH_HOME}/.gitconfig"
  mkdir -p "$BENCH_HOME"
  cat >"$BENCH_GITCONFIG" <<'EOF'
[user]
	name = bench
	email = b@b
[init]
	defaultBranch = main
[commit]
	gpgsign = false
[tag]
	gpgsign = false
EOF

  export HOME="$BENCH_HOME"
  export GIT_CONFIG_NOSYSTEM=1
  export GIT_CONFIG_GLOBAL="$BENCH_GITCONFIG"
  export GIT_CONFIG_SYSTEM=/dev/null
}

bench_teardown_env() {
  if [[ "${_bench_home_owned}" -eq 1 && -n "${BENCH_HOME:-}" ]]; then
    rm -rf "$BENCH_HOME"
    _bench_home_owned=0
  fi
}
