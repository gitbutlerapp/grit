#!/bin/sh
test_description='status with ignored/untracked files and check-ignore'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
		git config user.email "t@t.com" &&
		git config user.name "T" &&
		echo "hello" >tracked.txt &&
		printf "*.log\nbuild/\n" >.gitignore &&
		grit add tracked.txt .gitignore &&
		grit commit -m "initial"
	)
'

test_expect_success 'status --short shows clean repo' '
	(cd repo && grit status --short >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'status --porcelain shows branch' '
	(cd repo && grit status --porcelain -b >../actual) &&
	grep "## main" actual
'

test_expect_success 'check-ignore identifies ignored file' '
	(cd repo &&
		echo "test" >test.log &&
		grit check-ignore test.log >../actual
	) &&
	echo "test.log" >expect &&
	test_cmp expect actual
'

test_expect_success 'check-ignore -v shows rule source' '
	(cd repo && grit check-ignore -v test.log >../actual) &&
	grep ".gitignore" actual &&
	grep "\\*.log" actual &&
	grep "test.log" actual
'

test_expect_success 'check-ignore exits 1 for non-ignored file' '
	(cd repo && test_must_fail grit check-ignore tracked.txt)
'

test_expect_success 'check-ignore -v --non-matching shows empty source for non-ignored' '
	(cd repo && grit check-ignore -v --non-matching tracked.txt >../actual 2>&1 || true) &&
	grep "tracked.txt" actual
'

test_expect_success 'check-ignore with directory pattern' '
	(cd repo &&
		mkdir -p build &&
		echo "artifact" >build/out.bin &&
		grit check-ignore build/ >../actual
	) &&
	echo "build/" >expect &&
	test_cmp expect actual
'

test_expect_success 'check-ignore -v for directory pattern' '
	(cd repo && grit check-ignore -v build/ >../actual) &&
	grep ".gitignore" actual &&
	grep "build/" actual
'

test_expect_success 'check-ignore for file inside ignored directory' '
	(cd repo && grit check-ignore build/out.bin >../actual) &&
	echo "build/out.bin" >expect &&
	test_cmp expect actual
'

test_expect_success 'check-ignore with multiple arguments' '
	(cd repo && grit check-ignore test.log tracked.txt build/ >../actual) &&
	grep "test.log" actual &&
	grep "build/" actual &&
	! grep "tracked.txt" actual
'

test_expect_success 'check-ignore --stdin reads from stdin' '
	(cd repo && printf "test.log\ntracked.txt\nbuild/\n" | grit check-ignore --stdin >../actual) &&
	grep "test.log" actual &&
	grep "build/" actual &&
	! grep "tracked.txt" actual
'

test_expect_success 'status --short with untracked file' '
	(cd repo &&
		echo "new" >untracked.txt &&
		grit status --short >../actual
	) &&
	grep "??" actual &&
	grep "untracked.txt" actual
'

test_expect_success 'status --porcelain with untracked file' '
	(cd repo && grit status --porcelain >../actual) &&
	grep "?? untracked.txt" actual
'

test_expect_success 'status -u no hides untracked files' '
	(cd repo && grit status -u no --short >../actual) &&
	! grep "untracked.txt" actual &&
	! grep "test.log" actual
'

test_expect_success 'status --short with modification' '
	(cd repo &&
		echo "modified" >tracked.txt &&
		grit status --short >../actual
	) &&
	grep "M tracked.txt" actual
'

test_expect_success 'status --short with staged modification' '
	(cd repo &&
		grit add tracked.txt &&
		grit status --short >../actual
	) &&
	grep "^M" actual &&
	grep "tracked.txt" actual
'

test_expect_success 'status --short shows MM for staged + unstaged' '
	(cd repo &&
		echo "more changes" >tracked.txt &&
		grit status --short >../actual
	) &&
	grep "MM tracked.txt" actual
'

test_expect_success 'status long format shows sections' '
	(cd repo && grit status >../actual) &&
	grep "Changes to be committed" actual &&
	grep "Changes not staged for commit" actual &&
	grep "Untracked files" actual
'

test_expect_success 'status --short --branch shows branch name' '
	(cd repo && grit status --short --branch >../actual) &&
	grep "## main" actual
'

test_expect_success 'commit staged and check status' '
	(cd repo &&
		grit add tracked.txt &&
		grit commit -m "modify" &&
		grit status --short >../actual
	) &&
	grep "?? untracked.txt" actual &&
	! grep "^M.*tracked.txt" actual
'

test_expect_success 'status --short with new staged file' '
	(cd repo &&
		echo "staged" >new.txt &&
		grit add new.txt &&
		grit status --short >../actual
	) &&
	grep "^A" actual &&
	grep "new.txt" actual
'

test_expect_success 'status --short with staged deletion' '
	(cd repo &&
		grit commit -m "add new" &&
		grit rm new.txt &&
		grit status --short >../actual
	) &&
	grep "^D" actual &&
	grep "new.txt" actual
'

test_expect_success 'commit deletion and add nested gitignore' '
	(cd repo &&
		grit commit -m "delete new" &&
		mkdir -p subdir &&
		echo "*.tmp" >subdir/.gitignore &&
		grit add subdir/.gitignore &&
		grit commit -m "add nested gitignore"
	)
'

test_expect_success 'check-ignore respects nested gitignore' '
	(cd repo &&
		echo "data" >subdir/test.tmp &&
		grit check-ignore subdir/test.tmp >../actual
	) &&
	echo "subdir/test.tmp" >expect &&
	test_cmp expect actual
'

test_expect_success 'check-ignore -v shows nested gitignore source' '
	(cd repo && grit check-ignore -v subdir/test.tmp >../actual) &&
	grep "subdir/.gitignore" actual &&
	grep "\\*.tmp" actual
'

test_expect_success 'root gitignore still works with nested' '
	(cd repo && grit check-ignore test.log >../actual) &&
	echo "test.log" >expect &&
	test_cmp expect actual
'

test_expect_success 'negation pattern in gitignore' '
	(cd repo &&
		printf "*.dat\n!important.dat\n" >>.gitignore &&
		grit add .gitignore &&
		grit commit -m "negation pattern" &&
		echo "ignore me" >random.dat &&
		grit check-ignore random.dat >../actual
	) &&
	echo "random.dat" >expect &&
	test_cmp expect actual
'

test_expect_success 'negated file is not ignored' '
	(cd repo &&
		echo "keep me" >important.dat &&
		test_must_fail grit check-ignore important.dat
	)
'

test_expect_success 'check-ignore -v for negated file' '
	(cd repo && grit check-ignore -v --non-matching important.dat >../actual 2>&1) &&
	grep "important.dat" actual
'

test_expect_success 'status with untracked directory' '
	(cd repo &&
		mkdir -p newdir &&
		echo "file" >newdir/f1.txt &&
		grit status --short >../actual
	) &&
	grep "newdir" actual
'

test_expect_success 'clean up and verify final status' '
	(cd repo &&
		rm -f test.log untracked.txt random.dat important.dat &&
		rm -rf build newdir subdir/test.tmp &&
		grit status --short >../actual
	) &&
	! grep "test.log" actual
'

test_done
