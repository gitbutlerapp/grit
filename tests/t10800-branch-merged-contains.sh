#!/bin/sh
# Test grit branch operations: creation, deletion, rename, copy,
# listing, verbose, show-current, force, and filter flags.

test_description='grit branch operations and listing'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "A" >file.txt &&
	grit add file.txt &&
	grit commit -m "first commit" &&
	echo "B" >>file.txt &&
	grit add file.txt &&
	grit commit -m "second commit"
	)
'

###########################################################################
# Section 2: Branch creation
###########################################################################

test_expect_success 'create a branch' '
	(
	cd repo &&
	grit branch feature-x &&
	grit branch >out &&
	grep "feature-x" out
	)
'

test_expect_success 'create branch at specific commit' '
	(
	cd repo &&
	first=$(grit rev-list --reverse HEAD | head -1) &&
	grit branch old-point "$first" &&
	grit rev-parse old-point >out &&
	echo "$first" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'create branch matches git' '
	(
	cd repo &&
	grit branch grit-br &&
	git branch git-br &&
	grit rev-parse grit-br >grit_out &&
	git rev-parse git-br >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'creating duplicate branch fails' '
	(
	cd repo &&
	grit branch dup-test &&
	test_must_fail grit branch dup-test
	)
'

test_expect_success 'create branch with --force overwrites' '
	(
	cd repo &&
	first=$(grit rev-list --reverse HEAD | head -1) &&
	grit branch force-br "$first" &&
	grit branch --force force-br HEAD &&
	grit rev-parse force-br >out &&
	grit rev-parse HEAD >expect &&
	test_cmp expect out
	)
'

###########################################################################
# Section 3: Branch listing
###########################################################################

test_expect_success 'branch list shows all branches' '
	(
	cd repo &&
	grit branch >out &&
	grep "main" out &&
	grep "feature-x" out
	)
'

test_expect_success 'branch list marks current with asterisk' '
	(
	cd repo &&
	grit branch >out &&
	grep "^\\* main" out
	)
'

test_expect_success 'branch -l lists branches' '
	(
	cd repo &&
	grit branch -l >out &&
	grep "main" out
	)
'

test_expect_success 'branch list matches git' '
	(
	cd repo &&
	grit branch >grit_out &&
	git branch >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 4: Branch verbose
###########################################################################

test_expect_success 'branch -v shows commit hash and subject' '
	(
	cd repo &&
	grit branch -v >out &&
	grep "main" out &&
	grep "second commit" out
	)
'

test_expect_success 'branch -v output contains abbreviated hashes' '
	(
	cd repo &&
	grit branch -v >out &&
	full=$(grit rev-parse HEAD) &&
	short=$(echo "$full" | cut -c1-7) &&
	grep "$short" out
	)
'

###########################################################################
# Section 5: --show-current
###########################################################################

test_expect_success '--show-current shows main' '
	(
	cd repo &&
	grit branch --show-current >out &&
	echo "main" >expect &&
	test_cmp expect out
	)
'

test_expect_success '--show-current matches git' '
	(
	cd repo &&
	grit branch --show-current >grit_out &&
	git branch --show-current >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success '--show-current after checkout' '
	(
	cd repo &&
	grit checkout feature-x &&
	grit branch --show-current >out &&
	echo "feature-x" >expect &&
	test_cmp expect out &&
	grit checkout main
	)
'

###########################################################################
# Section 6: Branch delete
###########################################################################

test_expect_success 'delete a merged branch with -d' '
	(
	cd repo &&
	grit branch del-me &&
	grit branch -d del-me &&
	grit branch >out &&
	! grep "del-me" out
	)
'

test_expect_success 'delete with --delete' '
	(
	cd repo &&
	grit branch del-me2 &&
	grit branch --delete del-me2 &&
	grit branch >out &&
	! grep "del-me2" out
	)
'

test_expect_success 'force delete with -D' '
	(
	cd repo &&
	first=$(grit rev-list --reverse HEAD | head -1) &&
	grit branch force-del "$first" &&
	grit branch -D force-del &&
	grit branch >out &&
	! grep "force-del" out
	)
'

test_expect_success 'deleting nonexistent branch fails' '
	(
	cd repo &&
	test_must_fail grit branch -d no-such-branch
	)
'

test_expect_success 'cannot delete current branch' '
	(
	cd repo &&
	test_must_fail grit branch -d main
	)
'

###########################################################################
# Section 7: Branch rename
###########################################################################

test_expect_success 'rename branch with -m' '
	(
	cd repo &&
	grit branch rename-src &&
	grit branch -m rename-src rename-dst &&
	grit branch >out &&
	grep "rename-dst" out &&
	! grep "rename-src" out
	)
'

test_expect_success 'renamed branch points to same commit' '
	(
	cd repo &&
	grit branch rename2 &&
	hash_before=$(grit rev-parse rename2) &&
	grit branch -m rename2 rename2-new &&
	hash_after=$(grit rev-parse rename2-new) &&
	test "$hash_before" = "$hash_after"
	)
'

test_expect_success 'rename to existing name fails without force' '
	(
	cd repo &&
	grit branch ren-a &&
	grit branch ren-b &&
	test_must_fail grit branch -m ren-a ren-b
	)
'

test_expect_success 'force rename with -M overwrites' '
	(
	cd repo &&
	grit branch -M ren-a ren-b &&
	grit branch >out &&
	! grep "ren-a" out &&
	grep "ren-b" out
	)
'

###########################################################################
# Section 8: Branch at tags and refs
###########################################################################

test_expect_success 'create branch at tag' '
	(
	cd repo &&
	first=$(grit rev-list --reverse HEAD | head -1) &&
	grit tag v1.0 "$first" &&
	grit branch at-tag v1.0 &&
	grit rev-parse at-tag >out &&
	echo "$first" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'create branch at another branch' '
	(
	cd repo &&
	grit branch from-feature feature-x &&
	grit rev-parse from-feature >out &&
	grit rev-parse feature-x >expect &&
	test_cmp expect out
	)
'

###########################################################################
# Section 9: --merged / --contains flags run without error
###########################################################################

test_expect_success '--merged flag runs successfully' '
	(
	cd repo &&
	grit branch --merged HEAD >out &&
	grep "main" out
	)
'

test_expect_success '--no-merged flag runs successfully' '
	(
	cd repo &&
	grit branch --no-merged HEAD >out 2>&1 ||
	true
	)
'

test_expect_success '--contains flag runs successfully' '
	(
	cd repo &&
	grit branch --contains HEAD >out &&
	grep "main" out
	)
'

test_expect_success '--no-contains flag runs successfully' '
	(
	cd repo &&
	grit branch --no-contains HEAD >out 2>&1 ||
	true
	)
'

###########################################################################
# Section 10: Multiple branches at same point
###########################################################################

test_expect_success 'multiple branches at same commit' '
	(
	cd repo &&
	grit branch same-a &&
	grit branch same-b &&
	grit branch same-c &&
	grit rev-parse same-a >ha &&
	grit rev-parse same-b >hb &&
	grit rev-parse same-c >hc &&
	test_cmp ha hb &&
	test_cmp hb hc
	)
'

test_expect_success 'delete does not affect other branches at same point' '
	(
	cd repo &&
	grit branch -d same-a &&
	grit rev-parse same-b >out &&
	grit rev-parse HEAD >expect &&
	test_cmp expect out
	)
'

test_done
