#!/bin/sh
test_description='grit rm with --quiet, --cached, --force, --dry-run, -r, --ignore-unmatch'
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
	 grit commit -m "initial")
'

test_expect_success 'rm removes file from index and working tree' '
	(cd repo &&
	 grit rm file1.txt &&
	 test_path_is_missing file1.txt &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	echo "D  file1.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore after rm test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm --quiet suppresses output' '
	(cd repo &&
	 grit rm --quiet file1.txt >../actual 2>&1) &&
	test_must_be_empty actual
'

test_expect_success 'restore after quiet test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm without --quiet shows output' '
	(cd repo &&
	 grit rm file1.txt >../actual 2>&1) &&
	grep "file1.txt" actual
'

test_expect_success 'restore after verbose rm test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm --cached removes from index but keeps working tree' '
	(cd repo &&
	 grit rm --cached file1.txt &&
	 test_path_is_file file1.txt &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	cat >expect <<-\EOF &&
	?? file1.txt
	D  file1.txt
	EOF
	sort expect >expect_sorted &&
	test_cmp expect_sorted actual
'

test_expect_success 'restore after cached test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm --dry-run shows what would be removed but does not remove' '
	(cd repo &&
	 grit rm --dry-run file1.txt >../actual 2>&1) &&
	grep "file1.txt" actual &&
	(cd repo && test_path_is_file file1.txt)
'

test_expect_success 'rm -r removes directory recursively' '
	(cd repo &&
	 grit rm -r dir &&
	 test_path_is_missing dir/nested.txt &&
	 test_path_is_missing dir/sub/deep.txt &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	cat >expect <<-\EOF &&
	D  dir/nested.txt
	D  dir/sub/deep.txt
	EOF
	sort expect >expect_sorted &&
	test_cmp expect_sorted actual
'

test_expect_success 'restore after recursive rm test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm directory without -r fails' '
	(cd repo &&
	 test_must_fail grit rm dir 2>../errmsg) &&
	grep -i "recurs\|not removing" errmsg
'

test_expect_success 'rm --ignore-unmatch exits zero for missing file' '
	(cd repo &&
	 grit rm --ignore-unmatch nonexistent.txt)
'

test_expect_success 'rm without --ignore-unmatch fails for missing file' '
	(cd repo &&
	 test_must_fail grit rm nonexistent.txt 2>../errmsg) &&
	grep -i "did not match\|pathspec\|not found\|error" errmsg
'

test_expect_success 'rm multiple files at once' '
	(cd repo &&
	 grit rm file1.txt file2.txt &&
	 test_path_is_missing file1.txt &&
	 test_path_is_missing file2.txt &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	cat >expect <<-\EOF &&
	D  file1.txt
	D  file2.txt
	EOF
	sort expect >expect_sorted &&
	test_cmp expect_sorted actual
'

test_expect_success 'restore after multi rm test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm --force removes file with local modifications' '
	(cd repo &&
	 echo modified >file1.txt &&
	 grit rm --force file1.txt &&
	 test_path_is_missing file1.txt &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	echo "D  file1.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore after force rm test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm modified file without --force fails' '
	(cd repo &&
	 echo modified >file1.txt &&
	 test_must_fail grit rm file1.txt 2>../errmsg) &&
	grep -i "local modifications\|changes\|force\|staged\|has.*change" errmsg
'

test_expect_success 'restore after failed rm test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm --cached with --dry-run is informational only' '
	(cd repo &&
	 grit rm --cached --dry-run file1.txt >../actual 2>&1) &&
	grep "file1.txt" actual &&
	(cd repo && test_path_is_file file1.txt)
'

test_expect_success 'rm --quiet with multiple files produces no output' '
	(cd repo &&
	 grit rm --quiet file1.txt file2.txt >../actual 2>&1) &&
	test_must_be_empty actual
'

test_expect_success 'restore after quiet multi rm test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm then commit removes file permanently' '
	(cd repo &&
	 grit rm file3.txt &&
	 grit commit -m "remove file3" &&
	 test_path_is_missing file3.txt &&
	 grit ls-files >../actual) &&
	! grep "file3.txt" actual
'

test_expect_success 'rm --cached then add re-tracks file' '
	(cd repo &&
	 grit rm --cached file2.txt &&
	 grit add file2.txt &&
	 grit status --porcelain | grep -v "^##" | grep "^[MADRCU]" >../actual || true) &&
	test_must_be_empty actual
'

test_expect_success 'rm -r --cached keeps directory on disk' '
	(cd repo &&
	 grit rm -r --cached dir &&
	 test_path_is_file dir/nested.txt &&
	 test_path_is_file dir/sub/deep.txt &&
	 grit status --porcelain | grep -v "^##" | grep "^D" | sort >../actual) &&
	cat >expect <<-\EOF &&
	D  dir/nested.txt
	D  dir/sub/deep.txt
	EOF
	sort expect >expect_sorted &&
	test_cmp expect_sorted actual
'

test_expect_success 'restore after cached recursive rm test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm --force --cached removes staged file keeping worktree' '
	(cd repo &&
	 echo modified >file1.txt &&
	 grit add file1.txt &&
	 grit rm --force --cached file1.txt &&
	 test_path_is_file file1.txt &&
	 grit status --porcelain | grep -v "^##" | grep "file1" >../actual) &&
	grep "file1.txt" actual
'

test_expect_success 'restore after force cached rm test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm --dry-run with -r shows recursive removals' '
	(cd repo &&
	 grit rm --dry-run -r dir >../actual 2>&1) &&
	grep "nested.txt" actual &&
	grep "deep.txt" actual
'

test_expect_success 'rm --quiet --ignore-unmatch nonexistent is silent success' '
	(cd repo &&
	 grit rm --quiet --ignore-unmatch no-such-file >../actual 2>&1) &&
	test_must_be_empty actual
'

test_expect_success 'rm file with spaces in name' '
	(cd repo &&
	 echo content >"has spaces.txt" &&
	 grit add "has spaces.txt" &&
	 grit commit -m "add spaced file" &&
	 grit rm "has spaces.txt" &&
	 test_path_is_missing "has spaces.txt" &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	grep "has spaces.txt" actual
'

test_expect_success 'restore after spaces test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm file in subdirectory by full path' '
	(cd repo &&
	 grit rm dir/nested.txt &&
	 test_path_is_missing dir/nested.txt &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	grep "D  dir/nested.txt" actual
'

test_expect_success 'restore after subdir rm test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm from subdirectory with relative path' '
	(cd repo/dir &&
	 grit rm nested.txt &&
	 test_path_is_missing nested.txt) &&
	(cd repo &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	grep "D  dir/nested.txt" actual
'

test_expect_success 'restore after subdir relative rm test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm with -C flag' '
	(grit -C repo rm file1.txt &&
	 cd repo && grit status --porcelain | grep -v "^##" >../actual) &&
	echo "D  file1.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore after -C rm test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm --cached on unmodified file does not delete from disk' '
	(cd repo &&
	 grit rm --cached file2.txt &&
	 test -f file2.txt) &&
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'rm then status shows deletion' '
	(cd repo &&
	 grit rm file1.txt &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	echo "D  file1.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'final restore' '
	(cd repo && grit reset --hard HEAD)
'

test_done
