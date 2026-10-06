#!/bin/sh
# Tests for rev-parse miscellaneous options: --short, --verify, --symbolic-full-name,
# --show-prefix, --show-toplevel, --git-dir, --is-bare-repository, --is-inside-work-tree,
# --is-inside-git-dir, peel suffixes, and discovery flags.

test_description='rev-parse miscellaneous flags and discovery'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with branches and tags' '
	(
	grit init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo first >file.txt &&
	git add file.txt &&
	grit commit -m "first commit" &&
	grit tag v1.0 &&
	echo second >file.txt &&
	git add file.txt &&
	grit commit -m "second commit" &&
	grit tag -a -m "annotated v2" v2.0 &&
	git checkout -b feature &&
	echo third >file.txt &&
	git add file.txt &&
	grit commit -m "third commit" &&
	git checkout master &&
	mkdir -p sub/deep
	)
'

# ── --short ───────────────────────────────────────────────────────────────

test_expect_success '--short produces abbreviated hash' '
	(
	cd repo &&
	grit rev-parse --short HEAD >actual &&
	full=$(grit rev-parse HEAD) &&
	abbrev=$(cat actual) &&
	case "$full" in
	"$abbrev"*) true ;;
	*) false ;;
	esac
	)
'

test_expect_success '--short=4 produces 4-char minimum hash' '
	(
	cd repo &&
	grit rev-parse --short=4 HEAD >actual &&
	abbrev=$(cat actual) &&
	len=${#abbrev} &&
	test "$len" -ge 4
	)
'

test_expect_success '--short=12 produces longer abbreviation' '
	(
	cd repo &&
	grit rev-parse --short=12 HEAD >actual &&
	abbrev=$(cat actual) &&
	len=${#abbrev} &&
	test "$len" -ge 12
	)
'

test_expect_success '--short works with tag ref' '
	(
	cd repo &&
	grit rev-parse --short v1.0 >actual &&
	full=$(grit rev-parse v1.0) &&
	abbrev=$(cat actual) &&
	case "$full" in
	"$abbrev"*) true ;;
	*) false ;;
	esac
	)
'

# ── --verify ──────────────────────────────────────────────────────────────

test_expect_success '--verify resolves HEAD' '
	(
	cd repo &&
	grit rev-parse --verify HEAD >actual &&
	grit rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--verify resolves full SHA' '
	(
	cd repo &&
	full=$(grit rev-parse HEAD) &&
	grit rev-parse --verify "$full" >actual &&
	echo "$full" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--verify fails on nonexistent ref' '
	(
	cd repo &&
	test_must_fail grit rev-parse --verify refs/heads/nonexistent 2>err
	)
'

test_expect_success '--verify -q fails silently' '
	(
	cd repo &&
	test_must_fail grit rev-parse --verify -q refs/heads/nonexistent 2>err &&
	test_must_be_empty err
	)
'

test_expect_success '--verify with valid ref succeeds' '
	(
	cd repo &&
	grit rev-parse --verify refs/heads/master >actual &&
	grit rev-parse master >expect &&
	test_cmp expect actual
	)
'

# ── peel suffixes ─────────────────────────────────────────────────────────

test_expect_success 'HEAD^{tree} resolves to tree object' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	type=$(git cat-file -t "$tree") &&
	test "$type" = "tree"
	)
'

test_expect_success 'HEAD^{commit} resolves to commit' '
	(
	cd repo &&
	oid=$(grit rev-parse HEAD^{commit}) &&
	full=$(grit rev-parse HEAD) &&
	test "$oid" = "$full"
	)
'

test_expect_success 'tag^{} peels annotated tag to commit' '
	(
	cd repo &&
	peeled=$(grit rev-parse v2.0^{}) &&
	type=$(git cat-file -t "$peeled") &&
	test "$type" = "commit"
	)
'

test_expect_success 'HEAD~1 resolves to parent' '
	(
	cd repo &&
	a=$(grit rev-parse HEAD~1) &&
	b=$(grit rev-parse "HEAD^") &&
	test "$a" = "$b"
	)
'

test_expect_success 'HEAD^ is same as HEAD^1' '
	(
	cd repo &&
	a=$(grit rev-parse "HEAD^") &&
	b=$(grit rev-parse HEAD~1) &&
	test "$a" = "$b"
	)
'

test_expect_success 'HEAD~2 goes back two generations' '
	(
	cd repo &&
	git checkout feature &&
	grandparent=$(grit rev-parse HEAD~2) &&
	parent=$(grit rev-parse HEAD~1) &&
	pp=$(grit rev-parse "$parent"~1) &&
	test "$grandparent" = "$pp" &&
	git checkout master
	)
'

# ── --git-dir ─────────────────────────────────────────────────────────────

test_expect_success '--git-dir returns .git at repo root' '
	(
	cd repo &&
	grit rev-parse --git-dir >actual &&
	echo ".git" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--git-dir returns relative path from subdir' '
	(
	cd repo/sub &&
	grit rev-parse --git-dir >actual &&
	dir=$(cat actual) &&
	test -d "$dir"
	)
'

# ── --show-toplevel ───────────────────────────────────────────────────────

test_expect_success '--show-toplevel returns repo root' '
	(
	cd repo &&
	grit rev-parse --show-toplevel >actual &&
	toplevel=$(cat actual) &&
	test -d "$toplevel/.git"
	)
'

test_expect_success '--show-toplevel works from subdirectory' '
	(
	cd repo/sub/deep &&
	grit rev-parse --show-toplevel >actual &&
	toplevel=$(cat actual) &&
	test -d "$toplevel/.git"
	)
'

test_expect_success '--show-toplevel gives same result from any depth' '
	(
	cd repo &&
	a=$(grit rev-parse --show-toplevel) &&
	cd sub/deep &&
	b=$(grit rev-parse --show-toplevel) &&
	test "$a" = "$b"
	)
'

# ── --show-prefix ────────────────────────────────────────────────────────

test_expect_success '--show-prefix is empty at toplevel' '
	(
	cd repo &&
	grit rev-parse --show-prefix >actual &&
	echo "" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--show-prefix shows relative path in subdir' '
	(
	cd repo/sub/deep &&
	grit rev-parse --show-prefix >actual &&
	echo "sub/deep/" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--show-prefix shows relative path one level deep' '
	(
	cd repo/sub &&
	grit rev-parse --show-prefix >actual &&
	echo "sub/" >expect &&
	test_cmp expect actual
	)
'

# ── --is-inside-work-tree ─────────────────────────────────────────────────

test_expect_success '--is-inside-work-tree true at repo root' '
	(
	cd repo &&
	echo "true" >expect &&
	grit rev-parse --is-inside-work-tree >actual &&
	test_cmp expect actual
	)
'

test_expect_success '--is-inside-work-tree true in subdir' '
	(
	cd repo/sub/deep &&
	echo "true" >expect &&
	grit rev-parse --is-inside-work-tree >actual &&
	test_cmp expect actual
	)
'

# ── --is-inside-git-dir ──────────────────────────────────────────────────

test_expect_success '--is-inside-git-dir false in worktree' '
	(
	cd repo &&
	echo "false" >expect &&
	grit rev-parse --is-inside-git-dir >actual &&
	test_cmp expect actual
	)
'

# ── --is-bare-repository ─────────────────────────────────────────────────

test_expect_success '--is-bare-repository false for normal repo' '
	(
	cd repo &&
	echo "false" >expect &&
	grit rev-parse --is-bare-repository >actual &&
	test_cmp expect actual
	)
'

test_expect_success '--is-bare-repository true for bare repo' '
	(
	grit init --bare bare-test.git &&
	cd bare-test.git &&
	echo "true" >expect &&
	grit rev-parse --is-bare-repository >actual &&
	test_cmp expect actual
	)
'

# ── ref resolution edge cases ─────────────────────────────────────────────

test_expect_success 'rev-parse resolves branch name to SHA' '
	(
	cd repo &&
	grit rev-parse master >actual &&
	test -s actual &&
	len=$(cat actual | tr -d "\n" | wc -c) &&
	test "$len" -eq 40
	)
'

test_expect_success 'rev-parse resolves tag name to SHA' '
	(
	cd repo &&
	grit rev-parse v1.0 >actual &&
	test -s actual
	)
'

test_expect_success 'rev-parse resolves HEAD to current branch tip' '
	(
	cd repo &&
	grit rev-parse HEAD >head_sha &&
	grit rev-parse master >master_sha &&
	test_cmp head_sha master_sha
	)
'

test_expect_success 'rev-parse multiple refs outputs multiple lines' '
	(
	cd repo &&
	grit rev-parse HEAD v1.0 >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'rev-parse fails on invalid ref name' '
	(
	cd repo &&
	test_must_fail grit rev-parse refs/heads/does-not-exist 2>err
	)
'

test_done
