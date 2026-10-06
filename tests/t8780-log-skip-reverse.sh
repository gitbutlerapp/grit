#!/bin/sh
# Tests for log --skip, --reverse, --max-count, --oneline, --format, --graph.

test_description='log skip and reverse options'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

OUT="$TRASH_DIRECTORY/output"
mkdir -p "$OUT"

# -- setup: 10 commits --------------------------------------------------------

test_expect_success 'setup repository with 10 commits' '
	(
	git init repo &&
	cd repo &&
	for i in 1 2 3 4 5 6 7 8 9 10; do
		echo "content $i" >file.txt &&
		git add file.txt &&
		git commit -m "commit $i" || return 1
	done
	)
'

# -- basic log -----------------------------------------------------------------

test_expect_success 'log shows all 10 commits' '
	(
	cd repo &&
	git log --oneline >"$OUT/l1" &&
	test_line_count = 10 "$OUT/l1"
	)
'

test_expect_success 'log default shows most recent first' '
	(
	cd repo &&
	git log --oneline >"$OUT/l2" &&
	head -1 "$OUT/l2" | grep "commit 10"
	)
'

test_expect_success 'log last line is oldest commit' '
	(
	cd repo &&
	git log --oneline >"$OUT/l3" &&
	tail -1 "$OUT/l3" | grep "commit 1"
	)
'

# -- --max-count / -n ---------------------------------------------------------

test_expect_success 'log -n 3 shows only 3 commits' '
	(
	cd repo &&
	git log --oneline -n 3 >"$OUT/l4" &&
	test_line_count = 3 "$OUT/l4"
	)
'

test_expect_success 'log --max-count=5 shows 5 commits' '
	(
	cd repo &&
	git log --oneline --max-count=5 >"$OUT/l5" &&
	test_line_count = 5 "$OUT/l5"
	)
'

test_expect_success 'log -n 1 shows only HEAD commit' '
	(
	cd repo &&
	git log --oneline -n 1 >"$OUT/l6" &&
	test_line_count = 1 "$OUT/l6" &&
	grep "commit 10" "$OUT/l6"
	)
'

test_expect_success 'log -n 1 shows exactly one commit' '
	(
	cd repo &&
	git log --oneline -n 1 >"$OUT/l7" &&
	test_line_count = 1 "$OUT/l7"
	)
'

# -- --skip --------------------------------------------------------------------

test_expect_success 'log --skip=2 skips 2 most recent' '
	(
	cd repo &&
	git log --oneline --skip=2 >"$OUT/l8" &&
	test_line_count = 8 "$OUT/l8" &&
	head -1 "$OUT/l8" | grep "commit 8"
	)
'

test_expect_success 'log --skip=9 shows only first commit' '
	(
	cd repo &&
	git log --oneline --skip=9 >"$OUT/l9" &&
	test_line_count = 1 "$OUT/l9" &&
	grep "commit 1" "$OUT/l9"
	)
'

test_expect_success 'log --skip=10 shows nothing' '
	(
	cd repo &&
	git log --oneline --skip=10 >"$OUT/l10" &&
	test_line_count = 0 "$OUT/l10"
	)
'

test_expect_success 'log --skip=100 shows nothing for large skip' '
	(
	cd repo &&
	git log --oneline --skip=100 >"$OUT/l11" &&
	test_line_count = 0 "$OUT/l11"
	)
'

# -- --skip + -n combined ------------------------------------------------------

test_expect_success 'log --skip=2 -n 3 shows commits 8,7,6' '
	(
	cd repo &&
	git log --oneline --skip=2 -n 3 >"$OUT/l12" &&
	test_line_count = 3 "$OUT/l12" &&
	head -1 "$OUT/l12" | grep "commit 8" &&
	tail -1 "$OUT/l12" | grep "commit 6"
	)
'

test_expect_success 'log --skip=8 -n 5 shows only 2 remaining commits' '
	(
	cd repo &&
	git log --oneline --skip=8 -n 5 >"$OUT/l13" &&
	test_line_count = 2 "$OUT/l13"
	)
'

# -- --reverse -----------------------------------------------------------------

test_expect_success 'log --reverse shows oldest first' '
	(
	cd repo &&
	git log --oneline --reverse >"$OUT/l14" &&
	head -1 "$OUT/l14" | grep "commit 1" &&
	tail -1 "$OUT/l14" | grep "commit 10"
	)
'

test_expect_success 'log --reverse shows all 10 commits' '
	(
	cd repo &&
	git log --oneline --reverse >"$OUT/l15" &&
	test_line_count = 10 "$OUT/l15"
	)
'

test_expect_success 'log --reverse -n 3 shows 3 commits in reversed order' '
	(
	cd repo &&
	git log --oneline --reverse -n 3 >"$OUT/l16" &&
	test_line_count = 3 "$OUT/l16" &&
	head -1 "$OUT/l16" | grep "commit 8" &&
	tail -1 "$OUT/l16" | grep "commit 10"
	)
'

test_expect_success 'log --reverse --skip=7 skips from reversed output' '
	(
	cd repo &&
	git log --oneline --reverse --skip=7 >"$OUT/l17" &&
	test_line_count = 3 "$OUT/l17" &&
	head -1 "$OUT/l17" | grep "commit 1" &&
	tail -1 "$OUT/l17" | grep "commit 3"
	)
'

# -- --oneline format ----------------------------------------------------------

test_expect_success 'log --oneline produces short hash and subject' '
	(
	cd repo &&
	git log --oneline -n 1 >"$OUT/l18" &&
	line=$(cat "$OUT/l18") &&
	hash=$(echo "$line" | cut -d" " -f1) &&
	test ${#hash} -le 12 &&
	echo "$line" | grep "commit 10"
	)
'

# -- --format options ----------------------------------------------------------

test_expect_success 'log --format=%H shows full commit hashes' '
	(
	cd repo &&
	git log --format=%H -n 1 >"$OUT/l19" &&
	hash=$(cat "$OUT/l19") &&
	test ${#hash} = 40
	)
'

test_expect_success 'log --format=%s shows only subjects' '
	(
	cd repo &&
	git log --format=%s >"$OUT/l20" &&
	test_line_count = 10 "$OUT/l20" &&
	head -1 "$OUT/l20" | grep "commit 10" &&
	tail -1 "$OUT/l20" | grep "commit 1"
	)
'

test_expect_success 'log --format=%an shows author name' '
	(
	cd repo &&
	git log --format=%an -n 1 >"$OUT/l21" &&
	grep "Test Author" "$OUT/l21"
	)
'

test_expect_success 'log --format=%ae shows author email' '
	(
	cd repo &&
	git log --format=%ae -n 1 >"$OUT/l22" &&
	grep "author@test.com" "$OUT/l22"
	)
'

test_expect_success 'log --format with multiple placeholders' '
	(
	cd repo &&
	git log --format="%H %s" -n 1 >"$OUT/l23" &&
	grep "commit 10" "$OUT/l23"
	)
'

# -- log from specific ref -----------------------------------------------------

test_expect_success 'log from HEAD shows same as default' '
	(
	cd repo &&
	git log --oneline HEAD >"$OUT/l24" &&
	test_line_count = 10 "$OUT/l24"
	)
'

test_expect_success 'log from branch name shows same output' '
	(
	cd repo &&
	git log --oneline master >"$OUT/l25" &&
	test_line_count = 10 "$OUT/l25"
	)
'

# -- --format=%h for short hash ------------------------------------------------

test_expect_success 'log --format=%h shows abbreviated hash' '
	(
	cd repo &&
	git log --format=%h -n 1 >"$OUT/l26" &&
	hash=$(cat "$OUT/l26") &&
	test ${#hash} -le 12
	)
'

# -- log on different branch ---------------------------------------------------

test_expect_success 'setup: create branch with extra commit' '
	(
	cd repo &&
	git checkout -b side &&
	echo "side content" >side.txt &&
	git add side.txt &&
	git commit -m "side commit"
	)
'

test_expect_success 'log on side branch includes side commit' '
	(
	cd repo &&
	git log --oneline >"$OUT/l27" &&
	grep "side commit" "$OUT/l27"
	)
'

test_expect_success 'log on master does not include side commit' '
	(
	cd repo &&
	git log --oneline master >"$OUT/l28" &&
	! grep "side commit" "$OUT/l28"
	)
'

# -- --decorate ----------------------------------------------------------------

test_expect_success 'log --decorate shows ref names' '
	(
	cd repo &&
	git log --oneline --decorate -n 1 >"$OUT/l29" &&
	grep "HEAD" "$OUT/l29"
	)
'

test_expect_success 'log --no-decorate hides ref names' '
	(
	cd repo &&
	git log --oneline --no-decorate -n 1 >"$OUT/l30" &&
	! grep "HEAD" "$OUT/l30"
	)
'

test_done
