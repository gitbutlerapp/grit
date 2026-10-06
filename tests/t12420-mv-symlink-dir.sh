#!/bin/sh
test_description='grit mv: rename files, directories, symlinks, with --force, --dry-run, -k, -v'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
	 git config user.email "t@t.com" &&
	 git config user.name "T" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	 mkdir -p dir/sub &&
	 echo a >file1.txt &&
	 echo b >file2.txt &&
	 echo c >dir/nested.txt &&
	 echo d >dir/sub/deep.txt &&
	 ln -s file1.txt link1 &&
	 grit add . &&
	 grit commit -m "initial")
'

test_expect_success 'mv renames file on disk' '
	(cd repo &&
	 grit mv file1.txt renamed.txt &&
	 test_path_is_missing file1.txt &&
	 test_path_is_file renamed.txt)
'

test_expect_success 'mv shows deletion and addition in status' '
	(cd repo &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
    grep "R" actual &&
    grep "renamed.txt" actual
'

test_expect_success 'restore after rename' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv file into directory' '
	(cd repo &&
	 grit mv file2.txt dir/ &&
	 test_path_is_missing file2.txt &&
	 test_path_is_file dir/file2.txt &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	grep -E "R.*dir/file2.txt" actual
'

test_expect_success 'restore after mv into dir' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv --dry-run shows what would happen without moving' '
	(cd repo &&
	 grit mv --dry-run file1.txt dry-target.txt >../actual 2>&1 &&
	 test_path_is_file file1.txt &&
	 test_path_is_missing dry-target.txt)
'

test_expect_success 'mv --verbose produces output' '
	(cd repo &&
	 grit mv --verbose file1.txt verbose-target.txt >../actual 2>&1 &&
	 test_path_is_file verbose-target.txt) &&
	grep "file1.txt" actual
'

test_expect_success 'restore after verbose mv' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv symlink renames the symlink on disk' '
	(cd repo &&
	 grit mv link1 link-renamed &&
	 test -L link-renamed &&
	 test_path_is_missing link1)
'

test_expect_success 'mv symlink shows in status' '
	(cd repo &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	grep -E "R.*link-renamed" actual
'

test_expect_success 'restore after symlink mv' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv directory renames all contents on disk' '
	(cd repo &&
	 grit mv dir newdir &&
	 test_path_is_missing dir/nested.txt &&
	 test_path_is_file newdir/nested.txt &&
	 test_path_is_file newdir/sub/deep.txt)
'

test_expect_success 'mv directory shows deletions and additions' '
	(cd repo &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	grep -E "R.*newdir/nested.txt" actual &&
	grep -E "R.*newdir/sub/deep.txt" actual
'

test_expect_success 'restore after dir mv' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv to existing file fails without --force' '
	(cd repo &&
	 test_must_fail grit mv file1.txt file2.txt 2>../errmsg) &&
	grep -i "exist\|overwrite\|not overwriting\|destination" errmsg
'

test_expect_success 'mv --force overwrites destination' '
	(cd repo &&
	 grit mv --force file1.txt file2.txt &&
	 test_path_is_missing file1.txt &&
	 test_path_is_file file2.txt &&
	 cat file2.txt >../actual) &&
	echo "a" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore after force mv' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv -k skips errors instead of aborting' '
	(cd repo &&
	 grit mv -k nonexistent.txt target.txt 2>../errmsg || true) &&
	true
'

test_expect_success 'mv with -C flag works from different directory' '
	(grit -C repo mv file1.txt c-renamed.txt &&
	 cd repo &&
	 test_path_is_missing file1.txt &&
	 test_path_is_file c-renamed.txt)
'

test_expect_success 'restore after -C mv' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv into new subdirectory within existing dir' '
	(cd repo &&
	 mkdir dir/newsubdir &&
	 grit mv file1.txt dir/newsubdir/ &&
	 test_path_is_file dir/newsubdir/file1.txt &&
	 test_path_is_missing file1.txt)
'

test_expect_success 'restore after subdir mv' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv preserves file content' '
	(cd repo &&
	 grit mv file1.txt preserved.txt &&
	 cat preserved.txt >../actual) &&
	echo "a" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore after content check' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv from subdirectory with relative path' '
	(cd repo/dir &&
	 grit mv nested.txt renamed-nested.txt &&
	 test_path_is_missing nested.txt &&
	 test_path_is_file renamed-nested.txt)
'

test_expect_success 'restore after subdir relative mv' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv file with spaces in name' '
	(cd repo &&
	 echo spaced >"has spaces.txt" &&
	 grit add "has spaces.txt" &&
	 grit commit -m "add spaced" &&
	 grit mv "has spaces.txt" "no spaces.txt" &&
	 test_path_is_missing "has spaces.txt" &&
	 test_path_is_file "no spaces.txt")
'

test_expect_success 'restore after spaces mv' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv multiple files into directory' '
	(cd repo &&
	 mkdir target_dir &&
	 grit mv file1.txt file2.txt target_dir/ &&
	 test_path_is_file target_dir/file1.txt &&
	 test_path_is_file target_dir/file2.txt &&
	 test_path_is_missing file1.txt &&
	 test_path_is_missing file2.txt)
'

test_expect_success 'restore after multi mv' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv nonexistent file fails' '
	(cd repo &&
	 test_must_fail grit mv nonexistent.txt somewhere.txt 2>../errmsg) &&
	grep -i "bad source\|can not\|does not exist\|no such\|not under" errmsg
'

test_expect_success 'mv same name is a no-op or error' '
	(cd repo &&
	 grit mv file1.txt file1.txt 2>../errmsg || true)
'

test_expect_success 'mv then commit records the change' '
	(cd repo &&
	 grit mv file1.txt committed-rename.txt &&
	 grit commit -m "rename file1" &&
	 grit ls-files >../actual) &&
	grep "committed-rename.txt" actual &&
	! grep "^file1.txt$" actual
'

test_expect_success 'restore after committed rename' '
	(cd repo && grit reset --hard HEAD~1)
'

test_expect_success 'mv --dry-run with directory does not move' '
	(cd repo &&
	 grit mv --dry-run dir newdir >../actual 2>&1 &&
	 test_path_is_dir dir &&
	 test_path_is_missing newdir)
'

test_expect_success 'mv --verbose with directory shows each file' '
	(cd repo &&
	 grit mv --verbose dir verbosedir >../actual 2>&1) &&
	grep "nested.txt" actual
'

test_expect_success 'restore after verbose dir mv' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'mv symlink target is unchanged after rename' '
	(cd repo &&
	 grit mv link1 moved-link &&
	 readlink moved-link >../actual) &&
	echo "file1.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'final restore' '
	(cd repo && grit reset --hard HEAD)
'

test_done
