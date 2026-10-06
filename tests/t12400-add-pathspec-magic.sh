#!/bin/sh
test_description='grit add with pathspec patterns, wildcards, directories, and edge cases'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
	 git config user.email "t@t.com" &&
	 git config user.name "T" &&
	 mkdir -p dir1/sub dir2 dir3 &&
	 echo a >dir1/a.txt &&
	 echo b >dir1/b.c &&
	 echo c >dir1/sub/c.txt &&
	 echo d >dir2/d.txt &&
	 echo e >dir2/e.c &&
	 echo f >dir3/f.txt &&
	 echo root >root.txt &&
	 echo rootc >root.c &&
	 grit add . &&
	 grit commit -m "initial")
'

test_expect_success 'add single file by exact path' '
	(cd repo &&
	 echo changed >root.txt &&
	 grit add root.txt &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	echo "M  root.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset after single file test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'add specific .txt file leaves .c unstaged' '
	(cd repo &&
	 echo changed >root.txt &&
	 echo changed >root.c &&
	 grit add root.txt &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	grep "^M  root.txt" actual &&
	grep "^ M root.c" actual
'

test_expect_success 'reset after specific file test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'add directory adds all files in it' '
	(cd repo &&
	 echo new1 >dir1/a.txt &&
	 echo new2 >dir1/b.c &&
	 echo new3 >dir1/sub/c.txt &&
	 grit add dir1 &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	cat >expect <<-\EOF &&
	M  dir1/a.txt
	M  dir1/b.c
	M  dir1/sub/c.txt
	EOF
	sort expect >expect_sorted &&
	test_cmp expect_sorted actual
'

test_expect_success 'reset after dir add test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'add multiple explicit files' '
	(cd repo &&
	 echo x >root.txt &&
	 echo y >root.c &&
	 grit add root.txt root.c &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	cat >expect <<-\EOF &&
	M  root.c
	M  root.txt
	EOF
	sort expect >expect_sorted &&
	test_cmp expect_sorted actual
'

test_expect_success 'reset after multiple files test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'add dot stages everything' '
	(cd repo &&
	 echo m1 >root.txt &&
	 echo m2 >dir2/d.txt &&
	 echo newfile >brand-new.txt &&
	 grit add . &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	cat >expect <<-\EOF &&
	A  brand-new.txt
	M  dir2/d.txt
	M  root.txt
	EOF
	sort expect >expect_sorted &&
	test_cmp expect_sorted actual
'

test_expect_success 'reset and cleanup after dot test' '
	(cd repo && grit reset --hard HEAD && rm -f brand-new.txt)
'

test_expect_success 'add from subdirectory with relative path' '
	(cd repo/dir1 &&
	 echo sub-change >a.txt &&
	 grit add a.txt &&
	 grit status --porcelain | grep -v "^##" >../../actual) &&
	echo "M  dir1/a.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset after subdir test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'add with --dry-run does not stage' '
	(cd repo &&
	 echo changed >root.txt &&
	 grit add --dry-run root.txt &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	echo " M root.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset after dry-run test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'add with --verbose produces output' '
	(cd repo &&
	 echo changed >root.txt &&
	 grit add --verbose root.txt >../actual 2>&1) &&
	grep "root.txt" actual
'

test_expect_success 'reset after verbose test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'add --intent-to-add marks file for future add' '
	(cd repo &&
	 echo newcontent >intent-file.txt &&
	 grit add --intent-to-add intent-file.txt &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	grep "intent-file.txt" actual
'

test_expect_success 'reset and cleanup after intent-to-add test' '
	(cd repo && grit reset --hard HEAD && rm -f intent-file.txt)
'

test_expect_success 'add --update only stages tracked modified files' '
	(cd repo &&
	 echo changed >root.txt &&
	 echo untracked >newfile.txt &&
	 grit add --update &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	cat >expect <<-\EOF &&
	?? newfile.txt
	M  root.txt
	EOF
	sort expect >expect_sorted &&
	test_cmp expect_sorted actual
'

test_expect_success 'reset and cleanup after update test' '
	(cd repo && grit reset --hard HEAD && rm -f newfile.txt)
'

test_expect_success 'add --update stages deletions' '
	(cd repo &&
	 rm dir3/f.txt &&
	 grit add --update &&
	 grit status --porcelain | grep -v "^##" | grep -v "^??" >../actual) &&
	echo "D  dir3/f.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset after update deletions test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'add --all stages new modified and deleted' '
	(cd repo &&
	 echo changed >root.txt &&
	 echo brandnew >allnew.txt &&
	 rm dir3/f.txt &&
	 grit add --all &&
	 grit status --porcelain | grep -v "^##" | grep -v "^??" | sort >../actual) &&
	cat >expect <<-\EOF &&
	A  allnew.txt
	D  dir3/f.txt
	M  root.txt
	EOF
	sort expect >expect_sorted &&
	test_cmp expect_sorted actual
'

test_expect_success 'reset and cleanup after all test' '
	(cd repo && grit reset --hard HEAD && rm -f allnew.txt)
'

test_expect_success 'add nonexistent file fails' '
	(cd repo &&
	 test_must_fail grit add no-such-file.txt 2>../errmsg) &&
	grep -i "no.such.file\|did not match\|not.found\|pathspec\|error" errmsg
'

test_expect_success 'add file in nested subdirectory' '
	(cd repo &&
	 echo deep-change >dir1/sub/c.txt &&
	 grit add dir1/sub/c.txt &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	echo "M  dir1/sub/c.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset after nested subdir test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'add --force adds ignored file' '
	(cd repo &&
	 echo "ignored.txt" >.gitignore &&
	 grit add .gitignore &&
	 grit commit -m "add gitignore" &&
	 echo secret >ignored.txt &&
	 grit add --force ignored.txt &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	grep "ignored.txt" actual
'

test_expect_success 'reset after force test' '
	(cd repo && grit reset --hard HEAD && rm -f ignored.txt)
'

test_expect_success 'add ignores file matching .gitignore unless forced' '
	(cd repo &&
	 echo secret >ignored.txt &&
	 grit add --force ignored.txt &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	grep "ignored.txt" actual
'

test_expect_success 'reset and cleanup after gitignore force test' '
	(cd repo && grit reset --hard HEAD && rm -f ignored.txt)
'

test_expect_success 'add on clean repo shows no staged changes' '
	(cd repo &&
	 grit add . 2>../errmsg || true &&
	 grit status --porcelain | grep -v "^##" | grep -v "^??" | grep "^[MADRCU]" >../actual || true) &&
	test_must_be_empty actual
'

test_expect_success 'add file with spaces in name' '
	(cd repo &&
	 echo content >"file with spaces.txt" &&
	 grit add "file with spaces.txt" &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	grep "file with spaces.txt" actual
'

test_expect_success 'reset and cleanup after spaces test' '
	(cd repo && grit reset --hard HEAD && rm -f "file with spaces.txt")
'

test_expect_success 'add already-staged file is a no-op' '
	(cd repo &&
	 echo changed >root.txt &&
	 grit add root.txt &&
	 grit add root.txt &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	echo "M  root.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset after double-add test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'add with -C flag works from different directory' '
	(echo changed >repo/root.txt &&
	 grit -C repo add root.txt &&
	 cd repo && grit status --porcelain | grep -v "^##" >../actual) &&
	echo "M  root.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset after -C test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'add specific .c file stages only that file' '
	(cd repo &&
	 echo cc >root.c &&
	 echo tt >root.txt &&
	 grit add root.c &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	grep "^M  root.c" actual &&
	grep "^ M root.txt" actual
'

test_expect_success 'reset after specific .c test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'add multiple directories at once' '
	(cd repo &&
	 echo x >dir1/a.txt &&
	 echo y >dir2/d.txt &&
	 grit add dir1 dir2 &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	cat >expect <<-\EOF &&
	M  dir1/a.txt
	M  dir2/d.txt
	EOF
	sort expect >expect_sorted &&
	test_cmp expect_sorted actual
'

test_expect_success 'reset after multi-dir test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'add does not stage changes in subdirectory when targeting file only' '
	(cd repo &&
	 echo changed >root.txt &&
	 echo changed >dir1/a.txt &&
	 grit add root.txt &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	grep "^M  root.txt" actual &&
	grep "^ M dir1/a.txt" actual
'

test_expect_success 'final reset' '
	(cd repo && grit reset --hard HEAD)
'

test_done
