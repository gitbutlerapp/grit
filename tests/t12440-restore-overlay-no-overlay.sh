#!/bin/sh
test_description='grit restore: --staged, --worktree, --source, --quiet, --ignore-unmerged'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
	 git config user.email "t@t.com" &&
	 git config user.name "T" &&
	 mkdir -p dir/sub &&
	 echo a >file1.txt &&
	 echo b >file2.txt &&
	 echo c >file3.txt &&
	 echo d >dir/nested.txt &&
	 echo e >dir/sub/deep.txt &&
	 grit add . &&
	 grit commit -m "initial" &&
	 echo a2 >file1.txt &&
	 echo b2 >file2.txt &&
	 echo c2 >file3.txt &&
	 grit add . &&
	 grit commit -m "second")
'

test_expect_success 'restore worktree file to index version (default)' '
	(cd repo &&
	 echo modified >file1.txt &&
	 grit restore file1.txt &&
	 cat file1.txt >../actual) &&
	echo "a2" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --worktree explicitly restores working tree' '
	(cd repo &&
	 echo modified >file2.txt &&
	 grit restore --worktree file2.txt &&
	 cat file2.txt >../actual) &&
	echo "b2" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --staged unstages file' '
	(cd repo &&
	 echo staged-change >file1.txt &&
	 grit add file1.txt &&
	 grit restore --staged file1.txt &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	echo " M file1.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset after staged test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore --staged --worktree restores both' '
	(cd repo &&
	 echo changed >file1.txt &&
	 grit add file1.txt &&
	 grit restore --staged --worktree file1.txt &&
	 cat file1.txt >../actual &&
	 grit status --porcelain | grep -v "^##" | grep "file1" >../status_actual || true) &&
	echo "a2" >expect &&
	test_cmp expect actual &&
	test_must_be_empty status_actual
'

test_expect_success 'restore --source with resolved hash restores from older commit' '
	(cd repo &&
	 parent=$(grit rev-parse HEAD~1) &&
	 grit restore --source "$parent" file1.txt &&
	 cat file1.txt >../actual) &&
	echo "a" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset after source test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore --source with --staged puts old content in index' '
	(cd repo &&
	 parent=$(grit rev-parse HEAD~1) &&
	 grit restore --source "$parent" --staged file1.txt &&
	 grit diff --cached >../actual) &&
	grep "^-a2" actual &&
	grep "^+a$" actual
'

test_expect_success 'reset after source staged test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore dot restores all modified files' '
	(cd repo &&
	 echo x >file1.txt &&
	 echo y >file2.txt &&
	 grit restore . &&
	 cat file1.txt >../actual1 &&
	 cat file2.txt >../actual2) &&
	echo "a2" >expect1 &&
	echo "b2" >expect2 &&
	test_cmp expect1 actual1 &&
	test_cmp expect2 actual2
'

test_expect_success 'restore file in subdirectory' '
	(cd repo &&
	 echo changed >dir/nested.txt &&
	 grit restore dir/nested.txt &&
	 cat dir/nested.txt >../actual) &&
	echo "d" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore from subdirectory with relative path' '
	(cd repo/dir &&
	 echo changed >nested.txt &&
	 grit restore nested.txt &&
	 cat nested.txt >../../actual) &&
	echo "d" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore deeply nested file' '
	(cd repo &&
	 echo changed >dir/sub/deep.txt &&
	 grit restore dir/sub/deep.txt &&
	 cat dir/sub/deep.txt >../actual) &&
	echo "e" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --staged on multiple files' '
	(cd repo &&
	 echo x >file1.txt &&
	 echo y >file2.txt &&
	 grit add file1.txt file2.txt &&
	 grit restore --staged file1.txt file2.txt &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	cat >expect <<-\EOF &&
	 M file1.txt
	 M file2.txt
	EOF
	sort expect >expect_sorted &&
	test_cmp expect_sorted actual
'

test_expect_success 'reset after multi staged test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore nonexistent path fails' '
	(cd repo &&
	 test_must_fail grit restore nonexistent.txt 2>../errmsg) &&
	grep -i "error\|pathspec\|did not match\|not found" errmsg
'

test_expect_success 'restore --quiet suppresses output' '
	(cd repo &&
	 echo changed >file1.txt &&
	 grit restore --quiet file1.txt >../actual 2>&1 &&
	 cat file1.txt >../content_actual) &&
	echo "a2" >content_expect &&
	test_cmp content_expect content_actual
'

test_expect_success 'restore deleted file from index' '
	(cd repo &&
	 rm file1.txt &&
	 grit restore file1.txt &&
	 test_path_is_file file1.txt &&
	 cat file1.txt >../actual) &&
	echo "a2" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --source with hash on multiple files' '
	(cd repo &&
	 parent=$(grit rev-parse HEAD~1) &&
	 grit restore --source "$parent" file1.txt file2.txt &&
	 cat file1.txt >../actual1 &&
	 cat file2.txt >../actual2) &&
	echo "a" >expect1 &&
	echo "b" >expect2 &&
	test_cmp expect1 actual1 &&
	test_cmp expect2 actual2
'

test_expect_success 'reset after multi source test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore --staged on file that has no staged changes is a no-op' '
	(cd repo &&
	 grit restore --staged file1.txt 2>../errmsg || true &&
	 grit status --porcelain | grep -v "^##" | grep "file1" >../actual || true) &&
	test_must_be_empty actual
'

test_expect_success 'restore with -C flag' '
	(echo changed >repo/file1.txt &&
	 grit -C repo restore file1.txt &&
	 cat repo/file1.txt >actual) &&
	echo "a2" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore --source tag works' '
	(cd repo &&
	 parent=$(grit rev-parse HEAD~1) &&
	 grit tag v1.0 "$parent" &&
	 grit restore --source v1.0 file1.txt &&
	 cat file1.txt >../actual) &&
	echo "a" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset after tag source test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore worktree then file status is clean' '
	(cd repo &&
	 echo dirty >file1.txt &&
	 grit restore file1.txt &&
	 grit status --porcelain | grep -v "^##" | grep "file1" >../actual || true) &&
	test_must_be_empty actual
'

test_expect_success 'restore --staged then worktree still has changes' '
	(cd repo &&
	 echo staged >file1.txt &&
	 grit add file1.txt &&
	 grit restore --staged file1.txt &&
	 cat file1.txt >../actual) &&
	echo "staged" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset after staged-only restore' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore individual files in directory' '
	(cd repo &&
	 echo x >dir/nested.txt &&
	 echo y >dir/sub/deep.txt &&
	 grit restore dir/nested.txt dir/sub/deep.txt &&
	 cat dir/nested.txt >../actual1 &&
	 cat dir/sub/deep.txt >../actual2) &&
	echo "d" >expect1 &&
	echo "e" >expect2 &&
	test_cmp expect1 actual1 &&
	test_cmp expect2 actual2
'

test_expect_success 'restore --source with full commit hash' '
	(cd repo &&
	 hash=$(grit rev-parse HEAD~1) &&
	 grit restore --source "$hash" file1.txt &&
	 cat file1.txt >../actual) &&
	echo "a" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset after hash source test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'restore only affects specified file' '
	(cd repo &&
	 echo x >file1.txt &&
	 echo y >file2.txt &&
	 grit restore file1.txt &&
	 cat file1.txt >../actual1 &&
	 cat file2.txt >../actual2) &&
	echo "a2" >expect1 &&
	echo "y" >expect2 &&
	test_cmp expect1 actual1 &&
	test_cmp expect2 actual2
'

test_expect_success 'final reset' '
	(cd repo && grit reset --hard HEAD)
'

test_done
