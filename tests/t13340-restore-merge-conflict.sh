#!/bin/sh

test_description='grit restore: staged, worktree, source, quiet, edge cases'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
	 git config user.email "t@t.com" &&
	 git config user.name "T" &&
	 echo hello >file.txt &&
	 echo original >modify.txt &&
	 mkdir -p sub &&
	 echo nested >sub/nested.txt &&
	 grit add . &&
	 grit commit -m "initial" &&
	 grit rev-parse HEAD >../initial_commit
	)
'

test_expect_success 'restore --staged unstages a file' '
	(cd repo &&
	 echo changed >file.txt &&
	 grit add file.txt &&
	 grit restore --staged file.txt &&
	 grit diff-index --cached HEAD -- file.txt >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'restore --staged keeps worktree modification' '
	(cd repo &&
	 cat file.txt >../actual
	) &&
	echo changed >expect &&
	test_cmp expect actual
'

test_expect_success 'restore worktree reverts file to index content' '
	(cd repo &&
	 grit restore file.txt &&
	 cat file.txt >../actual
	) &&
	echo hello >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --worktree explicitly reverts working tree' '
	(cd repo &&
	 echo override >modify.txt &&
	 grit restore --worktree modify.txt &&
	 cat modify.txt >../actual
	) &&
	echo original >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --source from HEAD after commit' '
	(cd repo &&
	 echo v2 >file.txt &&
	 grit add file.txt &&
	 grit commit -m "change file" &&
	 echo v3 >file.txt &&
	 grit restore --source HEAD file.txt &&
	 cat file.txt >../actual
	) &&
	echo v2 >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --source from specific commit hash' '
	(cd repo &&
	 grit restore --source "$(cat ../initial_commit)" file.txt &&
	 cat file.txt >../actual
	) &&
	echo hello >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --staged --source HEAD unstages changes' '
	(cd repo &&
	 grit restore --source HEAD file.txt &&
	 echo staged-change >modify.txt &&
	 grit add modify.txt &&
	 grit restore --staged --source HEAD modify.txt &&
	 grit diff-index --cached HEAD -- modify.txt >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'restore dot restores all modified files' '
	(cd repo &&
	 grit reset --hard HEAD &&
	 echo mod1 >file.txt &&
	 echo mod2 >modify.txt &&
	 grit restore . &&
	 cat file.txt >../actual1 &&
	 cat modify.txt >../actual2
	) &&
	echo v2 >expect1 &&
	echo original >expect2 &&
	test_cmp expect1 actual1 &&
	test_cmp expect2 actual2
'

test_expect_success 'restore file in subdirectory' '
	(cd repo &&
	 echo changed-nested >sub/nested.txt &&
	 grit restore sub/nested.txt &&
	 cat sub/nested.txt >../actual
	) &&
	echo nested >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --staged on multiple files' '
	(cd repo &&
	 echo s1 >file.txt &&
	 echo s2 >modify.txt &&
	 grit add file.txt modify.txt &&
	 grit restore --staged file.txt modify.txt &&
	 grit diff-index --cached HEAD -- file.txt modify.txt >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'restore unmodified file is no-op' '
	(cd repo &&
	 grit reset --hard HEAD &&
	 cat file.txt >../before &&
	 grit restore file.txt &&
	 cat file.txt >../after
	) &&
	test_cmp before after
'

test_expect_success 'restore --quiet suppresses output' '
	(cd repo &&
	 echo noisy >file.txt &&
	 grit restore --quiet file.txt >../actual 2>&1
	) &&
	test_must_be_empty actual
'

test_expect_success 'restore deleted file from index' '
	(cd repo &&
	 rm file.txt &&
	 test_path_is_missing file.txt &&
	 grit restore file.txt &&
	 test_path_is_file file.txt
	)
'

test_expect_success 'restore deleted file has correct content' '
	(cd repo &&
	 cat file.txt >../actual
	) &&
	echo v2 >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --source with tag' '
	(cd repo &&
	 grit tag v1 &&
	 echo post-tag >file.txt &&
	 grit add file.txt &&
	 grit commit -m "post tag" &&
	 grit restore --source v1 file.txt &&
	 cat file.txt >../actual
	) &&
	echo v2 >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --staged after partial add' '
	(cd repo &&
	 grit restore --source HEAD file.txt &&
	 echo partial >file.txt &&
	 grit add file.txt &&
	 echo more >>file.txt &&
	 grit restore --staged file.txt &&
	 grit diff-index --cached HEAD -- file.txt >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'restore preserves executable bit' '
	(cd repo &&
	 echo "#!/bin/sh" >script.sh &&
	 chmod +x script.sh &&
	 grit add script.sh &&
	 grit commit -m "add script" &&
	 echo modified >script.sh &&
	 grit restore script.sh &&
	 test -x script.sh
	)
'

test_expect_success 'restore --staged --worktree fully resets file' '
	(cd repo &&
	 echo both >modify.txt &&
	 grit add modify.txt &&
	 echo worktree-extra >modify.txt &&
	 grit restore --staged --worktree --source HEAD modify.txt &&
	 cat modify.txt >../actual &&
	 grit diff-index --cached HEAD -- modify.txt >../staged_diff
	) &&
	echo original >expect &&
	test_cmp expect actual &&
	test_must_be_empty staged_diff
'

test_expect_success 'restore nonexistent path fails' '
	(cd repo &&
	 test_must_fail grit restore no-such-file.txt 2>../err
	) &&
	test -s err
'

test_expect_success 'restore symlink' '
	(cd repo &&
	 ln -sf modify.txt mylink &&
	 grit add mylink &&
	 grit commit -m "add link" &&
	 rm mylink &&
	 grit restore mylink &&
	 test -L mylink
	)
'

test_expect_success 'restore empty file' '
	(cd repo &&
	 : >empty.txt &&
	 grit add empty.txt &&
	 grit commit -m "add empty" &&
	 echo notempty >empty.txt &&
	 grit restore empty.txt &&
	 test_must_be_empty empty.txt
	)
'

test_expect_success 'restore multiple files from source commit' '
	(cd repo &&
	 echo m1 >file.txt &&
	 echo m2 >modify.txt &&
	 grit add file.txt modify.txt &&
	 grit commit -m "modify both" &&
	 grit restore --source "$(cat ../initial_commit)" file.txt modify.txt &&
	 cat file.txt >../a1 &&
	 cat modify.txt >../a2
	) &&
	echo hello >e1 &&
	echo original >e2 &&
	test_cmp e1 a1 &&
	test_cmp e2 a2
'

test_expect_success 'restore --staged with dot unstages everything' '
	(cd repo &&
	 echo us1 >file.txt &&
	 echo us2 >modify.txt &&
	 grit add . &&
	 grit restore --staged . &&
	 grit diff-index --cached HEAD >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'restore after reset --hard is no-op' '
	(cd repo &&
	 grit reset --hard HEAD &&
	 cat file.txt >../before &&
	 grit restore file.txt &&
	 cat file.txt >../after
	) &&
	test_cmp before after
'

test_expect_success 'restore with --source to branch name' '
	(cd repo &&
	 grit switch -c restore-branch &&
	 echo branch-data >file.txt &&
	 grit add file.txt &&
	 grit commit -m "branch change" &&
	 grit switch main &&
	 grit restore --source restore-branch file.txt &&
	 cat file.txt >../actual
	) &&
	echo branch-data >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --source HEAD restores current content' '
	(cd repo &&
	 grit restore --source HEAD file.txt &&
	 cat file.txt >../actual
	) &&
	test -s actual
'

test_expect_success 'restore --staged on newly added file removes from index' '
	(cd repo &&
	 echo brandnew >brandnew.txt &&
	 grit add brandnew.txt &&
	 grit restore --staged brandnew.txt &&
	 grit ls-files --cached >../actual
	) &&
	! grep "brandnew.txt" actual
'

test_expect_success 'restore file with spaces in name' '
	(cd repo &&
	 echo spaced >"sp ace.txt" &&
	 grit add "sp ace.txt" &&
	 grit commit -m "add spaced" &&
	 echo changed >"sp ace.txt" &&
	 grit restore "sp ace.txt" &&
	 cat "sp ace.txt" >../actual
	) &&
	echo spaced >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --source on deeply nested file' '
	(cd repo &&
	 mkdir -p a/b/c &&
	 echo deep >a/b/c/deep.txt &&
	 grit add a &&
	 grit commit -m "add deep" &&
	 echo changed >a/b/c/deep.txt &&
	 grit restore --source HEAD a/b/c/deep.txt &&
	 cat a/b/c/deep.txt >../actual
	) &&
	echo deep >expect &&
	test_cmp expect actual
'

test_done
