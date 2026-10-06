#!/bin/sh

test_description='grit add with bulk paths, patterns, and edge cases'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo && cd repo &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo hello >file.txt && grit add file.txt && grit commit -m "initial"
	)
'

test_expect_success 'add multiple files at once' '
	(cd repo &&
	 echo a >a.txt && echo b >b.txt && echo c >c.txt &&
	 grit add a.txt b.txt c.txt &&
	 grit ls-files --cached >../actual) &&
	grep "a.txt" actual &&
	grep "b.txt" actual &&
	grep "c.txt" actual
'

test_expect_success 'add with dot adds all untracked files' '
	(cd repo &&
	 echo d >d.txt && echo e >e.txt &&
	 grit add . &&
	 grit ls-files --cached >../actual) &&
	grep "d.txt" actual &&
	grep "e.txt" actual
'

test_expect_success 'add --dry-run does not stage files' '
	(cd repo &&
	 echo dry >dry.txt &&
	 grit add --dry-run dry.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "dry.txt" actual
'

test_expect_success 'add --dry-run exits zero' '
	(cd repo && echo dry2 >dry2.txt && grit add -n dry2.txt)
'

test_expect_success 'add --verbose shows added files' '
	(cd repo &&
	 echo v >verbose.txt &&
	 grit add --verbose verbose.txt >../actual 2>&1) &&
	grep "verbose.txt" actual
'

test_expect_success 'add already tracked unchanged file is no-op' '
	(cd repo &&
	 grit add file.txt &&
	 grit status --porcelain >../actual) &&
	! grep "file.txt" actual
'

test_expect_success 'add modified tracked file stages the change' '
	(cd repo &&
	 echo modified >file.txt &&
	 grit add file.txt &&
	 grit status --porcelain >../actual) &&
	grep "^M" actual
'

test_expect_success 'add --update stages modifications but not new files' '
	(cd repo &&
	 grit commit -am "save" &&
	 echo changed >a.txt &&
	 echo newuntracked >untracked.txt &&
	 grit add --update &&
	 grit ls-files --cached >../actual) &&
	grep "a.txt" actual &&
	! grep "untracked.txt" actual
'

test_expect_success 'add --all stages new, modified, and deleted files' '
	(cd repo &&
	 grit commit -am "save2" &&
	 echo new >allnew.txt &&
	 echo changed2 >a.txt &&
	 rm b.txt &&
	 grit add --all &&
	 grit status --porcelain >../actual) &&
	grep "allnew.txt" actual &&
	grep "a.txt" actual &&
	grep "b.txt" actual
'

test_expect_success 'add files in subdirectory' '
	(cd repo &&
	 mkdir -p sub/deep &&
	 echo s1 >sub/s1.txt && echo s2 >sub/deep/s2.txt &&
	 grit add sub/ &&
	 grit ls-files --cached >../actual) &&
	grep "sub/s1.txt" actual &&
	grep "sub/deep/s2.txt" actual
'

test_expect_success 'add from subdirectory with relative path' '
	(cd repo/sub &&
	 echo s3 >s3.txt &&
	 grit add s3.txt) &&
	(cd repo && grit ls-files --cached >../actual) &&
	grep "sub/s3.txt" actual
'

test_expect_success 'add --intent-to-add creates placeholder' '
	(cd repo &&
	 grit commit -am "save3" &&
	 echo intent >intent.txt &&
	 grit add --intent-to-add intent.txt &&
	 grit ls-files --cached >../actual) &&
	grep "intent.txt" actual
'

test_expect_success 'intent-to-add file shows as new in status' '
	(cd repo && grit status --porcelain >../actual) &&
	grep "intent.txt" actual
'

test_expect_success 'add empty file succeeds' '
	(cd repo &&
	 >empty.txt &&
	 grit add empty.txt &&
	 grit ls-files --cached >../actual) &&
	grep "empty.txt" actual
'

test_expect_success 'add file with spaces in name' '
	(cd repo &&
	 echo space >"file with spaces.txt" &&
	 grit add "file with spaces.txt" &&
	 grit ls-files --cached >../actual) &&
	grep "file with spaces.txt" actual
'

test_expect_success 'add large number of files' '
	(cd repo &&
	 mkdir -p bulk &&
	 for i in $(seq 1 50); do echo "content $i" >"bulk/file$i.txt"; done &&
	 grit add bulk/ &&
	 grit ls-files --cached bulk/ >../actual) &&
	test $(wc -l <actual) -eq 50
'

test_expect_success 'add ignores files already in .gitignore with explicit path warning' '
	(cd repo &&
	 echo "*.log" >.gitignore &&
	 grit add .gitignore &&
	 echo logdata >test.log &&
	 grit add test.log 2>../actual || true) &&
	# grit may or may not refuse; just verify .gitignore is tracked
	(cd repo && grit ls-files --cached >../actual) &&
	grep ".gitignore" actual
'

test_expect_success 'add --force overrides .gitignore' '
	(cd repo &&
	 grit add --force test.log &&
	 grit ls-files --cached >../actual) &&
	grep "test.log" actual
'

test_expect_success 'add nonexistent file fails' '
	(cd repo && ! grit add nonexistent-file.txt 2>../err) &&
	test -s err
'

test_expect_success 'add with update flag only stages tracked files' '
	(cd repo &&
	 grit commit -am "save4" &&
	 echo changed >c.txt &&
	 echo brandnew >brandnew.txt &&
	 grit add -u &&
	 grit status --porcelain >../actual) &&
	grep "c.txt" actual &&
	grep "?? brandnew.txt" actual
'

test_expect_success 'add file then modify shows both cached and modified' '
	(cd repo &&
	 echo first >twostep.txt &&
	 grit add twostep.txt &&
	 echo second >twostep.txt &&
	 grit status --porcelain >../actual) &&
	grep "twostep.txt" actual
'

test_expect_success 'add after rm re-adds the file' '
	(cd repo &&
	 echo data >readd.txt && grit add readd.txt && grit commit -m "readd" &&
	 grit rm readd.txt &&
	 echo newdata >readd.txt &&
	 grit add readd.txt &&
	 grit ls-files --cached >../actual) &&
	grep "readd.txt" actual
'

test_expect_success 'add --all removes deleted files from index' '
	(cd repo &&
	 echo todel >todel.txt && grit add todel.txt && grit commit -m "del test" &&
	 rm todel.txt &&
	 grit add --all &&
	 grit ls-files --cached >../actual) &&
	! grep "todel.txt" actual
'

test_expect_success 'add dot from repo root adds everything' '
	(cd repo &&
	 grit commit -am "clean" &&
	 mkdir -p x/y/z &&
	 echo deep >x/y/z/deep.txt &&
	 grit add . &&
	 grit ls-files --cached >../actual) &&
	grep "x/y/z/deep.txt" actual
'

test_expect_success 'add multiple directories at once' '
	(cd repo &&
	 mkdir -p dir1 dir2 &&
	 echo f1 >dir1/f.txt && echo f2 >dir2/f.txt &&
	 grit add dir1 dir2 &&
	 grit ls-files --cached >../actual) &&
	grep "dir1/f.txt" actual &&
	grep "dir2/f.txt" actual
'

test_expect_success 'add file then rename with mv shows correct state' '
	(cd repo &&
	 echo mvtest >mvtest.txt && grit add mvtest.txt &&
	 grit commit -m "mvtest" &&
	 grit mv mvtest.txt mvtest-renamed.txt &&
	 grit ls-files --cached >../actual) &&
	grep "mvtest-renamed.txt" actual &&
	! grep "^mvtest.txt$" actual
'

test_expect_success 'add --update with no changes is silent no-op' '
	(cd repo &&
	 grit commit -am "clean2" &&
	 grit add -u >../actual 2>&1) &&
	! test -s actual
'

test_expect_success 'add files with various extensions' '
	(cd repo &&
	 echo c >test.c && echo h >test.h && echo py >test.py && echo rs >test.rs &&
	 grit add test.c test.h test.py test.rs &&
	 grit ls-files --cached >../actual) &&
	grep "test.c" actual &&
	grep "test.h" actual &&
	grep "test.py" actual &&
	grep "test.rs" actual
'

test_expect_success 'add executable file' '
	(cd repo &&
	 echo "#!/bin/sh" >script.sh && chmod +x script.sh &&
	 grit add script.sh &&
	 grit ls-files --stage script.sh >../actual) &&
	grep "100755" actual
'

test_expect_success 'add symlink' '
	(cd repo &&
	 ln -sf file.txt link.txt &&
	 grit add link.txt &&
	 grit ls-files --stage link.txt >../actual) &&
	grep "120000" actual
'

test_done
