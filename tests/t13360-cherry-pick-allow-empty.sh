#!/bin/sh
# Tests for grit cherry-pick with empty commits, conflicts, and various scenarios.

test_description='grit cherry-pick allow-empty and related behavior'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup: create repo with branches for cherry-pick' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "line1" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial commit" &&
	"$REAL_GIT" branch side &&
	echo "line2" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "add line2 on main" &&
	echo "line3" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "add line3 on main" &&
	"$REAL_GIT" checkout side &&
	echo "sideA" >side.txt &&
	"$REAL_GIT" add side.txt &&
	"$REAL_GIT" commit -m "add side.txt on side" &&
	echo "sideB" >>side.txt &&
	"$REAL_GIT" add side.txt &&
	"$REAL_GIT" commit -m "append sideB on side" &&
	echo "sideC" >other.txt &&
	"$REAL_GIT" add other.txt &&
	"$REAL_GIT" commit -m "add other.txt on side" &&
	"$REAL_GIT" checkout main
	)
'

###########################################################################
# Basic cherry-pick
###########################################################################

test_expect_success 'cherry-pick single commit from side branch' '
	(cd repo &&
	 SIDE_TIP=$("$REAL_GIT" rev-parse side~2) &&
	 grit cherry-pick "$SIDE_TIP") &&
	(cd repo && test -f side.txt)
'

test_expect_success 'cherry-picked file has correct content' '
	(cd repo && cat side.txt >../actual) &&
	echo "sideA" >expect &&
	test_cmp expect actual
'

test_expect_success 'cherry-pick creates a new commit' '
	(cd repo &&
	 grit log --oneline >../actual) &&
	head -1 actual >first_line &&
	grep "add side.txt on side" first_line
'

test_expect_success 'cherry-pick preserves original commit message' '
	(cd repo &&
	 grit log -n 1 --format="%s" >../actual) &&
	echo "add side.txt on side" >expect &&
	test_cmp expect actual
'

test_expect_success 'cherry-pick commit has different SHA than original' '
	(cd repo &&
	 ORIG=$("$REAL_GIT" rev-parse side~2) &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 test "$ORIG" != "$HEAD")
'

###########################################################################
# Cherry-pick multiple commits
###########################################################################

test_expect_success 'setup: reset for multi-pick tests' '
	(cd repo &&
	 "$REAL_GIT" reset --hard HEAD~1)
'

test_expect_success 'cherry-pick two commits sequentially' '
	(cd repo &&
	 C1=$("$REAL_GIT" rev-parse side~2) &&
	 C2=$("$REAL_GIT" rev-parse side~1) &&
	 grit cherry-pick "$C1" &&
	 grit cherry-pick "$C2")
'

test_expect_success 'both cherry-picked files exist' '
	(cd repo && test -f side.txt && cat side.txt >../actual) &&
	printf "sideA\nsideB\n" >expect &&
	test_cmp expect actual
'

###########################################################################
# Cherry-pick with --allow-empty
###########################################################################

test_expect_success 'setup: create empty commit on side' '
	(cd repo &&
	 "$REAL_GIT" checkout side &&
	 "$REAL_GIT" commit --allow-empty -m "empty commit on side" &&
	 "$REAL_GIT" checkout main)
'

test_expect_success 'cherry-pick empty commit with --allow-empty' '
	(cd repo &&
	 EMPTY=$("$REAL_GIT" rev-parse side) &&
	 grit cherry-pick --allow-empty "$EMPTY")
'

test_expect_success 'empty cherry-pick preserves message' '
	(cd repo &&
	 grit log -n 1 --format="%s" >../actual) &&
	echo "empty commit on side" >expect &&
	test_cmp expect actual
'

test_expect_success 'cherry-pick empty commit without --allow-empty fails' '
	(cd repo &&
	 "$REAL_GIT" reset --hard HEAD~1 &&
	 EMPTY=$("$REAL_GIT" rev-parse side) &&
	 test_must_fail grit cherry-pick "$EMPTY")
'

###########################################################################
# Cherry-pick with conflicts
###########################################################################

test_expect_success 'setup: create conflicting branches' '
	(
	"$REAL_GIT" init conflict-repo &&
	cd conflict-repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "base" >conflict.txt &&
	"$REAL_GIT" add conflict.txt &&
	"$REAL_GIT" commit -m "base" &&
	"$REAL_GIT" branch other &&
	echo "main-change" >conflict.txt &&
	"$REAL_GIT" add conflict.txt &&
	"$REAL_GIT" commit -m "main change" &&
	"$REAL_GIT" checkout other &&
	echo "other-change" >conflict.txt &&
	"$REAL_GIT" add conflict.txt &&
	"$REAL_GIT" commit -m "other change" &&
	"$REAL_GIT" checkout main &&
	cd ..
	)
'

test_expect_success 'cherry-pick conflicting commit fails' '
	(cd conflict-repo &&
	 OTHER=$("$REAL_GIT" rev-parse other) &&
	 test_must_fail grit cherry-pick "$OTHER")
'

test_expect_success 'conflict markers are present in file after failed cherry-pick' '
	(cd conflict-repo &&
	 grep "<<<<<<<" conflict.txt)
'

###########################################################################
# Cherry-pick with -n (no-commit)
###########################################################################

test_expect_success 'setup: clean repo for no-commit tests' '
	(
	"$REAL_GIT" init nocommit-repo &&
	cd nocommit-repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "initial" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial" &&
	"$REAL_GIT" branch pick-from &&
	"$REAL_GIT" checkout pick-from &&
	echo "picked" >picked.txt &&
	"$REAL_GIT" add picked.txt &&
	"$REAL_GIT" commit -m "to pick" &&
	"$REAL_GIT" checkout main &&
	cd ..
	)
'

test_expect_success 'cherry-pick -n stages changes without committing' '
	(cd nocommit-repo &&
	 PICK=$("$REAL_GIT" rev-parse pick-from) &&
	 grit cherry-pick -n "$PICK")
'

test_expect_success 'after -n cherry-pick, file exists but no new commit' '
	(cd nocommit-repo &&
	 test -f picked.txt &&
	 grit log --oneline >../actual) &&
	test_line_count = 1 actual
'

test_expect_success 'after -n cherry-pick, changes are staged' '
	(cd nocommit-repo &&
	 grit status >../actual) &&
	grep "picked.txt" actual
'

###########################################################################
# Cherry-pick range
###########################################################################

test_expect_success 'setup: repo with multiple side commits for range' '
	(
	"$REAL_GIT" init range-repo &&
	cd range-repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "base" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "base" &&
	"$REAL_GIT" branch range-src &&
	"$REAL_GIT" checkout range-src &&
	echo "r1" >r1.txt && "$REAL_GIT" add r1.txt && "$REAL_GIT" commit -m "range1" &&
	echo "r2" >r2.txt && "$REAL_GIT" add r2.txt && "$REAL_GIT" commit -m "range2" &&
	echo "r3" >r3.txt && "$REAL_GIT" add r3.txt && "$REAL_GIT" commit -m "range3" &&
	"$REAL_GIT" checkout main &&
	cd ..
	)
'

test_expect_success 'cherry-pick a range of commits' '
	(cd range-repo &&
	 grit cherry-pick range-src~2..range-src)
'

test_expect_success 'range cherry-pick applied all commits' '
	(cd range-repo &&
	 test -f r2.txt &&
	 test -f r3.txt)
'

test_expect_success 'range cherry-pick created correct number of commits' '
	(cd range-repo &&
	 grit log --oneline >../actual) &&
	test_line_count = 3 actual
'

###########################################################################
# Cherry-pick with -x
###########################################################################

test_expect_success 'cherry-pick -x appends cherry-pick line' '
	(
	"$REAL_GIT" init xrepo &&
	cd xrepo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "base" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "base" &&
	"$REAL_GIT" branch xbranch &&
	"$REAL_GIT" checkout xbranch &&
	echo "xpick" >xpick.txt &&
	"$REAL_GIT" add xpick.txt &&
	"$REAL_GIT" commit -m "to-x-pick" &&
	"$REAL_GIT" checkout main &&
	cd .. &&
	(cd xrepo &&
	 PICK=$("$REAL_GIT" rev-parse xbranch) &&
	 grit cherry-pick -x "$PICK" &&
	 grit log -n 1 --format="%b" >../actual) &&
	grep "cherry picked from commit" actual
	)
'

###########################################################################
# Edge cases
###########################################################################

test_expect_success 'cherry-pick HEAD is a no-op (results in empty)' '
	(cd repo &&
	 HEAD=$("$REAL_GIT" rev-parse HEAD) &&
	 test_must_fail grit cherry-pick "$HEAD")
'

test_expect_success 'cherry-pick invalid ref fails' '
	(cd repo &&
	 test_must_fail grit cherry-pick nonexistent-ref)
'

test_expect_success 'cherry-pick with no arguments fails' '
	(cd repo &&
	 test_must_fail grit cherry-pick)
'

test_expect_success 'cherry-pick does not modify unrelated files' '
	(
	"$REAL_GIT" init unrelated-repo &&
	cd unrelated-repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "keep" >keep.txt &&
	"$REAL_GIT" add keep.txt &&
	"$REAL_GIT" commit -m "keep file" &&
	"$REAL_GIT" branch addfile &&
	"$REAL_GIT" checkout addfile &&
	echo "extra" >extra.txt &&
	"$REAL_GIT" add extra.txt &&
	"$REAL_GIT" commit -m "add extra" &&
	"$REAL_GIT" checkout main &&
	cd .. &&
	(cd unrelated-repo && cat keep.txt >../before) &&
	(cd unrelated-repo &&
	 PICK=$("$REAL_GIT" rev-parse addfile) &&
	 grit cherry-pick "$PICK") &&
	(cd unrelated-repo && cat keep.txt >../after) &&
	test_cmp before after
	)
'

test_expect_success 'cherry-pick matches git output for simple case' '
	(
	"$REAL_GIT" init match-repo &&
	cd match-repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "base" >f.txt &&
	"$REAL_GIT" add f.txt &&
	"$REAL_GIT" commit -m "base" &&
	"$REAL_GIT" branch cpbranch &&
	"$REAL_GIT" checkout cpbranch &&
	echo "new" >new.txt &&
	"$REAL_GIT" add new.txt &&
	"$REAL_GIT" commit -m "add new" &&
	"$REAL_GIT" checkout main &&
	cd .. &&
	(cd match-repo &&
	 PICK=$("$REAL_GIT" rev-parse cpbranch) &&
	 grit cherry-pick "$PICK" &&
	 grit log -n 1 --format="%s" >../grit_out) &&
	cp -r match-repo match-repo-git &&
	(cd match-repo-git &&
	 "$REAL_GIT" reset --hard HEAD~1 &&
	 PICK=$("$REAL_GIT" rev-parse cpbranch) &&
	 "$REAL_GIT" cherry-pick "$PICK" &&
	 "$REAL_GIT" log -n 1 --format="%s" >../git_out) &&
	test_cmp git_out grit_out
	)
'

test_done
