#!/bin/sh
# Tests for grit diff-index: comparing a tree (HEAD) against the working
# tree and/or the index. Covers --cached, --raw, --quiet, --exit-code,
# --abbrev, -m (merge), path filters, additions, deletions, modifications.

test_description='grit diff-index worktree and index comparisons'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

test_expect_success 'setup: initial repo' '
	(
	grit init repo &&
	cd repo &&
	"$REAL_GIT" config user.email "t@t.com" &&
	"$REAL_GIT" config user.name "T" &&
	echo "alpha" >a.txt &&
	echo "bravo" >b.txt &&
	echo "charlie" >c.txt &&
	mkdir -p sub &&
	echo "deep" >sub/d.txt &&
	grit add . &&
	grit commit -m "initial"
	)
'

# ---- clean state ----

test_expect_success 'diff-index HEAD: clean repo produces no output' '
	(cd repo && grit diff-index HEAD >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff-index --quiet HEAD: returns 0 on clean repo' '
	(cd repo && grit diff-index --quiet HEAD)
'

test_expect_success 'diff-index --exit-code HEAD: returns 0 on clean repo' '
	(cd repo && grit diff-index --exit-code HEAD >../actual) &&
	test_must_be_empty actual
'

# ---- worktree modification ----

test_expect_success 'setup: modify file in worktree' '
	(cd repo && echo "alpha2" >a.txt)
'

test_expect_success 'diff-index HEAD: shows modified file' '
	(cd repo && grit diff-index HEAD >../actual) &&
	grep "a.txt" actual &&
	grep "M" actual
'

test_expect_success 'diff-index HEAD: matches git' '
	(cd repo && grit diff-index HEAD >../grit_out &&
	 "$REAL_GIT" diff-index HEAD >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-index --exit-code HEAD: returns 1 with changes' '
	(cd repo && test_must_fail grit diff-index --exit-code HEAD)
'

test_expect_success 'diff-index --quiet HEAD: returns 1 with changes' '
	(cd repo && test_must_fail grit diff-index --quiet HEAD)
'

test_expect_success 'diff-index --quiet HEAD: no output' '
	(cd repo && test_must_fail grit diff-index --quiet HEAD >../actual) &&
	test_must_be_empty actual
'

# ---- staged modification ----

test_expect_success 'setup: stage the modification' '
	(cd repo && grit add a.txt)
'

test_expect_success 'diff-index --cached HEAD: shows staged change' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	grep "a.txt" actual &&
	grep "M" actual
'

test_expect_success 'diff-index --cached HEAD: matches git' '
	(cd repo && grit diff-index --cached HEAD >../grit_out &&
	 "$REAL_GIT" diff-index --cached HEAD >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-index HEAD (without --cached): also shows staged' '
	(cd repo && grit diff-index HEAD >../actual) &&
	grep "a.txt" actual
'

# ---- staged + worktree modifications (different files) ----

test_expect_success 'setup: modify another file in worktree only' '
	(cd repo && echo "bravo2" >b.txt)
'

test_expect_success 'diff-index HEAD: shows both staged and worktree changes' '
	(cd repo && grit diff-index HEAD >../actual) &&
	grep "a.txt" actual &&
	grep "b.txt" actual
'

test_expect_success 'diff-index HEAD: matches git' '
	(cd repo && grit diff-index HEAD >../grit_out &&
	 "$REAL_GIT" diff-index HEAD >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-index --cached HEAD: only staged file' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	grep "a.txt" actual &&
	! grep "b.txt" actual
'

# ---- new file (untracked then staged) ----

test_expect_success 'setup: add new file' '
	(cd repo && echo "new content" >new.txt && grit add new.txt)
'

test_expect_success 'diff-index --cached HEAD: new file shows A' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	grep "A" actual | grep "new.txt"
'

test_expect_success 'diff-index --cached HEAD: new file matches git' '
	(cd repo && grit diff-index --cached HEAD >../grit_out &&
	 "$REAL_GIT" diff-index --cached HEAD >../git_out) &&
	test_cmp git_out grit_out
'

# ---- deleted file ----

test_expect_success 'setup: delete a file' '
	(cd repo && "$REAL_GIT" rm c.txt)
'

test_expect_success 'diff-index --cached HEAD: deleted file shows D' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	grep "D" actual | grep "c.txt"
'

test_expect_success 'diff-index --cached HEAD: all changes match git' '
	(cd repo && grit diff-index --cached HEAD >../grit_out &&
	 "$REAL_GIT" diff-index --cached HEAD >../git_out) &&
	test_cmp git_out grit_out
'

# ---- --abbrev ----

test_expect_success 'diff-index --abbrev HEAD: abbreviates hashes' '
	(cd repo && grit diff-index --abbrev HEAD >../actual) &&
	# Full SHA is 40 chars; abbreviated should be shorter
	# Check that we do NOT see 40-char hashes followed by space
	! grep -E "[0-9a-f]{40} " actual &&
	grep "a.txt" actual
'

test_expect_success 'diff-index --abbrev=7 HEAD: matches git' '
	(cd repo && grit diff-index --abbrev=7 HEAD >../grit_out &&
	 "$REAL_GIT" diff-index --abbrev=7 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

# ---- -m flag (merge) ----

test_expect_success 'diff-index -m HEAD: shows all changes' '
	(cd repo && grit diff-index -m HEAD >../actual) &&
	grep "a.txt" actual &&
	grep "b.txt" actual
'

test_expect_success 'diff-index -m HEAD: matches git' '
	(cd repo && grit diff-index -m HEAD >../grit_out &&
	 "$REAL_GIT" diff-index -m HEAD >../git_out) &&
	test_cmp git_out grit_out
'

# ---- path filter ----

test_expect_success 'diff-index HEAD a.txt: only shows a.txt' '
	(cd repo && grit diff-index HEAD a.txt >../actual) &&
	grep "a.txt" actual &&
	! grep "b.txt" actual &&
	! grep "new.txt" actual
'

test_expect_success 'diff-index HEAD a.txt: matches git' '
	(cd repo && grit diff-index HEAD a.txt >../grit_out &&
	 "$REAL_GIT" diff-index HEAD a.txt >../git_out) &&
	test_cmp git_out grit_out
'

# ---- commit and test clean state again ----

test_expect_success 'setup: commit everything' '
	(cd repo &&
	 "$REAL_GIT" checkout -- b.txt &&
	 grit commit -m "second")
'

test_expect_success 'diff-index HEAD: clean again' '
	(cd repo && grit diff-index HEAD >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff-index --exit-code HEAD: returns 0 after commit' '
	(cd repo && grit diff-index --exit-code HEAD)
'

# ---- nested directory changes ----

test_expect_success 'setup: modify nested file' '
	(cd repo && echo "deep2" >sub/d.txt)
'

test_expect_success 'diff-index HEAD: nested worktree change detected' '
	(cd repo && grit diff-index HEAD >../actual) &&
	grep "sub/d.txt" actual
'

test_expect_success 'diff-index HEAD: nested change matches git' '
	(cd repo && grit diff-index HEAD >../grit_out &&
	 "$REAL_GIT" diff-index HEAD >../git_out) &&
	test_cmp git_out grit_out
'

test_done
