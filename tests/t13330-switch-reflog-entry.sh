#!/bin/sh

test_description='grit switch: create, detach, orphan, reflog entries, edge cases'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
	 git config user.email "t@t.com" &&
	 git config user.name "T" &&
	 echo hello >file.txt &&
	 grit add file.txt &&
	 grit commit -m "initial" &&
	 echo second >second.txt &&
	 grit add second.txt &&
	 grit commit -m "second commit"
	)
'

test_expect_success 'switch -c creates new branch' '
	(cd repo &&
	 grit switch -c feature1 &&
	 grit branch >../actual
	) &&
	grep "feature1" actual
'

test_expect_success 'switch -c new branch is current' '
	(cd repo &&
	 grit symbolic-ref HEAD >../actual
	) &&
	echo "refs/heads/feature1" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch back to main' '
	(cd repo &&
	 grit switch main &&
	 grit symbolic-ref HEAD >../actual
	) &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch --create creates new branch' '
	(cd repo &&
	 grit switch --create feature2 &&
	 grit symbolic-ref HEAD >../actual
	) &&
	echo "refs/heads/feature2" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch to existing branch' '
	(cd repo &&
	 grit switch feature1 &&
	 grit symbolic-ref HEAD >../actual
	) &&
	echo "refs/heads/feature1" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch --detach goes to detached HEAD' '
	(cd repo &&
	 grit switch --detach main &&
	 test_must_fail grit symbolic-ref HEAD 2>../err
	) &&
	test -s err
'

test_expect_success 'switch --detach HEAD is at correct commit' '
	(cd repo &&
	 grit rev-parse HEAD >../actual &&
	 grit rev-parse main >../expect
	) &&
	test_cmp expect actual
'

test_expect_success 'switch back from detached HEAD to branch' '
	(cd repo &&
	 grit switch main &&
	 grit symbolic-ref HEAD >../actual
	) &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch - goes to previous branch' '
	(cd repo &&
	 grit switch feature1 &&
	 grit switch main &&
	 grit switch - &&
	 grit symbolic-ref HEAD >../actual
	) &&
	echo "refs/heads/feature1" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch --orphan creates branch with no history' '
	(cd repo &&
	 grit switch --orphan orphan1 &&
	 grit symbolic-ref HEAD >../actual
	) &&
	echo "refs/heads/orphan1" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch --orphan branch has no commits' '
	(cd repo &&
	 test_must_fail grit rev-parse HEAD 2>../err
	)
'

test_expect_success 'switch --orphan clears the index' '
	(cd repo &&
	 grit ls-files --cached >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'switch back to main from orphan' '
	(cd repo &&
	 grit switch main &&
	 grit symbolic-ref HEAD >../actual
	) &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch to nonexistent branch fails' '
	(cd repo &&
	 test_must_fail grit switch no-such-branch 2>../err
	) &&
	test -s err
'

test_expect_success 'switch creates reflog entry' '
	(cd repo &&
	 grit switch feature1 &&
	 grit switch main &&
	 cat .git/logs/HEAD >../actual
	) &&
	test -s actual
'

test_expect_success 'switch -c from specific start point' '
	(cd repo &&
	 grit rev-parse HEAD~1 >../expect &&
	 grit switch -c from-parent HEAD~1 &&
	 grit rev-parse HEAD >../actual
	) &&
	test_cmp expect actual
'

test_expect_success 'switch -c from-parent is at correct commit' '
	(cd repo &&
	 grit log --oneline >../actual
	) &&
	test_line_count = 1 actual
'

test_expect_success 'switch preserves working tree changes on clean switch' '
	(cd repo &&
	 grit switch main &&
	 echo untracked >untracked.txt &&
	 grit switch feature1 &&
	 test -f untracked.txt
	)
'

test_expect_success 'switch -c multiple branches' '
	(cd repo &&
	 grit switch main &&
	 grit switch -c b1 &&
	 grit switch main &&
	 grit switch -c b2 &&
	 grit switch main &&
	 grit switch -c b3 &&
	 grit branch >../actual
	) &&
	grep "b1" actual &&
	grep "b2" actual &&
	grep "b3" actual
'

test_expect_success 'switch with files updates working tree' '
	(cd repo &&
	 grit switch main &&
	 echo branch-content >branch-file.txt &&
	 grit add branch-file.txt &&
	 grit commit -m "add branch-file on main" &&
	 grit switch feature1 &&
	 test_path_is_missing branch-file.txt
	)
'

test_expect_success 'switch back shows the file again' '
	(cd repo &&
	 grit switch main &&
	 test_path_is_file branch-file.txt
	)
'

test_expect_success 'switch --detach to tag' '
	(cd repo &&
	 grit tag v1.0 &&
	 grit switch --detach v1.0 &&
	 test_must_fail grit symbolic-ref HEAD 2>../err &&
	 grit rev-parse HEAD >../actual &&
	 grit rev-parse v1.0 >../expect
	) &&
	test_cmp expect actual
'

test_expect_success 'switch to branch from detached state' '
	(cd repo &&
	 grit switch main &&
	 grit symbolic-ref HEAD >../actual
	) &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch -c at tag creates branch at tag' '
	(cd repo &&
	 grit switch -c at-tag v1.0 &&
	 grit rev-parse HEAD >../actual &&
	 grit rev-parse v1.0 >../expect
	) &&
	test_cmp expect actual
'

test_expect_success 'switch to branch with different tree content' '
	(cd repo &&
	 grit switch main &&
	 grit switch -c diverge &&
	 echo divergent >divergent.txt &&
	 grit add divergent.txt &&
	 grit commit -m "diverge" &&
	 grit switch main &&
	 test_path_is_missing divergent.txt &&
	 grit switch diverge &&
	 test_path_is_file divergent.txt
	)
'

test_expect_success 'switch -c already-existing branch fails' '
	(cd repo &&
	 grit switch main &&
	 test_must_fail grit switch -c feature1 2>../err
	) &&
	test -s err
'

test_expect_success 'switch updates HEAD reflog' '
	(cd repo &&
	 grit switch feature1 &&
	 grit switch main &&
	 tail -1 .git/logs/HEAD >../actual
	) &&
	grep "main" actual
'

test_expect_success 'switch -c with no commits on orphan' '
	(cd repo &&
	 grit switch --orphan clean-orphan &&
	 echo orphan-data >orphan-file.txt &&
	 grit add orphan-file.txt &&
	 grit commit -m "orphan commit" &&
	 grit log --oneline >../actual
	) &&
	test_line_count = 1 actual
'

test_expect_success 'switch between unrelated branches' '
	(cd repo &&
	 grit switch main &&
	 grit switch clean-orphan &&
	 grit symbolic-ref HEAD >../actual
	) &&
	echo "refs/heads/clean-orphan" >expect &&
	test_cmp expect actual
'

test_done
