#!/bin/sh
test_description='diff --stat and --numstat with binary and text files'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
		git config user.email "t@t.com" &&
		git config user.name "T" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
		printf "line1\nline2\nline3\n" >text.txt &&
		grit add text.txt &&
		grit commit -m "initial text"
	)
'

test_expect_success 'diff --cached --stat for simple text change' '
	(cd repo &&
		printf "line1\nchanged\nline3\n" >text.txt &&
		grit add text.txt &&
		grit diff --cached --stat >../actual
	) &&
	grep "text.txt" actual &&
	grep "1 file changed" actual
'

test_expect_success 'diff --cached --numstat for simple text change' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	grep "text.txt" actual
'

test_expect_success 'commit text change and add binary file' '
	(cd repo &&
		grit commit -m "modify text" &&
		printf "\000\001\002\377" >binary.dat &&
		grit add binary.dat &&
		grit commit -m "add binary"
	)
'

test_expect_success 'diff --cached --stat for new binary' '
	(cd repo &&
		printf "\000\001\002\376" >binary.dat &&
		grit add binary.dat &&
		grit diff --cached --stat >../actual
	) &&
	grep "binary.dat" actual &&
	grep "1 file changed" actual
'

test_expect_success 'diff --cached --numstat for binary shows 0 0' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	grep "^0" actual &&
	grep "binary.dat" actual
'

test_expect_success 'diff --cached for binary file has no hunk content' '
	(cd repo && grit diff --cached >../actual) &&
	grep "diff --git a/binary.dat" actual &&
	! grep "^@@" actual
'

test_expect_success 'commit binary change' '
	(cd repo && grit commit -m "modify binary")
'

test_expect_success 'diff --cached --stat with both text and binary changes' '
	(cd repo &&
		printf "line1\nnew\nline3\n" >text.txt &&
		printf "\000\003" >binary.dat &&
		grit add text.txt binary.dat &&
		grit diff --cached --stat >../actual
	) &&
	grep "binary.dat" actual &&
	grep "text.txt" actual &&
	grep "2 files changed" actual
'

test_expect_success 'diff --cached --numstat with both text and binary' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	grep "text.txt" actual &&
	grep "binary.dat" actual
'

test_expect_success 'diff --cached --name-only with both types' '
	(cd repo && grit diff --cached --name-only >../actual) &&
	sort actual >actual_sorted &&
	printf "binary.dat\ntext.txt\n" >expect &&
	test_cmp expect actual_sorted
'

test_expect_success 'diff --cached --name-status with both types' '
	(cd repo && grit diff --cached --name-status >../actual) &&
	grep "M.*binary.dat" actual &&
	grep "M.*text.txt" actual
'

test_expect_success 'commit mixed changes' '
	(cd repo && grit commit -m "modify both")
'

test_expect_success 'diff-tree --stat between commits with text change' '
	(cd repo && grit diff-tree --stat -r HEAD~1 HEAD >../actual) &&
	grep "2 files changed" actual
'

test_expect_success 'diff --cached --stat for adding new text file' '
	(cd repo &&
		printf "a\nb\nc\nd\ne\n" >multi.txt &&
		grit add multi.txt &&
		grit diff --cached --stat >../actual
	) &&
	grep "multi.txt" actual &&
	grep "5 insertion" actual
'

test_expect_success 'diff --cached --numstat for new text file' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	grep "5.*0.*multi.txt" actual
'

test_expect_success 'commit and modify multiple lines' '
	(cd repo &&
		grit commit -m "add multi" &&
		printf "A\nb\nC\nd\nE\n" >multi.txt &&
		grit add multi.txt &&
		grit diff --cached --stat >../actual
	) &&
	grep "multi.txt" actual
'

test_expect_success 'diff --cached --numstat counts insertions and deletions' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	grep "multi.txt" actual
'

test_expect_success 'commit multiline change' '
	(cd repo && grit commit -m "modify multi")
'

test_expect_success 'diff --cached --stat for deleting a file' '
	(cd repo &&
		grit rm binary.dat &&
		grit diff --cached --stat >../actual
	) &&
	grep "binary.dat" actual &&
	grep "1 file changed" actual
'

test_expect_success 'diff --cached --name-status shows D for deletion' '
	(cd repo && grit diff --cached --name-status >../actual) &&
	printf "D\tbinary.dat\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'commit deletion' '
	(cd repo && grit commit -m "delete binary")
'

test_expect_success 'diff --cached --stat for new binary file' '
	(cd repo &&
		printf "\377\376\375" >new_bin.dat &&
		grit add new_bin.dat &&
		grit diff --cached --stat >../actual
	) &&
	grep "new_bin.dat" actual
'

test_expect_success 'diff --cached --numstat for new binary' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	grep "new_bin.dat" actual
'

test_expect_success 'commit new binary' '
	(cd repo && grit commit -m "add new binary")
'

test_expect_success 'diff --cached for empty file' '
	(cd repo &&
		: >empty.txt &&
		grit add empty.txt &&
		grit diff --cached --stat >../actual
	) &&
	grep "empty.txt" actual
'

test_expect_success 'diff --cached --numstat for empty file' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	grep "0.*0.*empty.txt" actual
'

test_expect_success 'commit and add content to empty file' '
	(cd repo &&
		grit commit -m "add empty" &&
		echo "content" >empty.txt &&
		grit add empty.txt &&
		grit diff --cached --stat >../actual
	) &&
	grep "empty.txt" actual &&
	grep "1 insertion" actual
'

test_expect_success 'diff --cached --numstat for content added to empty' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	grep "1.*0.*empty.txt" actual
'

test_expect_success 'commit and test stat with large text changes' '
	(cd repo &&
		grit commit -m "fill empty" &&
		seq 1 20 >numbers.txt &&
		grit add numbers.txt &&
		grit commit -m "add numbers" &&
		seq 1 10 >numbers.txt &&
		seq 15 25 >>numbers.txt &&
		grit add numbers.txt &&
		grit diff --cached --stat >../actual
	) &&
	grep "numbers.txt" actual
'

test_expect_success 'diff --cached --numstat for large text changes' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	grep "numbers.txt" actual
'

test_expect_success 'diff --cached --stat with multiple files of varying sizes' '
	(cd repo &&
		grit commit -m "modify numbers" &&
		echo "a" >small.txt &&
		seq 1 50 >large.txt &&
		grit add small.txt large.txt &&
		grit diff --cached --stat >../actual
	) &&
	grep "small.txt" actual &&
	grep "large.txt" actual &&
	grep "2 files changed" actual
'

test_expect_success 'final commit' '
	(cd repo && grit commit -m "final")
'

test_done
