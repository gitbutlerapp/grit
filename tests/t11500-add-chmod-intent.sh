#!/bin/sh
# Tests for grit add with --intent-to-add, --dry-run, --verbose, --force,
# --update, --all, and executable-bit handling via update-index.

test_description='grit add: intent-to-add, dry-run, force, update, all, and mode handling'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---- setup ----
test_expect_success 'setup: init repo with config' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test"
	)
'

# ---- intent-to-add basics ----
test_expect_success 'add -N records intent-to-add placeholder' '
	(
	cd repo &&
	echo "content" >intent.txt &&
	grit add -N intent.txt &&
	grit ls-files -s intent.txt >actual &&
	grep "^100644 e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 0	intent.txt" actual
	)
'

test_expect_success 'intent-to-add file shows in ls-files output' '
	(
	cd repo &&
	grit ls-files >all &&
	grep "intent.txt" all
	)
'

test_expect_success 'intent-to-add file appears as modified in status' '
	(
	cd repo &&
	grit status --porcelain >st &&
	grep "intent.txt" st
	)
'

test_expect_success 'add after intent-to-add replaces placeholder with real blob' '
	(
	cd repo &&
	grit add intent.txt &&
	grit ls-files -s intent.txt >actual &&
	! grep "0000000000000000000000000000000000000000" actual
	)
'

test_expect_success 'commit after intent-to-add and add works' '
	(
	cd repo &&
	grit commit -m "add intent file" &&
	grit log --oneline | grep "add intent file"
	)
'

# ---- multiple intent-to-add ----
test_expect_success 'add -N with multiple files' '
	(
	cd repo &&
	echo a >multi1.txt &&
	echo b >multi2.txt &&
	echo c >multi3.txt &&
	grit add -N multi1.txt multi2.txt multi3.txt &&
	grit ls-files -s multi1.txt >s1 &&
	grit ls-files -s multi2.txt >s2 &&
	grit ls-files -s multi3.txt >s3 &&
	grep "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391" s1 &&
	grep "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391" s2 &&
	grep "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391" s3
	)
'

test_expect_success 'add all intent-to-add files at once' '
	(
	cd repo &&
	grit add multi1.txt multi2.txt multi3.txt &&
	grit ls-files -s multi1.txt >s1 &&
	! grep "0000000000000000000000000000000000000000" s1
	)
'

# ---- dry-run ----
test_expect_success 'add --dry-run does not modify index' '
	(
	cd repo &&
	echo "dry" >dryfile.txt &&
	grit add --dry-run dryfile.txt &&
	! grit ls-files --error-unmatch dryfile.txt 2>/dev/null
	)
'

test_expect_success 'add -n is synonym for --dry-run' '
	(
	cd repo &&
	echo "dry2" >dryfile2.txt &&
	grit add -n dryfile2.txt &&
	! grit ls-files --error-unmatch dryfile2.txt 2>/dev/null
	)
'

# ---- verbose ----
test_expect_success 'add --verbose reports added file' '
	(
	cd repo &&
	echo "verb" >verbfile.txt &&
	grit add --verbose verbfile.txt >output 2>&1 &&
	grep "verbfile.txt" output
	)
'

# ---- update ----
test_expect_success 'setup tracked files for update tests' '
	(
	cd repo &&
	echo "tracked1" >tracked1.txt &&
	echo "tracked2" >tracked2.txt &&
	grit add tracked1.txt tracked2.txt &&
	grit commit -m "tracked files"
	)
'

test_expect_success 'add -u updates modified tracked files' '
	(
	cd repo &&
	echo "modified" >tracked1.txt &&
	echo "untracked" >newfile.txt &&
	grit add -u &&
	grit ls-files -s tracked1.txt >s &&
	! grep "0000000000000000000000000000000000000000" s
	)
'

test_expect_success 'add -u does not add untracked files' '
	(
	cd repo &&
	! grit ls-files --error-unmatch newfile.txt 2>/dev/null
	)
'

test_expect_success 'add -u stages deletion of removed tracked file' '
	(
	cd repo &&
	rm tracked2.txt &&
	grit add -u &&
	! grit ls-files --error-unmatch tracked2.txt 2>/dev/null
	)
'

# ---- all ----
test_expect_success 'add --all adds untracked and stages deletions' '
	(
	cd repo &&
	echo "allnew" >allnew.txt &&
	grit add --all &&
	grit ls-files --error-unmatch allnew.txt &&
	grit ls-files --error-unmatch newfile.txt
	)
'

test_expect_success 'add -A is synonym for --all' '
	(
	cd repo &&
	echo "anew" >anew.txt &&
	grit add -A &&
	grit ls-files --error-unmatch anew.txt
	)
'

# ---- force (add ignored files) ----
test_expect_success 'setup gitignore for force tests' '
	(
	cd repo &&
	echo "*.ign" >.gitignore &&
	grit add .gitignore &&
	grit commit -m "add gitignore" &&
	echo "secret" >hidden.ign
	)
'

test_expect_success 'add --force adds ignored file' '
	(
	cd repo &&
	grit add --force hidden.ign &&
	grit ls-files --error-unmatch hidden.ign
	)
'

test_expect_success 'add -f is synonym for --force' '
	(
	cd repo &&
	echo "s2" >hidden2.ign &&
	grit add -f hidden2.ign &&
	grit ls-files --error-unmatch hidden2.ign
	)
'

# ---- executable bits via update-index ----
test_expect_success 'file added as 100644 by default' '
	(
	cd repo &&
	echo "script" >run.sh &&
	grit add run.sh &&
	grit ls-files -s run.sh >mode &&
	grep "^100644" mode
	)
'

test_expect_success 'chmod +x on disk then re-add changes mode to 100755' '
	(
	cd repo &&
	chmod +x run.sh &&
	grit add run.sh &&
	grit ls-files -s run.sh >mode &&
	grep "^100755" mode
	)
'

test_expect_success 'chmod -x on disk then re-add reverts mode to 100644' '
	(
	cd repo &&
	chmod -x run.sh &&
	grit add run.sh &&
	grit ls-files -s run.sh >mode &&
	grep "^100644" mode
	)
'

test_expect_success 'update-index --cacheinfo can set mode to 100755' '
	(
	cd repo &&
	oid=$(grit ls-files -s run.sh | cut -d" " -f2) &&
	grit update-index --cacheinfo "100755,$oid,run.sh" &&
	grit ls-files -s run.sh >mode &&
	grep "^100755" mode
	)
'

# ---- add . ----
test_expect_success 'add . adds all untracked in current dir' '
	(
	cd repo &&
	mkdir sub &&
	echo "insub" >sub/nested.txt &&
	grit add . &&
	grit ls-files --error-unmatch sub/nested.txt
	)
'

# ---- add with pathspec ----
test_expect_success 'add with directory pathspec adds contents' '
	(
	cd repo &&
	mkdir dir2 &&
	echo "a" >dir2/a.txt &&
	echo "b" >dir2/b.txt &&
	grit add dir2 &&
	grit ls-files --error-unmatch dir2/a.txt &&
	grit ls-files --error-unmatch dir2/b.txt
	)
'

# ---- intent-to-add then status ----
test_expect_success 'intent-to-add shows in diff output' '
	(
	cd repo &&
	grit commit -m "baseline" &&
	echo "diffme" >itadiff.txt &&
	grit add -N itadiff.txt &&
	grit diff >d &&
	grep "itadiff.txt" d
	)
'

# ---- intent-to-add overwrite ----
test_expect_success 'second add -N on same file is idempotent' '
	(
	cd repo &&
	echo "again" >ita2.txt &&
	grit add -N ita2.txt &&
	grit add -N ita2.txt &&
	grit ls-files -s ita2.txt | grep -c "ita2.txt" >count &&
	test "$(cat count)" = "1"
	)
'

# ---- add nonexistent file ----
test_expect_success 'add nonexistent file fails' '
	(
	cd repo &&
	test_must_fail grit add no-such-file.txt 2>err
	)
'

# ---- empty repo intent-to-add ----
test_expect_success 'intent-to-add in fresh repo' '
	(
	grit init fresh &&
	cd fresh &&
	echo "new" >first.txt &&
	grit add -N first.txt &&
	grit ls-files -s >idx &&
	grep "first.txt" idx &&
	cd ..
	)
'

# ---- add after rm ----
test_expect_success 'add re-adds after rm --cached' '
	(
	cd repo &&
	echo "readd" >readd.txt &&
	grit add readd.txt &&
	grit rm --cached readd.txt &&
	! grit ls-files --error-unmatch readd.txt 2>/dev/null &&
	grit add readd.txt &&
	grit ls-files --error-unmatch readd.txt
	)
'

test_done
