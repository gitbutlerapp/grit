#!/bin/sh
# Tests for 'grit diff --cached' with staged deletes and adds.

test_description='grit diff --cached with deletions and additions'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo alpha >a.txt &&
	echo bravo >b.txt &&
	echo charlie >c.txt &&
	mkdir -p sub &&
	echo delta >sub/d.txt &&
	echo echo_ >sub/e.txt &&
	$REAL_GIT add . &&
	$REAL_GIT commit -m "initial"
	)
'

test_expect_success 'diff --cached shows nothing on clean index' '
	(cd repo && grit diff --cached >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff --cached --stat shows nothing on clean index' '
	(cd repo && grit diff --cached --stat >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff --cached --numstat shows nothing on clean index' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff --cached detects staged modification' '
	(cd repo && echo modified >a.txt && grit add a.txt &&
	 grit diff --cached --name-only >../actual) &&
	echo "a.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached --name-status shows M for modification' '
	(cd repo && grit diff --cached --name-status >../actual) &&
	echo "M	a.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached shows patch for staged modification' '
	(cd repo && grit diff --cached >../actual) &&
	grep "^-alpha" actual &&
	grep "^+modified" actual
'

test_expect_success 'diff --cached --stat for staged modification' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "a.txt" actual &&
	grep "1 file changed" actual
'

test_expect_success 'diff --cached --numstat for staged modification' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	echo "1	1	a.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'commit staged modification for clean slate' '
	(cd repo && grit commit -m "modify a")
'

test_expect_success 'diff --cached detects staged deletion' '
	(cd repo && $REAL_GIT rm b.txt &&
	 grit diff --cached --name-status >../actual) &&
	echo "D	b.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached shows delete patch' '
	(cd repo && grit diff --cached >../actual) &&
	grep "^deleted file mode" actual &&
	grep "^-bravo" actual
'

test_expect_success 'diff --cached --stat for deletion' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "b.txt" actual &&
	grep "1 deletion" actual
'

test_expect_success 'diff --cached --numstat for deletion' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	echo "0	1	b.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'commit deletion for clean slate' '
	(cd repo && grit commit -m "delete b")
'

test_expect_success 'diff --cached detects staged new file' '
	(cd repo && echo foxtrot >f.txt && grit add f.txt &&
	 grit diff --cached --name-status >../actual) &&
	echo "A	f.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached shows add patch for new file' '
	(cd repo && grit diff --cached >../actual) &&
	grep "^new file mode" actual &&
	grep "^+foxtrot" actual
'

test_expect_success 'diff --cached --stat for new file' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "f.txt" actual &&
	grep "1 insertion" actual
'

test_expect_success 'diff --cached --numstat for new file' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	echo "1	0	f.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'commit new file for clean slate' '
	(cd repo && grit commit -m "add f")
'

test_expect_success 'diff --cached with delete and add simultaneously' '
	(cd repo && $REAL_GIT rm c.txt &&
	 echo golf >g.txt && grit add g.txt &&
	 grit diff --cached --name-status >../actual) &&
	printf "D\tc.txt\nA\tg.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached --name-only with delete and add' '
	(cd repo && grit diff --cached --name-only >../actual) &&
	printf "c.txt\ng.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached --stat with delete and add' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "c.txt" actual &&
	grep "g.txt" actual &&
	grep "2 files changed" actual
'

test_expect_success 'diff --cached --numstat with delete and add' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	printf "0\t1\tc.txt\n1\t0\tg.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached shows both patches' '
	(cd repo && grit diff --cached >../actual) &&
	grep "^deleted file" actual &&
	grep "^-charlie" actual &&
	grep "^new file" actual &&
	grep "^+golf" actual
'

test_expect_success 'commit delete+add for clean slate' '
	(cd repo && grit commit -m "delete c add g")
'

test_expect_success 'diff --cached with subdirectory deletion' '
	(cd repo && $REAL_GIT rm sub/d.txt &&
	 grit diff --cached --name-status >../actual) &&
	echo "D	sub/d.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached with entire subdirectory deletion' '
	(cd repo && $REAL_GIT rm sub/e.txt &&
	 grit diff --cached --name-only >../actual) &&
	printf "sub/d.txt\nsub/e.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached --stat for multi-file subdirectory deletion' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "sub/d.txt" actual &&
	grep "sub/e.txt" actual &&
	grep "2 files changed" actual
'

test_expect_success 'commit subdirectory deletion' '
	(cd repo && grit commit -m "delete sub")
'

test_expect_success 'diff --cached with add in new subdirectory' '
	(cd repo && mkdir -p newdir &&
	 echo hotel >newdir/h.txt &&
	 echo india >newdir/i.txt &&
	 grit add newdir/ &&
	 grit diff --cached --name-status >../actual) &&
	printf "A\tnewdir/h.txt\nA\tnewdir/i.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached --numstat for new subdirectory files' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	printf "1\t0\tnewdir/h.txt\n1\t0\tnewdir/i.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'commit new subdirectory' '
	(cd repo && grit commit -m "add newdir")
'

test_expect_success 'diff --cached with empty file staged' '
	(cd repo && : >empty.txt && grit add empty.txt &&
	 grit diff --cached --name-status >../actual) &&
	echo "A	empty.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached --numstat for empty file' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	echo "0	0	empty.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --cached patch for empty file has no hunks' '
	(cd repo && grit diff --cached >../actual) &&
	grep "^new file mode" actual &&
	! grep "^@@" actual
'

test_expect_success 'commit empty file' '
	(cd repo && grit commit -m "add empty")
'

test_expect_success 'diff --cached --exit-code returns 0 on clean index' '
	(cd repo && grit diff --cached --exit-code)
'

test_expect_success 'diff --cached --exit-code returns 1 with staged changes' '
	(cd repo && echo juliet >j.txt && grit add j.txt &&
	 test_expect_code 1 grit diff --cached --exit-code)
'

test_expect_success 'diff --cached -q returns 1 with staged changes' '
	(cd repo && test_expect_code 1 grit diff --cached -q)
'

test_expect_success 'diff --cached -q produces no output' '
	(cd repo && grit diff --cached -q >../actual 2>&1; true) &&
	test_must_be_empty actual
'

test_expect_success 'commit for clean slate' '
	(cd repo && grit commit -m "add j")
'

test_expect_success 'diff --cached with replacement: delete old, add new content' '
	(cd repo && echo kilo >a.txt && grit add a.txt &&
	 grit diff --cached >../actual) &&
	grep "^-modified" actual &&
	grep "^+kilo" actual
'

test_expect_success 'diff --cached -U0 shows no context' '
	(cd repo && grit diff --cached -U0 >../actual) &&
	grep "^-modified" actual &&
	grep "^+kilo" actual &&
	! grep "^-$" actual
'

test_done
