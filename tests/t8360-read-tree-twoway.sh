#!/bin/sh
# Additional read-tree two-way merge scenarios focusing on different
# file states, directory/file conflicts, and -u flag behaviour.

test_description='read-tree two-way merge — additional scenarios'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup: create base and target trees ──────────────────────────────────────

test_expect_success 'setup' '
	grit init . &&
	echo fileA >fileA &&
	echo fileB >fileB &&
	echo fileC >fileC &&
	mkdir -p dir &&
	echo dirfile >dir/file &&
	grit update-index --add fileA fileB fileC dir/file &&
	grit write-tree >.treeBase &&

	echo fileA-modified >fileA &&
	echo fileD >fileD &&
	grit update-index --add fileA fileD &&
	grit update-index --force-remove fileC &&
	grit write-tree >.treeTarget &&

	rm -f .git/index
'

# ── Basic two-way merge ─────────────────────────────────────────────────────

test_expect_success 'two-way merge from base to target' '
	BASE=$(cat .treeBase) && TARGET=$(cat .treeTarget) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	grit read-tree -m "$BASE" "$TARGET" &&
	grit ls-files --stage >out &&
	grep "fileA" out &&
	grep "fileD" out &&
	! grep "fileC" out
'

test_expect_success 'two-way merge updates fileA content in index' '
	BASE=$(cat .treeBase) && TARGET=$(cat .treeTarget) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	grit read-tree -m "$BASE" "$TARGET" &&
	BLOB=$(grit ls-files -s fileA | awk "{print \$2}") &&
	EXPECTED=$(echo "fileA-modified" | grit hash-object --stdin) &&
	test "$BLOB" = "$EXPECTED"
'

test_expect_success 'two-way merge adds new file fileD' '
	BASE=$(cat .treeBase) && TARGET=$(cat .treeTarget) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	grit read-tree -m "$BASE" "$TARGET" &&
	grit ls-files --stage fileD >out &&
	grep "fileD" out
'

test_expect_success 'two-way merge removes fileC' '
	BASE=$(cat .treeBase) && TARGET=$(cat .treeTarget) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	grit read-tree -m "$BASE" "$TARGET" &&
	grit ls-files --stage fileC >out &&
	test_must_be_empty out
'

test_expect_success 'two-way merge preserves unchanged fileB' '
	BASE=$(cat .treeBase) && TARGET=$(cat .treeTarget) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	grit read-tree -m "$BASE" "$TARGET" &&
	BLOB=$(grit ls-files -s fileB | awk "{print \$2}") &&
	EXPECTED=$(echo "fileB" | grit hash-object --stdin) &&
	test "$BLOB" = "$EXPECTED"
'

# ── Two-way merge with -u updates worktree ──────────────────────────────────

test_expect_success 'two-way merge with -u updates worktree files' '
	BASE=$(cat .treeBase) && TARGET=$(cat .treeTarget) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	grit read-tree -m -u "$BASE" "$TARGET" &&
	echo fileA-modified >expect &&
	test_cmp expect fileA
'

test_expect_success 'two-way merge with -u creates new worktree file' '
	BASE=$(cat .treeBase) && TARGET=$(cat .treeTarget) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	grit read-tree -m -u "$BASE" "$TARGET" &&
	echo fileD >expect &&
	test_cmp expect fileD
'

test_expect_success 'two-way merge with -u removes worktree file' '
	BASE=$(cat .treeBase) && TARGET=$(cat .treeTarget) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	test -f fileC &&
	grit read-tree -m -u "$BASE" "$TARGET" &&
	test_path_is_missing fileC
'

# ── Local changes carry forward ─────────────────────────────────────────────

test_expect_success 'local addition is carried forward' '
	BASE=$(cat .treeBase) && TARGET=$(cat .treeTarget) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	echo local >localfile &&
	grit update-index --add localfile &&
	grit read-tree -m "$BASE" "$TARGET" &&
	grit ls-files --stage localfile >out &&
	grep "localfile" out
'

test_expect_success 'unchanged local file is carried forward' '
	BASE=$(cat .treeBase) && TARGET=$(cat .treeTarget) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	echo fileB >fileB &&
	grit update-index --add fileB &&
	grit read-tree -m "$BASE" "$TARGET" &&
	grit ls-files --stage fileB >out &&
	grep "fileB" out
'

# ── Conflicting local changes ───────────────────────────────────────────────

test_expect_success 'conflicting local change on updated file fails' '
	BASE=$(cat .treeBase) && TARGET=$(cat .treeTarget) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	echo conflict >fileA &&
	grit update-index --add fileA &&
	test_must_fail grit read-tree -m "$BASE" "$TARGET"
'

test_expect_success 'conflicting local addition same as target succeeds' '
	BASE=$(cat .treeBase) && TARGET=$(cat .treeTarget) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	echo fileD >fileD &&
	grit update-index --add fileD &&
	grit read-tree -m "$BASE" "$TARGET" &&
	grit ls-files --stage fileD >out &&
	grep "fileD" out
'

test_expect_success 'conflicting local addition different from target fails' '
	BASE=$(cat .treeBase) && TARGET=$(cat .treeTarget) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	echo different-fileD >fileD &&
	grit update-index --add fileD &&
	test_must_fail grit read-tree -m "$BASE" "$TARGET"
'

# ── read-tree without -m (reset) ────────────────────────────────────────────

test_expect_success 'read-tree without -m replaces index entirely' '
	TARGET=$(cat .treeTarget) &&
	rm -f .git/index &&
	grit read-tree "$TARGET" &&
	grit ls-files --stage >out &&
		grep "fileA" out &&
		grep "fileD" out &&
		! grep "fileC" out
'

test_expect_success 'read-tree single tree resets index' '
	BASE=$(cat .treeBase) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit ls-files --stage >out &&
	grep "fileA" out &&
	grep "fileB" out &&
	grep "fileC" out &&
	grep "dir/file" out
'

# ── Subdirectory handling ───────────────────────────────────────────────────

test_expect_success 'setup trees with subdirectory changes' '
	rm -f .git/index &&
	echo fileA >fileA &&
	echo fileB >fileB &&
	mkdir -p subdir &&
	echo sub1 >subdir/one &&
	echo sub2 >subdir/two &&
	grit update-index --add fileA fileB subdir/one subdir/two &&
	grit write-tree >.treeSub1 &&

	echo sub1-mod >subdir/one &&
	echo sub3 >subdir/three &&
	grit update-index --add subdir/one subdir/three &&
	grit update-index --force-remove subdir/two &&
	grit write-tree >.treeSub2
'

test_expect_success 'two-way merge handles subdirectory file addition' '
	S1=$(cat .treeSub1) && S2=$(cat .treeSub2) &&
	rm -f .git/index &&
	grit read-tree "$S1" &&
	grit checkout-index -f -a &&
	grit read-tree -m "$S1" "$S2" &&
	grit ls-files --stage subdir/three >out &&
	grep "subdir/three" out
'

test_expect_success 'two-way merge handles subdirectory file removal' '
	S1=$(cat .treeSub1) && S2=$(cat .treeSub2) &&
	rm -f .git/index &&
	grit read-tree "$S1" &&
	grit checkout-index -f -a &&
	grit read-tree -m "$S1" "$S2" &&
	grit ls-files --stage subdir/two >out &&
	test_must_be_empty out
'

test_expect_success 'two-way merge handles subdirectory file modification' '
	S1=$(cat .treeSub1) && S2=$(cat .treeSub2) &&
	rm -f .git/index &&
	grit read-tree "$S1" &&
	grit checkout-index -f -a &&
	grit read-tree -m "$S1" "$S2" &&
	BLOB=$(grit ls-files -s subdir/one | awk "{print \$2}") &&
	EXPECTED=$(echo "sub1-mod" | grit hash-object --stdin) &&
	test "$BLOB" = "$EXPECTED"
'

# ── --prefix ────────────────────────────────────────────────────────────────

test_expect_success 'read-tree --prefix stages tree under prefix' '
	BASE=$(cat .treeBase) &&
	rm -f .git/index &&
	grit read-tree --prefix=pfx/ "$BASE" &&
	grit ls-files --stage >out &&
	grep "pfx/fileA" out &&
	grep "pfx/fileB" out
'

# ── Empty tree ──────────────────────────────────────────────────────────────

test_expect_success 'setup empty tree' '
	rm -f .git/index &&
	grit write-tree >.treeEmpty
'

test_expect_success 'two-way merge from empty to populated' '
	EMPTY=$(cat .treeEmpty) && BASE=$(cat .treeBase) &&
	rm -f .git/index &&
	grit read-tree "$EMPTY" &&
	grit read-tree -m "$EMPTY" "$BASE" &&
	grit ls-files --stage >out &&
	grep "fileA" out &&
	grep "fileB" out &&
	grep "fileC" out
'

test_expect_success 'two-way merge from populated to empty removes all' '
	EMPTY=$(cat .treeEmpty) && BASE=$(cat .treeBase) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	grit read-tree -m "$BASE" "$EMPTY" &&
	grit ls-files --stage >out &&
	test_must_be_empty out
'

test_expect_success 'two-way merge same tree to same tree is no-op' '
	BASE=$(cat .treeBase) &&
	rm -f .git/index &&
	grit read-tree "$BASE" &&
	grit checkout-index -f -a &&
	grit read-tree -m "$BASE" "$BASE" &&
	grit ls-files --stage >out &&
	grep "fileA" out &&
	grep "fileB" out &&
	grep "fileC" out
'

test_done
