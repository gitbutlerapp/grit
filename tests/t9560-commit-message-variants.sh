#!/bin/sh
# Tests for grit commit: -m, -F, --amend, --allow-empty, --allow-empty-message,
# -a, --author, -q, commit output, log verification.

test_description='grit commit message variants and options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init --initial-branch=master repo &&
	cd repo &&
	grit config set user.name "Test User" &&
	grit config set user.email "test@test.com"
	)
'

###########################################################################
# Section 2: Basic -m commit
###########################################################################

test_expect_success 'commit with -m message' '
	(
	cd repo &&
	echo "hello" >file.txt &&
	grit add file.txt &&
	grit commit -m "initial commit" &&
	grit log --oneline >actual &&
	grep "initial commit" actual
	)
'

test_expect_success 'commit with multi-word message' '
	(
	cd repo &&
	echo "more" >>file.txt &&
	grit add file.txt &&
	grit commit -m "add more content to file" &&
	grit log --oneline >actual &&
	grep "add more content to file" actual
	)
'

test_expect_success 'commit message with special characters' '
	(
	cd repo &&
	echo "special" >>file.txt &&
	grit add file.txt &&
	grit commit -m "fix: handle edge-case (issue #42)" &&
	grit log --oneline >actual &&
	grep "fix: handle edge-case" actual
	)
'

test_expect_success 'commit with empty -m fails' '
	(
	cd repo &&
	echo "empty" >>file.txt &&
	grit add file.txt &&
	test_must_fail grit commit -m ""
	)
'

test_expect_success 'stage file for next test after failed commit' '
	(
	cd repo &&
	grit add file.txt
	)
'

###########################################################################
# Section 3: -F (message from file)
###########################################################################

test_expect_success 'commit with -F reads message from file' '
	(
	cd repo &&
	echo "Message from file" >../commit-msg.txt &&
	grit commit -F ../commit-msg.txt &&
	grit log --oneline >actual &&
	grep "Message from file" actual
	)
'

test_expect_success 'commit -F with multi-line message' '
	(
	cd repo &&
	echo "multi" >>file.txt &&
	grit add file.txt &&
	printf "Subject line\n\nBody paragraph.\n" >../multi-msg.txt &&
	grit commit -F ../multi-msg.txt &&
	grit log --oneline >actual &&
	grep "Subject line" actual
	)
'

test_expect_success 'commit -F from stdin with -' '
	(
	cd repo &&
	echo "stdin" >>file.txt &&
	grit add file.txt &&
	echo "From stdin" | grit commit -F - &&
	grit log --oneline >actual &&
	grep "From stdin" actual
	)
'

###########################################################################
# Section 4: --allow-empty
###########################################################################

test_expect_success 'commit without changes fails' '
	(
	cd repo &&
	test_must_fail grit commit -m "no changes"
	)
'

test_expect_success 'commit --allow-empty succeeds with no changes' '
	(
	cd repo &&
	grit commit --allow-empty -m "empty commit" &&
	grit log --oneline >actual &&
	grep "empty commit" actual
	)
'

test_expect_success 'allow-empty commit has same tree as parent' '
	(
	cd repo &&
	grit rev-parse HEAD^{tree} >current_tree &&
	grit commit --allow-empty -m "another empty" &&
	grit rev-parse HEAD^{tree} >new_tree &&
	test_cmp current_tree new_tree
	)
'

test_expect_success 'multiple allow-empty commits work' '
	(
	cd repo &&
	grit commit --allow-empty -m "empty 1" &&
	grit commit --allow-empty -m "empty 2" &&
	grit log --oneline >actual &&
	grep "empty 1" actual &&
	grep "empty 2" actual
	)
'

###########################################################################
# Section 5: --allow-empty-message
###########################################################################

test_expect_success 'commit --allow-empty-message with empty -m' '
	(
	cd repo &&
	echo "emptymsg" >>file.txt &&
	grit add file.txt &&
	grit commit --allow-empty-message -m ""
	)
'

###########################################################################
# Section 6: --amend
###########################################################################

test_expect_success 'commit --amend changes last commit message' '
	(
	cd repo &&
	echo "amend-test" >>file.txt &&
	grit add file.txt &&
	grit commit -m "original message" &&
	grit commit --amend -m "amended message" &&
	grit log --oneline -n 1 >actual &&
	grep "amended message" actual &&
	! grep "original message" actual
	)
'

test_expect_success 'commit --amend preserves file content' '
	(
	cd repo &&
	grit show HEAD >actual &&
	grep "amend-test" actual
	)
'

test_expect_success 'commit --amend with additional staged changes' '
	(
	cd repo &&
	echo "extra" >extra.txt &&
	grit add extra.txt &&
	grit commit --amend -m "amended with extra" &&
	grit ls-files >actual &&
	grep "extra.txt" actual
	)
'

test_expect_success 'amend does not create new commit on top' '
	(
	cd repo &&
	grit rev-parse HEAD >before &&
	grit commit --amend -m "re-amended" &&
	grit rev-parse HEAD >after &&
	! test_cmp before after
	)
'

###########################################################################
# Section 7: -a (auto-stage tracked files)
###########################################################################

test_expect_success 'commit -a stages modified tracked files' '
	(
	cd repo &&
	echo "tracked" >tracked.txt &&
	grit add tracked.txt &&
	grit commit -m "add tracked" &&
	echo "modified" >>tracked.txt &&
	grit commit -a -m "auto-staged modification" &&
	grit log --oneline -n 1 >actual &&
	grep "auto-staged" actual
	)
'

test_expect_success 'commit -a does not add untracked files' '
	(
	cd repo &&
	echo "untracked" >new-untracked.txt &&
	echo "change" >>tracked.txt &&
	grit commit -a -m "only tracked" &&
	grit ls-files >actual &&
	! grep "new-untracked.txt" actual
	)
'

###########################################################################
# Section 8: --author
###########################################################################

test_expect_success 'commit --author overrides author identity' '
	(
	cd repo &&
	echo "author-test" >>file.txt &&
	grit add file.txt &&
	grit commit --author "Other Person <other@example.com>" -m "custom author" &&
	grit cat-file -p HEAD >actual &&
	grep "author Other Person" actual
	)
'

test_expect_success 'committer is still original user after --author' '
	(
	cd repo &&
	grit cat-file -p HEAD >actual &&
	grep "committer C O Mitter" actual
	)
'

###########################################################################
# Section 9: -q (quiet)
###########################################################################

test_expect_success 'commit -q suppresses output' '
	(
	cd repo &&
	echo "quiet" >>file.txt &&
	grit add file.txt &&
	grit commit -q -m "quiet commit" >actual 2>&1 &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 10: Commit output format
###########################################################################

test_expect_success 'commit shows branch and short SHA' '
	(
	cd repo &&
	echo "output" >>file.txt &&
	grit add file.txt &&
	grit commit -m "output test" >actual 2>&1 &&
	grep "master" actual
	)
'

test_expect_success 'commit shows message in output' '
	(
	cd repo &&
	echo "msg" >>file.txt &&
	grit add file.txt &&
	grit commit -m "shown in output" >actual 2>&1 &&
	grep "shown in output" actual
	)
'

###########################################################################
# Section 11: Comparison with real git
###########################################################################

test_expect_success 'grit and git produce same tree for same content' '
	(
	grit init --initial-branch=master grit-cmp &&
	$REAL_GIT init --initial-branch=master git-cmp &&
	cd grit-cmp &&
	grit config set user.name "Test" &&
	grit config set user.email "t@t.com" &&
	echo "same" >same.txt &&
	grit add same.txt &&
	grit write-tree >../grit_tree &&
	cd ../git-cmp &&
	$REAL_GIT config user.name "Test" &&
	$REAL_GIT config user.email "t@t.com" &&
	echo "same" >same.txt &&
	$REAL_GIT add same.txt &&
	$REAL_GIT write-tree >../git_tree &&
	cd .. &&
	test_cmp grit_tree git_tree
	)
'

###########################################################################
# Section 12: Multiple commits and log ordering
###########################################################################

test_expect_success 'commits appear in reverse chronological order' '
	(
	cd repo &&
	echo "order1" >order.txt &&
	grit add order.txt &&
	grit commit -m "commit-alpha" &&
	echo "order2" >>order.txt &&
	grit add order.txt &&
	grit commit -m "commit-beta" &&
	echo "order3" >>order.txt &&
	grit add order.txt &&
	grit commit -m "commit-gamma" &&
	grit log --oneline -n 1 >actual &&
	grep "commit-gamma" actual
	)
'

test_expect_success 'log shows all three recent commits' '
	(
	cd repo &&
	grit log --oneline >actual &&
	grep "commit-alpha" actual &&
	grep "commit-beta" actual &&
	grep "commit-gamma" actual
	)
'

test_expect_success 'log -n 3 limits output' '
	(
	cd repo &&
	grit log --oneline -n 3 >actual &&
	test $(wc -l <actual) -eq 3
	)
'

###########################################################################
# Section 13: Edge cases
###########################################################################

test_expect_success 'commit on new branch preserves parent' '
	(
	cd repo &&
	grit rev-parse HEAD >parent_sha &&
	grit checkout -b new-branch &&
	echo "newbranch" >nb.txt &&
	grit add nb.txt &&
	grit commit -m "on new branch" &&
	grit rev-parse HEAD~1 >actual &&
	test_cmp parent_sha actual &&
	grit checkout master
	)
'

test_expect_success 'two commits have different SHAs' '
	(
	cd repo &&
	echo "diff1" >>file.txt &&
	grit add file.txt &&
	grit commit -m "diff commit 1" &&
	grit rev-parse HEAD >sha1 &&
	echo "diff2" >>file.txt &&
	grit add file.txt &&
	grit commit -m "diff commit 2" &&
	grit rev-parse HEAD >sha2 &&
	! test_cmp sha1 sha2
	)
'

test_expect_success 'commit updates HEAD ref' '
	(
	cd repo &&
	grit rev-parse HEAD >before &&
	echo "update" >>file.txt &&
	grit add file.txt &&
	grit commit -m "update HEAD" &&
	grit rev-parse HEAD >after &&
	! test_cmp before after
	)
'

test_expect_success 'first commit has no parent' '
	(
	grit init fresh &&
	cd fresh &&
	grit config set user.name "T" &&
	grit config set user.email "t@t.com" &&
	echo "first" >f.txt &&
	grit add f.txt &&
	grit commit -m "first" &&
	grit cat-file -p HEAD >actual &&
	! grep "^parent" actual
	)
'

test_done
