#!/bin/sh
# Test grit restore with --staged, --worktree, --source, and combinations.

test_description='grit restore staged and source'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with history' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo v1 >file.txt &&
	echo keep >keep.txt &&
	mkdir -p sub &&
	echo nested >sub/nested.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "v1" &&
	grit tag v1-tag &&
	echo v2 >file.txt &&
	echo v2-nested >sub/nested.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "v2" &&
	grit tag v2-tag &&
	echo v3 >file.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "v3" &&
	grit tag v3-tag
	)
'

test_expect_success 'restore worktree from index (default)' '
	(
	cd repo &&
	echo dirty >file.txt &&
	grit restore file.txt &&
	cat file.txt | grep "v3"
	)
'

test_expect_success 'restore worktree does not change index' '
	(
	cd repo &&
	echo dirty >file.txt &&
	grit add file.txt &&
	echo dirtier >file.txt &&
	grit restore file.txt &&
	cat file.txt | grep "dirty" &&
	grit reset --hard HEAD
	)
'

test_expect_success 'restore --staged unstages file' '
	(
	cd repo &&
	echo modified >file.txt &&
	grit add file.txt &&
	grit restore --staged file.txt &&
	grit diff --cached --name-only >cached &&
	! grep "file.txt" cached
	)
'

test_expect_success 'restore --staged keeps worktree change' '
	(
	cd repo &&
	cat file.txt | grep "modified" &&
	grit restore file.txt
	)
'

test_expect_success 'restore --staged with multiple files' '
	(
	cd repo &&
	echo mod1 >file.txt &&
	echo mod2 >keep.txt &&
	grit add file.txt keep.txt &&
	grit restore --staged file.txt keep.txt &&
	grit diff --cached --name-only >cached &&
	! grep "file.txt" cached &&
	! grep "keep.txt" cached
	)
'

test_expect_success 'restore --staged multiple keeps worktree' '
	(
	cd repo &&
	cat file.txt | grep "mod1" &&
	cat keep.txt | grep "mod2" &&
	grit restore file.txt keep.txt
	)
'

test_expect_success 'restore --source with tag restores older version' '
	(
	cd repo &&
	grit restore --source v2-tag file.txt &&
	cat file.txt | grep "v2"
	)
'

test_expect_success 'restore --source v1 restores first version' '
	(
	cd repo &&
	grit restore --source v1-tag file.txt &&
	cat file.txt | grep "v1"
	)
'

test_expect_success 'restore --source HEAD restores current commit' '
	(
	cd repo &&
	echo junk >file.txt &&
	grit restore --source HEAD file.txt &&
	cat file.txt | grep "v3"
	)
'

test_expect_success 'restore --source with full SHA' '
	(
	cd repo &&
	full_sha=$(grit rev-parse v1-tag) &&
	grit restore --source "$full_sha" file.txt &&
	cat file.txt | grep "v1" &&
	grit restore --source HEAD file.txt
	)
'

test_expect_success 'restore --source does not stage the change' '
	(
	cd repo &&
	grit restore --source v1-tag file.txt &&
	grit diff --cached --name-only >cached &&
	! grep "file.txt" cached &&
	grit restore --source HEAD file.txt
	)
'

test_expect_success 'restore --worktree explicit flag same as default' '
	(
	cd repo &&
	echo dirty >file.txt &&
	grit restore --worktree file.txt &&
	cat file.txt | grep "v3"
	)
'

test_expect_success 'restore . restores all modified files' '
	(
	cd repo &&
	echo dirty1 >file.txt &&
	echo dirty2 >keep.txt &&
	grit restore . &&
	cat file.txt | grep "v3" &&
	cat keep.txt | grep "keep"
	)
'

test_expect_success 'restore --staged multiple files unstages all' '
	(
	cd repo &&
	echo mod1 >file.txt &&
	echo mod2 >keep.txt &&
	grit add file.txt keep.txt &&
	grit restore --staged file.txt &&
	grit restore --staged keep.txt &&
	grit diff --cached --name-only >cached &&
	! grep "file.txt" cached &&
	! grep "keep.txt" cached &&
	grit restore .
	)
'

test_expect_success 'restore nested file' '
	(
	cd repo &&
	echo dirty-nested >sub/nested.txt &&
	grit restore sub/nested.txt &&
	cat sub/nested.txt | grep "v2-nested"
	)
'

test_expect_success 'restore --source on nested file with tag' '
	(
	cd repo &&
	grit restore --source v1-tag sub/nested.txt &&
	cat sub/nested.txt | grep "nested" &&
	grit restore sub/nested.txt
	)
'

test_expect_success 'restore file that was deleted from worktree' '
	(
	cd repo &&
	rm file.txt &&
	test_path_is_missing file.txt &&
	grit restore file.txt &&
	test_path_is_file file.txt &&
	cat file.txt | grep "v3"
	)
'

test_expect_success 'restore deleted nested file' '
	(
	cd repo &&
	rm sub/nested.txt &&
	grit restore sub/nested.txt &&
	test_path_is_file sub/nested.txt
	)
'

test_expect_success 'restore --staged after rm --cached' '
	(
	cd repo &&
	grit rm --cached keep.txt &&
	grit restore --staged keep.txt &&
	grit ls-files >index &&
	grep "keep.txt" index
	)
'

test_expect_success 'setup new file for restore tests' '
	(
	cd repo &&
	echo new >new.txt &&
	grit add new.txt &&
	test_tick &&
	grit commit -m "add new"
	)
'

test_expect_success 'restore new file after modification' '
	(
	cd repo &&
	echo changed >new.txt &&
	grit restore new.txt &&
	cat new.txt | grep "new"
	)
'

test_expect_success 'restore --staged new file after add' '
	(
	cd repo &&
	echo changed >new.txt &&
	grit add new.txt &&
	grit restore --staged new.txt &&
	grit diff --cached --name-only >cached &&
	! grep "new.txt" cached
	)
'

test_expect_success 'restore with -q is quiet' '
	(
	cd repo &&
	grit restore . &&
	echo dirty >file.txt &&
	grit restore -q file.txt >out 2>&1 &&
	test_must_be_empty out
	)
'

test_expect_success 'restore nonexistent path fails' '
	(
	cd repo &&
	test_must_fail grit restore nonexistent.txt 2>err
	)
'

test_expect_success 'setup repo2 for more source tests' '
	(
	rm -rf repo2 &&
	grit init repo2 &&
	cd repo2 &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo r1 >a.txt &&
	echo r1 >b.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "r1" &&
	grit tag r1 &&
	echo r2 >a.txt &&
	echo r2 >b.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "r2" &&
	grit tag r2 &&
	echo r3 >a.txt &&
	echo r3 >b.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "r3" &&
	grit tag r3
	)
'

test_expect_success 'restore --source tag multiple files' '
	(
	cd repo2 &&
	grit restore --source r1 a.txt b.txt &&
	cat a.txt | grep "r1" &&
	cat b.txt | grep "r1" &&
	grit restore .
	)
'

test_expect_success 'restore --source with full commit SHA' '
	(
	cd repo2 &&
	full=$(grit rev-parse r2) &&
	grit restore --source "$full" a.txt &&
	cat a.txt | grep "r2" &&
	grit restore a.txt
	)
'

test_expect_success 'restore --worktree --source combination' '
	(
	cd repo2 &&
	grit restore --worktree --source r1 a.txt &&
	cat a.txt | grep "r1" &&
	grit diff --cached --name-only >cached &&
	! grep "a.txt" cached &&
	grit restore a.txt
	)
'

test_expect_success 'restore --source on file not in old commit fails' '
	(
	cd repo2 &&
	echo brand-new >c.txt &&
	grit add c.txt &&
	test_tick &&
	grit commit -m "add c" &&
	test_must_fail grit restore --source r1 c.txt 2>err
	)
'

test_expect_success 'restore after reset --soft still works' '
	(
	cd repo2 &&
	grit reset --soft r3 &&
	echo junk >a.txt &&
	grit restore a.txt &&
	cat a.txt | grep "r3"
	)
'

test_expect_success 'restore --staged after reset --soft' '
	(
	cd repo2 &&
	grit restore --staged c.txt &&
	grit diff --cached --name-only >cached &&
	! grep "c.txt" cached &&
	grit reset --hard HEAD
	)
'

test_expect_success 'restore --source switches between versions' '
	(
	cd repo2 &&
	grit restore --source r1 a.txt &&
	cat a.txt | grep "r1" &&
	grit restore --source r2 a.txt &&
	cat a.txt | grep "r2" &&
	grit restore --source r3 a.txt &&
	cat a.txt | grep "r3"
	)
'

test_expect_success 'restore multiple deleted files at once' '
	(
	cd repo2 &&
	rm a.txt b.txt &&
	test_path_is_missing a.txt &&
	test_path_is_missing b.txt &&
	grit restore a.txt b.txt &&
	test_path_is_file a.txt &&
	test_path_is_file b.txt
	)
'

test_done
