#!/bin/sh
# Test grit rm with -r, -q, --cached, --force, --dry-run, --ignore-unmatch.

test_description='grit rm recursive and quiet options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with nested directories' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	mkdir -p dir/sub/deep &&
	echo a >dir/a.txt &&
	echo b >dir/sub/b.txt &&
	echo c >dir/sub/deep/c.txt &&
	echo top >top.txt &&
	echo keep >keep.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "initial"
	)
'

test_expect_success 'rm single file removes from worktree and index' '
	(
	cd repo &&
	grit rm top.txt &&
	test_path_is_missing top.txt &&
	grit status --porcelain >status &&
	grep "^D  top.txt" status
	)
'

test_expect_success 'rm directory without -r fails' '
	(
	cd repo &&
	grit checkout -- . 2>/dev/null || grit restore . 2>/dev/null || true &&
	grit reset --hard HEAD &&
	test_must_fail grit rm dir 2>err
	)
'

test_expect_success 'rm -r removes directory recursively' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	grit rm -r dir &&
	test_path_is_missing dir/a.txt &&
	test_path_is_missing dir/sub/b.txt &&
	test_path_is_missing dir/sub/deep/c.txt
	)
'

test_expect_success 'rm -r directory shows in status as deleted' '
	(
	cd repo &&
	grit status --porcelain >status &&
	grep "^D  dir/a.txt" status &&
	grep "^D  dir/sub/b.txt" status &&
	grep "^D  dir/sub/deep/c.txt" status
	)
'

test_expect_success 'rm -q suppresses output' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	grit rm -q top.txt >out 2>&1 &&
	test_must_be_empty out
	)
'

test_expect_success 'rm without -q shows removal message' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	grit rm top.txt >out 2>&1 &&
	grep "top.txt" out
	)
'

test_expect_success 'rm -r -q suppresses recursive removal output' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	grit rm -r -q dir >out 2>&1 &&
	test_must_be_empty out
	)
'

test_expect_success 'rm --cached removes from index but keeps worktree' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	grit rm --cached top.txt &&
	test_path_is_file top.txt &&
	grit status --porcelain >status &&
	grep "^D  top.txt" status
	)
'

test_expect_success 'rm --cached file content unchanged in worktree' '
	(
	cd repo &&
	content=$(cat top.txt) &&
	test "$content" = "top"
	)
'

test_expect_success 'rm --cached -r removes dir from index only' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	grit rm --cached -r dir &&
	test_path_is_file dir/a.txt &&
	test_path_is_file dir/sub/b.txt &&
	grit status --porcelain >status &&
	grep "^D  dir/a.txt" status
	)
'

test_expect_success 'rm -n dry-run does not actually remove' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	grit rm -n top.txt >out 2>&1 &&
	test_path_is_file top.txt &&
	grit diff --cached --name-only >diff &&
	! grep "top.txt" diff
	)
'

test_expect_success 'rm --dry-run shows what would be removed' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	grit rm --dry-run top.txt >out 2>&1 &&
	grep "top.txt" out
	)
'

test_expect_success 'rm -r -n dry-run keeps directory intact' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	grit rm -r -n dir >out 2>&1 &&
	test_path_is_file dir/a.txt &&
	test_path_is_file dir/sub/b.txt &&
	test_path_is_file dir/sub/deep/c.txt
	)
'

test_expect_success 'rm --ignore-unmatch with nonexistent file succeeds' '
	(
	cd repo &&
	grit rm --ignore-unmatch nonexistent.txt
	)
'

test_expect_success 'rm without --ignore-unmatch on nonexistent file fails' '
	(
	cd repo &&
	test_must_fail grit rm nonexistent.txt
	)
'

test_expect_success 'rm -f removes file with staged changes' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	echo modified >top.txt &&
	grit add top.txt &&
	echo modified-again >top.txt &&
	grit rm -f top.txt &&
	test_path_is_missing top.txt
	)
'

test_expect_success 'rm multiple files at once' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	grit rm top.txt keep.txt &&
	test_path_is_missing top.txt &&
	test_path_is_missing keep.txt
	)
'

test_expect_success 'rm multiple files shows in status' '
	(
	cd repo &&
	grit status --porcelain >status &&
	grep "^D  top.txt" status &&
	grep "^D  keep.txt" status
	)
'

test_expect_success 'setup fresh repo for more tests' '
	(
	rm -rf repo2 &&
	grit init repo2 &&
	cd repo2 &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	mkdir -p a/b/c &&
	echo 1 >a/1.txt &&
	echo 2 >a/b/2.txt &&
	echo 3 >a/b/c/3.txt &&
	echo root >root.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "initial"
	)
'

test_expect_success 'rm --cached -q is quiet' '
	(
	cd repo2 &&
	grit rm --cached -q root.txt >out 2>&1 &&
	test_must_be_empty out
	)
'

test_expect_success 'rm --cached file then re-add works' '
	(
	cd repo2 &&
	grit add root.txt &&
	grit diff --cached --name-only >diff &&
	! grep "root.txt" diff
	)
'

test_expect_success 'rm -r deep nested directory' '
	(
	cd repo2 &&
	grit rm -r a/b/c &&
	test_path_is_missing a/b/c/3.txt &&
	test_path_is_file a/1.txt &&
	test_path_is_file a/b/2.txt
	)
'

test_expect_success 'rm -r partial directory leaves sibling intact' '
	(
	cd repo2 &&
	grit status --porcelain >status &&
	grep "^D  a/b/c/3.txt" status &&
	! grep "a/1.txt" status &&
	! grep "a/b/2.txt" status
	)
'

test_expect_success 'rm then commit records deletion' '
	(
	cd repo2 &&
	grit reset --hard HEAD &&
	grit rm root.txt &&
	test_tick &&
	grit commit -m "remove root.txt" &&
	grit ls-tree -r HEAD --name-only >files &&
	! grep "root.txt" files
	)
'

test_expect_success 'rm -r then commit records all deletions' '
	(
	cd repo2 &&
	grit rm -r a &&
	test_tick &&
	grit commit -m "remove a/" &&
	grit ls-tree -r HEAD --name-only >files &&
	! grep "^a/" files
	)
'

test_expect_success 'rm --ignore-unmatch -q is completely silent' '
	(
	cd repo2 &&
	grit rm --ignore-unmatch -q no-such-file >out 2>&1 &&
	test_must_be_empty out
	)
'

test_expect_success 'setup repo with executable and symlink' '
	(
	rm -rf repo3 &&
	grit init repo3 &&
	cd repo3 &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	echo normal >normal.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "initial"
	)
'

test_expect_success 'rm executable file works' '
	(
	cd repo3 &&
	grit rm script.sh &&
	test_path_is_missing script.sh
	)
'

test_expect_success 'rm --cached preserves executable permission in worktree' '
	(
	cd repo3 &&
	grit reset --hard HEAD &&
	grit rm --cached script.sh &&
	test -x script.sh
	)
'

test_expect_success 'rm -n --dry-run on executable does not remove it' '
	(
	cd repo3 &&
	grit reset --hard HEAD &&
	grit rm -n script.sh >out 2>&1 &&
	test -x script.sh
	)
'

test_expect_success 'rm -f --cached with modified file removes from index only' '
	(
	cd repo3 &&
	grit reset --hard HEAD &&
	echo changed >normal.txt &&
	grit add normal.txt &&
	grit rm -f --cached normal.txt &&
	test_path_is_file normal.txt &&
	cat normal.txt | grep "changed"
	)
'

test_expect_success 'rm multiple tracked files by name' '
	(
	cd repo3 &&
	grit reset --hard HEAD &&
	grit rm normal.txt &&
	test_path_is_missing normal.txt &&
	test_path_is_file script.sh
	)
'

test_expect_success 'rm then commit records deletion in log' '
	(
	cd repo3 &&
	test_tick &&
	grit commit -a -m "remove normal" &&
	grit log --oneline >log &&
	grep "remove normal" log
	)
'

test_done
