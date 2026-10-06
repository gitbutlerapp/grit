#!/bin/sh
#
# Tests for prune-packed — removes loose objects that are in pack files.

test_description='prune-packed command tests'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# Helper: count loose objects (excluding pack/ and info/)
count_loose () {
	find "$1/.git/objects" -type f | grep -v pack | grep -v info | wc -l | tr -d ' '
}

# ---------------------------------------------------------------------------
# Basic prune-packed
# ---------------------------------------------------------------------------
test_expect_success 'setup: create repo with objects' '
	(
	git init repo1 &&
	cd repo1 &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "file1" >f1.txt &&
	git add f1.txt &&
	git commit -m "commit1" &&
	echo "file2" >f2.txt &&
	git add f2.txt &&
	git commit -m "commit2"
	)
'

test_expect_success 'loose objects exist before repack' '
	(
	cd repo1 &&
	count=$(count_loose .) &&
	test "$count" -gt 0
	)
'

test_expect_success 'repack -a creates pack but keeps loose objects' '
	(
	cd repo1 &&
	git repack -a &&
	ls .git/objects/pack/*.pack >packs &&
	test -s packs &&
	count=$(count_loose .) &&
	test "$count" -gt 0
	)
'

test_expect_success 'prune-packed removes all loose objects in pack' '
	(
	cd repo1 &&
	git prune-packed &&
	count=$(count_loose .) &&
	test "$count" = 0
	)
'

test_expect_success 'prune-packed with no loose objects is a no-op' '
	(
	cd repo1 &&
	git prune-packed &&
	count=$(count_loose .) &&
	test "$count" = 0
	)
'

# ---------------------------------------------------------------------------
# Dry-run mode (-n)
# ---------------------------------------------------------------------------
test_expect_success 'setup: repo for dry-run tests' '
	(
	git init repo-dryrun &&
	cd repo-dryrun &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "content" >f.txt &&
	git add f.txt &&
	git commit -m "init" &&
	git repack -a
	)
'

test_expect_success 'prune-packed -n shows what would be removed' '
	(
	cd repo-dryrun &&
	git prune-packed -n >out 2>&1 &&
	test -s out &&
	grep "rm -f" out
	)
'

test_expect_success 'prune-packed -n does not actually remove objects' '
	(
	cd repo-dryrun &&
	count_before=$(count_loose .) &&
	git prune-packed -n >/dev/null 2>&1 &&
	count_after=$(count_loose .) &&
	test "$count_before" = "$count_after"
	)
'

test_expect_success 'prune-packed --dry-run works like -n' '
	(
	cd repo-dryrun &&
	count_before=$(count_loose .) &&
	git prune-packed --dry-run >out 2>&1 &&
	count_after=$(count_loose .) &&
	test "$count_before" = "$count_after" &&
	grep "rm -f" out
	)
'

test_expect_success 'dry-run output paths are valid object files' '
	(
	cd repo-dryrun &&
	git prune-packed -n 2>&1 | sed "s/^rm -f //" >paths &&
	while read p; do
		test -f "$p" || { echo "Missing: $p"; exit 1; }
	done <paths
	)
'

# ---------------------------------------------------------------------------
# Quiet mode (-q)
# ---------------------------------------------------------------------------
test_expect_success 'setup: repo for quiet tests' '
	(
	git init repo-quiet &&
	cd repo-quiet &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "data" >d.txt &&
	git add d.txt &&
	git commit -m "data" &&
	git repack -a
	)
'

test_expect_success 'prune-packed -q produces no output' '
	(
	cd repo-quiet &&
	git prune-packed -q >out 2>&1 &&
	! test -s out
	)
'

test_expect_success 'prune-packed -q still removes objects' '
	(
	cd repo-quiet &&
	count=$(count_loose .) &&
	test "$count" = 0
	)
'

# ---------------------------------------------------------------------------
# Objects not in pack are preserved
# ---------------------------------------------------------------------------
test_expect_success 'setup: repo with orphan object' '
	(
	git init repo-orphan &&
	cd repo-orphan &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "tracked" >t.txt &&
	git add t.txt &&
	git commit -m "tracked" &&
	echo "orphan-content" | git hash-object -w --stdin >../orphan_hash &&
	git repack -a
	)
'

test_expect_success 'prune-packed preserves loose objects not in pack' '
	(
	cd repo-orphan &&
	git prune-packed &&
	hash=$(cat ../orphan_hash) &&
	prefix=$(echo "$hash" | cut -c1-2) &&
	suffix=$(echo "$hash" | cut -c3-) &&
	test -f ".git/objects/$prefix/$suffix"
	)
'

test_expect_success 'prune-packed removes tracked objects but not orphan' '
	(
	cd repo-orphan &&
	count=$(count_loose .) &&
	test "$count" = 1
	)
'

# ---------------------------------------------------------------------------
# prune-packed on repo with no packs does nothing
# ---------------------------------------------------------------------------
test_expect_success 'prune-packed with no packs does not remove anything' '
	(
	git init repo-nopacks &&
	cd repo-nopacks &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "data" >d.txt &&
	git add d.txt &&
	git commit -m "data" &&
	count_before=$(count_loose .) &&
	test "$count_before" -gt 0 &&
	git prune-packed &&
	count_after=$(count_loose .) &&
	test "$count_before" = "$count_after"
	)
'

# ---------------------------------------------------------------------------
# Single-commit repo
# ---------------------------------------------------------------------------
test_expect_success 'prune-packed works on single-commit repo' '
	(
	git init repo-single &&
	cd repo-single &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "only" >only.txt &&
	git add only.txt &&
	git commit -m "only commit" &&
	git repack -a &&
	count_before=$(count_loose .) &&
	test "$count_before" -gt 0 &&
	git prune-packed &&
	count_after=$(count_loose .) &&
	test "$count_after" = 0
	)
'

# ---------------------------------------------------------------------------
# Repack -a -d + prune-packed
# ---------------------------------------------------------------------------
test_expect_success 'repack -a -d then prune-packed leaves no loose' '
	(
	git init repo-ad &&
	cd repo-ad &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	git add . &&
	git commit -m "two files" &&
	git repack -a -d &&
	git prune-packed &&
	count=$(count_loose .) &&
	test "$count" = 0
	)
'

# ---------------------------------------------------------------------------
# Large number of objects
# ---------------------------------------------------------------------------
test_expect_success 'prune-packed handles many objects' '
	(
	git init repo-many &&
	cd repo-many &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	for i in $(seq 1 20); do
		echo "file $i" >"file$i.txt" || return 1
	done &&
	git add . &&
	git commit -m "20 files" &&
	count_before=$(count_loose .) &&
	test "$count_before" -gt 10 &&
	git repack -a &&
	git prune-packed &&
	count_after=$(count_loose .) &&
	test "$count_after" = 0
	)
'

test_done
