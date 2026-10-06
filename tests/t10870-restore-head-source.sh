#!/bin/sh
# Tests for grit restore with --source, --staged, --worktree, HEAD,
# and combinations of options.

test_description='grit restore --source, --staged, --worktree, HEAD interactions'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with history' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@example.com" &&
	git config user.name "Test User" &&
	echo "v1" >file.txt &&
	echo "orig" >other.txt &&
	echo "keep" >stable.txt &&
	mkdir -p sub &&
	echo "nested-v1" >sub/nested.txt &&
	grit add . &&
	grit commit -m "commit1" &&
	C1=$(grit rev-parse HEAD) &&
	echo "v2" >file.txt &&
	echo "orig2" >other.txt &&
	echo "nested-v2" >sub/nested.txt &&
	grit add . &&
	grit commit -m "commit2" &&
	C2=$(grit rev-parse HEAD) &&
	echo "v3" >file.txt &&
	echo "orig3" >other.txt &&
	echo "nested-v3" >sub/nested.txt &&
	grit add . &&
	grit commit -m "commit3" &&
	C3=$(grit rev-parse HEAD) &&
	echo "$C1" >"$TRASH_DIRECTORY/oid_c1" &&
	echo "$C2" >"$TRASH_DIRECTORY/oid_c2" &&
	echo "$C3" >"$TRASH_DIRECTORY/oid_c3"
	)
'

# --- restore worktree from index (default) ---

test_expect_success 'restore discards worktree changes (default)' '
	(
	cd repo &&
	echo "dirty" >file.txt &&
	grit restore file.txt &&
	test "$(cat file.txt)" = "v3"
	)
'

test_expect_success 'restore -W discards worktree changes explicitly' '
	(
	cd repo &&
	echo "dirty-W" >file.txt &&
	grit restore -W file.txt &&
	test "$(cat file.txt)" = "v3"
	)
'

test_expect_success 'restore --worktree discards worktree changes (long form)' '
	(
	cd repo &&
	echo "dirty-long" >other.txt &&
	grit restore --worktree other.txt &&
	test "$(cat other.txt)" = "orig3"
	)
'

test_expect_success 'restore dot restores all modified files' '
	(
	cd repo &&
	echo "dirty1" >file.txt &&
	echo "dirty2" >other.txt &&
	grit restore . &&
	test "$(cat file.txt)" = "v3" &&
	test "$(cat other.txt)" = "orig3"
	)
'

# --- restore staged (unstage) ---

test_expect_success 'restore --staged unstages a file' '
	(
	cd repo &&
	echo "staged-change" >file.txt &&
	grit add file.txt &&
	grit restore --staged file.txt &&
	grit diff --cached --name-only >cached &&
	! grep "file.txt" cached &&
	test "$(cat file.txt)" = "staged-change"
	)
'

test_expect_success 'restore -S unstages a file (short form)' '
	(
	cd repo &&
	grit add file.txt &&
	grit restore -S file.txt &&
	grit diff --cached --name-only >cached &&
	! grep "file.txt" cached
	)
'

test_expect_success 'restore --staged on multiple files' '
	(
	cd repo &&
	echo "s1" >file.txt &&
	echo "s2" >other.txt &&
	grit add file.txt other.txt &&
	grit restore --staged file.txt other.txt &&
	grit diff --cached --name-only >cached &&
	! grep "file.txt" cached &&
	! grep "other.txt" cached
	)
'

test_expect_success 'restore --staged with dot unstages everything' '
	(
	cd repo &&
	grit restore . &&
	echo "sa" >file.txt &&
	echo "sb" >other.txt &&
	grit add file.txt other.txt &&
	grit restore --staged file.txt other.txt &&
	grit diff --cached --name-only >cached &&
	test ! -s cached
	)
'

# --- restore from HEAD source ---

test_expect_success 'restore --source HEAD restores worktree from HEAD' '
	(
	cd repo &&
	grit restore . &&
	echo "manual-dirty" >file.txt &&
	grit restore --source HEAD file.txt &&
	test "$(cat file.txt)" = "v3"
	)
'

test_expect_success 'restore -s HEAD same as --source HEAD' '
	(
	cd repo &&
	echo "short-src" >file.txt &&
	grit restore -s HEAD file.txt &&
	test "$(cat file.txt)" = "v3"
	)
'

# --- restore from older commits (using saved OIDs) ---

test_expect_success 'restore --source parent commit restores from parent' '
	(
	cd repo &&
	C2=$(cat "$TRASH_DIRECTORY/oid_c2") &&
	grit restore --source "$C2" file.txt &&
	test "$(cat file.txt)" = "v2"
	)
'

test_expect_success 'restore --source grandparent restores from grandparent' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_c1") &&
	grit restore --source "$C1" file.txt &&
	test "$(cat file.txt)" = "v1"
	)
'

test_expect_success 'restore --source with commit hash on other.txt' '
	(
	cd repo &&
	C2=$(cat "$TRASH_DIRECTORY/oid_c2") &&
	grit restore --source "$C2" other.txt &&
	test "$(cat other.txt)" = "orig2"
	)
'

test_expect_success 'restore from older commit does not change other files' '
	(
	cd repo &&
	grit restore . &&
	C1=$(cat "$TRASH_DIRECTORY/oid_c1") &&
	grit restore --source "$C1" file.txt &&
	test "$(cat file.txt)" = "v1" &&
	test "$(cat other.txt)" = "orig3" &&
	test "$(cat stable.txt)" = "keep"
	)
'

# --- restore --source with --staged ---

test_expect_success 'restore --source HEAD --staged resets index entry' '
	(
	cd repo &&
	grit restore . &&
	echo "idx-change" >file.txt &&
	grit add file.txt &&
	grit restore --source HEAD --staged file.txt &&
	grit diff --cached --name-only >cached &&
	! grep "file.txt" cached
	)
'

test_expect_success 'restore --source parent --staged puts old version in index' '
	(
	cd repo &&
	grit restore . &&
	C2=$(cat "$TRASH_DIRECTORY/oid_c2") &&
	grit restore --source "$C2" --staged file.txt &&
	grit diff --cached --name-only >cached &&
	grep "file.txt" cached
	)
'

# --- restore --source with --staged --worktree ---

test_expect_success 'restore --source parent -SW restores both index and worktree' '
	(
	cd repo &&
	C2=$(cat "$TRASH_DIRECTORY/oid_c2") &&
	grit restore --source "$C2" --staged --worktree file.txt &&
	test "$(cat file.txt)" = "v2"
	)
'

test_expect_success 'restore back to HEAD for both index and worktree' '
	(
	cd repo &&
	C3=$(cat "$TRASH_DIRECTORY/oid_c3") &&
	grit restore --source "$C3" -S -W file.txt &&
	test "$(cat file.txt)" = "v3"
	)
'

# --- restore nested files ---

test_expect_success 'restore nested file from worktree changes' '
	(
	cd repo &&
	echo "dirty-nested" >sub/nested.txt &&
	grit restore sub/nested.txt &&
	test "$(cat sub/nested.txt)" = "nested-v3"
	)
'

test_expect_success 'restore --source grandparent on nested file' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_c1") &&
	grit restore --source "$C1" sub/nested.txt &&
	test "$(cat sub/nested.txt)" = "nested-v1"
	)
'

test_expect_success 'restore dot in subdirectory restores that subtree' '
	(
	cd repo &&
	echo "dirty-sub" >sub/nested.txt &&
	(cd sub && grit restore .) &&
	test "$(cat sub/nested.txt)" = "nested-v3"
	)
'

# --- error cases ---

test_expect_success 'restore nonexistent file fails' '
	(
	cd repo &&
	test_must_fail grit restore nonexistent.txt 2>err
	)
'

test_expect_success 'restore --source with invalid ref fails' '
	(
	cd repo &&
	test_must_fail grit restore --source invalid-ref file.txt 2>err
	)
'

test_expect_success 'restore with no pathspec fails' '
	(
	cd repo &&
	test_must_fail grit restore 2>err
	)
'

# --- restore does not affect untracked ---

test_expect_success 'restore does not affect untracked files' '
	(
	cd repo &&
	echo "untracked" >brand_new.txt &&
	grit restore . &&
	test -f brand_new.txt &&
	test "$(cat brand_new.txt)" = "untracked"
	)
'

# --- restore after rm ---

test_expect_success 'restore file deleted from worktree' '
	(
	cd repo &&
	rm file.txt &&
	! test -f file.txt &&
	grit restore file.txt &&
	test "$(cat file.txt)" = "v3"
	)
'

test_expect_success 'restore --source parent for deleted file brings old version' '
	(
	cd repo &&
	rm other.txt &&
	C2=$(cat "$TRASH_DIRECTORY/oid_c2") &&
	grit restore --source "$C2" other.txt &&
	test "$(cat other.txt)" = "orig2"
	)
'

# --- restore multiple specific paths from old commit ---

test_expect_success 'restore --source restores multiple paths at once' '
	(
	cd repo &&
	C1=$(cat "$TRASH_DIRECTORY/oid_c1") &&
	grit restore --source "$C1" file.txt other.txt &&
	test "$(cat file.txt)" = "v1" &&
	test "$(cat other.txt)" = "orig"
	)
'

test_expect_success 'restore back to HEAD state for clean slate' '
	(
	cd repo &&
	grit restore --source HEAD file.txt other.txt &&
	test "$(cat file.txt)" = "v3" &&
	test "$(cat other.txt)" = "orig3"
	)
'

# --- quiet mode ---

test_expect_success 'restore -q suppresses output' '
	(
	cd repo &&
	echo "noisy" >file.txt &&
	grit restore -q file.txt >out 2>&1 &&
	test ! -s out &&
	test "$(cat file.txt)" = "v3"
	)
'

test_done
