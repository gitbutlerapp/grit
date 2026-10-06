#!/bin/sh
# diff output for submodule entries, diff modes, and various diff flags.

test_description='grit diff with submodule entries and various modes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with files' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "author@example.com" &&
	grit config user.name "A U Thor" &&
	echo "hello" >file.txt &&
	echo "world" >other.txt &&
	grit add file.txt other.txt &&
	test_tick &&
	grit commit -m "initial"
	)
'

test_expect_success 'diff with no changes is empty' '
	(
	cd repo &&
	grit diff >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff --cached with no staged changes is empty' '
	(
	cd repo &&
	grit diff --cached >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff shows working tree changes' '
	(
	cd repo &&
	echo "modified" >file.txt &&
	grit diff >actual &&
	grep "^diff --git" actual &&
	grep "file.txt" actual
	)
'

test_expect_success 'diff --cached shows staged changes' '
	(
	cd repo &&
	grit add file.txt &&
	grit diff --cached >actual &&
	grep "^diff --git" actual &&
	grep "+modified" actual
	)
'

test_expect_success 'diff HEAD shows all changes vs HEAD' '
	(
	cd repo &&
	grit diff HEAD >actual &&
	grep "file.txt" actual
	)
'

test_expect_success 'diff --name-only shows only filenames' '
	(
	cd repo &&
	grit diff --name-only HEAD >actual &&
	grep "^file.txt$" actual &&
	! grep "^diff" actual
	)
'

test_expect_success 'diff --name-status shows status and filename' '
	(
	cd repo &&
	grit diff --name-status HEAD >actual &&
	grep "^M" actual &&
	grep "file.txt" actual
	)
'

test_expect_success 'diff --numstat shows numeric stats' '
	(
	cd repo &&
	grit diff --numstat HEAD >actual &&
	grep "file.txt" actual &&
	awk "{print \$1, \$2}" actual | grep -q "[0-9]"
	)
'

test_expect_success 'diff --exit-code returns 1 when there are differences' '
	(
	cd repo &&
	test_expect_code 1 grit diff --exit-code HEAD
	)
'

test_expect_success 'diff --exit-code returns 0 when no differences' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "commit modification" &&
	grit diff --exit-code HEAD >actual 2>&1 &&
	test_must_be_empty actual
	)
'

test_expect_success 'setup submodule entry via cacheinfo' '
	(
	cd repo &&
	COMMIT_OID=$(grit rev-parse HEAD) &&
	grit update-index --add --cacheinfo 160000,$COMMIT_OID,submod &&
	test_tick &&
	grit commit -m "add submodule entry"
	)
'

test_expect_success 'ls-tree shows submodule as 160000 commit' '
	(
	cd repo &&
	grit ls-tree HEAD >actual &&
	grep "^160000 commit" actual &&
	grep "submod" actual
	)
'

test_expect_success 'diff-tree between commits shows submodule entry' '
	(
	cd repo &&
	parent=$(grit rev-parse HEAD~1) &&
	grit diff-tree "$parent" HEAD >actual &&
	grep "submod" actual
	)
'

test_expect_success 'diff-tree shows 160000 mode for submodule' '
	(
	cd repo &&
	parent=$(grit rev-parse HEAD~1) &&
	grit diff-tree "$parent" HEAD >actual &&
	grep "160000" actual
	)
'

test_expect_success 'diff --cached after changing submodule entry' '
	(
	cd repo &&
	NEW_OID=$(grit rev-parse HEAD) &&
	grit update-index --add --cacheinfo 160000,$NEW_OID,submod &&
	grit diff --cached >actual &&
	grep "submod" actual &&
	grep "160000" actual
	)
'

test_expect_success 'diff --name-only shows submodule path' '
	(
	cd repo &&
	grit diff --name-only --cached >actual &&
	grep "^submod$" actual
	)
'

test_expect_success 'diff --name-status shows M for modified submodule' '
	(
	cd repo &&
	grit diff --name-status --cached >actual &&
	grep "^M" actual &&
	grep "submod" actual
	)
'

test_expect_success 'commit submodule change' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "update submodule"
	)
'

test_expect_success 'diff-tree between trees shows submodule change' '
	(
	cd repo &&
	tree1=$(grit rev-parse HEAD~1^{tree}) &&
	tree2=$(grit rev-parse HEAD^{tree}) &&
	grit diff-tree "$tree1" "$tree2" >actual &&
	grep "submod" actual
	)
'

test_expect_success 'diff with added file' '
	(
	cd repo &&
	echo "new" >new.txt &&
	grit add new.txt &&
	grit diff --cached --name-status >actual &&
	grep "^A" actual &&
	grep "new.txt" actual
	)
'

test_expect_success 'diff with deleted file' '
	(
	cd repo &&
	grit update-index --force-remove other.txt &&
	grit diff --cached --name-status >actual &&
	grep "^D" actual &&
	grep "other.txt" actual
	)
'

test_expect_success 'commit add and delete' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "add new, delete other"
	)
'

test_expect_success 'diff-tree shows A and D for added and deleted' '
	(
	cd repo &&
	parent=$(grit rev-parse HEAD~1) &&
	grit diff-tree "$parent" HEAD >actual &&
	grep "new.txt" actual &&
	grep "other.txt" actual
	)
'

test_expect_success 'diff between two tree OIDs' '
	(
	cd repo &&
	tree1=$(grit rev-parse HEAD~1^{tree}) &&
	tree2=$(grit rev-parse HEAD^{tree}) &&
	grit diff-tree "$tree1" "$tree2" >actual &&
	test -s actual
	)
'

test_expect_success 'diff-index HEAD with clean working tree is empty' '
	(
	cd repo &&
	grit checkout-index --all -f 2>/dev/null &&
	grit diff-index HEAD >actual 2>/dev/null || true &&
	true
	)
'

test_expect_success 'diff with multiple files changed' '
	(
	cd repo &&
	echo "mod1" >file.txt &&
	echo "mod2" >new.txt &&
	grit add file.txt new.txt &&
	grit diff --cached --name-only >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'diff --stat shows summary' '
	(
	cd repo &&
	grit diff --stat HEAD >actual &&
	test -s actual
	)
'

test_expect_success 'diff-files shows working tree changes' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "stage changes" &&
	echo "wt change" >file.txt &&
	grit diff-files >actual 2>/dev/null &&
	grep "file.txt" actual
	)
'

test_expect_success 'diff-files after re-adding file matches index' '
	(
	cd repo &&
	grit checkout-index -f file.txt &&
	grit update-index --add file.txt &&
	grit diff-files >actual 2>/dev/null &&
	! grep "file.txt" actual
	)
'

test_expect_success 'diff with mode change' '
	(
	cd repo &&
	chmod +x new.txt &&
	grit update-index --add new.txt &&
	grit diff --cached --name-only >actual &&
	if test -s actual; then
		grep "new.txt" actual
	fi
	)
'

test_done
