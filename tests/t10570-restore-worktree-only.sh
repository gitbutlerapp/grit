#!/bin/sh
# Tests for grit restore with --worktree (-W), --staged (-S), --source (-s),
# --ignore-unmerged, and -q options.

test_description='grit restore --worktree and related options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with history' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@example.com" &&
	git config user.name "Test User" &&
	echo "version1" >file.txt &&
	echo "keep" >stable.txt &&
	mkdir -p src &&
	echo "code_v1" >src/main.rs &&
	grit add . &&
	grit commit -m "v1" &&
	echo "version2" >file.txt &&
	echo "code_v2" >src/main.rs &&
	grit add . &&
	grit commit -m "v2" &&
	echo "version3" >file.txt &&
	echo "code_v3" >src/main.rs &&
	grit add . &&
	grit commit -m "v3" &&
	V1=$(grit rev-parse HEAD~2) &&
	V2=$(grit rev-parse HEAD~1) &&
	V3=$(grit rev-parse HEAD) &&
	echo "$V1" >"$TRASH_DIRECTORY/v1_oid" &&
	echo "$V2" >"$TRASH_DIRECTORY/v2_oid" &&
	echo "$V3" >"$TRASH_DIRECTORY/v3_oid"
	)
'

# --- --worktree / -W basics (restore working tree from index) ---

test_expect_success 'restore -W reverts working tree change' '
	(
	cd repo &&
	echo "dirty" >file.txt &&
	grit restore -W file.txt &&
	test "$(cat file.txt)" = "version3"
	)
'

test_expect_success 'restore --worktree reverts working tree change' '
	(
	cd repo &&
	echo "dirty again" >file.txt &&
	grit restore --worktree file.txt &&
	test "$(cat file.txt)" = "version3"
	)
'

test_expect_success 'restore -W is default behavior' '
	(
	cd repo &&
	echo "modified" >file.txt &&
	grit restore file.txt &&
	test "$(cat file.txt)" = "version3"
	)
'

test_expect_success 'restore -W on multiple files' '
	(
	cd repo &&
	echo "dirty1" >file.txt &&
	echo "dirty2" >src/main.rs &&
	grit restore -W file.txt src/main.rs &&
	test "$(cat file.txt)" = "version3" &&
	test "$(cat src/main.rs)" = "code_v3"
	)
'

test_expect_success 'restore -W with dot restores everything' '
	(
	cd repo &&
	echo "d1" >file.txt &&
	echo "d2" >stable.txt &&
	echo "d3" >src/main.rs &&
	grit restore -W . &&
	test "$(cat file.txt)" = "version3" &&
	test "$(cat stable.txt)" = "keep" &&
	test "$(cat src/main.rs)" = "code_v3"
	)
'

# --- --staged / -S (unstage from HEAD) ---

test_expect_success 'restore -S unstages a file' '
	(
	cd repo &&
	echo "staged change" >file.txt &&
	grit add file.txt &&
	grit restore -S file.txt &&
	grit diff --cached --name-only >staged &&
	! grep "file.txt" staged
	)
'

test_expect_success 'restore --staged unstages multiple files' '
	(
	cd repo &&
	echo "s1" >file.txt &&
	echo "s2" >src/main.rs &&
	grit add file.txt src/main.rs &&
	grit restore --staged file.txt src/main.rs &&
	grit diff --cached --name-only >staged &&
	! grep "file.txt" staged &&
	! grep "src/main.rs" staged
	)
'

test_expect_success 'restore -S keeps working tree modification' '
	(
	cd repo &&
	echo "modified for stage" >file.txt &&
	grit add file.txt &&
	grit restore -S file.txt &&
	test "$(cat file.txt)" = "modified for stage"
	)
'

test_expect_success 'restore -S on new file unstages but keeps file' '
	(
	cd repo &&
	echo "brand new" >newfile.txt &&
	grit add newfile.txt &&
	grit restore -S newfile.txt &&
	test -f newfile.txt &&
	grit ls-files >ls_out &&
	! grep "newfile.txt" ls_out &&
	rm -f newfile.txt
	)
'

# --- --source / -s (using resolved commit hashes) ---

test_expect_success 'restore -s prev-commit restores file from previous commit' '
	(
	cd repo &&
	V2=$(cat "$TRASH_DIRECTORY/v2_oid") &&
	grit restore -s "$V2" -W file.txt &&
	test "$(cat file.txt)" = "version2"
	)
'

test_expect_success 'restore --source oldest restores older version' '
	(
	cd repo &&
	V1=$(cat "$TRASH_DIRECTORY/v1_oid") &&
	grit restore --source "$V1" -W file.txt &&
	test "$(cat file.txt)" = "version1"
	)
'

test_expect_success 'restore -s with tag' '
	(
	cd repo &&
	V1=$(cat "$TRASH_DIRECTORY/v1_oid") &&
	grit tag v1-tag "$V1" &&
	grit restore -s v1-tag -W file.txt &&
	test "$(cat file.txt)" = "version1" &&
	grit restore -W file.txt
	)
'

test_expect_success 'restore -s HEAD restores to latest' '
	(
	cd repo &&
	echo "dirty" >file.txt &&
	V3=$(cat "$TRASH_DIRECTORY/v3_oid") &&
	grit restore -s "$V3" -W file.txt &&
	test "$(cat file.txt)" = "version3"
	)
'

test_expect_success 'restore -s to staged area' '
	(
	cd repo &&
	V2=$(cat "$TRASH_DIRECTORY/v2_oid") &&
	grit restore -s "$V2" -S file.txt &&
	grit diff --cached --name-only >staged &&
	grep "file.txt" staged &&
	V3=$(cat "$TRASH_DIRECTORY/v3_oid") &&
	grit restore -s "$V3" -S file.txt
	)
'

test_expect_success 'restore -s -W -S restores both worktree and index' '
	(
	cd repo &&
	V2=$(cat "$TRASH_DIRECTORY/v2_oid") &&
	grit restore -s "$V2" -W -S file.txt &&
	test "$(cat file.txt)" = "version2" &&
	grit diff --cached --name-only >staged &&
	grep "file.txt" staged &&
	V3=$(cat "$TRASH_DIRECTORY/v3_oid") &&
	grit restore -s "$V3" -W -S file.txt
	)
'

# --- quiet mode ---

test_expect_success 'restore -q produces no output' '
	(
	cd repo &&
	echo "dirty" >file.txt &&
	grit restore -q file.txt >out 2>&1 &&
	test_must_be_empty out
	)
'

test_expect_success 'restore -q -W works silently' '
	(
	cd repo &&
	grit restore -W file.txt &&
	echo "dirty" >file.txt &&
	grit restore -q -W file.txt >out 2>&1 &&
	test_must_be_empty out &&
	test "$(cat file.txt)" = "version3"
	)
'

# --- edge cases ---

test_expect_success 'restore file that is not modified is a no-op' '
	(
	cd repo &&
	cp file.txt file_backup.txt &&
	grit restore file.txt &&
	diff file.txt file_backup.txt &&
	rm -f file_backup.txt
	)
'

test_expect_success 'restore nonexistent pathspec fails' '
	(
	cd repo &&
	test_must_fail grit restore nonexistent_file.txt 2>err
	)
'

test_expect_success 'restore deleted file brings it back' '
	(
	cd repo &&
	grit restore -W -S file.txt &&
	rm file.txt &&
	! test -f file.txt &&
	grit restore file.txt &&
	test -f file.txt &&
	test "$(cat file.txt)" = "version3"
	)
'

test_expect_success 'restore -W only affects working tree not index' '
	(
	cd repo &&
	echo "change" >file.txt &&
	grit add file.txt &&
	echo "more change" >file.txt &&
	grit restore -W file.txt &&
	test "$(cat file.txt)" = "change" &&
	grit restore -S file.txt
	)
'

test_expect_success 'restore specific file in subdirectory' '
	(
	cd repo &&
	echo "dirty" >src/main.rs &&
	grit restore src/main.rs &&
	test "$(cat src/main.rs)" = "code_v3"
	)
'

test_expect_success 'restore -s from explicit commit hash' '
	(
	cd repo &&
	V1=$(cat "$TRASH_DIRECTORY/v1_oid") &&
	grit restore -s "$V1" -W file.txt &&
	test "$(cat file.txt)" = "version1" &&
	grit restore file.txt
	)
'

test_expect_success 'restore after rm brings file back' '
	(
	cd repo &&
	grit rm file.txt &&
	V3=$(cat "$TRASH_DIRECTORY/v3_oid") &&
	grit restore -s "$V3" -W -S file.txt &&
	test -f file.txt &&
	test "$(cat file.txt)" = "version3"
	)
'

test_expect_success 'restore multiple specific files from source' '
	(
	cd repo &&
	V2=$(cat "$TRASH_DIRECTORY/v2_oid") &&
	grit restore -s "$V2" -W file.txt src/main.rs &&
	test "$(cat file.txt)" = "version2" &&
	test "$(cat src/main.rs)" = "code_v2" &&
	grit restore -W file.txt src/main.rs
	)
'

test_expect_success 'restore -S dot unstages all' '
	(
	cd repo &&
	echo "s1" >file.txt &&
	echo "s2" >stable.txt &&
	grit add file.txt stable.txt &&
	grit restore -S . &&
	grit diff --cached --name-only >staged &&
	test_must_be_empty staged &&
	grit restore -W .
	)
'

test_expect_success 'restore with -s and -W on file with spaces' '
	(
	cd repo &&
	echo "spaced v3" >"spaced file.txt" &&
	grit add "spaced file.txt" &&
	grit commit -m "add spaced" &&
	echo "spaced v4" >"spaced file.txt" &&
	grit add "spaced file.txt" &&
	grit commit -m "update spaced" &&
	PREV=$(grit rev-parse HEAD~1) &&
	grit restore -s "$PREV" -W "spaced file.txt" &&
	test "$(cat "spaced file.txt")" = "spaced v3" &&
	grit restore "spaced file.txt"
	)
'

test_done
