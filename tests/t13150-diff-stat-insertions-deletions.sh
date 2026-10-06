#!/bin/sh
# Tests for 'grit diff --stat' and '--numstat' insertion/deletion counting.
# Focuses on staged (--cached) diffs where insertion/deletion counts
# are computed correctly, plus basic working tree diff stat behavior.

test_description='grit diff stat insertions and deletions'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	printf "line1\nline2\nline3\nline4\nline5\n" >five.txt &&
	printf "aaa\nbbb\nccc\n" >three.txt &&
	echo solo >solo.txt &&
	$REAL_GIT add . &&
	$REAL_GIT commit -m "initial"
	)
'

# --- cached numstat for single line deletion ---

test_expect_success 'cached numstat: single line deletion' '
	(cd repo && printf "line1\nline2\nline4\nline5\n" >five.txt &&
	 grit add five.txt &&
	 grit diff --cached --numstat >../actual) &&
	echo "0	1	five.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'cached stat: 1 deletion' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "1 file changed" actual &&
	grep "1 deletion" actual
'

test_expect_success 'commit and continue' '
	(cd repo && grit commit -m "remove line3")
'

# --- cached numstat for multiple deletions ---

test_expect_success 'cached numstat: multiple line deletion' '
	(cd repo && printf "line1\nline5\n" >five.txt &&
	 grit add five.txt &&
	 grit diff --cached --numstat >../actual) &&
	echo "0	2	five.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'cached stat: 2 deletions' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "2 deletion" actual
'

test_expect_success 'commit and continue' '
	(cd repo && grit commit -m "keep only 1 and 5")
'

# --- cached numstat for additions ---

test_expect_success 'cached numstat: adding lines' '
	(cd repo && printf "line1\nline5\nnew1\nnew2\nnew3\n" >five.txt &&
	 grit add five.txt &&
	 grit diff --cached --numstat >../actual) &&
	echo "3	0	five.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'cached stat: 3 insertions' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "3 insertion" actual
'

test_expect_success 'commit and continue' '
	(cd repo && grit commit -m "add new lines")
'

# --- cached numstat for replacements ---

test_expect_success 'cached numstat: replacing lines (insert + delete)' '
	(cd repo && printf "line1\nline5\nrep1\nrep2\n" >five.txt &&
	 grit add five.txt &&
	 grit diff --cached --numstat >../actual) &&
	echo "2	3	five.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'cached stat: both insertions and deletions' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "insertion" actual &&
	grep "deletion" actual
'

test_expect_success 'commit and continue' '
	(cd repo && grit commit -m "replace lines")
'

# --- multiple files ---

test_expect_success 'cached numstat: multiple files changed' '
	(cd repo && echo replaced >three.txt && grit add three.txt &&
	 echo extra >>solo.txt && grit add solo.txt &&
	 grit diff --cached --numstat >../actual) &&
	grep "solo.txt" actual &&
	grep "three.txt" actual
'

test_expect_success 'cached stat: counts all files' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "2 files changed" actual
'

test_expect_success 'cached numstat: line count per file' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	wc -l <actual >count &&
	echo "2" >expect_count &&
	test_cmp expect_count count
'

test_expect_success 'commit and continue' '
	(cd repo && grit commit -m "modify two files")
'

# --- staged deletion ---

test_expect_success 'cached numstat: file deletion' '
	(cd repo && $REAL_GIT rm solo.txt &&
	 grit diff --cached --numstat >../actual) &&
	echo "0	2	solo.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'cached stat: deletion of file' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "solo.txt" actual &&
	grep "2 deletion" actual
'

test_expect_success 'commit and continue' '
	(cd repo && grit commit -m "remove solo")
'

# --- staged new file ---

test_expect_success 'cached numstat: new file' '
	(cd repo && printf "x\ny\nz\n" >new.txt && grit add new.txt &&
	 grit diff --cached --numstat >../actual) &&
	echo "3	0	new.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'cached stat: new file insertions' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "new.txt" actual &&
	grep "3 insertion" actual
'

test_expect_success 'commit and continue' '
	(cd repo && grit commit -m "add new")
'

# --- empty file ---

test_expect_success 'cached numstat: empty file has 0/0' '
	(cd repo && : >empty.txt && grit add empty.txt &&
	 grit diff --cached --numstat >../actual) &&
	echo "0	0	empty.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'cached stat: empty file shows 0' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "empty.txt" actual &&
	grep "0" actual
'

test_expect_success 'commit and continue' '
	(cd repo && grit commit -m "add empty")
'

# --- emptying a file ---

test_expect_success 'cached numstat: emptying a file' '
	(cd repo && : >three.txt && grit add three.txt &&
	 grit diff --cached --numstat >../actual) &&
	echo "0	1	three.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'cached stat: file emptied' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "three.txt" actual &&
	grep "1 deletion" actual
'

test_expect_success 'commit and continue' '
	(cd repo && grit commit -m "empty three")
'

# --- large change counts ---

test_expect_success 'cached numstat: 50 line file added' '
	(cd repo && seq 1 50 >big.txt && grit add big.txt &&
	 grit diff --cached --numstat >../actual) &&
	echo "50	0	big.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'cached stat: 50 insertions' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "50 insertion" actual
'

test_expect_success 'commit and continue' '
	(cd repo && grit commit -m "add big")
'

test_expect_success 'cached numstat: deleting all 50 lines' '
	(cd repo && : >big.txt && grit add big.txt &&
	 grit diff --cached --numstat >../actual) &&
	echo "0	50	big.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'cached stat: 50 deletions' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "50 deletion" actual
'

test_expect_success 'restore big file' '
	(cd repo && $REAL_GIT checkout -- .)
'

# --- combined scenario ---

test_expect_success 'cached numstat: mixed staged changes' '
	(cd repo && echo x >brand_new.txt && grit add brand_new.txt &&
	 $REAL_GIT rm empty.txt &&
	 seq 1 5 >big.txt && grit add big.txt &&
	 grit diff --cached --numstat >../actual) &&
	wc -l <actual >count &&
	echo "3" >expect_count &&
	test_cmp expect_count count
'

test_expect_success 'cached stat: mixed changes file count' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "3 files changed" actual
'

# --- working tree stat ---

test_expect_success 'commit mixed changes for clean slate' '
	(cd repo && grit commit -m "mixed changes")
'

test_expect_success 'working tree numstat shows changes' '
	(cd repo && echo changed >five.txt &&
	 grit diff --numstat >../actual) &&
	grep "five.txt" actual
'

test_expect_success 'working tree stat shows file name' '
	(cd repo && grit diff --stat >../actual) &&
	grep "five.txt" actual
'

test_expect_success 'working tree name-only lists changed file' '
	(cd repo && grit diff --name-only >../actual) &&
	echo "five.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'working tree name-status shows M' '
	(cd repo && grit diff --name-status >../actual) &&
	printf "M\tfive.txt\n" >expect &&
	test_cmp expect actual
'

test_done
