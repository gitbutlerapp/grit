#!/bin/sh
# Tests for grit branch -v, -vv, -a, --all, and combined flags.

test_description='grit branch --verbose --all combinations'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with branches' '
	(
	"$REAL_GIT" init --initial-branch=master repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "initial" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial commit" &&
	"$REAL_GIT" branch feature-a &&
	"$REAL_GIT" branch feature-b &&
	echo "change" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit" &&
	"$REAL_GIT" branch bugfix
	)
'

###########################################################################
# Section 2: branch (no flags) listing
###########################################################################

test_expect_success 'branch with no args lists local branches' '
	(
	cd repo &&
	grit branch >actual &&
	grep "master\|main" actual &&
	grep "feature-a" actual &&
	grep "feature-b" actual &&
	grep "bugfix" actual
	)
'

test_expect_success 'branch listing matches real git' '
	(
	cd repo &&
	grit branch >actual &&
	"$REAL_GIT" branch >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch current branch is marked with asterisk' '
	(
	cd repo &&
	grit branch >actual &&
	grep "^\* " actual >current &&
	test_line_count = 1 current
	)
'

###########################################################################
# Section 3: branch -v (verbose)
###########################################################################

test_expect_success 'branch -v shows commit hash and subject' '
	(
	cd repo &&
	grit branch -v >actual &&
	grep "feature-a" actual | grep -qE "[0-9a-f]{7}"
	)
'

test_expect_success 'branch -v shows subject for each branch' '
	(
	cd repo &&
	grit branch -v >actual &&
	grep "feature-a" actual | grep -q "initial commit" &&
	grep "bugfix" actual | grep -q "second commit"
	)
'

test_expect_success 'branch -v shows same branches and hashes as real git' '
	(
	cd repo &&
	grit branch -v | sed "s/  */ /g" >actual &&
	"$REAL_GIT" branch -v | sed "s/  */ /g" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch --verbose is same as -v' '
	(
	cd repo &&
	grit branch -v >v_out &&
	grit branch --verbose >verbose_out &&
	test_cmp v_out verbose_out
	)
'

test_expect_success 'branch -v current branch has asterisk' '
	(
	cd repo &&
	grit branch -v >actual &&
	grep "^\* " actual | grep -qE "[0-9a-f]{7}"
	)
'

###########################################################################
# Section 4: branch -a / --all
###########################################################################

test_expect_success 'setup: add a remote with tracking branches' '
	(
	cd repo &&
	"$REAL_GIT" init --bare ../remote.git &&
	"$REAL_GIT" remote add origin ../remote.git &&
	"$REAL_GIT" push origin master feature-a feature-b bugfix
	)
'

test_expect_success 'branch -a shows local and remote branches' '
	(
	cd repo &&
	grit branch -a >actual &&
	grep "feature-a" actual &&
	grep "origin/feature-a" actual
	)
'

test_expect_success 'branch --all is same as -a' '
	(
	cd repo &&
	grit branch -a >a_out &&
	grit branch --all >all_out &&
	test_cmp a_out all_out
	)
'

test_expect_success 'branch -a matches real git' '
	(
	cd repo &&
	grit branch -a >actual &&
	"$REAL_GIT" branch -a >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch -a shows remote master' '
	(
	cd repo &&
	grit branch -a >actual &&
	grep "origin/master" actual
	)
'

test_expect_success 'branch -r shows only remote branches' '
	(
	cd repo &&
	grit branch -r >actual &&
	grep "origin/master" actual &&
	! grep "^\* " actual
	)
'

test_expect_success 'branch -r lists remote-tracking branches' '
	(
	cd repo &&
	grit branch -r >actual &&
	grep "origin/master" actual &&
	grep "origin/feature-a" actual
	)
'

###########################################################################
# Section 5: branch -v -a combined
###########################################################################

test_expect_success 'branch -v -a shows verbose all branches' '
	(
	cd repo &&
	grit branch -v -a >actual &&
	grep "feature-a" actual | grep -qE "[0-9a-f]{7}" &&
	grep "origin/feature-a" actual | grep -qE "[0-9a-f]{7}"
	)
'

test_expect_success 'branch -va combined short form' '
	(
	cd repo &&
	grit branch -v -a >va_long &&
	grit branch -va >va_short &&
	test_cmp va_long va_short
	)
'

test_expect_success 'branch -va matches real git (normalized whitespace)' '
	(
	cd repo &&
	grit branch -va | sed "s/  */ /g" >actual &&
	"$REAL_GIT" branch -va | sed "s/  */ /g" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 6: branch --contains
###########################################################################

test_expect_success 'branch --contains HEAD shows current branch' '
	(
	cd repo &&
	grit branch --contains HEAD >actual &&
	grep "master\|main" actual
	)
'

test_expect_success 'branch --contains shows branches containing commit' '
	(
	cd repo &&
	initial_hash=$("$REAL_GIT" rev-parse feature-a) &&
	grit branch --contains "$initial_hash" >actual &&
	grep "feature-a" actual
	)
'

test_expect_success 'branch --contains HEAD includes current branch' '
	(
	cd repo &&
	grit branch --contains HEAD >actual &&
	grep "master\|main" actual
	)
'

###########################################################################
# Section 7: branch --merged / --no-merged
###########################################################################

test_expect_success 'branch --merged shows merged branches' '
	(
	cd repo &&
	grit branch --merged HEAD >actual &&
	grep "bugfix" actual
	)
'

test_expect_success 'branch --merged matches real git' '
	(
	cd repo &&
	grit branch --merged HEAD | sed "s/  */ /g" >actual &&
	"$REAL_GIT" branch --merged HEAD | sed "s/  */ /g" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch --no-merged shows unmerged branches' '
	(
	cd repo &&
	"$REAL_GIT" checkout feature-a &&
	echo diverge >diverge.txt &&
	"$REAL_GIT" add diverge.txt &&
	"$REAL_GIT" commit -m "diverge on feature-a" &&
	"$REAL_GIT" checkout master &&
	grit branch --no-merged HEAD >actual &&
	grep "feature-a" actual
	)
'

test_expect_success 'branch --no-merged shows diverged branches' '
	(
	cd repo &&
	grit branch --no-merged HEAD >actual &&
	grep "feature-a" actual
	)
'

###########################################################################
# Section 8: branch --show-current
###########################################################################

test_expect_success 'branch --show-current shows current branch' '
	(
	cd repo &&
	grit branch --show-current >actual &&
	grep -q "master\|main" actual
	)
'

test_expect_success 'branch --show-current matches real git' '
	(
	cd repo &&
	grit branch --show-current >actual &&
	"$REAL_GIT" branch --show-current >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 9: branch -v with single branch
###########################################################################

test_expect_success 'setup: single-branch repo' '
	(
	"$REAL_GIT" init --initial-branch=master single-repo &&
	cd single-repo &&
	"$REAL_GIT" config user.name "T" &&
	"$REAL_GIT" config user.email "t@t.com" &&
	echo "only" >only.txt &&
	"$REAL_GIT" add only.txt &&
	"$REAL_GIT" commit -m "only commit"
	)
'

test_expect_success 'branch -v in single-branch repo' '
	(
	cd single-repo &&
	grit branch -v >actual &&
	test_line_count = 1 actual &&
	grep -qE "[0-9a-f]{7}" actual
	)
'

test_expect_success 'branch -v single branch matches real git' '
	(
	cd single-repo &&
	grit branch -v >actual &&
	"$REAL_GIT" branch -v >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 10: branch count and edge cases
###########################################################################

test_expect_success 'branch count matches real git' '
	(
	cd repo &&
	grit branch >actual &&
	"$REAL_GIT" branch >expect &&
	wc -l <actual >count_grit &&
	wc -l <expect >count_git &&
	test_cmp count_git count_grit
	)
'

test_expect_success 'branch -a count matches real git' '
	(
	cd repo &&
	grit branch -a >actual &&
	"$REAL_GIT" branch -a >expect &&
	wc -l <actual >count_grit &&
	wc -l <expect >count_git &&
	test_cmp count_git count_grit
	)
'

test_done
