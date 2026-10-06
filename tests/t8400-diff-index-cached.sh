#!/bin/sh
# Comprehensive tests for diff-index --cached: comparing HEAD against index.
# Tests raw output format only (grit diff-index does not yet support
# --name-only, --name-status, -p, or --stat).

test_description='diff-index --cached comprehensive'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

SYS_GIT=$(command -v git)

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup: repository with initial commit' '
	(
	"$SYS_GIT" init repo &&
	cd repo &&
	"$SYS_GIT" config user.name "Test User" &&
	"$SYS_GIT" config user.email "test@example.com" &&
	echo "hello" >file1.txt &&
	echo "world" >file2.txt &&
	mkdir -p sub &&
	echo "nested" >sub/file3.txt &&
	"$SYS_GIT" add . &&
	"$SYS_GIT" commit -m "initial"
	)
'

# ── No changes: clean index ──────────────────────────────────────────────

test_expect_success 'diff-index --cached: no changes produces empty output' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	test_must_be_empty ../di_out
	)
'

# ── Staged modification ─────────────────────────────────────────────────

test_expect_success 'setup: stage a modification' '
	(
	cd repo &&
	echo "modified hello" >file1.txt &&
	"$SYS_GIT" add file1.txt
	)
'

test_expect_success 'diff-index --cached: shows staged modification' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "file1.txt" ../di_out
	)
'

test_expect_success 'diff-index --cached: modification shows M status' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "M" ../di_out | grep "file1.txt"
	)
'

test_expect_success 'diff-index --cached: raw format has colon prefix' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "^:" ../di_out
	)
'

test_expect_success 'diff-index --cached: raw format has mode and oid' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep -E "^:[0-9]{6} [0-9]{6} [0-9a-f]{40}" ../di_out
	)
'

test_expect_success 'diff-index --cached: old oid is from HEAD' '
	(
	cd repo &&
	head_blob=$("$SYS_GIT" rev-parse HEAD:file1.txt) &&
	git diff-index --cached HEAD >../di_out &&
	grep "$head_blob" ../di_out
	)
'

test_expect_success 'diff-index --cached: new oid differs from old oid' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	old_oid=$(awk "{print \$3}" ../di_out | sed "s/.*//;" | head -1) &&
	line=$(head -1 ../di_out) &&
	old=$(echo "$line" | awk "{print \$3}") &&
	new=$(echo "$line" | awk "{print \$4}") &&
	test "$old" != "$new"
	)
'

# ── Staged addition ─────────────────────────────────────────────────────

test_expect_success 'setup: stage a new file' '
	(
	cd repo &&
	echo "brand new" >newfile.txt &&
	"$SYS_GIT" add newfile.txt
	)
'

test_expect_success 'diff-index --cached: shows staged addition' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "newfile.txt" ../di_out
	)
'

test_expect_success 'diff-index --cached: addition shows A status' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "A" ../di_out | grep "newfile.txt"
	)
'

test_expect_success 'diff-index --cached: addition has null old oid' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "newfile.txt" ../di_out | grep "0000000000000000000000000000000000000000"
	)
'

test_expect_success 'diff-index --cached: addition has 000000 old mode' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "newfile.txt" ../di_out | grep "^:000000"
	)
'

# ── Multiple staged changes at once ──────────────────────────────────────

test_expect_success 'diff-index --cached: shows both modification and addition' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "file1.txt" ../di_out &&
	grep "newfile.txt" ../di_out
	)
'

test_expect_success 'diff-index --cached: output has exactly 2 entries' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	count=$(wc -l <../di_out | tr -d " ") &&
	test "$count" = "2"
	)
'

# ── Commit staged changes, stage deletion ────────────────────────────────

test_expect_success 'setup: commit and then stage deletion' '
	(
	cd repo &&
	"$SYS_GIT" commit -m "add changes" &&
	"$SYS_GIT" rm file2.txt
	)
'

test_expect_success 'diff-index --cached: shows staged deletion' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "file2.txt" ../di_out
	)
'

test_expect_success 'diff-index --cached: deletion shows D status' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "D" ../di_out | grep "file2.txt"
	)
'

test_expect_success 'diff-index --cached: deletion has null new oid' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	line=$(grep "file2.txt" ../di_out) &&
	new_oid=$(echo "$line" | sed "s/.*\t.*//" | awk "{print \$2}" ) &&
	echo "$line" | grep "0000000000000000000000000000000000000000"
	)
'

# ── After commit: clean state again ──────────────────────────────────────

test_expect_success 'setup: commit deletion' '
	(
	cd repo &&
	"$SYS_GIT" commit -m "delete file2"
	)
'

test_expect_success 'diff-index --cached: clean after commit' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	test_must_be_empty ../di_out
	)
'

# ── Stage multiple changes in different dirs ─────────────────────────────

test_expect_success 'setup: stage changes in root and subdirectory' '
	(
	cd repo &&
	echo "updated hello again" >file1.txt &&
	echo "new sub file" >sub/file4.txt &&
	"$SYS_GIT" add file1.txt sub/file4.txt
	)
'

test_expect_success 'diff-index --cached: shows root and subdirectory changes' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "file1.txt" ../di_out &&
	grep "sub/file4.txt" ../di_out
	)
'

test_expect_success 'diff-index --cached: M for modification, A for addition' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "M" ../di_out | grep "file1.txt" &&
	grep "A" ../di_out | grep "sub/file4.txt"
	)
'

# ── Unstaged changes should NOT appear with --cached ─────────────────────

test_expect_success 'setup: create unstaged modification' '
	(
	cd repo &&
	"$SYS_GIT" commit -m "stage changes" &&
	echo "unstaged change" >file1.txt
	)
'

test_expect_success 'diff-index --cached: does not show unstaged changes' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	test_must_be_empty ../di_out
	)
'

# ── diff-index without --cached shows worktree vs HEAD ───────────────────

test_expect_success 'diff-index (no --cached): shows unstaged modification' '
	(
	cd repo &&
	git diff-index HEAD >../di_out &&
	grep "file1.txt" ../di_out
	)
'

test_expect_success 'diff-index (no --cached): shows M status for worktree change' '
	(
	cd repo &&
	git diff-index HEAD >../di_out &&
	grep "M" ../di_out | grep "file1.txt"
	)
'

# ── Empty file staged ───────────────────────────────────────────────────

test_expect_success 'setup: stage empty file' '
	(
	cd repo &&
	"$SYS_GIT" checkout -- file1.txt &&
	>emptyfile.txt &&
	"$SYS_GIT" add emptyfile.txt
	)
'

test_expect_success 'diff-index --cached: shows staged empty file as addition' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "emptyfile.txt" ../di_out &&
	grep "A" ../di_out
	)
'

test_expect_success 'diff-index --cached: empty file has empty blob oid' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "emptyfile.txt" ../di_out | grep "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
	)
'

# ── Stage file then unstage it ───────────────────────────────────────────

test_expect_success 'setup: stage then reset a file' '
	(
	cd repo &&
	"$SYS_GIT" commit -m "add empty" &&
	echo "temp" >temp.txt &&
	"$SYS_GIT" add temp.txt &&
	"$SYS_GIT" reset HEAD -- temp.txt
	)
'

test_expect_success 'diff-index --cached: reset file does not appear' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	! grep "temp.txt" ../di_out
	)
'

# ── Subdirectory only changes ────────────────────────────────────────────

test_expect_success 'setup: modify only subdirectory file' '
	(
	cd repo &&
	rm -f temp.txt &&
	echo "updated nested" >sub/file3.txt &&
	"$SYS_GIT" add sub/file3.txt
	)
'

test_expect_success 'diff-index --cached: shows subdirectory modification' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "sub/file3.txt" ../di_out
	)
'

test_expect_success 'diff-index --cached: subdirectory mod is M status' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "M" ../di_out | grep "sub/file3.txt"
	)
'

# ── Mode 100644 preserved in output ─────────────────────────────────────

test_expect_success 'diff-index --cached: shows 100644 mode for regular files' '
	(
	cd repo &&
	git diff-index --cached HEAD >../di_out &&
	grep "100644" ../di_out
	)
'

test_done
