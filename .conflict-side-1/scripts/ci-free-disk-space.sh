#!/usr/bin/env bash
# Reclaim several GiB on GitHub-hosted ubuntu-latest runners before workspace test links.
set -euo pipefail

echo "Disk before cleanup:"
df -h /

sudo rm -rf /usr/share/dotnet
sudo rm -rf /usr/local/lib/android
sudo rm -rf /opt/ghc
sudo rm -rf /opt/hostedtoolcache/CodeQL
if command -v docker >/dev/null 2>&1; then
  sudo docker image prune --all --force || true
fi

echo "Disk after cleanup:"
df -h /
