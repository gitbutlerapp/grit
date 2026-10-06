#!/bin/sh
# Tests for plumbing diff commands: diff-tree, diff-index, diff-files.

test_description='diff-tree, diff-index, diff-files plumbing'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ──────────────────────────────────────────────────────────────

test_expect_success 'setup repo with two commits' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "aaa" >a.txt &&
	echo "bbb" >b.txt &&
	mkdir sub &&
	echo "ccc" >sub/c.txt &&
	grit add . &&
	grit commit -m "c1" &&
	grit rev-parse HEAD >../c1_oid &&
	echo "aaa-modified" >a.txt &&
	echo "ddd" >d.txt &&
	grit add . &&
	grit commit -m "c2" &&
	grit rev-parse HEAD >../c2_oid
	)
'

# ═══════════════════════════════════════════════════════════════════════
# diff-tree: compare two tree-ish objects
# ═══════════════════════════════════════════════════════════════════════

test_expect_success 'diff-tree between two commits shows changed file' '
	(
	cd repo &&
	c1=$(cat ../c1_oid) && c2=$(cat ../c2_oid) &&
	grit diff-tree "$c1" "$c2" >actual &&
	grep "M.*a.txt" actual
	)
'

test_expect_success 'diff-tree shows added file' '
	(
	cd repo &&
	c1=$(cat ../c1_oid) && c2=$(cat ../c2_oid) &&
	grit diff-tree "$c1" "$c2" >actual &&
	grep "A.*d.txt" actual
	)
'

test_expect_success 'diff-tree does not show unchanged file' '
	(
	cd repo &&
	c1=$(cat ../c1_oid) && c2=$(cat ../c2_oid) &&
	grit diff-tree "$c1" "$c2" >actual &&
	! grep "b.txt" actual &&
	! grep "sub/c.txt" actual
	)
'

test_expect_success 'diff-tree -r shows same as diff-tree for flat changes' '
	(
	cd repo &&
	c1=$(cat ../c1_oid) && c2=$(cat ../c2_oid) &&
	grit diff-tree -r "$c1" "$c2" >actual &&
	grep "M.*a.txt" actual &&
	grep "A.*d.txt" actual
	)
'

test_expect_success 'diff-tree -p shows patch output' '
	(
	cd repo &&
	c1=$(cat ../c1_oid) && c2=$(cat ../c2_oid) &&
	grit diff-tree -p "$c1" "$c2" >actual &&
	grep "^diff --git" actual &&
	grep "^+aaa-modified" actual
	)
'

test_expect_success 'diff-tree --name-only shows just filenames' '
	(
	cd repo &&
	c1=$(cat ../c1_oid) && c2=$(cat ../c2_oid) &&
	grit diff-tree --name-only "$c1" "$c2" >actual &&
	grep "a.txt" actual &&
	grep "d.txt" actual &&
	! grep ":" actual
	)
'

test_expect_success 'diff-tree --stat shows diffstat' '
	(
	cd repo &&
	c1=$(cat ../c1_oid) && c2=$(cat ../c2_oid) &&
	grit diff-tree --stat "$c1" "$c2" >actual &&
	grep "a.txt" actual &&
	grep "d.txt" actual
	)
'

test_expect_success 'diff-tree same commit shows nothing' '
	(
	cd repo &&
	c2=$(cat ../c2_oid) &&
	grit diff-tree "$c2" "$c2" >actual &&
	test_line_count = 0 actual
	)
'

# ═══════════════════════════════════════════════════════════════════════
# diff-index: compare tree against working tree or index
# ═══════════════════════════════════════════════════════════════════════

test_expect_success 'diff-index HEAD on clean tree shows nothing' '
	(
	cd repo &&
	grit diff-index HEAD >actual &&
	test_line_count = 0 actual
	)
'

test_expect_success 'diff-index HEAD shows working tree changes' '
	(
	cd repo &&
	echo "worktree-change" >a.txt &&
	grit diff-index HEAD >actual &&
	grep "M.*a.txt" actual
	)
'

test_expect_success 'diff-index --cached HEAD shows nothing when nothing staged' '
	(
	cd repo &&
	grit diff-index --cached HEAD >actual &&
	test_line_count = 0 actual
	)
'

test_expect_success 'diff-index --cached HEAD shows staged changes' '
	(
	cd repo &&
	grit add a.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "M.*a.txt" actual
	)
'

test_expect_success 'diff-index output has raw diff format' '
	(
	cd repo &&
	grit diff-index --cached HEAD >actual &&
	# Raw format: :old_mode new_mode old_oid new_oid status\tpath
	grep "^:" actual
	)
'

# ═══════════════════════════════════════════════════════════════════════
# diff-files: compare working tree against index
# ═══════════════════════════════════════════════════════════════════════

test_expect_success 'diff-files on clean worktree shows nothing' '
	(
	cd repo &&
	grit diff-files >actual &&
	test_line_count = 0 actual
	)
'

test_expect_success 'diff-files shows working tree modifications not yet staged' '
	(
	cd repo &&
	echo "more changes" >>b.txt &&
	grit diff-files >actual &&
	grep "M.*b.txt" actual
	)
'

test_expect_success 'diff-files does not show already-staged changes' '
	(
	cd repo &&
	grit diff-files >actual &&
	! grep "a.txt" actual
	)
'

test_expect_success 'diff-files after staging shows nothing for that file' '
	(
	cd repo &&
	grit add b.txt &&
	grit diff-files >actual &&
	! grep "b.txt" actual
	)
'

# ── More diff-tree with deletions ─────────────────────────────────────

test_expect_success 'setup commit with deletion' '
	(
	cd repo &&
	grit commit -m "stage changes" &&
	grit rev-parse HEAD >../c3_oid &&
	grit rm d.txt &&
	grit commit -m "delete d.txt" &&
	grit rev-parse HEAD >../c4_oid
	)
'

test_expect_success 'diff-tree shows deletion' '
	(
	cd repo &&
	c3=$(cat ../c3_oid) && c4=$(cat ../c4_oid) &&
	grit diff-tree "$c3" "$c4" >actual &&
	grep "D.*d.txt" actual
	)
'

test_expect_success 'diff-tree in reverse shows addition' '
	(
	cd repo &&
	c3=$(cat ../c3_oid) && c4=$(cat ../c4_oid) &&
	grit diff-tree "$c4" "$c3" >actual &&
	grep "A.*d.txt" actual
	)
'

test_done
