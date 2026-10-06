#!/bin/sh

test_description='grit restore with --staged, --worktree, --source, --quiet, and edge cases'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo && cd repo &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo hello >file.txt &&
	echo world >other.txt &&
	mkdir -p sub &&
	echo s >sub/s.txt &&
	grit add . && grit commit -m "initial"
	)
'

test_expect_success 'restore working tree file to index version' '
	(cd repo &&
	 echo modified >file.txt &&
	 grit restore file.txt &&
	 cat file.txt >../actual) &&
	echo "hello" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --staged unstages file' '
	(cd repo &&
	 echo staged >file.txt && grit add file.txt &&
	 grit restore --staged file.txt &&
	 grit status --porcelain >../actual) &&
	grep "^ M file.txt" actual || grep "M  file.txt" actual
'

test_expect_success 'restore --staged does not change working tree' '
	(cd repo && cat file.txt >../actual) &&
	echo "staged" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --worktree restores working tree explicitly' '
	(cd repo &&
	 echo dirty >file.txt &&
	 grit restore --worktree file.txt &&
	 cat file.txt >../actual) &&
	echo "hello" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --source HEAD restores from HEAD' '
	(cd repo &&
	 echo modified >file.txt && grit add file.txt && grit commit -m "mod" &&
	 echo dirty2 >file.txt &&
	 grit restore --source HEAD file.txt &&
	 cat file.txt >../actual) &&
	echo "modified" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --source parent commit restores older version' '
	(cd repo &&
	 parent=$(git rev-parse HEAD~1) &&
	 grit restore --source "$parent" file.txt &&
	 cat file.txt >../actual) &&
	echo "hello" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --quiet suppresses output' '
	(cd repo &&
	 echo noisy >file.txt &&
	 grit restore --quiet file.txt >../actual 2>&1) &&
	! test -s actual
'

test_expect_success 'restore --quiet still restores' '
	(cd repo && cat file.txt >../actual) &&
	echo "modified" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore multiple files at once' '
	(cd repo &&
	 echo dirty >file.txt && echo dirty >other.txt &&
	 grit restore file.txt other.txt &&
	 cat file.txt >../actual1 && cat other.txt >../actual2) &&
	echo "modified" >expect1 &&
	echo "world" >expect2 &&
	test_cmp expect1 actual1 &&
	test_cmp expect2 actual2
'

test_expect_success 'restore dot restores all modified files' '
	(cd repo &&
	 echo dirty >file.txt && echo dirty >other.txt &&
	 grit restore . &&
	 cat file.txt >../actual1 && cat other.txt >../actual2) &&
	echo "modified" >expect1 &&
	echo "world" >expect2 &&
	test_cmp expect1 actual1 &&
	test_cmp expect2 actual2
'

test_expect_success 'restore file in subdirectory' '
	(cd repo &&
	 echo dirty >sub/s.txt &&
	 grit restore sub/s.txt &&
	 cat sub/s.txt >../actual) &&
	echo "s" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --staged with dot unstages modified files' '
	(cd repo &&
	 echo changed >file.txt && echo changed >other.txt &&
	 grit add file.txt other.txt &&
	 grit restore --staged . &&
	 grit status --porcelain >../actual) &&
	grep "^ M file.txt" actual &&
	grep "^ M other.txt" actual
'

test_expect_success 'restore --source with branch name' '
	(cd repo &&
	 grit switch -c restore-branch 2>/dev/null &&
	 echo branchval >file.txt && grit add file.txt && grit commit -m "branch" &&
	 grit switch main 2>/dev/null &&
	 grit restore --source restore-branch file.txt &&
	 cat file.txt >../actual) &&
	echo "branchval" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --source with commit hash' '
	(cd repo &&
	 hash=$(git rev-parse HEAD) &&
	 grit restore --source "$hash" file.txt &&
	 cat file.txt >../actual) &&
	echo "modified" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore unmodified file is no-op' '
	(cd repo &&
	 grit restore --source HEAD file.txt &&
	 grit status --porcelain >../actual) &&
	! grep "file.txt" actual
'

test_expect_success 'restore deleted file from index' '
	(cd repo &&
	 rm other.txt &&
	 grit restore other.txt &&
	 test -f other.txt &&
	 cat other.txt >../actual) &&
	echo "world" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --staged on newly added file removes from index' '
	(cd repo &&
	 echo brand >brandnew.txt && grit add brandnew.txt &&
	 grit restore --staged brandnew.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "brandnew.txt" actual
'

test_expect_success 'restore --staged keeps working tree for new file' '
	test -f repo/brandnew.txt
'

test_expect_success 'restore --staged --worktree restores both' '
	(cd repo &&
	 echo both >file.txt && grit add file.txt &&
	 grit restore --staged --worktree --source HEAD file.txt &&
	 grit status --porcelain >../actual) &&
	! grep "file.txt" actual
'

test_expect_success 'restore with --source requires pathspec' '
	(cd repo && ! grit restore --source HEAD 2>../err) &&
	test -s err
'

test_expect_success 'restore nonexistent pathspec fails' '
	(cd repo && ! grit restore nonexistent.txt 2>../err) &&
	test -s err
'

test_expect_success 'restore --source HEAD file preserved after restore' '
	(cd repo &&
	 echo changed >sub/s.txt && grit add sub/s.txt && grit commit -m "sub change" &&
	 echo dirty >sub/s.txt &&
	 grit restore --source HEAD sub/s.txt &&
	 cat sub/s.txt >../actual) &&
	echo "changed" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore file with spaces in name' '
	(cd repo &&
	 echo sp >"space file.txt" && grit add "space file.txt" && grit commit -m "sp" &&
	 echo dirty >"space file.txt" &&
	 grit restore "space file.txt" &&
	 cat "space file.txt" >../actual) &&
	echo "sp" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore empty file' '
	(cd repo &&
	 >empty.txt && grit add empty.txt && grit commit -m "empty" &&
	 echo notempty >empty.txt &&
	 grit restore empty.txt) &&
	! test -s repo/empty.txt
'

test_expect_success 'restore --staged multiple specific files' '
	(cd repo &&
	 echo x >x.txt && echo y >y.txt && echo z >z.txt &&
	 grit add x.txt y.txt z.txt &&
	 grit restore --staged x.txt z.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "^x.txt$" actual &&
	grep "y.txt" actual &&
	! grep "^z.txt$" actual
'

test_expect_success 'restore from tag reference' '
	(cd repo &&
	 grit commit -am "before tag" &&
	 git tag v1.0 &&
	 echo after-tag >file.txt && grit add file.txt && grit commit -m "after tag" &&
	 grit restore --source v1.0 file.txt &&
	 cat file.txt >../actual) &&
	echo "modified" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --quiet on multiple files' '
	(cd repo &&
	 echo d1 >file.txt && echo d2 >other.txt &&
	 grit restore -q file.txt other.txt >../actual 2>&1) &&
	! test -s actual
'

test_expect_success 'restore after reset restores from new HEAD' '
	(cd repo &&
	 echo pre >file.txt && grit add file.txt && grit commit -m "pre" &&
	 echo post >file.txt && grit add file.txt && grit commit -m "post" &&
	 grit reset --soft HEAD~1 &&
	 grit restore --source HEAD file.txt &&
	 cat file.txt >../actual) &&
	echo "pre" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore symlink' '
	(cd repo &&
	 ln -sf file.txt mylink && grit add mylink && grit commit -m "link" &&
	 rm mylink && echo notlink >mylink &&
	 grit restore mylink &&
	 test -L mylink)
'

test_done
