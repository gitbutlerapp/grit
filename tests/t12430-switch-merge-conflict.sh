#!/bin/sh
test_description='grit switch: create branches, switch between them, handle dirty worktree, detached HEAD'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
	 git config user.email "t@t.com" &&
	 git config user.name "T" &&
	 echo hello >file.txt &&
	 echo base >common.txt &&
	 grit add . &&
	 grit commit -m "initial")
'

test_expect_success 'switch -c creates new branch' '
	(cd repo &&
	 grit switch -c feature1 &&
	 grit branch >../actual) &&
	grep "\\* feature1" actual
'

test_expect_success 'switch back to main' '
	(cd repo &&
	 grit switch main &&
	 grit branch >../actual) &&
	grep "\\* main" actual
'

test_expect_success 'switch to existing branch' '
	(cd repo &&
	 grit switch feature1 &&
	 grit branch >../actual) &&
	grep "\\* feature1" actual
'

test_expect_success 'switch back to main again' '
	(cd repo && grit switch main)
'

test_expect_success 'switch -c from non-HEAD commit' '
	(cd repo &&
	 echo second >file.txt &&
	 grit add file.txt &&
	 grit commit -m "second" &&
	 grit switch -c from-first HEAD~1 &&
	 cat file.txt >../actual) &&
	echo "hello" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch back to main from from-first' '
	(cd repo && grit switch main)
'

test_expect_success 'switch with uncommitted changes that do not conflict' '
	(cd repo &&
	 echo untracked >newfile.txt &&
	 grit switch feature1 &&
	 test_path_is_file newfile.txt &&
	 grit branch >../actual) &&
	grep "\\* feature1" actual
'

test_expect_success 'cleanup and switch back' '
	(cd repo &&
	 rm -f newfile.txt &&
	 grit switch main)
'

test_expect_success 'switch refuses with conflicting dirty worktree' '
	(cd repo &&
	 grit switch -c conflict-branch &&
	 echo conflict-side >common.txt &&
	 grit add common.txt &&
	 grit commit -m "conflict side" &&
	 grit switch main &&
	 echo main-side >common.txt &&
	 grit add common.txt &&
	 grit commit -m "main side" &&
	 echo dirty-local >common.txt &&
	 test_must_fail grit switch conflict-branch 2>../errmsg) &&
	grep -i "uncommitted\|changes\|overwritten\|conflict\|would be\|local" errmsg
'

test_expect_success 'reset dirty worktree' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'switch --detach goes to detached HEAD' '
	(cd repo &&
	 grit switch --detach HEAD &&
	 grit status >../actual 2>&1) &&
	grep -i "detach\|HEAD" actual
'

test_expect_success 'switch back from detached HEAD' '
	(cd repo && grit switch main)
'

test_expect_success 'switch -c with starting point' '
	(cd repo &&
	 grit switch -c from-main main &&
	 grit branch >../actual) &&
	grep "\\* from-main" actual
'

test_expect_success 'switch back to main from from-main' '
	(cd repo && grit switch main)
'

test_expect_success 'switch to nonexistent branch fails' '
	(cd repo &&
	 test_must_fail grit switch no-such-branch 2>../errmsg) &&
	grep -i "not find\|did not match\|invalid\|no such\|error\|unknown\|not a valid" errmsg
'

test_expect_success 'switch -c existing branch name fails' '
	(cd repo &&
	 test_must_fail grit switch -c feature1 2>../errmsg) &&
	grep -i "already exists\|exists\|fatal" errmsg
'

test_expect_success 'switch updates working tree files' '
	(cd repo &&
	 grit switch feature1 &&
	 cat file.txt >../actual) &&
	echo "hello" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch back and verify main content' '
	(cd repo &&
	 grit switch main &&
	 cat file.txt >../actual) &&
	echo "second" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch with staged changes that do not conflict carries them' '
	(cd repo &&
	 echo staged-new >staged.txt &&
	 grit add staged.txt &&
	 grit switch feature1 &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	grep "staged.txt" actual
'

test_expect_success 'cleanup staged changes' '
	(cd repo &&
	 grit reset HEAD staged.txt &&
	 rm -f staged.txt &&
	 grit switch main)
'

test_expect_success 'switch -c and immediately commit on new branch' '
	(cd repo &&
	 grit switch -c quick-branch &&
	 echo quick >quick.txt &&
	 grit add quick.txt &&
	 grit commit -m "quick commit" &&
	 grit log --oneline >../actual) &&
	head -1 actual >actual_first &&
	grep "quick commit" actual_first
'

test_expect_success 'switch back to main from quick-branch' '
	(cd repo && grit switch main)
'

test_expect_success 'file from other branch is gone after switch' '
	(cd repo &&
	 test_path_is_missing quick.txt)
'

test_expect_success 'switch -c creates branch at correct point' '
	(cd repo &&
	 grit switch -c at-head &&
	 grit rev-parse HEAD >../head_actual &&
	 grit switch main &&
	 grit rev-parse HEAD >../head_expect) &&
	test_cmp head_expect head_actual
'

test_expect_success 'switch between branches preserves untracked files' '
	(cd repo &&
	 echo untracked >keepme.txt &&
	 grit switch feature1 &&
	 test_path_is_file keepme.txt &&
	 grit switch main &&
	 test_path_is_file keepme.txt &&
	 rm -f keepme.txt)
'

test_expect_success 'switch --detach to specific commit' '
	(cd repo &&
	 grit switch --detach HEAD~1 &&
	 grit rev-parse HEAD >../actual &&
	 grit rev-parse main~1 >../expect) &&
	test_cmp expect actual
'

test_expect_success 'switch back from detached to main' '
	(cd repo && grit switch main)
'

test_expect_success 'switch -c branch from tag' '
	(cd repo &&
	 grit tag v1.0 &&
	 grit switch -c from-tag v1.0 &&
	 grit branch >../actual) &&
	grep "\\* from-tag" actual
'

test_expect_success 'switch back to main from tag branch' '
	(cd repo && grit switch main)
'

test_expect_success 'switch to branch that was already checked out before' '
	(cd repo &&
	 grit switch feature1 &&
	 grit branch >../actual) &&
	grep "\\* feature1" actual
'

test_expect_success 'final switch to main' '
	(cd repo && grit switch main)
'

test_done
