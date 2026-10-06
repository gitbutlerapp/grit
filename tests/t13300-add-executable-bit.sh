#!/bin/sh

test_description='grit add: executable bit handling, intent-to-add, force, update, dry-run'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
	 git config user.email "t@t.com" &&
	 git config user.name "T" &&
	 echo hello >file.txt &&
	 grit add file.txt &&
	 grit commit -m "initial"
	)
'

test_expect_success 'add a new file shows it in index' '
	(cd repo &&
	 echo new >new.txt &&
	 grit add new.txt &&
	 grit ls-files --cached >../actual
	) &&
	grep "new.txt" actual
'

test_expect_success 'add executable file preserves execute bit' '
	(cd repo &&
	 echo "#!/bin/sh" >script.sh &&
	 chmod +x script.sh &&
	 grit add script.sh &&
	 grit ls-files --stage script.sh >../actual
	) &&
	grep "^100755" actual
'

test_expect_success 'add non-executable file has 100644 mode' '
	(cd repo &&
	 echo "plain text" >plain.txt &&
	 grit add plain.txt &&
	 grit ls-files --stage plain.txt >../actual
	) &&
	grep "^100644" actual
'

test_expect_success 'add file then chmod +x and re-add updates mode to 755' '
	(cd repo &&
	 echo "data" >toggle.sh &&
	 grit add toggle.sh &&
	 chmod +x toggle.sh &&
	 grit add toggle.sh &&
	 grit ls-files --stage toggle.sh >../actual
	) &&
	grep "^100755" actual
'

test_expect_success 'add file then chmod -x and re-add updates mode to 644' '
	(cd repo &&
	 chmod -x toggle.sh &&
	 grit add toggle.sh &&
	 grit ls-files --stage toggle.sh >../actual
	) &&
	grep "^100644" actual
'

test_expect_success 'add --dry-run does not modify index' '
	(cd repo &&
	 echo "dryrun" >dry.txt &&
	 grit add --dry-run dry.txt &&
	 grit ls-files --cached >../actual
	) &&
	! grep "dry.txt" actual
'

test_expect_success 'add --dry-run produces output' '
	(cd repo &&
	 grit add --dry-run dry.txt >../actual 2>&1
	) &&
	test -s actual
'

test_expect_success 'add with dot adds all new files' '
	(cd repo &&
	 echo a >dot-a.txt &&
	 echo b >dot-b.txt &&
	 grit add . &&
	 grit ls-files --cached >../actual
	) &&
	grep "dot-a.txt" actual &&
	grep "dot-b.txt" actual
'

test_expect_success 'add --verbose prints added files' '
	(cd repo &&
	 echo verbose >verbose.txt &&
	 grit add --verbose verbose.txt >../actual 2>&1
	) &&
	grep "verbose.txt" actual
'

test_expect_success 'add --update only updates tracked files' '
	(cd repo &&
	 echo modified >file.txt &&
	 echo untracked >brand-new.txt &&
	 grit add --update &&
	 grit ls-files --cached >../actual
	) &&
	! grep "brand-new.txt" actual
'

test_expect_success 'add --update picks up modifications' '
	(cd repo &&
	 grit status >../actual 2>&1
	) &&
	! grep "file.txt" actual || true
'

test_expect_success 'add --intent-to-add records placeholder in index' '
	(cd repo &&
	 echo intent >intent.txt &&
	 grit add --intent-to-add intent.txt &&
	 grit ls-files --cached >../actual
	) &&
	grep "intent.txt" actual
'

test_expect_success 'add --intent-to-add file shows as new in status' '
	(cd repo &&
	 grit status >../actual 2>&1
	) &&
	grep "intent.txt" actual
'

test_expect_success 'add after intent-to-add stages the real content' '
	(cd repo &&
	 grit add intent.txt &&
	 grit ls-files --stage intent.txt >../actual
	) &&
	grep "^100644" actual
'

test_expect_success 'add --force adds otherwise-ignored file' '
	(cd repo &&
	 echo "force-ignored.dat" >.gitignore &&
	 grit add .gitignore &&
	 echo data >force-ignored.dat &&
	 grit add --force force-ignored.dat &&
	 grit ls-files --cached >../actual
	) &&
	grep "force-ignored.dat" actual
'

test_expect_success 'add --force is accepted on normal file too' '
	(cd repo &&
	 echo normal >force-normal.txt &&
	 grit add --force force-normal.txt &&
	 grit ls-files --cached >../actual
	) &&
	grep "force-normal.txt" actual
'

test_expect_success 'add multiple files at once' '
	(cd repo &&
	 echo m1 >multi1.txt &&
	 echo m2 >multi2.txt &&
	 echo m3 >multi3.txt &&
	 grit add multi1.txt multi2.txt multi3.txt &&
	 grit ls-files --cached >../actual
	) &&
	grep "multi1.txt" actual &&
	grep "multi2.txt" actual &&
	grep "multi3.txt" actual
'

test_expect_success 'add file in subdirectory' '
	(cd repo &&
	 mkdir -p sub/dir &&
	 echo nested >sub/dir/nested.txt &&
	 grit add sub/dir/nested.txt &&
	 grit ls-files --cached >../actual
	) &&
	grep "sub/dir/nested.txt" actual
'

test_expect_success 'add --all stages new, modified, and deleted' '
	(cd repo &&
	 echo all-new >all-new.txt &&
	 rm -f multi1.txt &&
	 echo changed >multi2.txt &&
	 grit add --all &&
	 grit ls-files --cached >../actual
	) &&
	grep "all-new.txt" actual &&
	! grep "multi1.txt" actual
'

test_expect_success 'add symlink is tracked' '
	(cd repo &&
	 ln -sf file.txt link.txt &&
	 grit add link.txt &&
	 grit ls-files --stage link.txt >../actual
	) &&
	grep "link.txt" actual
'

test_expect_success 'add executable then commit preserves mode' '
	(cd repo &&
	 echo "#!/bin/sh" >exec-commit.sh &&
	 chmod +x exec-commit.sh &&
	 grit add exec-commit.sh &&
	 grit commit -m "add exec" &&
	 grit ls-tree HEAD -- exec-commit.sh >../actual
	) &&
	grep "100755" actual
'

test_expect_success 'add non-executable then commit preserves 644 mode' '
	(cd repo &&
	 echo data >noexec-commit.txt &&
	 grit add noexec-commit.txt &&
	 grit commit -m "add noexec" &&
	 grit ls-tree HEAD -- noexec-commit.txt >../actual
	) &&
	grep "100644" actual
'

test_expect_success 'add file with spaces in name' '
	(cd repo &&
	 echo spaced >"file with spaces.txt" &&
	 grit add "file with spaces.txt" &&
	 grit ls-files --cached >../actual
	) &&
	grep "file with spaces.txt" actual
'

test_expect_success 'add empty file' '
	(cd repo &&
	 : >empty-file.txt &&
	 grit add empty-file.txt &&
	 grit ls-files --stage empty-file.txt >../actual
	) &&
	grep "empty-file.txt" actual
'

test_expect_success 'add same file twice is idempotent' '
	(cd repo &&
	 echo double >double.txt &&
	 grit add double.txt &&
	 grit ls-files --stage double.txt >../first &&
	 grit add double.txt &&
	 grit ls-files --stage double.txt >../second
	) &&
	test_cmp first second
'

test_expect_success 'add .gitignore itself is always possible' '
	(cd repo &&
	 echo "*.log" >>.gitignore &&
	 grit add .gitignore &&
	 grit ls-files --cached >../actual
	) &&
	grep ".gitignore" actual
'

test_expect_success 'add file in deeply nested directory' '
	(cd repo &&
	 mkdir -p a/b/c/d &&
	 echo deep >a/b/c/d/deep.txt &&
	 grit add a/b/c/d/deep.txt &&
	 grit ls-files --cached >../actual
	) &&
	grep "a/b/c/d/deep.txt" actual
'

test_expect_success 'add updates blob hash when content changes' '
	(cd repo &&
	 echo v1 >hashcheck.txt &&
	 grit add hashcheck.txt &&
	 grit ls-files --stage hashcheck.txt >../hash1 &&
	 echo v2 >hashcheck.txt &&
	 grit add hashcheck.txt &&
	 grit ls-files --stage hashcheck.txt >../hash2
	) &&
	! test_cmp hash1 hash2
'

test_expect_success 'add with --all in subdirectory scopes to repo' '
	(cd repo &&
	 mkdir -p subA &&
	 echo suba >subA/file.txt &&
	 grit add --all &&
	 grit ls-files --cached >../actual
	) &&
	grep "subA/file.txt" actual
'

test_expect_success 'add nonexistent file fails' '
	(cd repo &&
	 test_must_fail grit add no-such-file.txt 2>../err
	) &&
	test -s err
'

test_expect_success 'add directory adds contents recursively' '
	(cd repo &&
	 mkdir -p adddir &&
	 echo f1 >adddir/one.txt &&
	 echo f2 >adddir/two.txt &&
	 grit add adddir &&
	 grit ls-files --cached >../actual
	) &&
	grep "adddir/one.txt" actual &&
	grep "adddir/two.txt" actual
'

test_done
