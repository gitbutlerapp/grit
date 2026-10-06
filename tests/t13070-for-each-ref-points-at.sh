#!/bin/sh
# Tests for 'grit for-each-ref' with --points-at and related options.

test_description='grit for-each-ref --points-at'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup repository with branches and tags' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo first >file.txt &&
	$REAL_GIT add file.txt &&
	$REAL_GIT commit -m "first" &&
	$REAL_GIT branch branch-a &&
	echo second >file.txt &&
	$REAL_GIT add file.txt &&
	$REAL_GIT commit -m "second" &&
	$REAL_GIT branch branch-b &&
	echo third >file.txt &&
	$REAL_GIT add file.txt &&
	$REAL_GIT commit -m "third" &&
	$REAL_GIT tag v1.0 HEAD~2 &&
	$REAL_GIT tag v2.0 HEAD~1 &&
	$REAL_GIT tag v3.0 HEAD &&
	$REAL_GIT tag -a annotated-v1 -m "annotated tag v1" HEAD~2 &&
	$REAL_GIT tag -a annotated-v2 -m "annotated tag v2" HEAD
	)
'

test_expect_success 'for-each-ref lists all refs' '
	(
	cd repo &&
	grit for-each-ref >../actual &&
	test $(wc -l <../actual) -ge 5
	)
'

test_expect_success 'for-each-ref refs/heads/ lists only branches' '
	(
	cd repo &&
	grit for-each-ref refs/heads/ >../actual &&
	grep "refs/heads/" ../actual &&
	! grep "refs/tags/" ../actual
	)
'

test_expect_success 'for-each-ref refs/tags/ lists only tags' '
	(
	cd repo &&
	grit for-each-ref refs/tags/ >../actual &&
	grep "refs/tags/" ../actual &&
	! grep "refs/heads/" ../actual
	)
'

test_expect_success 'for-each-ref --format with refname' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname)" refs/heads/ >../actual &&
	grep "refs/heads/main" ../actual &&
	grep "refs/heads/branch-a" ../actual &&
	grep "refs/heads/branch-b" ../actual
	)
'

test_expect_success 'for-each-ref --format with refname:short' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" refs/heads/ >../actual &&
	grep "^main$" ../actual &&
	grep "^branch-a$" ../actual
	)
'

test_expect_success 'for-each-ref --format with objectname' '
	(
	cd repo &&
	grit for-each-ref --format="%(objectname)" refs/heads/main >../actual &&
	HEAD_SHA=$($REAL_GIT rev-parse main) &&
	echo "$HEAD_SHA" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'for-each-ref --format with objecttype' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype)" refs/heads/main >../actual &&
	echo "commit" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'for-each-ref annotated tag objecttype is tag' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype)" refs/tags/annotated-v1 >../actual &&
	echo "tag" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'for-each-ref lightweight tag objecttype is commit' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype)" refs/tags/v1.0 >../actual &&
	echo "commit" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'for-each-ref --points-at HEAD shows branches at HEAD' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" --points-at HEAD refs/heads/ >../actual &&
	grep "^main$" ../actual
	)
'

test_expect_success 'for-each-ref --points-at HEAD does not show old branches' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" --points-at HEAD refs/heads/ >../actual &&
	! grep "^branch-a$" ../actual
	)
'

test_expect_success 'for-each-ref --points-at first commit shows branch-a' '
	(
	cd repo &&
	FIRST=$($REAL_GIT rev-parse HEAD~2) &&
	grit for-each-ref --format="%(refname:short)" --points-at "$FIRST" refs/heads/ >../actual &&
	grep "^branch-a$" ../actual
	)
'

test_expect_success 'for-each-ref --points-at second commit shows branch-b' '
	(
	cd repo &&
	SECOND=$($REAL_GIT rev-parse HEAD~1) &&
	grit for-each-ref --format="%(refname:short)" --points-at "$SECOND" refs/heads/ >../actual &&
	grep "^branch-b$" ../actual
	)
'

test_expect_success 'for-each-ref --points-at with tags' '
	(
	cd repo &&
	FIRST=$($REAL_GIT rev-parse HEAD~2) &&
	grit for-each-ref --format="%(refname:short)" --points-at "$FIRST" refs/tags/ >../actual &&
	grep "v1.0" ../actual
	)
'

test_expect_success 'for-each-ref --points-at HEAD shows v3.0 tag' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" --points-at HEAD refs/tags/ >../actual &&
	grep "v3.0" ../actual
	)
'

test_expect_success 'for-each-ref --points-at annotated tag resolves to commit' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" --points-at HEAD refs/tags/ >../actual &&
	grep "annotated-v2" ../actual
	)
'

test_expect_success 'for-each-ref --points-at nonexistent SHA errors' '
	(
	cd repo &&
	test_must_fail grit for-each-ref --format="%(refname:short)" --points-at 0000000000000000000000000000000000000001 refs/heads/
	)
'

test_expect_success 'for-each-ref --sort=refname orders alphabetically' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" --sort=refname refs/heads/ >../actual &&
	head -1 ../actual >../first &&
	echo "branch-a" >../expect &&
	test_cmp ../expect ../first
	)
'

test_expect_success 'for-each-ref --sort=-refname orders reverse alphabetically' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" --sort=-refname refs/heads/ >../actual &&
	head -1 ../actual >../first &&
	echo "main" >../expect &&
	test_cmp ../expect ../first
	)
'

test_expect_success 'for-each-ref --count limits output' '
	(
	cd repo &&
	grit for-each-ref --count=1 refs/heads/ >../actual &&
	test $(wc -l <../actual) -eq 1
	)
'

test_expect_success 'for-each-ref --count=2 with 3 refs' '
	(
	cd repo &&
	grit for-each-ref --count=2 refs/heads/ >../actual &&
	test $(wc -l <../actual) -eq 2
	)
'

test_expect_success 'for-each-ref --contains HEAD includes HEAD branches' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" --contains HEAD refs/heads/ >../actual &&
	grep "^main$" ../actual
	)
'

test_expect_success 'for-each-ref --contains HEAD shows main' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" --contains HEAD refs/heads/ >../actual &&
	grep "^main$" ../actual
	)
'

test_expect_success 'for-each-ref --merged main shows all merged branches' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" --merged main refs/heads/ >../actual &&
	grep "^branch-a$" ../actual &&
	grep "^branch-b$" ../actual
	)
'

test_expect_success 'for-each-ref default format includes objectname' '
	(
	cd repo &&
	HEAD_SHA=$($REAL_GIT rev-parse main) &&
	grit for-each-ref refs/heads/main >../actual &&
	grep "$HEAD_SHA" ../actual
	)
'

test_expect_success 'for-each-ref default format includes objecttype' '
	(
	cd repo &&
	grit for-each-ref refs/heads/main >../actual &&
	grep "commit" ../actual
	)
'

test_expect_success 'for-each-ref default format includes refname' '
	(
	cd repo &&
	grit for-each-ref refs/heads/main >../actual &&
	grep "refs/heads/main" ../actual
	)
'

test_expect_success 'for-each-ref with no matching pattern returns empty' '
	(
	cd repo &&
	grit for-each-ref refs/nonexistent/ >../actual &&
	test ! -s ../actual
	)
'

test_expect_success 'for-each-ref --format with multiple atoms' '
	(
	cd repo &&
	grit for-each-ref --format="%(objecttype) %(refname:short)" refs/heads/main >../actual &&
	echo "commit main" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'for-each-ref --points-at with both heads and tags' '
	(
	cd repo &&
	FIRST=$($REAL_GIT rev-parse HEAD~2) &&
	grit for-each-ref --format="%(refname:short)" --points-at "$FIRST" >../actual &&
	grep "branch-a" ../actual &&
	grep "v1.0" ../actual
	)
'

test_expect_success 'for-each-ref tag count is correct' '
	(
	cd repo &&
	grit for-each-ref --format="%(refname:short)" refs/tags/ >../actual &&
	test $(wc -l <../actual) -eq 5
	)
'

test_done
