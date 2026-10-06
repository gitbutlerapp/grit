#!/bin/sh
# Tests for 'grit branch -f / --force' — force-creating and updating branches.

test_description='branch -f force update and --force with tracking'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup: create repository with tagged linear history' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "first" >file.txt &&
	git add file.txt &&
	git commit -m "first" &&
	git tag c1 &&
	echo "second" >file.txt &&
	git add file.txt &&
	git commit -m "second" &&
	git tag c2 &&
	echo "third" >file.txt &&
	git add file.txt &&
	git commit -m "third" &&
	git tag c3
	)
'

# ── branch -f basics ────────────────────────────────────────────────────────

test_expect_success 'branch -f creates new branch at HEAD' '
	(
	cd repo &&
	git branch -f new-branch HEAD &&
	git rev-parse new-branch >actual &&
	git rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch -f creates new branch at specific tag' '
	(
	cd repo &&
	git branch -f at-c1 c1 &&
	git rev-parse at-c1 >actual &&
	git rev-parse c1 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch -f updates existing branch to different commit' '
	(
	cd repo &&
	git branch target c1 &&
	git rev-parse target >before &&
	git rev-parse c1 >expect_before &&
	test_cmp expect_before before &&
	git branch -f target c3 &&
	git rev-parse target >after &&
	git rev-parse c3 >expect_after &&
	test_cmp expect_after after
	)
'

test_expect_success 'branch without -f fails when branch exists' '
	(
	cd repo &&
	git branch existing c1 &&
	test_must_fail git branch existing c2 2>err
	)
'

test_expect_success 'branch -f can point branch to ancestor' '
	(
	cd repo &&
	git branch fwd c3 &&
	git branch -f fwd c1 &&
	git rev-parse fwd >actual &&
	git rev-parse c1 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch --force works same as -f' '
	(
	cd repo &&
	git branch --force fwd c2 &&
	git rev-parse fwd >actual &&
	git rev-parse c2 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch -f with full SHA as start point' '
	(
	cd repo &&
	full=$(git rev-parse c1) &&
	git branch -f sha-test "$full" &&
	git rev-parse sha-test >actual &&
	echo "$full" >expect &&
	test_cmp expect actual
	)
'

# ── branch -f repeated updates ──────────────────────────────────────────────

test_expect_success 'branch -f can update same branch multiple times' '
	(
	cd repo &&
	git branch -f bounce c1 &&
	git rev-parse bounce >a1 &&
	git rev-parse c1 >e1 &&
	test_cmp e1 a1 &&
	git branch -f bounce c2 &&
	git rev-parse bounce >a2 &&
	git rev-parse c2 >e2 &&
	test_cmp e2 a2 &&
	git branch -f bounce c3 &&
	git rev-parse bounce >a3 &&
	git rev-parse c3 >e3 &&
	test_cmp e3 a3
	)
'

# ── branch -f with no start_point defaults to HEAD ──────────────────────────

test_expect_success 'branch -f with no start point defaults to HEAD' '
	(
	cd repo &&
	git branch side c1 &&
	git branch -f side &&
	git rev-parse side >actual &&
	git rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

# ── branch -f and show-ref ──────────────────────────────────────────────────

test_expect_success 'branch -f updates ref (verified by show-ref)' '
	(
	cd repo &&
	git branch -f show-me c1 &&
	git show-ref --verify refs/heads/show-me >out &&
	grep "$(git rev-parse c1)" out
	)
'

# ── force delete vs normal delete ────────────────────────────────────────────

test_expect_success 'branch -d deletes a fully-merged branch' '
	(
	cd repo &&
	git branch merged-work c2 &&
	git branch -d merged-work &&
	test_must_fail git rev-parse --verify refs/heads/merged-work 2>err
	)
'

test_expect_success 'branch -D deletes any branch' '
	(
	cd repo &&
	git branch doomed c1 &&
	git branch -D doomed &&
	test_must_fail git rev-parse --verify refs/heads/doomed 2>err
	)
'

test_expect_success 'branch -d on nonexistent branch fails' '
	(
	cd repo &&
	test_must_fail git branch -d no-such-branch 2>err
	)
'

# ── branch -f combined with tracking ────────────────────────────────────────

test_expect_success 'setup: create remote tracking refs' '
	(
	cd repo &&
	sha3=$(git rev-parse c3) &&
	sha2=$(git rev-parse c2) &&
	git update-ref refs/remotes/origin/main "$sha3" &&
	git update-ref refs/remotes/origin/develop "$sha2"
	)
'

test_expect_success 'branch -f can target a remote-tracking ref by SHA' '
	(
	cd repo &&
	sha=$(git rev-parse refs/remotes/origin/main) &&
	git branch -f from-remote "$sha" &&
	git rev-parse from-remote >actual &&
	echo "$sha" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch -f updates branch pointing to remote develop SHA' '
	(
	cd repo &&
	sha=$(git rev-parse refs/remotes/origin/develop) &&
	git branch -f from-remote "$sha" &&
	git rev-parse from-remote >actual &&
	echo "$sha" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch -f between commits preserves both tags' '
	(
	cd repo &&
	git branch -f tag-check c2 &&
	git rev-parse c1 >t1 &&
	git rev-parse c3 >t3 &&
	test -s t1 &&
	test -s t3
	)
'

# ── branch -f preserves other branches ──────────────────────────────────────

test_expect_success 'branch -f does not affect other branches' '
	(
	cd repo &&
	git branch -f alpha c2 &&
	git branch beta c1 &&
	git rev-parse beta >before &&
	git branch -f alpha c3 &&
	git rev-parse beta >after &&
	test_cmp before after
	)
'

# ── branch -M force rename ──────────────────────────────────────────────────

test_expect_success 'branch -M force-renames even if target exists' '
	(
	cd repo &&
	git branch src-name c2 &&
	git branch dst-name c1 &&
	git branch -M src-name dst-name &&
	git rev-parse dst-name >actual &&
	git rev-parse c2 >expect &&
	test_cmp expect actual &&
	test_must_fail git rev-parse --verify refs/heads/src-name 2>err
	)
'

test_expect_success 'branch -m refuses rename over existing branch' '
	(
	cd repo &&
	git branch rename-src c2 &&
	git branch rename-dst c1 &&
	test_must_fail git branch -m rename-src rename-dst 2>err
	)
'

# ── branch -f with resolved HEAD~ and HEAD^ ─────────────────────────────────

test_expect_success 'branch -f using resolved parent of HEAD' '
	(
	cd repo &&
	parent=$(git rev-parse HEAD~1) &&
	git branch -f at-parent "$parent" &&
	git rev-parse at-parent >actual &&
	echo "$parent" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch -f using resolved HEAD^' '
	(
	cd repo &&
	caret=$(git rev-parse "HEAD^") &&
	git branch -f at-caret "$caret" &&
	git rev-parse at-caret >actual &&
	echo "$caret" >expect &&
	test_cmp expect actual
	)
'

# ── branch -f with branch name as start point ───────────────────────────────

test_expect_success 'branch -f with another branch as start point' '
	(
	cd repo &&
	git branch -f source-branch c1 &&
	git branch -f dest-branch source-branch &&
	git rev-parse dest-branch >actual &&
	git rev-parse source-branch >expect &&
	test_cmp expect actual
	)
'

# ── verify listing after force operations ────────────────────────────────────

test_expect_success 'branch listing reflects force-updated branches' '
	(
	cd repo &&
	git branch -f list-check c2 &&
	git branch >out &&
	grep "list-check" out
	)
'

test_expect_success 'show-ref shows force-updated branch' '
	(
	cd repo &&
	git branch -f ref-check c1 &&
	git show-ref refs/heads/ref-check >out &&
	grep "$(git rev-parse c1)" out
	)
'

test_expect_success 'branch -f to same commit is a no-op' '
	(
	cd repo &&
	git branch -f same-test c2 &&
	git rev-parse same-test >before &&
	git branch -f same-test c2 &&
	git rev-parse same-test >after &&
	test_cmp before after
	)
'

test_done
