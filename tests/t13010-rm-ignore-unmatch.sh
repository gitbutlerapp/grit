#!/bin/sh

test_description='grit rm with --ignore-unmatch, --cached, --force, -r, and edge cases'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo && cd repo &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo hello >file.txt &&
	echo world >other.txt &&
	mkdir -p sub/deep &&
	echo s1 >sub/s1.txt &&
	echo s2 >sub/deep/s2.txt &&
	grit add . && grit commit -m "initial"
	)
'

test_expect_success 'rm removes file from index and working tree' '
	(cd repo && grit rm other.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "other.txt" actual &&
	! test -f repo/other.txt
'

test_expect_success 'rm file shows removal message' '
	(cd repo &&
	 echo back >other.txt && grit add other.txt && grit commit -m "re-add" &&
	 grit rm other.txt >../actual 2>&1) &&
	grep "other.txt" actual
'

test_expect_success 'rm --quiet suppresses removal message' '
	(cd repo &&
	 echo q >quiet.txt && grit add quiet.txt && grit commit -m "quiet" &&
	 grit rm -q quiet.txt >../actual 2>&1) &&
	! test -s actual
'

test_expect_success 'rm nonexistent file fails' '
	(cd repo && ! grit rm nonexistent.txt 2>../err) &&
	test -s err
'

test_expect_success 'rm --ignore-unmatch with nonexistent file exits zero' '
	(cd repo && grit rm --ignore-unmatch nonexistent.txt 2>../err) &&
	true
'

test_expect_success 'rm --ignore-unmatch with existing file still removes it' '
	(cd repo &&
	 echo iu >iu.txt && grit add iu.txt && grit commit -m "iu" &&
	 grit rm --ignore-unmatch iu.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "iu.txt" actual
'

test_expect_success 'rm --cached keeps working tree file' '
	(cd repo &&
	 echo cached >cached.txt && grit add cached.txt && grit commit -m "cached" &&
	 grit rm --cached cached.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "cached.txt" actual &&
	test -f repo/cached.txt
'

test_expect_success 'rm --cached on modified file succeeds' '
	(cd repo &&
	 echo orig >modcache.txt && grit add modcache.txt && grit commit -m "mc" &&
	 echo changed >modcache.txt &&
	 grit rm --cached modcache.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "modcache.txt" actual &&
	test -f repo/modcache.txt
'

test_expect_success 'rm refuses modified tracked file without --force' '
	(cd repo &&
	 echo orig >forceme.txt && grit add forceme.txt && grit commit -m "force" &&
	 echo changed >forceme.txt &&
	 ! grit rm forceme.txt 2>../err) &&
	test -s err
'

test_expect_success 'rm --force removes modified tracked file' '
	(cd repo &&
	 grit rm --force forceme.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "forceme.txt" actual &&
	! test -f repo/forceme.txt
'

test_expect_success 'rm -r removes directory recursively' '
	(cd repo &&
	 mkdir -p rmdir/nested &&
	 echo a >rmdir/a.txt && echo b >rmdir/nested/b.txt &&
	 grit add rmdir/ && grit commit -m "rmdir" &&
	 grit rm -r rmdir/ &&
	 grit ls-files --cached >../actual) &&
	! grep "rmdir/" actual
'

test_expect_success 'rm directory without -r fails' '
	(cd repo &&
	 mkdir -p rmdir2 && echo x >rmdir2/x.txt &&
	 grit add rmdir2/ && grit commit -m "rmdir2" &&
	 ! grit rm rmdir2/ 2>../err) &&
	test -s err
'

test_expect_success 'rm multiple files at once' '
	(cd repo &&
	 echo m1 >multi1.txt && echo m2 >multi2.txt && echo m3 >multi3.txt &&
	 grit add multi1.txt multi2.txt multi3.txt && grit commit -m "multi" &&
	 grit rm multi1.txt multi2.txt multi3.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "multi1.txt" actual &&
	! grep "multi2.txt" actual &&
	! grep "multi3.txt" actual
'

test_expect_success 'rm --dry-run does not actually remove' '
	(cd repo &&
	 echo dn >dryrun.txt && grit add dryrun.txt && grit commit -m "dn" &&
	 grit rm --dry-run dryrun.txt &&
	 grit ls-files --cached >../actual) &&
	grep "dryrun.txt" actual &&
	test -f repo/dryrun.txt
'

test_expect_success 'rm --dry-run shows what would be removed' '
	(cd repo && grit rm --dry-run dryrun.txt >../actual 2>&1) &&
	grep "dryrun.txt" actual
'

test_expect_success 'rm after unstaging still removes from index' '
	(cd repo &&
	 echo un >unstaged.txt && grit add unstaged.txt && grit commit -m "unstaged" &&
	 grit rm unstaged.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "unstaged.txt" actual
'

test_expect_success 'rm --ignore-unmatch with glob pattern exits zero' '
	(cd repo && grit rm --ignore-unmatch "*.nonexistent" 2>../err) &&
	true
'

test_expect_success 'rm --cached then re-add restores file to index' '
	(cd repo &&
	 echo re >readd.txt && grit add readd.txt && grit commit -m "readd" &&
	 grit rm --cached readd.txt &&
	 grit add readd.txt &&
	 grit ls-files --cached >../actual) &&
	grep "readd.txt" actual
'

test_expect_success 'rm file in subdirectory' '
	(cd repo &&
	 mkdir -p sdir && echo sf >sdir/sf.txt &&
	 grit add sdir/sf.txt && grit commit -m "sdir" &&
	 grit rm sdir/sf.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "sdir/sf.txt" actual
'

test_expect_success 'rm from subdirectory with relative path' '
	(cd repo &&
	 mkdir -p rel && echo rf >rel/rf.txt &&
	 grit add rel/rf.txt && grit commit -m "rel") &&
	(cd repo/rel && grit rm rf.txt) &&
	(cd repo && grit ls-files --cached >../actual) &&
	! grep "rel/rf.txt" actual
'

test_expect_success 'rm staged but uncommitted file with --force' '
	(cd repo &&
	 echo staged >staged-only.txt && grit add staged-only.txt &&
	 grit rm --force staged-only.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "staged-only.txt" actual
'

test_expect_success 'rm --cached -r removes directory from index only' '
	(cd repo &&
	 mkdir -p cachedir && echo cd1 >cachedir/cd1.txt && echo cd2 >cachedir/cd2.txt &&
	 grit add cachedir/ && grit commit -m "cachedir" &&
	 grit rm --cached -r cachedir/ &&
	 grit ls-files --cached >../actual) &&
	! grep "cachedir/" actual &&
	test -f repo/cachedir/cd1.txt &&
	test -f repo/cachedir/cd2.txt
'

test_expect_success 'rm file then commit shows deletion' '
	(cd repo &&
	 echo dc >delcommit.txt && grit add delcommit.txt && grit commit -m "dc" &&
	 grit rm delcommit.txt &&
	 grit status --porcelain >../actual) &&
	grep "D  delcommit.txt" actual
'

test_expect_success 'rm --ignore-unmatch combined with valid file removes valid file' '
	(cd repo &&
	 echo combo >combo.txt && grit add combo.txt && grit commit -m "combo" &&
	 grit rm --ignore-unmatch combo.txt nonexistent.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "combo.txt" actual
'

test_expect_success 'rm with file having spaces in name' '
	(cd repo &&
	 echo sp >"space file.txt" && grit add "space file.txt" && grit commit -m "sp" &&
	 grit rm "space file.txt" &&
	 grit ls-files --cached >../actual) &&
	! grep "space file.txt" actual
'

test_expect_success 'rm empty file' '
	(cd repo &&
	 >empty-rm.txt && grit add empty-rm.txt && grit commit -m "emrm" &&
	 grit rm empty-rm.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "empty-rm.txt" actual
'

test_expect_success 'rm --force symlink' '
	(cd repo &&
	 echo target >linktgt.txt && grit add linktgt.txt &&
	 ln -sf linktgt.txt mylink && grit add mylink && grit commit -m "link" &&
	 grit rm -f mylink &&
	 grit ls-files --cached >../actual) &&
	! grep "^mylink$" actual
'

test_expect_success 'rm --ignore-unmatch with already removed file exits zero' '
	(cd repo &&
	 echo gone >gonefile.txt && grit add gonefile.txt && grit commit -m "gone" &&
	 rm gonefile.txt &&
	 grit rm --ignore-unmatch gonefile.txt) &&
	true
'

test_expect_success 'rm multiple files some nonexistent with --ignore-unmatch' '
	(cd repo &&
	 echo exist >exist.txt && grit add exist.txt && grit commit -m "exist" &&
	 grit rm --ignore-unmatch exist.txt nope1.txt nope2.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "exist.txt" actual
'

test_done
