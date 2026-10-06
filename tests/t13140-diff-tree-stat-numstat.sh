#!/bin/sh
# Tests for 'grit diff-tree' with --stat, --name-only, --name-status, -r, -p.

test_description='grit diff-tree stat and output formats'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup repository with multiple commits' '
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
	$REAL_GIT commit -m "first" &&
	echo alpha2 >a.txt &&
	echo foxtrot >f.txt &&
	$REAL_GIT add . &&
	$REAL_GIT commit -m "second" &&
	$REAL_GIT rm b.txt &&
	echo echo2 >sub/e.txt &&
	$REAL_GIT add . &&
	$REAL_GIT commit -m "third" &&
	echo golf >g.txt &&
	echo hotel >h.txt &&
	$REAL_GIT add . &&
	$REAL_GIT commit -m "fourth"
	)
'

# --- raw output ---

test_expect_success 'diff-tree shows raw output for two commits' '
	(cd repo && grit diff-tree HEAD~1 HEAD >../actual) &&
	grep "A	g.txt" actual &&
	grep "A	h.txt" actual
'

test_expect_success 'diff-tree raw shows status letters' '
	(cd repo && grit diff-tree HEAD~2 HEAD~1 >../actual) &&
	grep "D	b.txt" actual
'

test_expect_success 'diff-tree single commit compares to parent' '
	(cd repo && grit diff-tree HEAD >../actual) &&
	grep "g.txt" actual &&
	grep "h.txt" actual
'

test_expect_success 'diff-tree -r recurses into subdirectories' '
	(cd repo && grit diff-tree -r HEAD~2 HEAD~1 >../actual) &&
	grep "sub/e.txt" actual &&
	! grep "sub$" actual
'

test_expect_success 'diff-tree without -r shows directory entry' '
	(cd repo && grit diff-tree HEAD~2 HEAD~1 >../actual) &&
	grep "	sub$" actual
'

# --- --name-only ---

test_expect_success 'diff-tree --name-only lists changed paths' '
	(cd repo && grit diff-tree --name-only HEAD~1 HEAD >../actual) &&
	printf "g.txt\nh.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff-tree -r --name-only shows files not dirs' '
	(cd repo && grit diff-tree -r --name-only HEAD~2 HEAD~1 >../actual) &&
	printf "b.txt\nsub/e.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff-tree --name-only for modification' '
	(cd repo && grit diff-tree --name-only HEAD~3 HEAD~2 >../actual) &&
	grep "a.txt" actual &&
	grep "f.txt" actual
'

# --- --name-status ---

test_expect_success 'diff-tree --name-status shows A for added files' '
	(cd repo && grit diff-tree --name-status HEAD~1 HEAD >../actual) &&
	grep "^A	g.txt$" actual &&
	grep "^A	h.txt$" actual
'

test_expect_success 'diff-tree --name-status shows D for deleted files' '
	(cd repo && grit diff-tree --name-status HEAD~2 HEAD~1 >../actual) &&
	grep "^D	b.txt$" actual
'

test_expect_success 'diff-tree --name-status shows M for modified files' '
	(cd repo && grit diff-tree --name-status HEAD~3 HEAD~2 >../actual) &&
	grep "^M	a.txt$" actual
'

test_expect_success 'diff-tree -r --name-status shows file-level status' '
	(cd repo && grit diff-tree -r --name-status HEAD~2 HEAD~1 >../actual) &&
	printf "D\tb.txt\nM\tsub/e.txt\n" >expect &&
	test_cmp expect actual
'

# --- --stat ---

test_expect_success 'diff-tree --stat shows summary' '
	(cd repo && grit diff-tree --stat HEAD~1 HEAD >../actual) &&
	grep "g.txt" actual &&
	grep "h.txt" actual &&
	grep "2 files changed" actual
'

test_expect_success 'diff-tree --stat shows insertions' '
	(cd repo && grit diff-tree --stat HEAD~1 HEAD >../actual) &&
	grep "insertion" actual
'

test_expect_success 'diff-tree --stat shows deletions' '
	(cd repo && grit diff-tree --stat HEAD~2 HEAD~1 >../actual) &&
	grep "deletion" actual
'

test_expect_success 'diff-tree -r --stat recurses into subdirs' '
	(cd repo && grit diff-tree -r --stat HEAD~2 HEAD~1 >../actual) &&
	grep "sub/e.txt" actual
'

test_expect_success 'diff-tree --stat across multiple commits' '
	(cd repo && grit diff-tree --stat HEAD~3 HEAD >../actual) &&
	grep "a.txt" actual
'

test_expect_success 'diff-tree -r --stat across multiple commits' '
	(cd repo && grit diff-tree -r --stat HEAD~3 HEAD >../actual) &&
	grep "a.txt" actual &&
	grep "b.txt" actual &&
	grep "f.txt" actual &&
	grep "g.txt" actual &&
	grep "h.txt" actual &&
	grep "sub/e.txt" actual
'

test_expect_success 'diff-tree -r --stat shows correct file count' '
	(cd repo && grit diff-tree -r --stat HEAD~3 HEAD >../actual) &&
	grep "6 files changed" actual
'

# --- -p (patch) ---

test_expect_success 'diff-tree -p shows patch output' '
	(cd repo && grit diff-tree -p HEAD~1 HEAD >../actual) &&
	grep "^diff --git" actual &&
	grep "^+golf" actual &&
	grep "^+hotel" actual
'

test_expect_success 'diff-tree -r -p shows recursive patches' '
	(cd repo && grit diff-tree -r -p HEAD~2 HEAD~1 >../actual) &&
	grep "^-bravo" actual &&
	grep "^-echo_" actual &&
	grep "^+echo2" actual
'

test_expect_success 'diff-tree -p shows deleted file patch' '
	(cd repo && grit diff-tree -r -p HEAD~2 HEAD~1 >../actual) &&
	grep "^deleted file mode" actual
'

test_expect_success 'diff-tree -p shows new file patch' '
	(cd repo && grit diff-tree -p HEAD~1 HEAD >../actual) &&
	grep "^new file mode" actual
'

# --- Combined scenarios ---

test_expect_success 'diff-tree with identical commits shows nothing' '
	(cd repo && grit diff-tree HEAD HEAD >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff-tree --stat with identical commits shows 0 files' '
	(cd repo && grit diff-tree --stat HEAD HEAD >../actual) &&
	grep "0 files changed" actual
'

test_expect_success 'diff-tree --name-only with identical commits shows nothing' '
	(cd repo && grit diff-tree --name-only HEAD HEAD >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'setup additional commits for more tests' '
	(cd repo &&
	 mkdir -p deep/nested/dir &&
	 echo india >deep/nested/dir/i.txt &&
	 $REAL_GIT add . &&
	 $REAL_GIT commit -m "fifth" &&
	 echo india2 >deep/nested/dir/i.txt &&
	 echo juliet >deep/nested/dir/j.txt &&
	 $REAL_GIT add . &&
	 $REAL_GIT commit -m "sixth")
'

test_expect_success 'diff-tree -r --name-only with deeply nested paths' '
	(cd repo && grit diff-tree -r --name-only HEAD~1 HEAD >../actual) &&
	printf "deep/nested/dir/i.txt\ndeep/nested/dir/j.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff-tree -r --name-status with deeply nested paths' '
	(cd repo && grit diff-tree -r --name-status HEAD~1 HEAD >../actual) &&
	grep "^M	deep/nested/dir/i.txt$" actual &&
	grep "^A	deep/nested/dir/j.txt$" actual
'

test_expect_success 'diff-tree -r --stat with deeply nested paths' '
	(cd repo && grit diff-tree -r --stat HEAD~1 HEAD >../actual) &&
	grep "deep/nested/dir/i.txt" actual &&
	grep "deep/nested/dir/j.txt" actual
'

test_expect_success 'diff-tree -r -p with deeply nested paths' '
	(cd repo && grit diff-tree -r -p HEAD~1 HEAD >../actual) &&
	grep "^+india2" actual &&
	grep "^+juliet" actual
'

test_done
