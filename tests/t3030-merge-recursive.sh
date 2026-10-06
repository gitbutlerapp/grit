#!/bin/sh
# Test 3-way merge via read-tree -m (merge-recursive plumbing).

test_description='merge-recursive via read-tree 3-way merge'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup base, ours, theirs trees' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&

	# base tree: shared file, common file, base-only files
	echo "base content" >file.txt &&
	echo "common" >common.txt &&
	echo "base lib" >lib.c &&
	rm -f .git/index &&
	grit update-index --add file.txt common.txt lib.c &&
	tree_base=$(grit write-tree) &&
	echo "$tree_base" >../tree_base &&
	commit_base=$(grit commit-tree -m "base" $tree_base) &&
	echo "$commit_base" >../commit_base &&

	# ours tree: modify file.txt, add main-only.txt
	echo "main change" >file.txt &&
	echo "common" >common.txt &&
	echo "base lib" >lib.c &&
	echo "main-only" >main-only.txt &&
	rm -f .git/index &&
	grit update-index --add file.txt common.txt lib.c main-only.txt &&
	tree_ours=$(grit write-tree) &&
	echo "$tree_ours" >../tree_ours &&
	commit_ours=$(grit commit-tree -m "ours" $tree_ours -p $commit_base) &&
	echo "$commit_ours" >../commit_ours &&
	grit update-ref refs/heads/master $commit_ours &&

	# theirs tree: keep file.txt as base, add side-only.txt
	echo "base content" >file.txt &&
	echo "common" >common.txt &&
	echo "base lib" >lib.c &&
	echo "side-only" >side-only.txt &&
	rm -f .git/index &&
	grit update-index --add file.txt common.txt lib.c side-only.txt &&
	tree_theirs=$(grit write-tree) &&
	echo "$tree_theirs" >../tree_theirs &&
	commit_theirs=$(grit commit-tree -m "theirs" $tree_theirs -p $commit_base) &&
	echo "$commit_theirs" >../commit_theirs &&
	grit update-ref refs/heads/side $commit_theirs
	)
'

test_expect_success 'merge-base finds common ancestor' '
	(
	cd repo &&
	MBASE=$(grit merge-base "$(cat ../commit_ours)" "$(cat ../commit_theirs)") &&
	test "$MBASE" = "$(cat ../commit_base)"
	)
'

test_expect_success 'read-tree -m with 3 trees succeeds (no file conflict)' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree -m "$(cat ../tree_base)" "$(cat ../tree_ours)" "$(cat ../tree_theirs)"
	)
'

test_expect_success 'merged index has common.txt' '
	(
	cd repo &&
	grit ls-files >actual &&
	grep "common.txt" actual
	)
'

test_expect_success 'merged index has file.txt' '
	(
	cd repo &&
	grit ls-files >actual &&
	grep "file.txt" actual
	)
'

test_expect_success 'merged index has main-only.txt from ours' '
	(
	cd repo &&
	grit ls-files >actual &&
	grep "main-only.txt" actual
	)
'

test_expect_success 'merged index has side-only.txt from theirs' '
	(
	cd repo &&
	grit ls-files >actual &&
	grep "side-only.txt" actual
	)
'

test_expect_success 'merged index has lib.c' '
	(
	cd repo &&
	grit ls-files >actual &&
	grep "lib.c" actual
	)
'

test_expect_success 'common.txt resolved at stage 0 (unchanged)' '
	(
	cd repo &&
	grit ls-files -s common.txt >actual &&
	grep "0	common.txt" actual
	)
'

test_expect_success 'read-tree -m with identical trees (trivial merge)' '
	(
	cd repo &&
	rm -f .git/index &&
	TREE=$(cat ../tree_ours) &&
	grit read-tree -m $TREE $TREE $TREE
	)
'

test_expect_success 'trivial merge index matches single tree read' '
	(
	cd repo &&
	TREE=$(cat ../tree_ours) &&
	rm -f .git/index &&
	grit read-tree $TREE &&
	grit ls-files -s >expect &&
	rm -f .git/index &&
	grit read-tree -m $TREE $TREE $TREE &&
	grit ls-files -s >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'setup conflicting trees' '
	(
	cd repo &&
	echo "conflict-base" >conflict.txt &&
	rm -f .git/index &&
	grit update-index --add conflict.txt &&
	tree_cbase=$(grit write-tree) &&
	echo "$tree_cbase" >../tree_cbase &&

	echo "master version" >conflict.txt &&
	rm -f .git/index &&
	grit update-index --add conflict.txt &&
	tree_cours=$(grit write-tree) &&
	echo "$tree_cours" >../tree_cours &&

	echo "side version" >conflict.txt &&
	rm -f .git/index &&
	grit update-index --add conflict.txt &&
	tree_ctheirs=$(grit write-tree) &&
	echo "$tree_ctheirs" >../tree_ctheirs
	)
'

test_expect_success '3-way merge with conflict creates unmerged entries' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree -m "$(cat ../tree_cbase)" "$(cat ../tree_cours)" "$(cat ../tree_ctheirs)" &&
	grit ls-files -u >actual &&
	grep "conflict.txt" actual &&
	# Should have stage 1, 2, 3 entries
	lines=$(wc -l <actual | tr -d " ") &&
	test "$lines" -ge 2
	)
'

test_expect_success 'ls-files -u shows unmerged after conflict' '
	(
	cd repo &&
	grit ls-files -u >actual &&
	test -s actual
	)
'

test_expect_success 'read-tree -m 2-way merge succeeds' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree -m "$(cat ../tree_ours)" "$(cat ../tree_theirs)"
	)
'

test_expect_success 'write-tree after clean 3-way merge produces valid tree' '
	(
	cd repo &&
	rm -f .git/index &&
	TREE=$(cat ../tree_base) &&
	grit read-tree -m $TREE $TREE $TREE &&
	NEWTREE=$(grit write-tree) &&
	test -n "$NEWTREE" &&
	grit ls-tree $NEWTREE >actual &&
	test -s actual
	)
'

test_expect_success 'commit-tree after merge creates valid commit' '
	(
	cd repo &&
	rm -f .git/index &&
	TREE=$(cat ../tree_base) &&
	grit read-tree -m $TREE $TREE $TREE &&
	NEWTREE=$(grit write-tree) &&
	NEWCOMM=$(grit commit-tree -m "merged" $NEWTREE) &&
	test -n "$NEWCOMM" &&
	grit cat-file -t $NEWCOMM >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'read-tree --reset clears index' '
	(
	cd repo &&
	echo "extra" >extra.txt &&
	grit update-index --add extra.txt &&
	grit ls-files >before &&
	grep "extra.txt" before &&
	grit read-tree --reset "$(cat ../tree_base)" &&
	grit ls-files >after &&
	! grep "extra.txt" after
	)
'

test_expect_success 'read-tree single tree loads into index' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree "$(cat ../tree_base)" &&
	grit ls-files >actual &&
	grep "common.txt" actual &&
	grep "file.txt" actual
	)
'

test_expect_success 'read-tree with nonexistent tree fails' '
	(
	cd repo &&
	test_must_fail grit read-tree 0000000000000000000000000000000000000000
	)
'

test_expect_success 'read-tree -m fails with 4 trees' '
	(
	cd repo &&
	TREE=$(cat ../tree_base) &&
	test_must_fail grit read-tree -m $TREE $TREE $TREE $TREE 2>err
	)
'

test_expect_success 'merge-base with same commit returns itself' '
	(
	cd repo &&
	COMM=$(cat ../commit_ours) &&
	MBASE=$(grit merge-base $COMM $COMM) &&
	test "$MBASE" = "$COMM"
	)
'

test_expect_success '3-way merge: file added only in ours stays at stage 0' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree -m "$(cat ../tree_base)" "$(cat ../tree_ours)" "$(cat ../tree_theirs)" &&
	grit ls-files -s main-only.txt >actual &&
	grep "0	main-only.txt" actual
	)
'

test_expect_success '3-way merge: file added only in theirs stays at stage 0' '
	(
	cd repo &&
	grit ls-files -s side-only.txt >actual &&
	grep "0	side-only.txt" actual
	)
'

test_expect_success 'full plumbing merge workflow' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree "$(cat ../tree_ours)" &&
	TREE=$(grit write-tree) &&
	COMM=$(grit commit-tree -m "workflow test" $TREE) &&
	grit update-ref refs/heads/test-branch $COMM &&
	grit rev-list test-branch >actual &&
	test -s actual
	)
'

test_expect_success 'read-tree -m -u updates working tree files' '
	(
	cd repo &&
	rm -f common.txt file.txt lib.c &&
	rm -f .git/index &&
	TREE=$(cat ../tree_base) &&
	grit read-tree -m -u $TREE $TREE &&
	test -f common.txt &&
	test -f file.txt
	)
'

test_done
