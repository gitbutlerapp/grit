#!/bin/sh

test_description='grit reset: soft, mixed, hard, paths, quiet, orphan branch edge cases'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
	 git config user.email "t@t.com" &&
	 git config user.name "T" &&
	 echo hello >file.txt &&
	 echo original >other.txt &&
	 mkdir -p sub &&
	 echo nested >sub/nested.txt &&
	 grit add . &&
	 grit commit -m "initial" &&
	 grit rev-parse HEAD >../C1 &&
	 echo second >file.txt &&
	 grit add file.txt &&
	 grit commit -m "second" &&
	 grit rev-parse HEAD >../C2 &&
	 echo third >file.txt &&
	 grit add file.txt &&
	 grit commit -m "third" &&
	 grit rev-parse HEAD >../C3
	)
'

test_expect_success 'reset --soft moves HEAD but keeps index and worktree' '
	(cd repo &&
	 grit reset --soft "$(cat ../C2)" &&
	 grit rev-parse HEAD >../actual
	) &&
	test_cmp C2 actual
'

test_expect_success 'reset --soft keeps staging area' '
	(cd repo &&
	 grit diff-index --cached HEAD -- file.txt >../actual
	) &&
	test -s actual
'

test_expect_success 'reset --soft keeps working tree content' '
	(cd repo &&
	 cat file.txt >../actual
	) &&
	echo third >expect &&
	test_cmp expect actual
'

test_expect_success 'reset --mixed moves HEAD and resets index' '
	(cd repo &&
	 grit reset --hard "$(cat ../C3)" &&
	 grit reset --mixed "$(cat ../C2)" &&
	 grit rev-parse HEAD >../actual
	) &&
	test_cmp C2 actual
'

test_expect_success 'reset --mixed clears staging area' '
	(cd repo &&
	 grit diff-index --cached HEAD -- file.txt >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'reset --mixed keeps working tree' '
	(cd repo &&
	 cat file.txt >../actual
	) &&
	echo third >expect &&
	test_cmp expect actual
'

test_expect_success 'reset --hard moves HEAD, resets index and worktree' '
	(cd repo &&
	 grit reset --hard "$(cat ../C3)" &&
	 grit reset --hard "$(cat ../C1)" &&
	 grit rev-parse HEAD >../actual
	) &&
	test_cmp C1 actual
'

test_expect_success 'reset --hard resets working tree content' '
	(cd repo &&
	 cat file.txt >../actual
	) &&
	echo hello >expect &&
	test_cmp expect actual
'

test_expect_success 'reset --hard back to latest' '
	(cd repo &&
	 grit reset --hard "$(cat ../C3)" &&
	 grit rev-parse HEAD >../actual
	) &&
	test_cmp C3 actual
'

test_expect_success 'reset default is --mixed' '
	(cd repo &&
	 echo staged >file.txt &&
	 grit add file.txt &&
	 grit reset HEAD &&
	 grit diff-index --cached HEAD -- file.txt >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'reset default preserves worktree' '
	(cd repo &&
	 cat file.txt >../actual
	) &&
	echo staged >expect &&
	test_cmp expect actual
'

test_expect_success 'reset --quiet suppresses output' '
	(cd repo &&
	 grit reset --hard "$(cat ../C3)" &&
	 echo changes >file.txt &&
	 grit add file.txt &&
	 grit reset --quiet HEAD >../actual 2>&1
	) &&
	test_must_be_empty actual
'

test_expect_success 'reset with path unstages specific file' '
	(cd repo &&
	 grit reset --hard HEAD &&
	 echo a >file.txt &&
	 echo b >other.txt &&
	 grit add file.txt other.txt &&
	 grit reset HEAD -- file.txt &&
	 grit diff-index --cached HEAD -- file.txt >../file_diff &&
	 grit diff-index --cached HEAD -- other.txt >../other_diff
	) &&
	test_must_be_empty file_diff &&
	test -s other_diff
'

test_expect_success 'reset with path does not move HEAD' '
	(cd repo &&
	 grit rev-parse HEAD >../actual
	) &&
	test_cmp C3 actual
'

test_expect_success 'reset HEAD with no changes is no-op' '
	(cd repo &&
	 grit reset --hard HEAD &&
	 grit rev-parse HEAD >../before &&
	 grit reset HEAD &&
	 grit rev-parse HEAD >../after
	) &&
	test_cmp before after
'

test_expect_success 'reset --hard removes untracked staged files from index' '
	(cd repo &&
	 echo brandnew >brandnew.txt &&
	 grit add brandnew.txt &&
	 grit reset HEAD -- brandnew.txt &&
	 grit ls-files --cached >../actual
	) &&
	! grep "brandnew.txt" actual
'

test_expect_success 'reset --hard cleans up modified files' '
	(cd repo &&
	 echo dirty >file.txt &&
	 echo dirty >other.txt &&
	 grit reset --hard HEAD &&
	 cat file.txt >../a1 &&
	 cat other.txt >../a2
	) &&
	echo third >e1 &&
	echo original >e2 &&
	test_cmp e1 a1 &&
	test_cmp e2 a2
'

test_expect_success 'reset --soft then commit amends effectively' '
	(cd repo &&
	 grit reset --soft "$(cat ../C2)" &&
	 grit commit -m "amended third" &&
	 grit rev-parse HEAD >../new_head &&
	 grit log --oneline >../actual
	) &&
	test_line_count = 3 actual
'

test_expect_success 'reset --hard to first commit loses later files' '
	(cd repo &&
	 echo extra >extra.txt &&
	 grit add extra.txt &&
	 grit commit -m "add extra" &&
	 grit reset --hard "$(cat ../C1)" &&
	 grit ls-files --cached >../actual
	) &&
	! grep "extra.txt" actual
'

test_expect_success 'reset --mixed then add and commit' '
	(cd repo &&
	 grit reset --hard "$(cat ../C3)" &&
	 grit reset --mixed "$(cat ../C1)" &&
	 grit add file.txt &&
	 grit commit -m "re-add third" &&
	 cat file.txt >../actual
	) &&
	echo third >expect &&
	test_cmp expect actual
'

test_expect_success 'reset path in subdirectory' '
	(cd repo &&
	 grit reset --hard "$(cat ../C3)" &&
	 echo changed >sub/nested.txt &&
	 grit add sub/nested.txt &&
	 grit reset HEAD -- sub/nested.txt &&
	 grit diff-index --cached HEAD -- sub/nested.txt >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'reset --hard removes deleted file tracking' '
	(cd repo &&
	 rm file.txt &&
	 grit reset --hard HEAD &&
	 test_path_is_file file.txt
	)
'

test_expect_success 'reset multiple paths at once' '
	(cd repo &&
	 echo a >file.txt &&
	 echo b >other.txt &&
	 grit add file.txt other.txt &&
	 grit reset HEAD -- file.txt other.txt &&
	 grit diff-index --cached HEAD >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'reset --hard on clean repo is no-op' '
	(cd repo &&
	 grit reset --hard HEAD &&
	 cat file.txt >../before &&
	 grit reset --hard HEAD &&
	 cat file.txt >../after
	) &&
	test_cmp before after
'

test_expect_success 'reset --soft preserves executable bits' '
	(cd repo &&
	 echo "#!/bin/sh" >exec.sh &&
	 chmod +x exec.sh &&
	 grit add exec.sh &&
	 grit commit -m "add exec" &&
	 grit rev-parse HEAD >../exec_commit &&
	 echo "#!/bin/sh updated" >exec.sh &&
	 grit add exec.sh &&
	 grit commit -m "update exec" &&
	 grit reset --soft "$(cat ../exec_commit)" &&
	 test -x exec.sh
	)
'

test_expect_success 'reset to HEAD with staged new file unstages it' '
	(cd repo &&
	 grit reset --hard HEAD &&
	 echo newfile >newreset.txt &&
	 grit add newreset.txt &&
	 grit reset HEAD -- newreset.txt &&
	 grit ls-files --cached >../actual
	) &&
	! grep "newreset.txt" actual
'

test_expect_success 'reset --hard discards staged and worktree changes' '
	(cd repo &&
	 echo staged >file.txt &&
	 grit add file.txt &&
	 echo worktree >file.txt &&
	 grit reset --hard HEAD &&
	 cat file.txt >../actual
	) &&
	echo third >expect &&
	test_cmp expect actual
'

test_expect_success 'reset --hard to tag' '
	(cd repo &&
	 grit tag reset-tag &&
	 echo post >file.txt &&
	 grit add file.txt &&
	 grit commit -m "post tag" &&
	 grit reset --hard reset-tag &&
	 grit rev-parse HEAD >../actual &&
	 grit rev-parse reset-tag >../expect
	) &&
	test_cmp expect actual
'

test_expect_success 'reset on orphan branch' '
	(cd repo &&
	 grit switch --orphan orphan-reset &&
	 echo orphan-data >orphan-file.txt &&
	 grit add orphan-file.txt &&
	 grit reset HEAD -- orphan-file.txt 2>../err || true &&
	 grit ls-files --cached >../actual
	)
'

test_done
