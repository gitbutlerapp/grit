#!/bin/sh
# Test for-each-ref on detached HEAD and %(HEAD) marker.

test_description='for-each-ref detached HEAD and %(HEAD) marker'

. ./test-lib.sh

GIT_COMMITTER_EMAIL=git@comm.iter.xz
GIT_COMMITTER_NAME='C O Mmiter'
GIT_AUTHOR_NAME='A U Thor'
GIT_AUTHOR_EMAIL=git@au.thor.xz
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

M=1130000000
Z=+0000
export M Z

doit () {
	OFFSET=$1 &&
	NAME=$2 &&
	shift 2 &&
	PARENTS= &&
	for P
	do
		PARENTS="$PARENTS -p $P"
	done &&
	GIT_COMMITTER_DATE="$(($M + $OFFSET)) $Z" &&
	GIT_AUTHOR_DATE="$GIT_COMMITTER_DATE" &&
	export GIT_COMMITTER_DATE GIT_AUTHOR_DATE &&
	commit=$(echo "$NAME" | git commit-tree "$(git write-tree)" $PARENTS) &&
	echo "$commit"
}

test_expect_success 'setup repo with branches' '
	(
	grit init repo &&
	cd repo &&
	first=$(doit 1 first) &&
	second=$(doit 2 second "$first") &&
	third=$(doit 3 third "$second") &&
	git update-ref refs/heads/master "$third" &&
	git update-ref refs/heads/other "$second" &&
	git update-ref refs/heads/feature "$first" &&
	echo "$first" >../oid_first &&
	echo "$second" >../oid_second &&
	echo "$third" >../oid_third
	)
'

test_expect_success 'for-each-ref lists branches on detached HEAD' '
	(
	cd repo &&
	third=$(cat ../oid_third) &&
	echo "$third" >.git/HEAD &&
	git for-each-ref --format="%(refname)" refs/heads/ >actual &&
	grep "refs/heads/master" actual &&
	grep "refs/heads/other" actual &&
	grep "refs/heads/feature" actual
	)
'

test_expect_success 'for-each-ref count is correct on detached HEAD' '
	(
	cd repo &&
	third=$(cat ../oid_third) &&
	echo "$third" >.git/HEAD &&
	git for-each-ref --format="%(refname)" refs/heads/ >actual &&
	count=$(wc -l <actual | tr -d " ") &&
	test "$count" = "3"
	)
'

test_expect_success 'for-each-ref %(refname:short) on detached HEAD' '
	(
	cd repo &&
	third=$(cat ../oid_third) &&
	echo "$third" >.git/HEAD &&
	git for-each-ref --format="%(refname:short)" refs/heads/ >actual &&
	grep "master" actual &&
	grep "other" actual &&
	grep "feature" actual
	)
'

test_expect_success 'for-each-ref %(objectname) on detached HEAD' '
	(
	cd repo &&
	third=$(cat ../oid_third) &&
	second=$(cat ../oid_second) &&
	first=$(cat ../oid_first) &&
	echo "$third" >.git/HEAD &&
	git for-each-ref --format="%(refname:short) %(objectname)" refs/heads/ >actual &&
	grep "master $third" actual &&
	grep "other $second" actual &&
	grep "feature $first" actual
	)
'

test_expect_success '%(HEAD) marks the current branch' '
	(
	cd repo &&
	git checkout master 2>/dev/null &&
	git for-each-ref --format="%(HEAD) %(refname:short)" refs/heads/ >actual &&
	grep "^\\* master$" actual &&
	grep "^  other$" actual
	)
'

test_expect_success '%(HEAD) shows no star on detached HEAD' '
	(
	cd repo &&
	third=$(cat ../oid_third) &&
	echo "$third" >.git/HEAD &&
	git for-each-ref --format="%(HEAD) %(refname:short)" refs/heads/ >actual &&
	! grep "^\\*" actual
	)
'

test_expect_success 'for-each-ref --sort=refname on detached HEAD' '
	(
	cd repo &&
	third=$(cat ../oid_third) &&
	echo "$third" >.git/HEAD &&
	git for-each-ref --format="%(refname)" --sort=refname refs/heads/ >actual &&
	head -1 actual >first_ref &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect first_ref
	)
'

test_expect_success 'for-each-ref with tags on detached HEAD' '
	(
	cd repo &&
	third=$(cat ../oid_third) &&
	echo "$third" >.git/HEAD &&
	git update-ref refs/tags/v1.0 "$third" &&
	git for-each-ref --format="%(refname)" refs/tags/ >actual &&
	grep "refs/tags/v1.0" actual
	)
'

test_expect_success '%(objectname:short) shows abbreviated hash' '
	(
	cd repo &&
	git for-each-ref --format="%(objectname:short)" refs/heads/ >actual &&
	count=$(wc -l <actual | tr -d " ") &&
	test "$count" = "3" &&
	len=$(head -1 actual | wc -c | tr -d " ") &&
	test "$len" -le 12
	)
'

test_done
