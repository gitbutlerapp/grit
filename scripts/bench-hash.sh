#!/usr/bin/env bash
# Compare grit-lib SHA throughput with OpenSSL and Git on this machine.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "=== grit-lib hash backends ==="
cargo run -q -p grit-examples --bin hash-info -- --json

echo
echo "=== OpenSSL speed (16 KiB blocks for SHA-1) ==="
openssl speed -evp sha1 -bytes 16384 2>&1 | tail -5

echo
echo "=== OpenSSL speed SHA-256 ==="
openssl speed -evp sha256 2>&1 | tail -5

echo
echo "=== Criterion hash bench (release, sample) ==="
cargo bench -p grit-lib --bench hash -- --sample-size 10 2>&1 | rg '^(hash_|                        thrpt:)'

BIG="${TMPDIR:-/tmp}/grit-bench-hash-256m.bin"
if [[ ! -f "$BIG" ]] || [[ "$(stat -c%s "$BIG" 2>/dev/null || stat -f%z "$BIG")" != "$((256 * 1024 * 1024))" ]]; then
  echo "Creating 256 MiB test file at $BIG ..."
  dd if=/dev/zero of="$BIG" bs=1M count=256 status=none
fi

GRITX_HASH_FILE="$ROOT/target/release/gritx-hash-file"
cargo build -q -p grit-examples --release --bin gritx-hash-file
chmod +x "$GRITX_HASH_FILE"

BENCH_REPO="${TMPDIR:-/tmp}/grit-bench-hash-repo"
rm -rf "$BENCH_REPO"
mkdir -p "$BENCH_REPO"
git -C "$BENCH_REPO" init -q

echo
echo "=== hyperfine: git hash-object vs gritx-hash-file (256 MiB blob, SHA-1) ==="
if command -v hyperfine >/dev/null 2>&1; then
  hyperfine --warmup 1 \
    "git -C '$BENCH_REPO' hash-object '$BIG'" \
    "'$GRITX_HASH_FILE' '$BIG'"
else
  echo "hyperfine not installed; timing with date"
  bench_timed() {
    local label=$1
    shift
    local start end
    start=$(date +%s.%N)
    "$@" >/dev/null
    end=$(date +%s.%N)
    python3 -c "import sys; print(f'{sys.argv[1]}: {float(sys.argv[3]) - float(sys.argv[2]):.3f} sec')" \
      "$label" "$start" "$end"
  }
  bench_timed "git hash-object" git -C "$BENCH_REPO" hash-object "$BIG"
  bench_timed "gritx-hash-file" "$GRITX_HASH_FILE" "$BIG"
fi
