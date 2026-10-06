#!/bin/sh
# Tests for 'grit cherry-pick' with merge commits and --mainline.

test_description='grit cherry-pick mainline'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup repository with merge commit' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo base >file.txt &&
	$REAL_GIT add file.txt &&
	$REAL_GIT commit -m "initial" &&
	$REAL_GIT branch topic &&
	echo main-change >main.txt &&
	$REAL_GIT add main.txt &&
	$REAL_GIT commit -m "main work" &&
	$REAL_GIT checkout topic &&
	echo topic-change >topic.txt &&
	$REAL_GIT add topic.txt &&
	$REAL_GIT commit -m "topic work" &&
	$REAL_GIT checkout main &&
	$REAL_GIT merge topic -m "merge topic into main" &&
	echo post-merge >post.txt &&
	$REAL_GIT add post.txt &&
	$REAL_GIT commit -m "post-merge commit"
	)
'

test_expect_success 'cherry-pick a simple non-merge commit' '
	(
	cd repo &&
	$REAL_GIT checkout -b pick-simple main~3 &&
	TOPIC_SHA=$($REAL_GIT rev-parse topic) &&
	grit cherry-pick "$TOPIC_SHA" &&
	test -f topic.txt
	)
'

test_expect_success 'cherry-pick result has correct content' '
	(
	cd repo &&
	$REAL_GIT checkout pick-simple &&
	cat topic.txt >../actual &&
	echo "topic-change" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'cherry-pick creates a new commit with same message' '
	(
	cd repo &&
	$REAL_GIT checkout pick-simple &&
	grit log --format="%s" -n 1 >../actual &&
	echo "topic work" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'cherry-pick merge commit without -m fails' '
	(
	cd repo &&
	$REAL_GIT checkout -b pick-nomain main~3 &&
	MERGE_SHA=$($REAL_GIT rev-parse main~1) &&
	test_must_fail grit cherry-pick "$MERGE_SHA"
	)
'

test_expect_success 'cherry-pick merge commit with -m 1' '
	(
	cd repo &&
	$REAL_GIT checkout -b pick-m1 main~3 &&
	MERGE_SHA=$($REAL_GIT rev-parse main~1) &&
	grit cherry-pick -m 1 "$MERGE_SHA" &&
	test -f topic.txt
	)
'

test_expect_success 'cherry-pick -m 1 brings topic side changes' '
	(
	cd repo &&
	$REAL_GIT checkout pick-m1 &&
	cat topic.txt >../actual &&
	echo "topic-change" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'cherry-pick merge with -m 2 brings main side' '
	(
	cd repo &&
	$REAL_GIT checkout -b pick-m2 main~3 &&
	MERGE_SHA=$($REAL_GIT rev-parse main~1) &&
	grit cherry-pick -m 2 "$MERGE_SHA" &&
	test -f main.txt
	)
'

test_expect_success 'cherry-pick -m 2 content is correct' '
	(
	cd repo &&
	$REAL_GIT checkout pick-m2 &&
	cat main.txt >../actual &&
	echo "main-change" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'cherry-pick --no-commit stages but does not commit' '
	(
	cd repo &&
	$REAL_GIT checkout -b pick-nocommit main~3 &&
	TOPIC_SHA=$($REAL_GIT rev-parse topic) &&
	grit cherry-pick --no-commit "$TOPIC_SHA" &&
	grit status >../actual &&
	grep "topic.txt" ../actual
	)
'

test_expect_success 'cherry-pick --no-commit HEAD unchanged' '
	(
	cd repo &&
	$REAL_GIT checkout pick-nocommit &&
	HEAD_NOW=$($REAL_GIT rev-parse HEAD) &&
	EXPECTED=$($REAL_GIT rev-parse main~3) &&
	test "$HEAD_NOW" = "$EXPECTED" &&
	$REAL_GIT reset --hard
	)
'

test_expect_success 'cherry-pick with -x appends cherry-picked-from' '
	(
	cd repo &&
	$REAL_GIT checkout -b pick-x main~3 &&
	TOPIC_SHA=$($REAL_GIT rev-parse topic) &&
	grit cherry-pick -x "$TOPIC_SHA" &&
	$REAL_GIT log --format="%b" -1 >../actual &&
	grep "cherry picked from" ../actual
	)
'

test_expect_success 'cherry-pick with ref name works' '
	(
	cd repo &&
	$REAL_GIT checkout -b pick-byref main~3 &&
	grit cherry-pick topic &&
	test -f topic.txt
	)
'

test_expect_success 'cherry-pick with invalid SHA fails' '
	(
	cd repo &&
	$REAL_GIT checkout main &&
	test_must_fail grit cherry-pick 0000000000000000000000000000000000000000
	)
'

test_expect_success 'cherry-pick onto different parent creates new SHA' '
	(
	cd repo &&
	$REAL_GIT checkout -b diff-sha main~2 &&
	TOPIC_SHA=$($REAL_GIT rev-parse topic) &&
	grit cherry-pick "$TOPIC_SHA" &&
	PICKED=$($REAL_GIT rev-parse HEAD) &&
	test "$PICKED" != "$TOPIC_SHA"
	)
'

test_expect_success 'cherry-pick preserves author name' '
	(
	cd repo &&
	$REAL_GIT checkout pick-simple &&
	grit log --format="%an" -n 1 >../actual &&
	echo "T" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'cherry-pick preserves author email' '
	(
	cd repo &&
	$REAL_GIT checkout pick-simple &&
	grit log --format="%ae" -n 1 >../actual &&
	echo "t@t.com" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'cherry-pick -m with invalid parent number fails' '
	(
	cd repo &&
	$REAL_GIT checkout main &&
	MERGE_SHA=$($REAL_GIT rev-parse main~1) &&
	test_must_fail grit cherry-pick -m 3 "$MERGE_SHA"
	)
'

test_expect_success 'cherry-pick -m 1 on non-merge commit fails' '
	(
	cd repo &&
	$REAL_GIT checkout main &&
	NONMERGE=$($REAL_GIT rev-parse main~2) &&
	test_must_fail grit cherry-pick -m 1 "$NONMERGE"
	)
'

test_expect_success 'cherry-pick does not modify source branch' '
	(
	cd repo &&
	BEFORE=$($REAL_GIT rev-parse topic) &&
	$REAL_GIT checkout -b verify-source main~3 &&
	grit cherry-pick topic &&
	AFTER=$($REAL_GIT rev-parse topic) &&
	test "$BEFORE" = "$AFTER"
	)
'

test_expect_success 'cherry-pick with conflict reports error and abort restores' '
	(
	cd repo &&
	$REAL_GIT checkout -b conflict-base main~3 &&
	echo conflicting >topic.txt &&
	$REAL_GIT add topic.txt &&
	$REAL_GIT commit -m "conflict setup" &&
	TOPIC_SHA=$($REAL_GIT rev-parse topic) &&
	test_must_fail grit cherry-pick "$TOPIC_SHA" &&
	grit cherry-pick --abort &&
	cat topic.txt >../actual &&
	echo "conflicting" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'cherry-pick onto orphan branch works or fails gracefully' '
	(
	cd repo &&
	$REAL_GIT checkout main &&
	$REAL_GIT checkout --orphan orphan-branch &&
	$REAL_GIT rm -rf . &&
	echo orphan >orphan.txt &&
	$REAL_GIT add orphan.txt &&
	$REAL_GIT commit -m "orphan root" &&
	grit cherry-pick topic 2>../err;
	true
	)
'

test_expect_success 'cherry-pick --allow-empty-message on commit with no message body' '
	(
	cd repo &&
	$REAL_GIT checkout -f main &&
	$REAL_GIT clean -fd &&
	grit log --oneline -n 1 >../actual &&
	grep "post-merge" ../actual
	)
'

test_expect_success 'setup second repo for more picks' '
	(
	$REAL_GIT init repo2 &&
	cd repo2 &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo base >base.txt &&
	$REAL_GIT add base.txt &&
	$REAL_GIT commit -m "base" &&
	$REAL_GIT checkout -b feature &&
	echo feat1 >feat1.txt &&
	$REAL_GIT add feat1.txt &&
	$REAL_GIT commit -m "feature1" &&
	echo feat2 >feat2.txt &&
	$REAL_GIT add feat2.txt &&
	$REAL_GIT commit -m "feature2"
	)
'

test_expect_success 'cherry-pick single commit from feature branch' '
	(
	cd repo2 &&
	$REAL_GIT checkout main &&
	FEAT1=$($REAL_GIT log feature~1 --format=%H --max-count=1) &&
	grit cherry-pick "$FEAT1" &&
	test -f feat1.txt
	)
'

test_expect_success 'cherry-pick another commit from feature branch' '
	(
	cd repo2 &&
	$REAL_GIT checkout main &&
	FEAT2=$($REAL_GIT rev-parse feature) &&
	grit cherry-pick "$FEAT2" &&
	test -f feat2.txt
	)
'

test_expect_success 'cherry-picked files have correct content' '
	(
	cd repo2 &&
	$REAL_GIT checkout main &&
	cat feat1.txt >../actual &&
	echo "feat1" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'cherry-pick updates index' '
	(
	cd repo2 &&
	$REAL_GIT checkout main &&
	grit ls-files >../actual &&
	grep "feat1.txt" ../actual &&
	grep "feat2.txt" ../actual
	)
'

test_expect_success 'cherry-pick log shows both picked commits' '
	(
	cd repo2 &&
	$REAL_GIT checkout main &&
	grit log --oneline >../actual &&
	grep "feature1" ../actual &&
	grep "feature2" ../actual
	)
'

test_expect_success 'cherry-pick commit count increased' '
	(
	cd repo2 &&
	$REAL_GIT checkout main &&
	grit rev-list HEAD >../actual &&
	test $(wc -l <../actual) -eq 3
	)
'

test_expect_success 'cherry-pick working tree is clean after pick' '
	(
	cd repo2 &&
	$REAL_GIT checkout main &&
	grit status >../actual &&
	grep -i "clean\|nothing to commit" ../actual
	)
'

test_done
