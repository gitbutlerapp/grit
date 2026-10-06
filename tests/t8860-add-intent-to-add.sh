#!/bin/sh
# Tests for git add --intent-to-add (-N) and related workflows.

test_description='add --intent-to-add (-N) placeholder entries'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- setup -------------------------------------------------------------------

test_expect_success 'setup: create repo with initial commit' '
	(
	git init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "base" >base.txt &&
	git add base.txt &&
	test_tick &&
	git commit -m "initial"
	)
'

# -- basic intent-to-add -----------------------------------------------------

test_expect_success 'add -N records intent-to-add for new file' '
	(
	cd repo &&
	echo "new content" >new.txt &&
	git add -N new.txt &&
	git ls-files --stage new.txt >out &&
	grep "new.txt" out
	)
'

test_expect_success 'intent-to-add entry shows empty blob hash' '
	(
	cd repo &&
	empty_blob=$(printf "" | git hash-object --stdin) &&
	git ls-files --stage new.txt >out &&
	grep "$empty_blob" out
	)
'

test_expect_success 'intent-to-add file shows in status as unstaged add' '
	(
	cd repo &&
	git status --porcelain >out &&
	grep "^ A" out | grep "new.txt"
	)
'

test_expect_success 'diff shows intent-to-add file content' '
	(
	cd repo &&
	git diff >out &&
	grep "new.txt" out
	)
'

test_expect_success 'diff --cached omits ita-only file' '
	(
	cd repo &&
	git diff --cached --name-only >out &&
	test_must_be_empty out
	)
'

test_expect_success 'git add converts intent-to-add to regular entry' '
	(
	cd repo &&
	git add new.txt &&
	git diff --cached --name-only >out &&
	grep "new.txt" out
	)
'

test_expect_success 'commit after add -N then add works' '
	(
	cd repo &&
	test_tick &&
	git commit -m "add new.txt" &&
	git log --oneline >out &&
	grep "add new.txt" out
	)
'

# -- multiple files -----------------------------------------------------------

test_expect_success 'add -N works with multiple files' '
	(
	cd repo &&
	echo "alpha" >alpha.txt &&
	echo "beta" >beta.txt &&
	echo "gamma" >gamma.txt &&
	git add -N alpha.txt beta.txt gamma.txt &&
	git ls-files --stage alpha.txt beta.txt gamma.txt >out &&
	test $(wc -l <out) -eq 3
	)
'

test_expect_success 'status shows all intent-to-add files' '
	(
	cd repo &&
	git status --porcelain >out &&
	grep "alpha.txt" out &&
	grep "beta.txt" out &&
	grep "gamma.txt" out
	)
'

test_expect_success 'all ita entries have empty blob hash' '
	(
	cd repo &&
	empty_blob=$(printf "" | git hash-object --stdin) &&
	git ls-files --stage alpha.txt >out &&
	grep "$empty_blob" out &&
	git ls-files --stage beta.txt >out &&
	grep "$empty_blob" out
	)
'

test_expect_success 'add -N then add stages correct content' '
	(
	cd repo &&
	git add alpha.txt &&
	git ls-files --stage alpha.txt >out &&
	! grep "0000000000000000000000000000000000000000" out
	)
'

test_expect_success 'cleanup multi-file test' '
	(
	cd repo &&
	git add beta.txt gamma.txt &&
	test_tick &&
	git commit -m "multi add"
	)
'

# -- intent-to-add and rm ----------------------------------------------------

test_expect_success 'rm --cached removes intent-to-add entry' '
	(
	cd repo &&
	echo "removable" >removeme.txt &&
	git add -N removeme.txt &&
	git rm --cached removeme.txt &&
	git ls-files removeme.txt >out &&
	test -z "$(cat out)"
	)
'

test_expect_success 'working tree file survives rm --cached of ita' '
	(
	cd repo &&
	test -f removeme.txt &&
	rm removeme.txt
	)
'

# -- intent-to-add and restore -----------------------------------------------

test_expect_success 'restore --staged removes intent-to-add entry' '
	(
	cd repo &&
	echo "restore test" >restore-ita.txt &&
	git add -N restore-ita.txt &&
	git restore --staged restore-ita.txt &&
	git ls-files restore-ita.txt >out &&
	test -z "$(cat out)"
	)
'

test_expect_success 'working tree preserved after restore --staged ita' '
	(
	cd repo &&
	test -f restore-ita.txt &&
	rm restore-ita.txt
	)
'

# -- intent-to-add in subdirectory --------------------------------------------

test_expect_success 'add -N works in subdirectory' '
	(
	cd repo &&
	mkdir -p sub/deep &&
	echo "deep file" >sub/deep/file.txt &&
	git add -N sub/deep/file.txt &&
	git ls-files sub/deep/file.txt >out &&
	grep "sub/deep/file.txt" out
	)
'

test_expect_success 'status shows subdirectory ita file' '
	(
	cd repo &&
	git status --porcelain >out &&
	grep "sub/deep/file.txt" out
	)
'

test_expect_success 'add converts subdirectory ita to regular' '
	(
	cd repo &&
	git add sub/deep/file.txt &&
	git ls-files --stage sub/deep/file.txt >out &&
	! grep "0000000000000000000000000000000000000000" out &&
	test_tick &&
	git commit -m "deep file"
	)
'

# -- add -N with --dry-run ---------------------------------------------------

test_expect_success 'add -N --dry-run does not actually add' '
	(
	cd repo &&
	echo "dryrun" >dryrun.txt &&
	git add -N --dry-run dryrun.txt &&
	git ls-files dryrun.txt >out &&
	test -z "$(cat out)" &&
	rm dryrun.txt
	)
'

# -- add -N --force -----------------------------------------------------------

test_expect_success 'setup gitignore' '
	(
	cd repo &&
	echo "*.ign" >.gitignore &&
	git add .gitignore &&
	test_tick &&
	git commit -m "gitignore"
	)
'

test_expect_success 'add -N --force on ignored file works' '
	(
	cd repo &&
	echo "forced" >force.ign &&
	git add -N --force force.ign &&
	git ls-files force.ign >out &&
	grep "force.ign" out &&
	git rm --cached force.ign &&
	rm force.ign
	)
'

# -- add -N then diff --cached ------------------------------------------------

test_expect_success 'diff --cached omits ita-only file entry' '
	(
	cd repo &&
	echo "cached diff" >cdiff.txt &&
	git add -N cdiff.txt &&
	git diff --cached --name-only >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff shows ita content as addition' '
	(
	cd repo &&
	git diff >out &&
	grep "cdiff.txt" out
	)
'

test_expect_success 'cleanup ita for diff test' '
	(
	cd repo &&
	git add cdiff.txt &&
	test_tick &&
	git commit -m "cdiff"
	)
'

# -- add -N with update flag ---------------------------------------------------

test_expect_success 'add --update stages modified tracked files' '
	(
	cd repo &&
	echo "modified base" >base.txt &&
	git add --update &&
	git diff --cached --name-only >out &&
	grep "base.txt" out
	)
'

test_expect_success 'ita entry alongside tracked update' '
	(
	cd repo &&
	echo "ita update test" >ita-update.txt &&
	git add -N ita-update.txt &&
	empty_blob=$(printf "" | git hash-object --stdin) &&
	git ls-files --stage ita-update.txt >out &&
	grep "$empty_blob" out
	)
'

test_expect_success 'cleanup ita update test' '
	(
	cd repo &&
	git add ita-update.txt &&
	test_tick &&
	git commit -m "ita-update cleanup"
	)
'

# -- add -N verbose -----------------------------------------------------------

test_expect_success 'add -N --verbose runs without error' '
	(
	cd repo &&
	echo "verbose" >verb.txt &&
	git add -N -v verb.txt &&
	git ls-files verb.txt >out &&
	grep "verb.txt" out &&
	git add verb.txt &&
	test_tick &&
	git commit -m "verb"
	)
'

test_done
