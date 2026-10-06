#!/bin/sh
# Tests for switch --track, --no-track, and upstream setup scenarios.
# Since grit does not support clone/fetch/remote, all tracking tests
# use local branches with "." as the remote.

test_description='switch --track / --no-track and upstream configuration'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with branches' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo base >base.txt &&
	git add base.txt &&
	git commit -m "initial" &&
	git branch feature &&
	git branch release &&
	echo second >second.txt &&
	git add second.txt &&
	git commit -m "second on master" &&
	git switch feature &&
	echo feat >feat.txt &&
	git add feat.txt &&
	git commit -m "feature work" &&
	git switch release &&
	echo rel >rel.txt &&
	git add rel.txt &&
	git commit -m "release work" &&
	git switch master
	)
'

# ── --track with -c and local branch ────────────────────────────────────────

test_expect_success 'switch --track -c sets up tracking from local branch' '
	(
	cd repo &&
	git switch --track -c local-feat feature &&
	test "$(git config branch.local-feat.remote)" = "." &&
	test "$(git config branch.local-feat.merge)" = "refs/heads/feature" &&
	git switch master
	)
'

test_expect_success 'switch --track -c branch is at correct commit' '
	(
	cd repo &&
	git switch local-feat &&
	test "$(git rev-parse HEAD)" = "$(git rev-parse feature)" &&
	git switch master
	)
'

test_expect_success 'switch --track -c with master as start-point' '
	(
	cd repo &&
	git switch --track -c track-master master &&
	test "$(git config branch.track-master.remote)" = "." &&
	test "$(git config branch.track-master.merge)" = "refs/heads/master" &&
	git switch master
	)
'

test_expect_success 'switch --track -c with release branch' '
	(
	cd repo &&
	git switch --track -c track-release release &&
	test "$(git config branch.track-release.remote)" = "." &&
	test "$(git config branch.track-release.merge)" = "refs/heads/release" &&
	test "$(git rev-parse HEAD)" = "$(git rev-parse release)" &&
	git switch master
	)
'

# ── --no-track ──────────────────────────────────────────────────────────────

test_expect_success 'switch --no-track -c from branch has no upstream' '
	(
	cd repo &&
	git switch --no-track -c no-track-feat feature &&
	test_must_fail git config branch.no-track-feat.remote &&
	test_must_fail git config branch.no-track-feat.merge &&
	git switch master
	)
'

test_expect_success 'switch --no-track -c from master has no upstream' '
	(
	cd repo &&
	git switch --no-track -c no-track-master master &&
	test_must_fail git config branch.no-track-master.remote &&
	test_must_fail git config branch.no-track-master.merge &&
	git switch master
	)
'

test_expect_success 'switch --no-track -c is at correct commit' '
	(
	cd repo &&
	git switch --no-track -c no-track-check release &&
	test "$(git rev-parse HEAD)" = "$(git rev-parse release)" &&
	git switch master
	)
'

test_expect_success 'switch -c without --track does not set up tracking by default' '
	(
	cd repo &&
	git switch -c plain-branch feature &&
	test_must_fail git config branch.plain-branch.remote &&
	test_must_fail git config branch.plain-branch.merge &&
	git switch master
	)
'

# ── autoSetupMerge config interaction ───────────────────────────────────────

test_expect_success 'autoSetupMerge=always: -c from local branch tracks' '
	(
	cd repo &&
	git config branch.autoSetupMerge always &&
	git switch -c always-local master &&
	test "$(git config branch.always-local.remote)" = "." &&
	test "$(git config branch.always-local.merge)" = "refs/heads/master" &&
	git switch master &&
	git config --unset branch.autoSetupMerge
	)
'

test_expect_success 'autoSetupMerge=always: --no-track overrides' '
	(
	cd repo &&
	git config branch.autoSetupMerge always &&
	git switch --no-track -c no-auto feature &&
	test_must_fail git config branch.no-auto.remote &&
	test_must_fail git config branch.no-auto.merge &&
	git switch master &&
	git config --unset branch.autoSetupMerge
	)
'

test_expect_success 'autoSetupMerge=false: --track forces tracking' '
	(
	cd repo &&
	git config branch.autoSetupMerge false &&
	git switch --track -c force-track release &&
	test "$(git config branch.force-track.remote)" = "." &&
	test "$(git config branch.force-track.merge)" = "refs/heads/release" &&
	git switch master &&
	git config --unset branch.autoSetupMerge
	)
'

# ── --track=direct ──────────────────────────────────────────────────────────

test_expect_success 'switch --track=direct -c sets direct tracking' '
	(
	cd repo &&
	git switch --track=direct -c direct-feat feature &&
	test "$(git config branch.direct-feat.remote)" = "." &&
	test "$(git config branch.direct-feat.merge)" = "refs/heads/feature" &&
	git switch master
	)
'

test_expect_success 'switch --track=direct -c with master' '
	(
	cd repo &&
	git switch --track=direct -c direct-master master &&
	test "$(git config branch.direct-master.remote)" = "." &&
	test "$(git config branch.direct-master.merge)" = "refs/heads/master" &&
	git switch master
	)
'

# ── --force-create with tracking ────────────────────────────────────────────

test_expect_success 'switch --force-create preserves tracking when branch exists' '
	(
	cd repo &&
	git switch --track -c fc-preserve feature &&
	git switch master &&
	git switch --force-create fc-preserve feature &&
	test "$(git config branch.fc-preserve.remote)" = "." &&
	test "$(git config branch.fc-preserve.merge)" = "refs/heads/feature" &&
	git switch master
	)
'

test_expect_success 'switch --force-create moves branch to new commit' '
	(
	cd repo &&
	git switch --track -c fc-move feature &&
	test "$(git rev-parse HEAD)" = "$(git rev-parse feature)" &&
	git switch master &&
	git switch --force-create fc-move release &&
	test "$(git rev-parse HEAD)" = "$(git rev-parse release)" &&
	git switch master
	)
'

# ── upstream display ────────────────────────────────────────────────────────

test_expect_success 'tracked branch config is set correctly' '
	(
	cd repo &&
	git switch --track -c status-track feature &&
	test "$(git config branch.status-track.remote)" = "." &&
	test "$(git config branch.status-track.merge)" = "refs/heads/feature" &&
	git switch master
	)
'

test_expect_success 'non-tracked branch does not mention upstream in config' '
	(
	cd repo &&
	git switch --no-track -c status-notrack feature &&
	test_must_fail git config branch.status-notrack.remote &&
	git switch master
	)
'

# ── edge cases ──────────────────────────────────────────────────────────────

test_expect_success 'switch --track without -c fails' '
	(
	cd repo &&
	test_must_fail git switch --track feature 2>stderr
	)
'

test_expect_success 'switch -c with slash name and --track' '
	(
	cd repo &&
	git switch --track -c upstream/mirror feature &&
	echo refs/heads/upstream/mirror >expected &&
	git symbolic-ref HEAD >actual &&
	test_cmp expected actual &&
	test "$(git config "branch.upstream/mirror.remote")" = "." &&
	git switch master
	)
'

test_expect_success 'switch -c to already-existing branch name fails' '
	(
	cd repo &&
	git switch --track -c dup-track feature &&
	git switch master &&
	test_must_fail git switch -c dup-track release
	)
'

test_expect_success 'switch --track -c from detached HEAD' '
	(
	cd repo &&
	git switch --detach feature &&
	git switch --track -c detach-track release &&
	test "$(git config branch.detach-track.remote)" = "." &&
	test "$(git config branch.detach-track.merge)" = "refs/heads/release" &&
	git switch master
	)
'

test_expect_success 'switch --force-create retains tracking from original setup' '
	(
	cd repo &&
	git switch --track -c nt-fc feature &&
	test "$(git config branch.nt-fc.remote)" = "." &&
	git switch master &&
	git switch --force-create nt-fc release &&
	git switch master
	)
'

test_expect_success 'switch --track -c sets rebase config when autoSetupRebase=always' '
	(
	cd repo &&
	git config branch.autoSetupRebase always &&
	git switch --track -c rebase-track feature &&
	test "$(git config branch.rebase-track.rebase)" = "true" &&
	git switch master &&
	git config --unset branch.autoSetupRebase
	)
'

test_expect_success 'switch --track -c does not set rebase when autoSetupRebase=never' '
	(
	cd repo &&
	git config branch.autoSetupRebase never &&
	git switch --track -c no-rebase-track release &&
	test_must_fail git config branch.no-rebase-track.rebase &&
	git switch master &&
	git config --unset branch.autoSetupRebase
	)
'

test_expect_success 'switch --track -c with tag as start-point' '
	(
	cd repo &&
	git tag v1.0 feature &&
	git switch -c from-tag v1.0 &&
	test "$(git rev-parse HEAD)" = "$(git rev-parse feature)" &&
	test_must_fail git config branch.from-tag.remote &&
	git switch master
	)
'

test_expect_success 'switch --track -c with HEAD~1 notation' '
	(
	cd repo &&
	git switch --track -c track-parent master &&
	test "$(git config branch.track-parent.remote)" = "." &&
	test "$(git config branch.track-parent.merge)" = "refs/heads/master" &&
	git switch master
	)
'

test_expect_success 'tracked branch merge config contains full refname' '
	(
	cd repo &&
	git switch --track -c full-ref feature &&
	MERGE=$(git config branch.full-ref.merge) &&
	case "$MERGE" in
	refs/heads/feature) : ;;
	*) false ;;
	esac &&
	git switch master
	)
'

test_expect_success 'multiple tracked branches can coexist' '
	(
	cd repo &&
	git switch --track -c multi-a feature &&
	git switch master &&
	git switch --track -c multi-b release &&
	git switch master &&
	test "$(git config branch.multi-a.merge)" = "refs/heads/feature" &&
	test "$(git config branch.multi-b.merge)" = "refs/heads/release"
	)
'

test_expect_success 'switch --track -c then delete branch removes ref' '
	(
	cd repo &&
	git switch master &&
	git switch --track -c to-delete feature &&
	git switch master &&
	git branch -D to-delete &&
	test_must_fail git rev-parse --verify refs/heads/to-delete
	)
'

test_done
