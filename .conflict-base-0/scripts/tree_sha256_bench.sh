#!/usr/bin/env bash
# Compare grit-lib tree_sha256 vs reference tree-sha256 on a warm-cache 1776-file loose-object repo.
# This script measures whole-process median latency (bash + guide_tree_sha256). For in-process
# Criterion numbers, run `cargo bench -p grit-lib --bench tree_sha256`.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIX="${TREE_SHA256_FIXTURE:-$ROOT/grit-lib/tests/fixtures/tree_sha256/git-dot-git-1776}"
REF_BIN="${TREE_SHA256_BIN:-tree-sha256}"
ITERS="${TREE_SHA256_ITERS:-25}"
WARM="${TREE_SHA256_WARM:-3}"

mkdir -p "$FIX"
if [[ ! -d "$FIX/.git" ]]; then
  git init -q -b main "$FIX"
  for i in $(seq 0 1775); do
    printf 'payload %s\n' "$i" >"$FIX/f$i.txt"
  done
  git -C "$FIX" add .
  git -C "$FIX" commit -q -m '1776 loose files'
fi

HEAD="$(git -C "$FIX" rev-parse HEAD)"
export FIX HEAD ROOT

python3 <<'PY'
import json, os, statistics, subprocess, sys, time

fix = os.environ["FIX"]
head = os.environ["HEAD"]
root = os.environ["ROOT"]
iters = int(os.environ.get("TREE_SHA256_ITERS", "25"))
warm = int(os.environ.get("TREE_SHA256_WARM", "3"))
ref_bin = os.environ.get("TREE_SHA256_BIN", "tree-sha256")

grit_bin = os.path.join(root, "target", "release", "guide_tree_sha256")
if not os.path.isfile(grit_bin):
    subprocess.check_call(
        ["cargo", "build", "--release", "-p", "grit-examples", "--bin", "guide_tree_sha256"],
        cwd=root,
    )

def run_ref():
    return subprocess.check_output(
        [ref_bin, "--repo", fix, head],
        stderr=subprocess.DEVNULL,
        text=True,
    ).strip().splitlines()[0]

def run_grit():
    return subprocess.check_output(
        ["bash", "-lc", f"cd {fix!r} && {grit_bin} {head}"],
        stderr=subprocess.DEVNULL,
        text=True,
    ).strip().splitlines()[0]

for _ in range(warm):
    run_ref()
    run_grit()

ref_d = run_ref()
grit_d = run_grit()
if ref_d != grit_d:
    print(f"digest mismatch ref={ref_d} grit={grit_d}", file=sys.stderr)
    sys.exit(1)

def bench(fn):
    samples = []
    for _ in range(iters):
        t0 = time.perf_counter()
        fn()
        samples.append((time.perf_counter() - t0) * 1000)
    samples.sort()
    return statistics.median(samples)

ref_ms = bench(run_ref)
grit_ms = bench(run_grit)
ratio = grit_ms / ref_ms if ref_ms else float("inf")

def peak_rss(cmd):
    out = subprocess.run(
        ["/usr/bin/time", "-f", "%M", "bash", "-lc", cmd],
        capture_output=True,
        text=True,
        check=False,
    )
    line = out.stderr.strip().splitlines()[-1]
    return int(line)

ref_rss = peak_rss(f"{ref_bin} --repo {fix!r} {head!r} >/dev/null")
grit_rss = peak_rss(f"cd {fix!r} && {grit_bin} {head!r} >/dev/null")

print(json.dumps({
    "fixture": fix,
    "files": 1776,
    "digest": ref_d,
    "reference_median_ms": round(ref_ms, 3),
    "grit_median_ms": round(grit_ms, 3),
    "ratio_grit_over_ref": round(ratio, 3),
    "reference_peak_rss_kb": ref_rss,
    "grit_peak_rss_kb": grit_rss,
    "iters": iters,
    "warm": warm,
}, indent=2))
PY
