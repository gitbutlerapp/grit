#!/bin/sh
test_description='grit log --decorate and --no-decorate

Tests decoration display for branches, tags (lightweight and annotated),
HEAD, and the --decorate=full/short/auto and --no-decorate flags.'

. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup: repo with branches and tags' '
	(
	git init -b master repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@test.com" &&
	echo "a" >file.txt && git add file.txt && git commit -m "first" &&
	echo "b" >file.txt && git add file.txt && git commit -m "second" &&
	echo "c" >file.txt && git add file.txt && git commit -m "third" &&
	git tag lightweight-tag &&
	git tag -a annotated-tag -m "annotated tag message" &&
	git branch feature
	)
'

# ── Default decoration ───────────────────────────────────────────────────────

test_expect_success 'log --decorate shows decorations' '
	(
	cd repo &&
	git log --decorate --oneline -n 1 >out &&
	grep "HEAD" out
	)
'

test_expect_success 'decorations include branch name' '
	(
	cd repo &&
	git log --decorate --oneline -n 1 >out &&
	grep "master" out
	)
'

test_expect_success 'decorations include lightweight tag' '
	(
	cd repo &&
	git log --decorate --oneline -n 1 >out &&
	grep "lightweight-tag" out
	)
'

test_expect_success 'decorations include annotated tag' '
	(
	cd repo &&
	git log --decorate --oneline -n 1 >out &&
	grep "annotated-tag" out
	)
'

test_expect_success 'decorations include feature branch' '
	(
	cd repo &&
	git log --decorate --oneline -n 1 >out &&
	grep "feature" out
	)
'

test_expect_success 'decoration uses parentheses' '
	(
	cd repo &&
	git log --decorate --oneline -n 1 >out &&
	grep "(" out &&
	grep ")" out
	)
'

# ── --decorate=short ─────────────────────────────────────────────────────────

test_expect_success '--decorate=short shows short ref names' '
	(
	cd repo &&
	git log --decorate=short --oneline -n 1 >out &&
	grep "master" out &&
	! grep "refs/heads/master" out
	)
'

test_expect_success '--decorate=short shows short tag names' '
	(
	cd repo &&
	git log --decorate=short --oneline -n 1 >out &&
	grep "lightweight-tag" out &&
	! grep "refs/tags/lightweight-tag" out
	)
'

# ── --decorate=full ──────────────────────────────────────────────────────────

test_expect_success '--decorate=full shows full ref paths (not yet distinct from short)' '
	(
	cd repo &&
	git log --decorate=full --oneline -n 1 >out &&
	grep "refs/" out
	)
'

# ── --no-decorate ────────────────────────────────────────────────────────────

test_expect_success '--no-decorate suppresses decorations' '
	(
	cd repo &&
	git log --no-decorate --oneline -n 1 >out &&
	! grep "HEAD" out &&
	! grep "master" out &&
	! grep "lightweight-tag" out
	)
'

test_expect_success '--no-decorate still shows commit hash and subject' '
	(
	cd repo &&
	git log --no-decorate --oneline -n 1 >out &&
	grep "third" out
	)
'

# ── --decorate overrides --no-decorate (last wins) ───────────────────────────

test_expect_success '--decorate after --no-decorate enables decorations (last-wins)' '
	(
	cd repo &&
	git log --no-decorate --decorate --oneline -n 1 >out &&
	grep "master" out
	)
'

test_expect_success '--no-decorate after --decorate disables decorations' '
	(
	cd repo &&
	git log --decorate --no-decorate --oneline -n 1 >out &&
	! grep "master" out
	)
'

# ── Decoration on non-HEAD commits ──────────────────────────────────────────

test_expect_success 'no decoration on undecorated commits' '
	(
	cd repo &&
	git log --oneline -n 3 >out &&
	# Second line (commit "second") should have no decoration
	sed -n 2p out >line2 &&
	! grep "(" line2
	)
'

test_expect_success 'decoration only on commit that has refs' '
	(
	cd repo &&
	git log --decorate --oneline >out &&
	# Only first line (HEAD) should be decorated
	head -1 out | grep "(" &&
	tail -1 out >lastline &&
	! grep "(" lastline
	)
'

# ── Tags on different commits ────────────────────────────────────────────────

test_expect_success 'setup: tag on older commit' '
	(
	cd repo &&
	first=$(git rev-parse HEAD~2) &&
	git tag old-tag "$first"
	)
'

test_expect_success 'older commit shows its tag in decoration' '
	(
	cd repo &&
	git log --decorate --oneline >out &&
	grep "old-tag" out
	)
'

test_expect_success 'old-tag appears on correct commit line' '
	(
	cd repo &&
	git log --decorate --oneline >out &&
	grep "old-tag" out | grep "first"
	)
'

# ── Multiple branches on same commit ────────────────────────────────────────

test_expect_success 'setup: multiple branches on HEAD' '
	(
	cd repo &&
	git branch another-branch
	)
'

test_expect_success 'multiple branches shown in decoration' '
	(
	cd repo &&
	git log --decorate --oneline -n 1 >out &&
	grep "master" out &&
	grep "feature" out &&
	grep "another-branch" out
	)
'

# ── Decoration with --format ────────────────────────────────────────────────

test_expect_success '--decorate works with --format' '
	(
	cd repo &&
	git log --decorate --format="%H %s" -n 1 >out &&
	grep "third" out
	)
'

test_expect_success '--no-decorate works with --format' '
	(
	cd repo &&
	git log --no-decorate --format="%H %s" -n 1 >out &&
	grep "third" out &&
	! grep "master" out
	)
'

test_done
