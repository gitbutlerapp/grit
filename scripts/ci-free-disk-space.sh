#!/usr/bin/env bash
# Reclaim several GiB on GitHub-hosted ubuntu-latest runners before workspace test links.
set -euo pipefail

echo "Disk before cleanup:"
df -h /

# Large preinstalled stacks not needed for Rust workspace tests.
sudo rm -rf /usr/share/dotnet
sudo rm -rf /usr/local/lib/android
sudo rm -rf /opt/ghc
sudo rm -rf /opt/hostedtoolcache/CodeQL
sudo rm -rf /usr/local/share/boost
sudo rm -rf /usr/local/share/chromium
sudo rm -rf /usr/local/share/powershell
sudo rm -rf /usr/share/swift
sudo rm -rf /usr/local/share/vcpkg
sudo rm -rf /usr/lib/jvm
sudo rm -rf /usr/local/julia*
sudo rm -rf /opt/az
sudo rm -rf /opt/microsoft/msedge
sudo rm -rf /opt/microsoft/powershell
sudo rm -rf /usr/local/share/emacs
sudo rm -rf /usr/local/share/kotlin

# Runner toolcache (Rust toolchain is installed after this step in CI workflows).
if [ -n "${AGENT_TOOLSDIRECTORY:-}" ]; then
  sudo rm -rf "${AGENT_TOOLSDIRECTORY:?}"/*
fi

if command -v docker >/dev/null 2>&1; then
  sudo docker image prune --all --force || true
fi

sudo apt-get autoremove -y -qq || true
sudo apt-get clean -y -qq || true

echo "Disk after cleanup:"
df -h /
