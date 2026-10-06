#!/bin/sh
# Test diff --stat and --numstat options across various scenarios
# including commit ranges, staged changes, working tree changes,
# file additions, deletions, modifications, renames, and binary files.

test_description='grit diff --stat and --numstat'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---- setup ----

test_expect_success 'setup: initial commit with multiple files' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "line1" >file1.txt &&
	echo "line1" >file2.txt &&
	echo "line1" >file3.txt &&
	mkdir -p dir &&
	echo "nested" >dir/nested.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "initial commit"
	)
'

test_expect_success 'setup: second commit with modifications' '
	(
	cd repo &&
	echo "line2" >>file1.txt &&
	echo "line2" >>file2.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "add line2 to file1 and file2"
	)
'

test_expect_success 'setup: third commit with new file and deletion' '
	(
	cd repo &&
	echo "new file content" >file4.txt &&
	rm file3.txt &&
	grit add file4.txt &&
	grit add file3.txt &&
	test_tick &&
	grit commit -m "add file4 remove file3"
	)
'

# ---- diff --stat between commits ----

test_expect_success 'diff --stat HEAD~1 HEAD shows changed files' '
	(
	cd repo &&
	grit diff --stat HEAD~1 HEAD >actual &&
	grep "file4.txt" actual &&
	grep "file3.txt" actual
	)
'

test_expect_success 'diff --stat HEAD~1 HEAD shows summary line' '
	(
	cd repo &&
	grit diff --stat HEAD~1 HEAD >actual &&
	grep "file.*changed" actual
	)
'

test_expect_success 'diff --stat HEAD~2 HEAD~1 shows file1 and file2' '
	(
	cd repo &&
	grit diff --stat HEAD~2 HEAD~1 >actual &&
	grep "file1.txt" actual &&
	grep "file2.txt" actual
	)
'

test_expect_success 'diff --stat HEAD~2 HEAD shows all changes' '
	(
	cd repo &&
	grit diff --stat HEAD~2 HEAD >actual &&
	grep "file1.txt" actual &&
	grep "file2.txt" actual &&
	grep "file3.txt" actual &&
	grep "file4.txt" actual
	)
'

test_expect_success 'diff --stat shows insertion marker (+)' '
	(
	cd repo &&
	grit diff --stat HEAD~2 HEAD~1 >actual &&
	grep "+" actual
	)
'

test_expect_success 'diff --stat shows deletion marker (-)' '
	(
	cd repo &&
	grit diff --stat HEAD~1 HEAD >actual &&
	grep "\-" actual
	)
'

# ---- diff --numstat between commits ----

test_expect_success 'diff --numstat HEAD~1 HEAD outputs tab-separated values' '
	(
	cd repo &&
	grit diff --numstat HEAD~1 HEAD >actual &&
	# numstat format: added<TAB>deleted<TAB>filename
	grep "	" actual
	)
'

test_expect_success 'diff --numstat HEAD~2 HEAD~1 shows correct additions for file1' '
	(
	cd repo &&
	grit diff --numstat HEAD~2 HEAD~1 >actual &&
	grep "^1	0	file1.txt$" actual
	)
'

test_expect_success 'diff --numstat HEAD~2 HEAD~1 shows correct additions for file2' '
	(
	cd repo &&
	grit diff --numstat HEAD~2 HEAD~1 >actual &&
	grep "^1	0	file2.txt$" actual
	)
'

test_expect_success 'diff --numstat HEAD~1 HEAD shows addition of file4' '
	(
	cd repo &&
	grit diff --numstat HEAD~1 HEAD >actual &&
	grep "^1	0	file4.txt$" actual
	)
'

test_expect_success 'diff --numstat HEAD~1 HEAD shows deletion of file3' '
	(
	cd repo &&
	grit diff --numstat HEAD~1 HEAD >actual &&
	grep "^0	1	file3.txt$" actual
	)
'

test_expect_success 'diff --numstat file count matches diff --stat file count' '
	(
	cd repo &&
	grit diff --numstat HEAD~2 HEAD >numstat_out &&
	grit diff --stat HEAD~2 HEAD >stat_out &&
	num_files=$(wc -l <numstat_out | tr -d " ") &&
	# stat has a summary line at the end
	stat_lines=$(wc -l <stat_out | tr -d " ") &&
	stat_files=$(($stat_lines - 1)) &&
	test "$num_files" = "$stat_files"
	)
'

# ---- diff --stat / --numstat for staged changes ----

test_expect_success 'setup: stage a modification' '
	(
	cd repo &&
	echo "extra" >>file1.txt &&
	grit add file1.txt
	)
'

test_expect_success 'diff --cached --stat shows staged file' '
	(
	cd repo &&
	grit diff --cached --stat >actual &&
	grep "file1.txt" actual
	)
'

test_expect_success 'diff --cached --numstat shows staged addition' '
	(
	cd repo &&
	grit diff --cached --numstat >actual &&
	grep "^1	0	file1.txt$" actual
	)
'

# ---- diff --stat / --numstat for unstaged changes ----

test_expect_success 'setup: make unstaged change' '
	(
	cd repo &&
	echo "unstaged line" >>file2.txt
	)
'

test_expect_success 'diff --stat shows unstaged modification' '
	(
	cd repo &&
	grit diff --stat >actual &&
	grep "file2.txt" actual
	)
'

test_expect_success 'diff --numstat shows unstaged modification count' '
	(
	cd repo &&
	grit diff --numstat >actual &&
	grep "file2.txt" actual
	)
'

test_expect_success 'diff --stat does not show staged file in unstaged diff' '
	(
	cd repo &&
	grit diff --stat >actual &&
	! grep "file1.txt" actual
	)
'

# ---- diff --stat with no changes ----

test_expect_success 'diff --stat between identical commits shows nothing' '
	(
	cd repo &&
	grit diff --stat HEAD HEAD >actual &&
	test ! -s actual
	)
'

test_expect_success 'diff --numstat between identical commits shows nothing' '
	(
	cd repo &&
	grit diff --numstat HEAD HEAD >actual &&
	test ! -s actual
	)
'

# ---- edge cases ----

test_expect_success 'setup: commit with multiple line changes' '
	(
	cd repo &&
	grit checkout -- file2.txt &&
	grit commit -m "staged extra line" &&
	for i in 1 2 3 4 5 6 7 8 9 10; do
		echo "bulk line $i" >>file1.txt
	done &&
	grit add file1.txt &&
	test_tick &&
	grit commit -m "bulk additions"
	)
'

test_expect_success 'diff --numstat shows correct count for bulk addition' '
	(
	cd repo &&
	grit diff --numstat HEAD~1 HEAD >actual &&
	grep "^10	0	file1.txt$" actual
	)
'

test_expect_success 'diff --stat shows bar graph for bulk changes' '
	(
	cd repo &&
	grit diff --stat HEAD~1 HEAD >actual &&
	grep "file1.txt" actual | grep "+" 
	)
'

test_expect_success 'diff --stat summary shows correct total' '
	(
	cd repo &&
	grit diff --stat HEAD~1 HEAD >actual &&
	grep "1 file changed" actual &&
	grep "10 insertion" actual
	)
'

test_expect_success 'diff --numstat across full range' '
	(
	cd repo &&
	grit diff --numstat HEAD~4 HEAD >actual &&
	line_count=$(wc -l <actual | tr -d " ") &&
	test "$line_count" -ge 3
	)
'

# ---- nested directory files ----

test_expect_success 'setup: modify nested file' '
	(
	cd repo &&
	echo "more nested content" >>dir/nested.txt &&
	grit add dir/nested.txt &&
	test_tick &&
	grit commit -m "update nested file"
	)
'

test_expect_success 'diff --stat shows full path for nested file' '
	(
	cd repo &&
	grit diff --stat HEAD~1 HEAD >actual &&
	grep "dir/nested.txt" actual
	)
'

test_expect_success 'diff --numstat shows full path for nested file' '
	(
	cd repo &&
	grit diff --numstat HEAD~1 HEAD >actual &&
	grep "dir/nested.txt" actual
	)
'

test_expect_success 'diff --numstat nested file has correct counts' '
	(
	cd repo &&
	grit diff --numstat HEAD~1 HEAD >actual &&
	grep "^1	0	dir/nested.txt$" actual
	)
'

# ---- path-limited stat ----

test_expect_success 'diff --stat with path filter limits output' '
	(
	cd repo &&
	grit diff --stat HEAD~5 HEAD -- file1.txt >actual &&
	grep "file1.txt" actual &&
	! grep "file2.txt" actual
	)
'

test_expect_success 'diff --numstat with path filter limits output' '
	(
	cd repo &&
	grit diff --numstat HEAD~5 HEAD -- file1.txt >actual &&
	grep "file1.txt" actual &&
	! grep "file2.txt" actual
	)
'

test_done
