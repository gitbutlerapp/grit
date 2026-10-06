#!/bin/sh
# Tests for grit restore: --staged, --worktree, --source, -S -W combined,
# path-based restore, and interaction with various states.

test_description='grit restore: staged, worktree, source, combined modes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: init repo with committed files' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "original" >file.txt &&
	echo "keep" >keep.txt &&
	mkdir -p sub &&
	echo "nested" >sub/nested.txt &&
	grit add . &&
	grit commit -m "initial" &&
	grit rev-parse HEAD >../initial_oid
	)
'

# ---- worktree restore (default) ----
test_expect_success 'restore discards worktree changes (default mode)' '
	(
	cd repo &&
	echo "modified" >file.txt &&
	grit restore file.txt &&
	test "$(cat file.txt)" = "original"
	)
'

test_expect_success 'restore -W is explicit worktree restore' '
	(
	cd repo &&
	echo "changed" >file.txt &&
	grit restore -W file.txt &&
	test "$(cat file.txt)" = "original"
	)
'

test_expect_success 'restore --worktree is same as -W' '
	(
	cd repo &&
	echo "again" >file.txt &&
	grit restore --worktree file.txt &&
	test "$(cat file.txt)" = "original"
	)
'

# ---- staged restore ----
test_expect_success 'restore --staged unstages a file' '
	(
	cd repo &&
	echo "staged change" >file.txt &&
	grit add file.txt &&
	grit restore --staged file.txt &&
	grit diff --cached --name-only >staged &&
	! grep "file.txt" staged
	)
'

test_expect_success 'restore -S is synonym for --staged' '
	(
	cd repo &&
	echo "staged2" >file.txt &&
	grit add file.txt &&
	grit restore -S file.txt &&
	grit diff --cached --name-only >staged &&
	! grep "file.txt" staged
	)
'

test_expect_success 'restore --staged keeps worktree modification' '
	(
	cd repo &&
	echo "both" >file.txt &&
	grit add file.txt &&
	grit restore --staged file.txt &&
	test "$(cat file.txt)" = "both"
	)
'

# ---- combined -S -W ----
test_expect_success 'restore -S -W unstages and reverts worktree' '
	(
	cd repo &&
	echo "combo" >file.txt &&
	grit add file.txt &&
	grit restore -S -W file.txt &&
	test "$(cat file.txt)" = "original" &&
	grit diff --cached --name-only >staged &&
	! grep "file.txt" staged
	)
'

# ---- restore with --source ----
test_expect_success 'restore --source from HEAD works' '
	(
	cd repo &&
	echo "changed" >file.txt &&
	grit add file.txt &&
	grit commit -m "changed" &&
	echo "newer" >file.txt &&
	grit restore --source HEAD file.txt &&
	test "$(cat file.txt)" = "changed"
	)
'

test_expect_success 'restore --source from older commit' '
	(
	cd repo &&
	old=$(cat ../initial_oid) &&
	grit restore --source "$old" file.txt &&
	test "$(cat file.txt)" = "original"
	)
'

test_expect_success 'restore -s is synonym for --source' '
	(
	cd repo &&
	echo "latest" >file.txt &&
	grit add file.txt &&
	grit commit -m "latest" &&
	old=$(cat ../initial_oid) &&
	grit restore -s "$old" file.txt &&
	test "$(cat file.txt)" = "original"
	)
'

# ---- restore multiple files ----
test_expect_success 'restore multiple files at once' '
	(
	cd repo &&
	echo "mod1" >file.txt &&
	echo "mod2" >keep.txt &&
	grit restore file.txt keep.txt &&
	test "$(cat keep.txt)" = "keep"
	)
'

# ---- restore with dot (all) ----
test_expect_success 'restore . reverts all worktree changes' '
	(
	cd repo &&
	echo "mod" >file.txt &&
	echo "mod" >keep.txt &&
	echo "mod" >sub/nested.txt &&
	grit restore . &&
	test "$(cat keep.txt)" = "keep" &&
	test "$(cat sub/nested.txt)" = "nested"
	)
'

# ---- restore staged with dot ----
test_expect_success 'restore --staged . unstages all' '
	(
	cd repo &&
	echo "s1" >file.txt &&
	echo "s2" >keep.txt &&
	grit add file.txt keep.txt &&
	grit restore --staged . &&
	grit diff --cached --name-only >staged &&
	test ! -s staged
	)
'

# ---- restore deleted file ----
test_expect_success 'restore brings back deleted worktree file' '
	(
	cd repo &&
	rm file.txt &&
	! test -f file.txt &&
	grit restore file.txt &&
	test -f file.txt
	)
'

# ---- restore --staged on new file ----
test_expect_success 'restore --staged on newly added file removes from index' '
	(
	cd repo &&
	echo "brand new" >brandnew.txt &&
	grit add brandnew.txt &&
	grit restore --staged brandnew.txt &&
	! grit ls-files --error-unmatch brandnew.txt 2>/dev/null &&
	test -f brandnew.txt
	)
'

# ---- restore from source to staging ----
test_expect_success 'restore --source HEAD --staged restores index from commit' '
	(
	cd repo &&
	echo "idx_change" >file.txt &&
	grit add file.txt &&
	grit restore --source HEAD --staged file.txt &&
	grit diff --cached --name-only >staged &&
	! grep "file.txt" staged
	)
'

# ---- restore preserves other staged changes ----
test_expect_success 'restore --staged on one file keeps other staged files' '
	(
	cd repo &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	grit add a.txt b.txt &&
	grit restore --staged a.txt &&
	grit diff --cached --name-only >staged &&
	! grep "a.txt" staged &&
	grep "b.txt" staged
	)
'

# ---- restore in subdirectory ----
test_expect_success 'restore file in subdirectory' '
	(
	cd repo &&
	echo "submod" >sub/nested.txt &&
	grit restore sub/nested.txt &&
	test "$(cat sub/nested.txt)" = "nested"
	)
'

# ---- restore after rm ----
test_expect_success 'restore after rm --cached brings file back to index' '
	(
	cd repo &&
	grit rm --cached keep.txt &&
	grit restore --staged keep.txt &&
	grit ls-files --error-unmatch keep.txt
	)
'

# ---- restore nonexistent path fails ----
test_expect_success 'restore nonexistent path fails' '
	(
	cd repo &&
	test_must_fail grit restore nosuch.txt 2>err
	)
'

# ---- restore --source with --staged --worktree ----
test_expect_success 'restore --source with -S -W restores both from commit' '
	(
	cd repo &&
	grit add . &&
	grit commit -m "clean" --allow-empty &&
	echo "future" >file.txt &&
	grit add file.txt &&
	grit commit -m "future" &&
	echo "wt_change" >file.txt &&
	grit add file.txt &&
	old=$(cat ../initial_oid) &&
	grit restore --source "$old" -S -W file.txt &&
	test "$(cat file.txt)" = "original"
	)
'

# ---- restore executable file ----
test_expect_success 'restore preserves executable bit' '
	(
	cd repo &&
	echo "exec" >exec.sh &&
	chmod +x exec.sh &&
	grit add exec.sh &&
	grit commit -m "exec" &&
	echo "modified" >exec.sh &&
	grit restore exec.sh &&
	test -x exec.sh &&
	test "$(cat exec.sh)" = "exec"
	)
'

# ---- restore with --quiet ----
test_expect_success 'restore --quiet suppresses output' '
	(
	cd repo &&
	echo "q" >file.txt &&
	grit restore --quiet file.txt >output 2>&1 &&
	test ! -s output
	)
'

# ---- restore --staged then diff shows nothing staged ----
test_expect_success 'diff --cached empty after restore --staged' '
	(
	cd repo &&
	echo "diff_test" >file.txt &&
	grit add file.txt &&
	grit restore --staged file.txt &&
	grit diff --cached >d &&
	test ! -s d
	)
'

# ---- restore deleted file from source ----
test_expect_success 'restore --source HEAD deleted file' '
	(
	cd repo &&
	echo "willdelete" >todel.txt &&
	grit add todel.txt &&
	grit commit -m "todel" &&
	rm todel.txt &&
	grit restore --source HEAD todel.txt &&
	test "$(cat todel.txt)" = "willdelete"
	)
'

# ---- restore . after many modifications ----
test_expect_success 'restore . resets multiple tracked files to index state' '
	(
	cd repo &&
	grit add . 2>/dev/null &&
	grit commit -m "sync" --allow-empty &&
	echo "m1" >file.txt &&
	echo "m3" >sub/nested.txt &&
	grit restore file.txt sub/nested.txt &&
	test "$(cat sub/nested.txt)" = "nested"
	)
'

# ---- restore file added with intent-to-add ----
test_expect_success 'restore --staged on intent-to-add removes from index' '
	(
	cd repo &&
	echo "ita" >ita_restore.txt &&
	grit add -N ita_restore.txt &&
	grit restore --staged ita_restore.txt &&
	! grit ls-files --error-unmatch ita_restore.txt 2>/dev/null &&
	test -f ita_restore.txt
	)
'

test_expect_success 'restore --source HEAD --staged puts HEAD version in index' '
	(
	cd repo &&
	echo "idx_modified" >file.txt &&
	grit add file.txt &&
	grit restore --source HEAD --staged file.txt &&
	grit diff --cached --name-only >staged &&
	! grep "file.txt" staged
	)
'

test_expect_success 'restore worktree from source does not affect index' '
	(
	cd repo &&
	old=$(cat ../initial_oid) &&
	echo "newcontent" >file.txt &&
	grit add file.txt &&
	grit commit -m "newcontent" &&
	echo "wt_only" >file.txt &&
	grit restore --source "$old" file.txt &&
	test "$(cat file.txt)" = "original" &&
	grit diff --cached --name-only >staged &&
	! grep "file.txt" staged
	)
'

test_done
