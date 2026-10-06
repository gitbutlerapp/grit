#!/bin/sh
test_description='diff --cached behavior including empty tree and initial commit scenarios'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
		git config user.email "t@t.com" &&
		git config user.name "T"
	)
'

test_expect_success 'diff --cached with no commits and no index is empty' '
	(cd repo && grit diff --cached >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff --cached shows new file before first commit' '
	(cd repo &&
		echo "hello" >file.txt &&
		grit add file.txt &&
		grit diff --cached >../actual
	) &&
	grep "new file mode 100644" actual &&
	grep "+hello" actual
'

test_expect_success 'diff --cached --name-only before first commit' '
	(cd repo && grit diff --cached --name-only >../actual) &&
	echo "file.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached --name-status before first commit' '
	(cd repo && grit diff --cached --name-status >../actual) &&
	printf "A\tfile.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached --stat before first commit' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "file.txt" actual &&
	grep "1 file changed" actual
'

test_expect_success 'diff --cached --numstat before first commit' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	grep "file.txt" actual
'

test_expect_success 'diff --cached --exit-code fails before first commit' '
	(cd repo && test_must_fail grit diff --cached --exit-code)
'

test_expect_success 'add multiple files before first commit' '
	(cd repo &&
		echo "world" >file2.txt &&
		echo "script" >run.sh &&
		chmod 755 run.sh &&
		grit add file2.txt run.sh
	)
'

test_expect_success 'diff --cached shows all files before first commit' '
	(cd repo && grit diff --cached --name-only >../actual) &&
	sort actual >actual_sorted &&
	printf "file.txt\nfile2.txt\nrun.sh\n" >expect &&
	test_cmp expect actual_sorted
'

test_expect_success 'diff --cached --stat shows all files' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "3 files changed" actual
'

test_expect_success 'executable file mode shows in diff --cached before first commit' '
	(cd repo && grit diff --cached >../actual) &&
	grep "new file mode 100755" actual
'

test_expect_success 'first commit' '
	(cd repo && grit commit -m "initial")
'

test_expect_success 'diff --cached is empty after commit with clean index' '
	(cd repo && grit diff --cached >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff --cached --exit-code succeeds with clean index' '
	(cd repo && grit diff --cached --exit-code)
'

test_expect_success 'stage a modification' '
	(cd repo &&
		echo "changed" >file.txt &&
		grit add file.txt
	)
'

test_expect_success 'diff --cached shows modification' '
	(cd repo && grit diff --cached >../actual) &&
	grep "\-hello" actual &&
	grep "+changed" actual
'

test_expect_success 'diff --cached --name-status shows M' '
	(cd repo && grit diff --cached --name-status >../actual) &&
	printf "M\tfile.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'stage a deletion' '
	(cd repo && grit rm file2.txt)
'

test_expect_success 'diff --cached shows deletion' '
	(cd repo && grit diff --cached --name-status >../actual) &&
	grep "M" actual &&
	grep "D" actual
'

test_expect_success 'diff --cached --stat shows deletion' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "file2.txt" actual &&
	grep "2 files changed" actual
'

test_expect_success 'commit modifications and deletion' '
	(cd repo && grit commit -m "modify and delete")
'

test_expect_success 'stage new file after commits exist' '
	(cd repo &&
		echo "new content" >new.txt &&
		grit add new.txt &&
		grit diff --cached --name-status >../actual
	) &&
	printf "A\tnew.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached shows new file content' '
	(cd repo && grit diff --cached >../actual) &&
	grep "new file mode 100644" actual &&
	grep "+new content" actual
'

test_expect_success 'commit new file' '
	(cd repo && grit commit -m "add new file")
'

test_expect_success 'stage empty file' '
	(cd repo &&
		: >empty.txt &&
		grit add empty.txt &&
		grit diff --cached >../actual
	) &&
	grep "new file mode 100644" actual
'

test_expect_success 'diff --cached --stat for empty file' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "empty.txt" actual
'

test_expect_success 'commit empty file and modify it' '
	(cd repo &&
		grit commit -m "add empty" &&
		echo "no longer empty" >empty.txt &&
		grit add empty.txt &&
		grit diff --cached >../actual
	) &&
	grep "+no longer empty" actual
'

test_expect_success 'stage multiple changes at once' '
	(cd repo &&
		echo "updated" >file.txt &&
		echo "also new" >another.txt &&
		grit add file.txt another.txt &&
		grit diff --cached --name-only >../actual
	) &&
	sort actual >actual_sorted &&
	printf "another.txt\nempty.txt\nfile.txt\n" >expect &&
	test_cmp expect actual_sorted
'

test_expect_success 'diff --cached --stat shows all staged changes' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "3 files changed" actual
'

test_expect_success 'diff --cached --numstat for multiple changes' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	grep "file.txt" actual &&
	grep "another.txt" actual &&
	grep "empty.txt" actual
'

test_expect_success 'commit and stage mode change' '
	(cd repo &&
		grit commit -m "multiple changes" &&
		chmod 755 file.txt &&
		grit add file.txt &&
		grit diff --cached >../actual
	) &&
	grep "old mode 100644" actual &&
	grep "new mode 100755" actual
'

test_expect_success 'diff --cached --quiet with staged changes' '
	(cd repo &&
		test_must_fail grit diff --cached --quiet >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'commit mode change and verify clean' '
	(cd repo &&
		grit commit -m "chmod" &&
		grit diff --cached >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'diff --cached after unstaging shows nothing' '
	(cd repo &&
		echo "temp" >temp.txt &&
		grit add temp.txt &&
		git reset HEAD temp.txt &&
		grit diff --cached >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'diff --cached with only whitespace changes' '
	(cd repo &&
		printf "updated\n\n" >file.txt &&
		grit add file.txt &&
		grit diff --cached >../actual
	) &&
	grep "file.txt" actual
'

test_expect_success 'final commit' '
	(cd repo && grit commit -m "final")
'

test_done
