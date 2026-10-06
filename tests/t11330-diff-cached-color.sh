#!/bin/sh
# Tests for grit diff --cached with --name-only, --name-status, --stat, --numstat.

test_description='grit diff --cached: output formats (name-only, name-status, stat, numstat)'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with several files' '
	(
	"$REAL_GIT" init -b main repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&

	echo "alpha" >a.txt &&
	echo "bravo" >b.txt &&
	echo "charlie" >c.txt &&
	mkdir sub &&
	echo "delta" >sub/d.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial commit"
	)
'

###########################################################################
# Section 2: diff --cached with no staged changes
###########################################################################

test_expect_success 'diff --cached produces no output when index matches HEAD' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff --cached --name-only empty when no staged changes' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --name-only >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff --cached --name-status empty when no staged changes' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --name-status >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff --cached --stat empty when no staged changes' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --stat >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff --cached --numstat empty when no staged changes' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --numstat >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 3: diff --cached after modifying and staging a file
###########################################################################

test_expect_success 'stage a modification to a.txt' '
	(
	cd repo &&
	echo "alpha modified" >a.txt &&
	"$REAL_GIT" add a.txt
	)
'

test_expect_success 'diff --cached shows patch for staged modification' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached >out &&
	grep "diff --git" out &&
	grep "a.txt" out
	)
'

test_expect_success 'diff --cached --name-only lists modified file' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --name-only >out &&
	grep "^a.txt$" out
	)
'

test_expect_success 'diff --cached --name-status shows M for modified file' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --name-status >out &&
	grep "^M" out &&
	grep "a.txt" out
	)
'

test_expect_success 'diff --cached --stat shows stat line' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --stat >out &&
	grep "a.txt" out &&
	grep "changed" out
	)
'

test_expect_success 'diff --cached --numstat shows numeric stats' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --numstat >out &&
	grep "a.txt" out
	)
'

test_expect_success 'diff --cached --exit-code returns 1 with staged changes' '
	(
	cd repo &&
	test_expect_code 1 "$GUST_BIN" diff --cached --exit-code
	)
'

###########################################################################
# Section 4: diff --cached after staging multiple files
###########################################################################

test_expect_success 'stage modifications to b.txt and c.txt' '
	(
	cd repo &&
	echo "bravo modified" >b.txt &&
	echo "charlie modified" >c.txt &&
	"$REAL_GIT" add b.txt c.txt
	)
'

test_expect_success 'diff --cached --name-only lists all staged files' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --name-only >out &&
	grep "a.txt" out &&
	grep "b.txt" out &&
	grep "c.txt" out
	)
'

test_expect_success 'diff --cached --name-only does not list unstaged file' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --name-only >out &&
	! grep "d.txt" out
	)
'

test_expect_success 'diff --cached --numstat shows stats for all staged files' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --numstat >out &&
	test_line_count = 3 out
	)
'

###########################################################################
# Section 5: diff --cached with newly added file
###########################################################################

test_expect_success 'add a new file and stage it' '
	(
	cd repo &&
	echo "echo new file" >new.txt &&
	"$REAL_GIT" add new.txt
	)
'

test_expect_success 'diff --cached --name-status shows A for new file' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --name-status >out &&
	grep "^A" out &&
	grep "new.txt" out
	)
'

test_expect_success 'diff --cached patch shows new file header' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached >out &&
	grep "new file mode" out
	)
'

###########################################################################
# Section 6: diff --cached with deleted file
###########################################################################

test_expect_success 'delete a file and stage the deletion' '
	(
	cd repo &&
	"$REAL_GIT" rm -f c.txt
	)
'

test_expect_success 'diff --cached --name-status shows D for deleted file' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --name-status >out &&
	grep "D" out &&
	grep "c.txt" out
	)
'

test_expect_success 'diff --cached shows deleted file mode in patch' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached >out &&
	grep "deleted file mode" out
	)
'

###########################################################################
# Section 7: Commit and verify clean state
###########################################################################

test_expect_success 'commit all staged changes' '
	(
	cd repo &&
	"$REAL_GIT" commit -m "modify a, b, add new, delete c"
	)
'

test_expect_success 'diff --cached empty after commit' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff --cached --exit-code returns 0 after commit' '
	(
	cd repo &&
	test_expect_code 0 "$GUST_BIN" diff --cached --exit-code
	)
'

###########################################################################
# Section 8: diff --cached with subdirectory changes
###########################################################################

test_expect_success 'modify and stage file in subdirectory' '
	(
	cd repo &&
	echo "delta modified" >sub/d.txt &&
	"$REAL_GIT" add sub/d.txt
	)
'

test_expect_success 'diff --cached --name-only shows subdir path' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --name-only >out &&
	grep "sub/d.txt" out
	)
'

test_expect_success 'diff --cached patch shows subdir file diff' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached >out &&
	grep "sub/d.txt" out
	)
'

test_expect_success 'diff --cached --numstat for subdir file' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --numstat >out &&
	grep "sub/d.txt" out
	)
'

###########################################################################
# Section 9: diff --cached with context lines
###########################################################################

test_expect_success 'diff --cached -U0 shows no context lines' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached -U0 >out &&
	! grep "^  " out
	)
'

test_expect_success 'diff --cached -U5 works with larger context' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached -U5 >out &&
	grep "diff --git" out
	)
'

test_expect_success 'commit subdir change' '
	(
	cd repo &&
	"$REAL_GIT" commit -m "modify sub/d.txt"
	)
'

###########################################################################
# Section 10: diff --cached --quiet
###########################################################################

test_expect_success 'diff --cached --quiet returns 0 when clean' '
	(
	cd repo &&
	test_expect_code 0 "$GUST_BIN" diff --cached --quiet
	)
'

test_expect_success 'diff --cached --quiet returns 1 when staged changes exist' '
	(
	cd repo &&
	echo "more changes" >>a.txt &&
	"$REAL_GIT" add a.txt &&
	test_expect_code 1 "$GUST_BIN" diff --cached --quiet
	)
'

test_expect_success 'diff --cached --quiet produces no output' '
	(
	cd repo &&
	"$GUST_BIN" diff --cached --quiet >out 2>&1 || true &&
	test_must_be_empty out
	)
'

test_expect_success 'final cleanup' '
	(
	cd repo &&
	"$REAL_GIT" reset --hard HEAD
	)
'

test_done
