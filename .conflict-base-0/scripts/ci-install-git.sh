#!/usr/bin/env bash
# Install a Git >= 2.45 for reftable interop tests on CI runners.
set -euo pipefail

git_version_ge_2_45() {
  local ver="${1#git version }"
  ver="${ver%%-*}"
  local major minor
  IFS=. read -r major minor _ <<<"$ver"
  if (( major > 2 )); then
    return 0
  fi
  (( major == 2 && minor >= 45 ))
}

if git_version_ge_2_45 "$(git --version)"; then
  git version
  exit 0
fi

export DEBIAN_FRONTEND=noninteractive
if [[ ! -f /etc/apt/apt.conf.d/99-ci-no-translations ]]; then
  echo 'Acquire::Languages "none";' | sudo tee /etc/apt/apt.conf.d/99-ci-no-translations >/dev/null
fi
sudo add-apt-repository -y ppa:git-core/ppa
sudo apt-get update -o Acquire::Languages=none
sudo apt-get install -y git
git version
if ! git_version_ge_2_45 "$(git --version)"; then
  echo "installed git is still older than 2.45" >&2
  exit 1
fi
