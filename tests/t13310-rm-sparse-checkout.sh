#!/bin/sh

test_description='grit rm: cached, force, recursive, dry-run, quiet, ignore-unmatch'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
	 git config user.email "t@t.com" &&
	 git config user.name "T" &&
	 echo hello >file.txt &&
	 echo world >other.txt &&
	 mkdir -p sub/dir &&
	 echo nested >sub/dir/deep.txt &&
	 echo top >sub/top.txt &&
	 grit add . &&
	 grit commit -m "initial"
	)
'

test_expect_success 'rm removes file from working tree and index' '
	(cd repo &&
	 grit rm other.txt &&
	 grit ls-files --cached >../actual
	) &&
	! grep "other.txt" actual
'

test_expect_success 'rm removed file is gone from working tree' '
	test_path_is_missing repo/other.txt
'

test_expect_success 'rm --cached removes from index but keeps file' '
	(cd repo &&
	 echo cached >cached.txt &&
	 grit add cached.txt &&
	 grit rm --cached cached.txt &&
	 grit ls-files --cached >../actual
	) &&
	! grep "cached.txt" actual &&
	test_path_is_file repo/cached.txt
'

test_expect_success 'rm nonexistent file fails' '
	(cd repo &&
	 test_must_fail grit rm no-such-file.txt 2>../err
	) &&
	test -s err
'

test_expect_success 'rm --ignore-unmatch on nonexistent file succeeds' '
	(cd repo &&
	 grit rm --ignore-unmatch no-such-file.txt
	)
'

test_expect_success 'rm -r removes directory recursively' '
	(cd repo &&
	 grit rm -r sub &&
	 grit ls-files --cached >../actual
	) &&
	! grep "sub/" actual
'

test_expect_success 'rm -r removed files from working tree' '
	test_path_is_missing repo/sub/dir/deep.txt &&
	test_path_is_missing repo/sub/top.txt
'

test_expect_success 'rm directory without -r fails' '
	(cd repo &&
	 mkdir -p dir2 &&
	 echo f >dir2/f.txt &&
	 grit add dir2 &&
	 test_must_fail grit rm dir2/f.txt dir2 2>../err || true
	)
'

test_expect_success 'rm --dry-run does not actually remove' '
	(cd repo &&
	 echo dryfile >dry.txt &&
	 grit add dry.txt &&
	 grit commit -m "add dry" &&
	 grit rm --dry-run dry.txt &&
	 grit ls-files --cached >../actual
	) &&
	grep "dry.txt" actual &&
	test_path_is_file repo/dry.txt
'

test_expect_success 'rm --dry-run produces output' '
	(cd repo &&
	 grit rm --dry-run dry.txt >../actual 2>&1
	) &&
	grep "dry.txt" actual
'

test_expect_success 'rm --quiet suppresses output' '
	(cd repo &&
	 grit rm --quiet dry.txt >../actual 2>&1
	) &&
	test_must_be_empty actual
'

test_expect_success 'rm after quiet actually removed the file' '
	test_path_is_missing repo/dry.txt
'

test_expect_success 'rm multiple files at once' '
	(cd repo &&
	 echo a >a.txt && echo b >b.txt && echo c >c.txt &&
	 grit add a.txt b.txt c.txt &&
	 grit commit -m "add abc" &&
	 grit rm a.txt b.txt c.txt &&
	 grit ls-files --cached >../actual
	) &&
	! grep "^a.txt" actual &&
	! grep "^b.txt" actual &&
	! grep "^c.txt" actual
'

test_expect_success 'rm file with spaces in name' '
	(cd repo &&
	 echo spaced >"sp ace.txt" &&
	 grit add "sp ace.txt" &&
	 grit commit -m "add spaced" &&
	 grit rm "sp ace.txt" &&
	 grit ls-files --cached >../actual
	) &&
	! grep "sp ace.txt" actual
'

test_expect_success 'rm --cached on committed file keeps worktree copy' '
	(cd repo &&
	 echo keep >keep.txt &&
	 grit add keep.txt &&
	 grit commit -m "add keep" &&
	 grit rm --cached keep.txt &&
	 grit ls-files --cached >../actual
	) &&
	! grep "keep.txt" actual &&
	test_path_is_file repo/keep.txt
'

test_expect_success 'rm --force removes even with local modifications' '
	(cd repo &&
	 echo orig >force.txt &&
	 grit add force.txt &&
	 grit commit -m "add force" &&
	 echo modified >force.txt &&
	 grit rm --force force.txt &&
	 grit ls-files --cached >../actual
	) &&
	! grep "force.txt" actual &&
	test_path_is_missing repo/force.txt
'

test_expect_success 'rm modified file without --force fails' '
	(cd repo &&
	 echo orig >mod.txt &&
	 grit add mod.txt &&
	 grit commit -m "add mod" &&
	 echo changed >mod.txt &&
	 grit add mod.txt &&
	 echo changed-again >mod.txt &&
	 test_must_fail grit rm mod.txt 2>../err
	) &&
	test -s err
'

test_expect_success 'rm executable file' '
	(cd repo &&
	 echo "#!/bin/sh" >exec.sh &&
	 chmod +x exec.sh &&
	 grit add exec.sh &&
	 grit commit -m "add exec" &&
	 grit rm exec.sh &&
	 grit ls-files --cached >../actual
	) &&
	! grep "exec.sh" actual &&
	test_path_is_missing repo/exec.sh
'

test_expect_success 'rm then commit records deletion' '
	(cd repo &&
	 echo del >del.txt &&
	 grit add del.txt &&
	 grit commit -m "add del" &&
	 grit rm del.txt &&
	 grit commit -m "remove del" &&
	 grit ls-tree HEAD >../actual
	) &&
	! grep "del.txt" actual
'

test_expect_success 'rm --cached then commit records deletion in tree' '
	(cd repo &&
	 echo cachedel >cachedel.txt &&
	 grit add cachedel.txt &&
	 grit commit -m "add cachedel" &&
	 grit rm --cached cachedel.txt &&
	 grit commit -m "rm cached cachedel" &&
	 grit ls-tree HEAD >../actual
	) &&
	! grep "cachedel.txt" actual
'

test_expect_success 'rm -r --dry-run on directory does not remove' '
	(cd repo &&
	 mkdir -p drydir &&
	 echo f >drydir/f.txt &&
	 grit add drydir &&
	 grit commit -m "add drydir" &&
	 grit rm -r --dry-run drydir >../actual 2>&1
	) &&
	test_path_is_file repo/drydir/f.txt &&
	grep "drydir" actual
'

test_expect_success 'rm file not in index fails' '
	(cd repo &&
	 echo untracked >untracked.txt &&
	 test_must_fail grit rm untracked.txt 2>../err
	) &&
	test -s err
'

test_expect_success 'rm --ignore-unmatch --quiet produces no output' '
	(cd repo &&
	 grit rm --ignore-unmatch --quiet no-file.txt >../actual 2>&1
	) &&
	test_must_be_empty actual
'

test_expect_success 'rm empty file that is tracked' '
	(cd repo &&
	 : >empty.txt &&
	 grit add empty.txt &&
	 grit commit -m "add empty" &&
	 grit rm empty.txt &&
	 grit ls-files --cached >../actual
	) &&
	! grep "^empty.txt" actual
'

test_expect_success 'rm symlink removes it' '
	(cd repo &&
	 echo target >target.txt &&
	 grit add target.txt &&
	 ln -sf target.txt link.txt &&
	 grit add link.txt &&
	 grit commit -m "add link" &&
	 grit rm -f link.txt &&
	 grit ls-files --cached >../actual
	) &&
	! grep "link.txt" actual
'

test_expect_success 'rm -r nested directories' '
	(cd repo &&
	 mkdir -p a/b/c &&
	 echo deep >a/b/c/file.txt &&
	 echo mid >a/b/mid.txt &&
	 grit add a &&
	 grit commit -m "add nested" &&
	 grit rm -r a &&
	 grit ls-files --cached >../actual
	) &&
	! grep "^a/" actual
'

test_expect_success 'rm --cached --ignore-unmatch on unknown file' '
	(cd repo &&
	 grit rm --cached --ignore-unmatch phantom.txt
	)
'

test_expect_success 'rm --force --cached removes staged changes' '
	(cd repo &&
	 echo staged >staged.txt &&
	 grit add staged.txt &&
	 grit rm --force --cached staged.txt &&
	 grit ls-files --cached >../actual
	) &&
	! grep "staged.txt" actual &&
	test_path_is_file repo/staged.txt
'

test_expect_success 'rm -r --force removes dirty directory' '
	(cd repo &&
	 mkdir -p forcedir &&
	 echo f1 >forcedir/one.txt &&
	 echo f2 >forcedir/two.txt &&
	 grit add forcedir &&
	 grit commit -m "add forcedir" &&
	 echo modified >forcedir/one.txt &&
	 grit rm -r --force forcedir &&
	 grit ls-files --cached >../actual
	) &&
	! grep "forcedir/" actual
'

test_expect_success 'rm from repo root with path prefix' '
	(cd repo &&
	 mkdir -p reldir &&
	 echo rel >reldir/rel.txt &&
	 grit add reldir &&
	 grit commit -m "add reldir" &&
	 grit rm reldir/rel.txt &&
	 grit ls-files --cached >../actual
	) &&
	! grep "reldir/rel.txt" actual
'

test_done
