#!/bin/sh
# Tests for rev-parse with branch names, HEAD, detached HEAD, tags, utility flags.

test_description='rev-parse branch names, HEAD, detached, tags, utility'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository with branches' '
	(
	grit init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&

	echo "base" >file.txt &&
	grit add file.txt &&
	grit commit -m "initial commit" &&

	C1=$(grit rev-parse HEAD) &&

	echo "second" >file2.txt &&
	grit add file2.txt &&
	grit commit -m "second commit" &&

	C2=$(grit rev-parse HEAD) &&

	echo "third" >file3.txt &&
	grit add file3.txt &&
	grit commit -m "third commit" &&

	C3=$(grit rev-parse HEAD) &&

	grit branch feature-a $C1 &&
	grit branch feature-b $C1 &&
	grit branch topic $C2 &&

	echo $C1 >.c1 &&
	echo $C2 >.c2 &&
	echo $C3 >.c3
	)
'

###########################################################################
# Section 1: HEAD resolution
###########################################################################

test_expect_success 'rev-parse HEAD returns a 40-char hex sha' '
	(
	cd repo &&
	grit rev-parse HEAD >actual &&
	test $(wc -c <actual | tr -d " ") -ge 40
	)
'

test_expect_success 'rev-parse HEAD matches the tip commit' '
	(
	cd repo &&
	grit rev-parse HEAD >actual &&
	test_cmp .c3 actual
	)
'

test_expect_success 'rev-parse HEAD^ gives parent of HEAD' '
	(
	cd repo &&
	grit rev-parse HEAD^ >actual &&
	test_cmp .c2 actual
	)
'

test_expect_success 'rev-parse HEAD~1 equals HEAD^' '
	(
	cd repo &&
	grit rev-parse HEAD~1 >tilde &&
	grit rev-parse HEAD^ >caret &&
	test_cmp tilde caret
	)
'

test_expect_success 'rev-parse HEAD~2 gives grandparent' '
	(
	cd repo &&
	grit rev-parse HEAD~2 >actual &&
	test_cmp .c1 actual
	)
'

test_expect_success 'rev-parse HEAD~0 equals HEAD' '
	(
	cd repo &&
	grit rev-parse HEAD~0 >actual &&
	grit rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse HEAD^^ equals HEAD~2' '
	(
	cd repo &&
	grit rev-parse HEAD^^ >caret2 &&
	grit rev-parse HEAD~2 >tilde2 &&
	test_cmp caret2 tilde2
	)
'

###########################################################################
# Section 2: Branch name resolution
###########################################################################

test_expect_success 'rev-parse master resolves to a commit' '
	(
	cd repo &&
	grit rev-parse master >actual &&
	test $(wc -c <actual | tr -d " ") -ge 40
	)
'

test_expect_success 'rev-parse master equals HEAD (on master)' '
	(
	cd repo &&
	grit rev-parse master >branch_oid &&
	grit rev-parse HEAD >head_oid &&
	test_cmp branch_oid head_oid
	)
'

test_expect_success 'rev-parse feature-a resolves to first commit' '
	(
	cd repo &&
	grit rev-parse feature-a >actual &&
	test_cmp .c1 actual
	)
'

test_expect_success 'rev-parse feature-b resolves same as feature-a' '
	(
	cd repo &&
	grit rev-parse feature-a >a_oid &&
	grit rev-parse feature-b >b_oid &&
	test_cmp a_oid b_oid
	)
'

test_expect_success 'rev-parse topic resolves to second commit' '
	(
	cd repo &&
	grit rev-parse topic >actual &&
	test_cmp .c2 actual
	)
'

test_expect_success 'rev-parse refs/heads/master works with full refname' '
	(
	cd repo &&
	grit rev-parse refs/heads/master >full &&
	grit rev-parse master >short &&
	test_cmp full short
	)
'

test_expect_success 'rev-parse nonexistent branch fails' '
	(
	cd repo &&
	test_must_fail grit rev-parse no-such-branch 2>err &&
	test -s err
	)
'

test_expect_success 'rev-parse refs/heads/feature-a matches feature-a' '
	(
	cd repo &&
	grit rev-parse refs/heads/feature-a >full &&
	grit rev-parse feature-a >short &&
	test_cmp full short
	)
'

###########################################################################
# Section 3: Detached HEAD
###########################################################################

test_expect_success 'detach HEAD at second commit' '
	(
	cd repo &&
	C2=$(cat .c2) &&
	git checkout $C2 2>/dev/null
	)
'

test_expect_success 'rev-parse HEAD works in detached state' '
	(
	cd repo &&
	grit rev-parse HEAD >actual &&
	test_cmp .c2 actual
	)
'

test_expect_success 'HEAD file contains raw sha in detached state' '
	(
	cd repo &&
	head_content=$(cat .git/HEAD) &&
	C2=$(cat .c2) &&
	test "$head_content" = "$C2"
	)
'

test_expect_success 'reattach HEAD to master' '
	(
	cd repo &&
	git checkout master 2>/dev/null
	)
'

test_expect_success 'HEAD is back on master after reattach' '
	(
	cd repo &&
	grit rev-parse HEAD >actual &&
	test_cmp .c3 actual
	)
'

###########################################################################
# Section 4: Tags
###########################################################################

test_expect_success 'create lightweight tags' '
	(
	cd repo &&
	C1=$(cat .c1) &&
	C3=$(cat .c3) &&
	grit tag v1.0 $C1 &&
	grit tag v2.0 $C3
	)
'

test_expect_success 'rev-parse v1.0 resolves to first commit' '
	(
	cd repo &&
	grit rev-parse v1.0 >actual &&
	test_cmp .c1 actual
	)
'

test_expect_success 'rev-parse v2.0 resolves to HEAD' '
	(
	cd repo &&
	grit rev-parse v2.0 >actual &&
	grit rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse refs/tags/v1.0 works with full refname' '
	(
	cd repo &&
	grit rev-parse refs/tags/v1.0 >full &&
	grit rev-parse v1.0 >short &&
	test_cmp full short
	)
'

test_expect_success 'rev-parse tag^ resolves parent of tagged commit' '
	(
	cd repo &&
	grit rev-parse v2.0^ >actual &&
	test_cmp .c2 actual
	)
'

###########################################################################
# Section 5: --verify
###########################################################################

test_expect_success 'rev-parse --verify HEAD succeeds' '
	(
	cd repo &&
	grit rev-parse --verify HEAD >actual &&
	test $(wc -c <actual | tr -d " ") -ge 40
	)
'

test_expect_success 'rev-parse --verify with invalid ref fails' '
	(
	cd repo &&
	test_must_fail grit rev-parse --verify nonexistent 2>err
	)
'

test_expect_success 'rev-parse --verify master succeeds' '
	(
	cd repo &&
	grit rev-parse --verify master >actual &&
	grit rev-parse master >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 6: Utility flags
###########################################################################

test_expect_success 'rev-parse --git-dir shows .git' '
	(
	cd repo &&
	grit rev-parse --git-dir >actual &&
	echo ".git" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse --is-inside-work-tree returns true' '
	(
	cd repo &&
	grit rev-parse --is-inside-work-tree >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse --show-toplevel returns repo root' '
	(
	cd repo &&
	grit rev-parse --show-toplevel >actual &&
	test -n "$(cat actual)"
	)
'

test_expect_success 'rev-parse --is-bare-repository returns false' '
	(
	cd repo &&
	grit rev-parse --is-bare-repository >actual &&
	echo "false" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse --show-prefix from subdirectory' '
	(
	cd repo &&
	mkdir -p sub/dir &&
	cd sub/dir &&
	grit rev-parse --show-prefix >actual &&
	echo "sub/dir/" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse --is-bare-repository in bare repo' '
	(
	cd repo &&
	grit init --bare ../bare-repo &&
	cd ../bare-repo &&
	grit rev-parse --is-bare-repository >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

test_done
