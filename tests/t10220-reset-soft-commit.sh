#!/bin/sh
# Test grit reset with --soft, --mixed, --hard, and path-based reset.

test_description='grit reset soft mixed hard'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with multiple commits' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo c1 >file.txt &&
	echo keep >keep.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "c1" &&
	grit tag c1 &&
	echo c2 >file.txt &&
	echo c2-keep >keep.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "c2" &&
	grit tag c2 &&
	echo c3 >file.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "c3" &&
	grit tag c3
	)
'

test_expect_success 'reset --soft moves HEAD but keeps index and worktree' '
	(
	cd repo &&
	grit reset --soft c2 &&
	head_sha=$(grit rev-parse HEAD) &&
	c2_sha=$(grit rev-parse c2) &&
	test "$head_sha" = "$c2_sha"
	)
'

test_expect_success 'reset --soft keeps changes staged' '
	(
	cd repo &&
	grit diff --cached --name-only >cached &&
	grep "file.txt" cached
	)
'

test_expect_success 'reset --soft keeps worktree content' '
	(
	cd repo &&
	cat file.txt | grep "c3"
	)
'

test_expect_success 'reset --soft then recommit' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "c3-redo" &&
	grit log --oneline >log &&
	grep "c3-redo" log
	)
'

test_expect_success 'reset --soft back to c1' '
	(
	cd repo &&
	grit reset --soft c1 &&
	head_sha=$(grit rev-parse HEAD) &&
	c1_sha=$(grit rev-parse c1) &&
	test "$head_sha" = "$c1_sha"
	)
'

test_expect_success 'reset --soft to c1 stages everything since c1' '
	(
	cd repo &&
	grit diff --cached --name-only >cached &&
	grep "file.txt" cached &&
	grep "keep.txt" cached
	)
'

test_expect_success 'reset --soft to c1 worktree has latest content' '
	(
	cd repo &&
	cat file.txt | grep "c3" &&
	cat keep.txt | grep "c2-keep"
	)
'

test_expect_success 'reset --soft squash workflow' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "squashed c2+c3" &&
	grit log --oneline >log &&
	test_line_count = 2 log &&
	grep "squashed" log
	)
'

test_expect_success 'reset --mixed (default) moves HEAD and resets index' '
	(
	cd repo &&
	grit reset --hard c3 &&
	grit reset --mixed c2 &&
	head_sha=$(grit rev-parse HEAD) &&
	c2_sha=$(grit rev-parse c2) &&
	test "$head_sha" = "$c2_sha"
	)
'

test_expect_success 'reset --mixed unstages changes' '
	(
	cd repo &&
	grit diff --cached --name-only >cached &&
	test_must_be_empty cached
	)
'

test_expect_success 'reset --mixed keeps worktree changes' '
	(
	cd repo &&
	cat file.txt | grep "c3"
	)
'

test_expect_success 'reset --mixed shows modified in status' '
	(
	cd repo &&
	grit status --porcelain >status &&
	grep "file.txt" status
	)
'

test_expect_success 'default reset (no flag) is --mixed' '
	(
	cd repo &&
	grit reset --hard c3 &&
	grit reset c2 &&
	head_sha=$(grit rev-parse HEAD) &&
	c2_sha=$(grit rev-parse c2) &&
	test "$head_sha" = "$c2_sha" &&
	grit diff --cached --name-only >cached &&
	test_must_be_empty cached &&
	cat file.txt | grep "c3"
	)
'

test_expect_success 'reset --hard moves HEAD and resets everything' '
	(
	cd repo &&
	grit reset --hard c3 &&
	grit reset --hard c1 &&
	head_sha=$(grit rev-parse HEAD) &&
	c1_sha=$(grit rev-parse c1) &&
	test "$head_sha" = "$c1_sha"
	)
'

test_expect_success 'reset --hard resets worktree' '
	(
	cd repo &&
	cat file.txt | grep "c1"
	)
'

test_expect_success 'reset --hard resets index' '
	(
	cd repo &&
	grit diff --cached --name-only >cached &&
	test_must_be_empty cached
	)
'

test_expect_success 'reset --hard leaves clean status' '
	(
	cd repo &&
	grit diff --name-only >diff_wt &&
	test_must_be_empty diff_wt &&
	grit diff --cached --name-only >diff_idx &&
	test_must_be_empty diff_idx
	)
'

test_expect_success 'reset --hard forward to later commit' '
	(
	cd repo &&
	grit reset --hard c3 &&
	cat file.txt | grep "c3" &&
	grit diff --name-only >diff_wt &&
	test_must_be_empty diff_wt
	)
'

test_expect_success 'reset --hard HEAD is no-op on clean tree' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	cat file.txt | grep "c3" &&
	grit diff --name-only >diff_wt &&
	test_must_be_empty diff_wt
	)
'

test_expect_success 'reset --hard discards staged changes' '
	(
	cd repo &&
	echo dirty >file.txt &&
	grit add file.txt &&
	grit reset --hard HEAD &&
	cat file.txt | grep "c3"
	)
'

test_expect_success 'reset --hard discards worktree changes' '
	(
	cd repo &&
	echo dirty >file.txt &&
	grit reset --hard HEAD &&
	cat file.txt | grep "c3"
	)
'

test_expect_success 'reset -q is quiet' '
	(
	cd repo &&
	grit reset -q --hard c2 >out 2>&1 &&
	test_must_be_empty out &&
	grit reset --hard c3
	)
'

test_expect_success 'reset with path unstages specific file' '
	(
	cd repo &&
	echo mod >file.txt &&
	echo mod2 >keep.txt &&
	grit add file.txt keep.txt &&
	grit reset -- file.txt &&
	grit diff --cached --name-only >cached &&
	! grep "file.txt" cached &&
	grep "keep.txt" cached
	)
'

test_expect_success 'reset with path keeps worktree' '
	(
	cd repo &&
	cat file.txt | grep "mod" &&
	grit reset --hard HEAD
	)
'

test_expect_success 'setup repo2 for more reset tests' '
	(
	rm -rf repo2 &&
	grit init repo2 &&
	cd repo2 &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	mkdir -p dir &&
	echo a >dir/a.txt &&
	echo b >dir/b.txt &&
	echo root >root.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "initial" &&
	grit tag initial &&
	echo a2 >dir/a.txt &&
	echo b2 >dir/b.txt &&
	echo root2 >root.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "second" &&
	grit tag second
	)
'

test_expect_success 'reset --soft with nested files' '
	(
	cd repo2 &&
	grit reset --soft initial &&
	grit diff --cached --name-only >cached &&
	grep "dir/a.txt" cached &&
	grep "dir/b.txt" cached &&
	grep "root.txt" cached
	)
'

test_expect_success 'reset --soft nested recommit' '
	(
	cd repo2 &&
	test_tick &&
	grit commit -m "recommit" &&
	grit log --oneline >log &&
	test_line_count = 2 log
	)
'

test_expect_success 'reset --hard to initial cleans nested files' '
	(
	cd repo2 &&
	grit reset --hard initial &&
	cat dir/a.txt | grep "a" &&
	cat dir/b.txt | grep "b" &&
	cat root.txt | grep "root"
	)
'

test_expect_success 'reset --hard to initial has clean status' '
	(
	cd repo2 &&
	grit diff --name-only >diff_wt &&
	test_must_be_empty diff_wt &&
	grit diff --cached --name-only >diff_idx &&
	test_must_be_empty diff_idx
	)
'

test_expect_success 'reset --mixed to initial with nested' '
	(
	cd repo2 &&
	grit reset --hard second &&
	grit reset --mixed initial &&
	cat dir/a.txt | grep "a2" &&
	grit diff --cached --name-only >cached &&
	test_must_be_empty cached
	)
'

test_expect_success 'reset path on nested file' '
	(
	cd repo2 &&
	grit reset --hard second &&
	echo mod >dir/a.txt &&
	grit add dir/a.txt &&
	grit reset -- dir/a.txt &&
	grit diff --cached --name-only >cached &&
	! grep "dir/a.txt" cached
	)
'

test_expect_success 'reset --soft preserves new files in index' '
	(
	cd repo2 &&
	grit reset --hard second &&
	echo new >new.txt &&
	grit add new.txt &&
	test_tick &&
	grit commit -m "add new" &&
	grit tag with-new &&
	grit reset --soft second &&
	grit diff --cached --name-only >cached &&
	grep "new.txt" cached
	)
'

test_expect_success 'reset --hard removes new file' '
	(
	cd repo2 &&
	grit reset --hard second &&
	test_path_is_missing new.txt
	)
'

test_done
