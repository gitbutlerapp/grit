#!/bin/sh
# Test grit diff with pathspec patterns and directory filtering.

test_description='grit diff with pathspec magic and directory filters'

. ./test-lib.sh

test_expect_success 'setup repository with files in multiple dirs' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@test.com" &&
	mkdir -p src lib doc &&
	echo "main code" >src/main.c &&
	echo "helper code" >src/helper.c &&
	echo "library" >lib/util.c &&
	echo "readme" >doc/README &&
	echo "notes" >doc/NOTES &&
	echo "root" >Makefile &&
	git add -A &&
	git commit -m "initial"
	)
'

test_expect_success 'modify files for diffing' '
	(
	cd repo &&
	echo "main code v2" >src/main.c &&
	echo "helper code v2" >src/helper.c &&
	echo "library v2" >lib/util.c &&
	echo "readme v2" >doc/README &&
	echo "root v2" >Makefile &&
	git add -A &&
	git commit -m "update all"
	)
'

test_expect_success 'diff HEAD~1 HEAD -- src/' '
	(
	cd repo &&
	grit diff HEAD~1 HEAD -- src/ >actual &&
	grep "src/main.c" actual &&
	grep "src/helper.c" actual &&
	! grep "lib/" actual &&
	! grep "doc/" actual &&
	! grep "Makefile" actual
	)
'

test_expect_success 'diff HEAD~1 HEAD -- lib/' '
	(
	cd repo &&
	grit diff HEAD~1 HEAD -- lib/ >actual &&
	grep "lib/util.c" actual &&
	! grep "src/" actual
	)
'

test_expect_success 'diff HEAD~1 HEAD -- doc/' '
	(
	cd repo &&
	grit diff HEAD~1 HEAD -- doc/ >actual &&
	grep "doc/README" actual &&
	! grep "src/" actual &&
	! grep "Makefile" actual
	)
'

test_expect_success 'diff HEAD~1 HEAD -- Makefile' '
	(
	cd repo &&
	grit diff HEAD~1 HEAD -- Makefile >actual &&
	grep "Makefile" actual &&
	! grep "src/" actual
	)
'

test_expect_success 'diff HEAD~1 HEAD with multiple pathspecs' '
	(
	cd repo &&
	grit diff HEAD~1 HEAD -- src/ lib/ >actual &&
	grep "src/main.c" actual &&
	grep "lib/util.c" actual &&
	! grep "doc/" actual
	)
'

test_expect_success 'diff HEAD~1 HEAD with file pathspec' '
	(
	cd repo &&
	grit diff HEAD~1 HEAD -- src/main.c >actual &&
	grep "src/main.c" actual &&
	! grep "helper" actual
	)
'

test_expect_success 'diff-tree with pathspec restricts output' '
	(
	cd repo &&
	grit diff-tree HEAD -- src/ >actual &&
	grep "src" actual &&
	! grep "lib" actual &&
	! grep "Makefile" actual
	)
'

test_expect_success 'diff-tree with single file pathspec' '
	(
	cd repo &&
	grit diff-tree HEAD -- Makefile >actual &&
	grep "Makefile" actual &&
	# diff-tree on a commit prints the commit id header line first
	# (matches real git), so 1 changed file -> 2 lines total.
	test_line_count = 2 actual
	)
'

test_expect_success 'diff with no changes shows nothing' '
	(
	cd repo &&
	grit diff HEAD HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff HEAD~1 HEAD shows all changes without pathspec' '
	(
	cd repo &&
	grit diff HEAD~1 HEAD >actual &&
	grep "src/main.c" actual &&
	grep "src/helper.c" actual &&
	grep "lib/util.c" actual &&
	grep "doc/README" actual &&
	grep "Makefile" actual
	)
'

test_expect_success 'setup deep directory structure' '
	(
	cd repo &&
	mkdir -p a/b/c/d &&
	echo "deep" >a/b/c/d/deep.txt &&
	echo "mid" >a/b/mid.txt &&
	echo "top" >a/top.txt &&
	git add -A &&
	git commit -m "deep dirs"
	)
'

test_expect_success 'modify deep files' '
	(
	cd repo &&
	echo "deep v2" >a/b/c/d/deep.txt &&
	echo "mid v2" >a/b/mid.txt &&
	echo "top v2" >a/top.txt &&
	git add -A &&
	git commit -m "update deep"
	)
'

test_expect_success 'diff with deep pathspec a/b/c/' '
	(
	cd repo &&
	grit diff HEAD~1 HEAD -- a/b/c/ >actual &&
	grep "a/b/c/d/deep.txt" actual &&
	! grep "mid.txt" actual &&
	! grep "top.txt" actual
	)
'

test_expect_success 'diff with pathspec a/b/' '
	(
	cd repo &&
	grit diff HEAD~1 HEAD -- a/b/ >actual &&
	grep "a/b/c/d/deep.txt" actual &&
	grep "a/b/mid.txt" actual &&
	! grep "a/top.txt" actual
	)
'

test_expect_success 'diff with pathspec a/' '
	(
	cd repo &&
	grit diff HEAD~1 HEAD -- a/ >actual &&
	grep "a/b/c/d/deep.txt" actual &&
	grep "a/b/mid.txt" actual &&
	grep "a/top.txt" actual
	)
'

test_expect_success 'diff with nonexistent pathspec shows nothing' '
	(
	cd repo &&
	grit diff HEAD~1 HEAD -- nonexistent/ >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff-index with pathspec' '
	(
	cd repo &&
	echo "changed" >src/main.c &&
	git add src/main.c &&
	grit diff-index --cached HEAD -- src/ >actual &&
	grep "src/main.c" actual &&
	! grep "lib/" actual &&
	git checkout -- src/main.c
	)
'

test_expect_success 'diff-files with pathspec' '
	(
	cd repo &&
	echo "worktree change" >src/main.c &&
	echo "lib change" >lib/util.c &&
	grit diff-files -- src/ >actual &&
	grep "src/main.c" actual &&
	! grep "lib/" actual &&
	git checkout -- .
	)
'

test_expect_success 'diff shows correct content for restricted pathspec' '
	(
	cd repo &&
	grit diff HEAD~1 HEAD -- a/b/c/d/deep.txt >actual &&
	grep "+deep v2" actual &&
	grep "\\-deep" actual
	)
'

test_expect_success 'diff with trailing slash vs without' '
	(
	cd repo &&
	grit diff HEAD~1 HEAD -- a/b/c >actual_no_slash &&
	grit diff HEAD~1 HEAD -- a/b/c/ >actual_slash &&
	test_cmp actual_no_slash actual_slash
	)
'

test_done
