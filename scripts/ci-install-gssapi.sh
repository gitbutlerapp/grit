#!/usr/bin/env bash
# Kerberos/GSSAPI headers and libs for grit-lib's optional `http-negotiate-gss` feature.
# grit-cli enables that feature on Unix, and rustdoc runs `--all-features`.
set -euo pipefail

if command -v apt-get >/dev/null 2>&1; then
	sudo apt-get update
	sudo DEBIAN_FRONTEND=noninteractive apt-get install -y libkrb5-dev pkg-config
else
	echo "ci-install-gssapi.sh: unsupported package manager (expected apt on CI runners)" >&2
	exit 1
fi