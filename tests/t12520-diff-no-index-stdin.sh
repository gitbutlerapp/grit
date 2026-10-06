#!/bin/sh
# Tests for grit diff-tree --stdin mode: feeding commit SHAs via stdin,
# various output formats, root commits, and multiple commit batches.

test_description='grit diff-tree --stdin'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

test_expect_success 'setup: create repo with history' '
	(
	grit init repo &&
	cd repo &&
	"$REAL_GIT" config user.email "t@t.com" &&
	"$REAL_GIT" config user.name "T" &&
	echo "line1" >file1.txt &&
	echo "line1" >file2.txt &&
	mkdir -p sub &&
	echo "nested" >sub/deep.txt &&
	grit add . &&
	grit commit -m "initial" &&
	echo "line2" >>file1.txt &&
	grit add file1.txt &&
	grit commit -m "modify file1" &&
	echo "new" >file3.txt &&
	grit add file3.txt &&
	grit commit -m "add file3" &&
	echo "changed" >sub/deep.txt &&
	grit add sub/deep.txt &&
	grit commit -m "modify sub/deep.txt" &&
	echo "extra" >>file2.txt &&
	grit add file2.txt &&
	grit commit -m "modify file2"
	)
'

# ---- basic stdin raw output ----

test_expect_success 'diff-tree --stdin: single commit shows raw output' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD | grit diff-tree --stdin -r >../actual) &&
	grep "file2.txt" actual
'

test_expect_success 'diff-tree --stdin: single commit matches git' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD | grit diff-tree --stdin -r >../grit_out &&
	 "$REAL_GIT" rev-parse HEAD | "$REAL_GIT" diff-tree --stdin -r >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-tree --stdin: commit SHA printed before output' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD | grit diff-tree --stdin -r >../actual) &&
	head -1 actual >first_line &&
	(cd repo && "$REAL_GIT" rev-parse HEAD >../expect_sha) &&
	test_cmp expect_sha first_line
'

test_expect_success 'diff-tree --stdin: multiple commits produce output for each' '
	(cd repo &&
	 "$REAL_GIT" log --format=%H | grit diff-tree --stdin -r >../actual) &&
	(cd repo && "$REAL_GIT" log --format=%H | "$REAL_GIT" diff-tree --stdin -r >../expect) &&
	test_cmp expect actual
'

test_expect_success 'diff-tree --stdin: root commit with --root shows additions' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD~4 | grit diff-tree --stdin --root -r >../actual) &&
	grep "A" actual &&
	grep "file1.txt" actual &&
	grep "file2.txt" actual &&
	grep "sub/deep.txt" actual
'

test_expect_success 'diff-tree --stdin --root: root commit matches git' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD~4 | grit diff-tree --stdin --root -r >../grit_out &&
	 "$REAL_GIT" rev-parse HEAD~4 | "$REAL_GIT" diff-tree --stdin --root -r >../git_out) &&
	test_cmp git_out grit_out
'

# ---- stdin with --name-only ----

test_expect_success 'diff-tree --stdin --name-only: shows only filenames' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD | grit diff-tree --stdin --name-only >../actual) &&
	grep "file2.txt" actual &&
	! grep ":" actual
'

test_expect_success 'diff-tree --stdin --name-only: matches git' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD | grit diff-tree --stdin --name-only >../grit_out &&
	 "$REAL_GIT" rev-parse HEAD | "$REAL_GIT" diff-tree --stdin --name-only >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-tree --stdin --name-only: multiple commits match git' '
	(cd repo &&
	 "$REAL_GIT" log --format=%H | grit diff-tree --stdin --name-only >../grit_out &&
	 "$REAL_GIT" log --format=%H | "$REAL_GIT" diff-tree --stdin --name-only >../git_out) &&
	test_cmp git_out grit_out
'

# ---- stdin with --name-status ----

test_expect_success 'diff-tree --stdin --name-status: shows status letters' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD | grit diff-tree --stdin --name-status >../actual) &&
	grep "^M" actual
'

test_expect_success 'diff-tree --stdin --name-status: matches git' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD | grit diff-tree --stdin --name-status >../grit_out &&
	 "$REAL_GIT" rev-parse HEAD | "$REAL_GIT" diff-tree --stdin --name-status >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-tree --stdin --name-status: multiple commits match git' '
	(cd repo &&
	 "$REAL_GIT" log --format=%H | grit diff-tree --stdin --name-status >../grit_out &&
	 "$REAL_GIT" log --format=%H | "$REAL_GIT" diff-tree --stdin --name-status >../git_out) &&
	test_cmp git_out grit_out
'

# ---- stdin with --stat ----

test_expect_success 'diff-tree --stdin --stat: shows stat output' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD | grit diff-tree --stdin --stat >../actual) &&
	grep "file2.txt" actual &&
	grep "changed" actual
'

test_expect_success 'diff-tree --stdin --stat: multiple commits match git' '
	(cd repo &&
	 "$REAL_GIT" log --format=%H | grit diff-tree --stdin --stat >../grit_out &&
	 "$REAL_GIT" log --format=%H | "$REAL_GIT" diff-tree --stdin --stat >../git_out) &&
	test_cmp git_out grit_out
'

# ---- stdin with -p (patch) ----

test_expect_success 'diff-tree --stdin -p: shows patch output' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD | grit diff-tree --stdin -p >../actual) &&
	grep "diff --git" actual &&
	grep "file2.txt" actual
'

test_expect_success 'diff-tree --stdin -p: multiple commits match git' '
	(cd repo &&
	 "$REAL_GIT" log --format=%H | grit diff-tree --stdin -p >../grit_out &&
	 "$REAL_GIT" log --format=%H | "$REAL_GIT" diff-tree --stdin -p >../git_out) &&
	test_cmp git_out grit_out
'

# ---- stdin with added/deleted files ----

test_expect_success 'setup: create commit with addition and deletion' '
	(cd repo &&
	 "$REAL_GIT" rm file1.txt &&
	 echo "brand new" >file4.txt &&
	 grit add file4.txt &&
	 grit commit -m "remove file1 add file4")
'

test_expect_success 'diff-tree --stdin --name-status: A and D for add/delete' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD | grit diff-tree --stdin --name-status >../actual) &&
	grep "^D" actual | grep "file1.txt" &&
	grep "^A" actual | grep "file4.txt"
'

test_expect_success 'diff-tree --stdin: add/delete matches git' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD | grit diff-tree --stdin -r >../grit_out &&
	 "$REAL_GIT" rev-parse HEAD | "$REAL_GIT" diff-tree --stdin -r >../git_out) &&
	test_cmp git_out grit_out
'

# ---- stdin with empty diff (identical parent/child trees for a path subset) ----

test_expect_success 'setup: create commit touching only one file' '
	(cd repo &&
	 echo "only-this" >>file2.txt &&
	 grit add file2.txt &&
	 grit commit -m "only file2")
'

test_expect_success 'diff-tree --stdin: only changed file appears' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD | grit diff-tree --stdin --name-only >../actual) &&
	grep "file2.txt" actual &&
	! grep "file3.txt" actual &&
	! grep "file4.txt" actual
'

# ---- stdin with subdirectory changes ----

test_expect_success 'setup: modify nested file' '
	(cd repo &&
	 echo "deep change" >>sub/deep.txt &&
	 grit add sub/deep.txt &&
	 grit commit -m "modify nested again")
'

test_expect_success 'diff-tree --stdin -r: nested file changes appear' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD | grit diff-tree --stdin -r >../actual) &&
	grep "sub/deep.txt" actual
'

test_expect_success 'diff-tree --stdin -r: nested change matches git' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD | grit diff-tree --stdin -r >../grit_out &&
	 "$REAL_GIT" rev-parse HEAD | "$REAL_GIT" diff-tree --stdin -r >../git_out) &&
	test_cmp git_out grit_out
'

# ---- batch of all commits ----

test_expect_success 'diff-tree --stdin: full history batch matches git' '
	(cd repo &&
	 "$REAL_GIT" log --format=%H | grit diff-tree --stdin -r >../grit_out &&
	 "$REAL_GIT" log --format=%H | "$REAL_GIT" diff-tree --stdin -r >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-tree --stdin: full history --name-only batch matches git' '
	(cd repo &&
	 "$REAL_GIT" log --format=%H | grit diff-tree --stdin --name-only >../grit_out &&
	 "$REAL_GIT" log --format=%H | "$REAL_GIT" diff-tree --stdin --name-only >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-tree --stdin: full history --name-status batch matches git' '
	(cd repo &&
	 "$REAL_GIT" log --format=%H | grit diff-tree --stdin --name-status >../grit_out &&
	 "$REAL_GIT" log --format=%H | "$REAL_GIT" diff-tree --stdin --name-status >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-tree --stdin: full history -p batch matches git' '
	(cd repo &&
	 "$REAL_GIT" log --format=%H | grit diff-tree --stdin -p >../grit_out &&
	 "$REAL_GIT" log --format=%H | "$REAL_GIT" diff-tree --stdin -p >../git_out) &&
	test_cmp git_out grit_out
'

# ---- stdin with single root commit only ----

test_expect_success 'diff-tree --stdin: root-only commit without --root is empty' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD~7 | grit diff-tree --stdin -r >../actual) &&
	(cd repo && "$REAL_GIT" rev-parse HEAD~7 >../sha_line) &&
	# output should be just the SHA (no diff entries without --root)
	test_cmp sha_line actual
'

test_expect_success 'diff-tree --stdin --root: root-only commit lists all files' '
	(cd repo &&
	 "$REAL_GIT" rev-parse HEAD~7 | grit diff-tree --stdin --root -r >../actual) &&
	grep "file1.txt" actual &&
	grep "file2.txt" actual &&
	grep "sub/deep.txt" actual
'

test_done
