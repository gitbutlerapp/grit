#!/bin/sh

test_description='grit switch with -c, -q, detach, and branch operations'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# helper: get current branch name using git
current_branch () {
	git symbolic-ref --short HEAD 2>/dev/null || echo "HEAD"
}

test_expect_success 'setup' '
	(
	grit init repo && cd repo &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo hello >file.txt && grit add file.txt && grit commit -m "initial" &&
	echo second >file2.txt && grit add file2.txt && grit commit -m "second"
	)
'

test_expect_success 'switch -c creates new branch' '
	(cd repo && grit switch -c feature1 2>/dev/null) &&
	(cd repo && git branch >../actual) &&
	grep "feature1" actual
'

test_expect_success 'switch -c puts us on the new branch' '
	(cd repo && current_branch >../actual) &&
	echo "feature1" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch back to main' '
	(cd repo && grit switch main 2>/dev/null) &&
	(cd repo && current_branch >../actual) &&
	echo "main" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch -q suppresses output' '
	(cd repo && grit switch -q feature1 >../actual 2>&1) &&
	! test -s actual
'

test_expect_success 'switch -q still changes branch' '
	(cd repo && current_branch >../actual) &&
	echo "feature1" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch to nonexistent branch fails' '
	(cd repo && ! grit switch nonexistent 2>../err) &&
	test -s err
'

test_expect_success 'switch -c from specific commit' '
	(cd repo &&
	 grit switch main 2>/dev/null &&
	 first=$(git rev-parse HEAD~1) &&
	 grit switch -c from-first "$first" 2>/dev/null &&
	 git rev-parse HEAD >../actual &&
	 echo "$first" >../expect) &&
	test_cmp expect actual
'

test_expect_success 'switch --detach goes to detached HEAD' '
	(cd repo &&
	 grit switch main 2>/dev/null &&
	 grit switch --detach main 2>/dev/null &&
	 current_branch >../actual) &&
	echo "HEAD" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch back to named branch from detached HEAD' '
	(cd repo && grit switch main 2>/dev/null &&
	 current_branch >../actual) &&
	echo "main" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch -c with branch that already exists fails' '
	(cd repo && ! grit switch -c feature1 2>../err) &&
	test -s err
'

test_expect_success 'switch preserves working tree on clean switch' '
	(cd repo &&
	 grit switch main 2>/dev/null &&
	 cat file.txt >../actual) &&
	echo "hello" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch with dirty working tree carries changes' '
	(cd repo &&
	 echo dirty >file.txt &&
	 grit switch feature1 2>../err &&
	 current_branch >../actual) &&
	echo "feature1" >expect &&
	test_cmp expect actual &&
	(cd repo && git checkout -- file.txt && grit switch main 2>/dev/null)
'

test_expect_success 'switch between branches with different content' '
	(cd repo &&
	 grit switch -c content-branch 2>/dev/null &&
	 echo branchdata >branchfile.txt && grit add branchfile.txt &&
	 grit commit -m "branch content" &&
	 grit switch main 2>/dev/null) &&
	! test -f repo/branchfile.txt
'

test_expect_success 'switch back shows branch-specific file' '
	(cd repo && grit switch content-branch 2>/dev/null) &&
	test -f repo/branchfile.txt &&
	test "$(cat repo/branchfile.txt)" = "branchdata"
'

test_expect_success 'switch to same branch is no-op' '
	(cd repo && grit switch content-branch 2>/dev/null &&
	 current_branch >../actual) &&
	echo "content-branch" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch --detach to commit hash' '
	(cd repo &&
	 hash=$(git rev-parse HEAD) &&
	 grit switch main 2>/dev/null &&
	 grit switch --detach "$hash" 2>/dev/null &&
	 git rev-parse HEAD >../actual &&
	 echo "$hash" >../expect) &&
	test_cmp expect actual
'

test_expect_success 'switch -c creates branch at current HEAD by default' '
	(cd repo &&
	 grit switch main 2>/dev/null &&
	 main_hash=$(git rev-parse HEAD) &&
	 grit switch -c at-head 2>/dev/null &&
	 git rev-parse HEAD >../actual &&
	 echo "$main_hash" >../expect) &&
	test_cmp expect actual
'

test_expect_success 'switch with - goes to previous branch' '
	(cd repo &&
	 grit switch main 2>/dev/null &&
	 grit switch feature1 2>/dev/null &&
	 grit switch - 2>/dev/null &&
	 current_branch >../actual) &&
	echo "main" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch - again goes back' '
	(cd repo &&
	 grit switch - 2>/dev/null &&
	 current_branch >../actual) &&
	echo "feature1" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch -c from detached HEAD' '
	(cd repo &&
	 grit switch --detach main 2>/dev/null &&
	 grit switch -c from-detached 2>/dev/null &&
	 current_branch >../actual) &&
	echo "from-detached" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch to branch with slashes in name' '
	(cd repo &&
	 grit switch -c feature/sub/branch 2>/dev/null &&
	 current_branch >../actual) &&
	echo "feature/sub/branch" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch back from slashed branch' '
	(cd repo && grit switch main 2>/dev/null &&
	 current_branch >../actual) &&
	echo "main" >expect &&
	test_cmp expect actual
'

test_expect_success 'list branches after multiple creates' '
	(cd repo && git branch >../actual) &&
	grep "main" actual &&
	grep "feature1" actual &&
	grep "content-branch" actual
'

test_expect_success 'switch --detach with short hash' '
	(cd repo &&
	 short=$(git rev-parse --short HEAD) &&
	 grit switch --detach "$short" 2>/dev/null &&
	 current_branch >../actual) &&
	echo "HEAD" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch -c with empty name fails' '
	(cd repo &&
	 grit switch main 2>/dev/null &&
	 ! grit switch -c "" 2>../err) &&
	test -s err
'

test_expect_success 'switch --orphan creates empty branch' '
	(cd repo &&
	 grit switch main 2>/dev/null &&
	 grit switch --orphan empty-branch 2>../err &&
	 current_branch >../actual) &&
	echo "empty-branch" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch back to main from orphan' '
	(cd repo &&
	 grit switch main 2>/dev/null &&
	 current_branch >../actual) &&
	echo "main" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch -q -c combined creates branch silently' '
	(cd repo && grit switch -q -c silent-branch >../actual 2>&1) &&
	! test -s actual &&
	(cd repo && current_branch >../actual) &&
	echo "silent-branch" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch -c with tracking message' '
	(cd repo &&
	 grit switch main 2>/dev/null &&
	 grit switch -c tracked-branch 2>../actual) &&
	grep -i "branch\|switch" actual
'

test_done
