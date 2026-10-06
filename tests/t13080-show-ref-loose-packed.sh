#!/bin/sh
# Tests for 'grit show-ref' with loose and packed refs.

test_description='grit show-ref loose and packed'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup repository with multiple refs' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo first >file.txt &&
	$REAL_GIT add file.txt &&
	$REAL_GIT commit -m "first" &&
	$REAL_GIT branch branch-one &&
	echo second >file.txt &&
	$REAL_GIT add file.txt &&
	$REAL_GIT commit -m "second" &&
	$REAL_GIT branch branch-two &&
	echo third >file.txt &&
	$REAL_GIT add file.txt &&
	$REAL_GIT commit -m "third" &&
	$REAL_GIT tag v1.0 HEAD~2 &&
	$REAL_GIT tag v2.0 HEAD~1 &&
	$REAL_GIT tag v3.0 HEAD &&
	$REAL_GIT tag -a annotated-tag -m "annotated" HEAD
	)
'

test_expect_success 'show-ref lists all loose refs' '
	(
	cd repo &&
	grit show-ref >../actual &&
	grep "refs/heads/main" ../actual &&
	grep "refs/heads/branch-one" ../actual &&
	grep "refs/tags/v1.0" ../actual
	)
'

test_expect_success 'show-ref output format is SHA space refname' '
	(
	cd repo &&
	grit show-ref refs/heads/main >../actual &&
	SHA=$($REAL_GIT rev-parse main) &&
	echo "$SHA refs/heads/main" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'show-ref --branches shows only branches' '
	(
	cd repo &&
	grit show-ref --branches >../actual &&
	grep "refs/heads/" ../actual &&
	! grep "refs/tags/" ../actual
	)
'

test_expect_success 'show-ref --tags shows only tags' '
	(
	cd repo &&
	grit show-ref --tags >../actual &&
	grep "refs/tags/" ../actual &&
	! grep "refs/heads/" ../actual
	)
'

test_expect_success 'show-ref --head includes HEAD' '
	(
	cd repo &&
	grit show-ref --head >../actual &&
	head -1 ../actual >../first_line &&
	grep "HEAD" ../first_line
	)
'

test_expect_success 'show-ref --verify with valid ref succeeds' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/main >../actual &&
	SHA=$($REAL_GIT rev-parse main) &&
	grep "$SHA" ../actual
	)
'

test_expect_success 'show-ref --verify with invalid ref fails' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify refs/heads/nonexistent
	)
'

test_expect_success 'show-ref --exists with valid ref returns 0' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/main
	)
'

test_expect_success 'show-ref --exists with invalid ref returns nonzero' '
	(
	cd repo &&
	test_must_fail grit show-ref --exists refs/heads/nonexistent
	)
'

test_expect_success 'show-ref --hash shows only SHA' '
	(
	cd repo &&
	grit show-ref --hash refs/heads/main >../actual &&
	SHA=$($REAL_GIT rev-parse main) &&
	echo "$SHA" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'show-ref --hash with abbreviation' '
	(
	cd repo &&
	grit show-ref --hash=7 refs/heads/main >../actual &&
	SHA=$($REAL_GIT rev-parse --short=7 main) &&
	echo "$SHA" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'show-ref --abbrev abbreviates SHAs' '
	(
	cd repo &&
	grit show-ref --abbrev refs/heads/main >../actual &&
	! grep "$($REAL_GIT rev-parse main)" ../actual &&
	grep "refs/heads/main" ../actual
	)
'

test_expect_success 'show-ref --quiet suppresses output' '
	(
	cd repo &&
	grit show-ref --quiet --verify refs/heads/main >../actual &&
	test ! -s ../actual
	)
'

test_expect_success 'show-ref with pattern filter' '
	(
	cd repo &&
	grit show-ref refs/heads/branch-one >../actual &&
	test $(wc -l <../actual) -eq 1 &&
	grep "branch-one" ../actual
	)
'

test_expect_success 'show-ref multiple patterns' '
	(
	cd repo &&
	grit show-ref refs/heads/branch-one refs/heads/branch-two >../actual &&
	grep "branch-one" ../actual &&
	grep "branch-two" ../actual
	)
'

test_expect_success 'pack all refs and show-ref still works' '
	(
	cd repo &&
	$REAL_GIT pack-refs --all &&
	grit show-ref >../actual &&
	grep "refs/heads/main" ../actual &&
	grep "refs/heads/branch-one" ../actual &&
	grep "refs/tags/v1.0" ../actual
	)
'

test_expect_success 'packed refs --verify works' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/main >../actual &&
	SHA=$($REAL_GIT rev-parse main) &&
	grep "$SHA" ../actual
	)
'

test_expect_success 'packed refs --exists works' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/main &&
	test_must_fail grit show-ref --exists refs/heads/nonexistent
	)
'

test_expect_success 'packed refs --hash works' '
	(
	cd repo &&
	grit show-ref --hash refs/heads/main >../actual &&
	SHA=$($REAL_GIT rev-parse main) &&
	echo "$SHA" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'packed refs --tags works' '
	(
	cd repo &&
	grit show-ref --tags >../actual &&
	grep "refs/tags/" ../actual
	)
'

test_expect_success 'packed refs --branches works' '
	(
	cd repo &&
	grit show-ref --branches >../actual &&
	grep "refs/heads/" ../actual &&
	! grep "refs/tags/" ../actual
	)
'

test_expect_success 'create new loose ref after packing' '
	(
	cd repo &&
	$REAL_GIT branch new-after-pack &&
	grit show-ref refs/heads/new-after-pack >../actual &&
	grep "refs/heads/new-after-pack" ../actual
	)
'

test_expect_success 'show-ref shows both packed and new loose refs' '
	(
	cd repo &&
	grit show-ref >../actual &&
	grep "refs/heads/main" ../actual &&
	grep "refs/heads/new-after-pack" ../actual
	)
'

test_expect_success 'show-ref --dereference on annotated tag' '
	(
	cd repo &&
	grit show-ref --dereference refs/tags/annotated-tag >../actual &&
	grep "refs/tags/annotated-tag$" ../actual &&
	grep "refs/tags/annotated-tag\^{}" ../actual
	)
'

test_expect_success 'show-ref --dereference peeled value is commit SHA' '
	(
	cd repo &&
	grit show-ref --dereference refs/tags/annotated-tag >../actual &&
	COMMIT_SHA=$($REAL_GIT rev-parse annotated-tag^{commit}) &&
	grep "$COMMIT_SHA.*annotated-tag\^{}" ../actual
	)
'

test_expect_success 'show-ref --dereference on lightweight tag shows single line' '
	(
	cd repo &&
	grit show-ref --dereference refs/tags/v1.0 >../actual &&
	test $(wc -l <../actual) -eq 1
	)
'

test_expect_success 'show-ref ref count is correct' '
	(
	cd repo &&
	grit show-ref --branches >../actual &&
	test $(wc -l <../actual) -ge 4
	)
'

test_expect_success 'show-ref tag count is correct' '
	(
	cd repo &&
	grit show-ref --tags >../actual &&
	test $(wc -l <../actual) -eq 4
	)
'

test_expect_success 'delete branch and show-ref reflects it' '
	(
	cd repo &&
	$REAL_GIT branch -d branch-one &&
	grit show-ref >../actual &&
	! grep "refs/heads/branch-one" ../actual
	)
'

test_expect_success 'show-ref --head shows HEAD entry' '
	(
	cd repo &&
	grit show-ref --head >../actual &&
	grep "HEAD" ../actual &&
	grep "refs/heads/" ../actual
	)
'

test_done
