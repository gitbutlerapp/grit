#!/bin/sh
# Tests for show-ref: patterns, --branches, --tags, --hash, --abbrev,
# --dereference, --head, --verify, --exists, --quiet, and combinations.

test_description='show-ref patterns and formatting options'
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
	echo A >file.txt &&
	git add file.txt &&
	grit commit -m "commit A" &&
	commit_A=$(grit rev-parse HEAD) &&
	grit tag lightweight-tag &&
	grit tag -a -m "annotated" annotated-tag &&
	git checkout -b topic &&
	echo B >file.txt &&
	git add file.txt &&
	grit commit -m "commit B" &&
	commit_B=$(grit rev-parse HEAD) &&
	git checkout -b side &&
	echo C >file.txt &&
	git add file.txt &&
	grit commit -m "commit C" &&
	git checkout master &&
	echo D >file.txt &&
	git add file.txt &&
	grit commit -m "commit D" &&
	grit tag v2.0 &&
	grit tag -a -m "annotated v2" v2.0-annotated
	)
'

# ── basic listing ─────────────────────────────────────────────────────────

test_expect_success 'show-ref lists all refs' '
	(
	cd repo &&
	grit show-ref >actual &&
	test $(wc -l <actual) -ge 5
	)
'

test_expect_success 'show-ref output format is SHA<space>refname' '
	(
	cd repo &&
	grit show-ref >actual &&
	while IFS= read -r line; do
		sha=$(echo "$line" | cut -d" " -f1) &&
		ref=$(echo "$line" | cut -d" " -f2) &&
		len=$(echo -n "$sha" | wc -c) &&
		test "$len" -eq 40 || return 1
	done <actual
	)
'

# ── --branches ────────────────────────────────────────────────────────────

test_expect_success '--branches shows only branch refs' '
	(
	cd repo &&
	grit show-ref --branches >actual &&
	while IFS= read -r line; do
		ref=$(echo "$line" | cut -d" " -f2) &&
		case "$ref" in
		refs/heads/*) true ;;
		*) return 1 ;;
		esac
	done <actual
	)
'

test_expect_success '--branches lists correct number of branches' '
	(
	cd repo &&
	grit show-ref --branches >actual &&
	# master, topic, side = 3
	test $(wc -l <actual) -eq 3
	)
'

# ── --tags ────────────────────────────────────────────────────────────────

test_expect_success '--tags shows only tag refs' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	while IFS= read -r line; do
		ref=$(echo "$line" | cut -d" " -f2) &&
		case "$ref" in
		refs/tags/*) true ;;
		*) return 1 ;;
		esac
	done <actual
	)
'

test_expect_success '--tags lists correct number of tags' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	test $(wc -l <actual) -eq 4
	)
'

# ── --hash ────────────────────────────────────────────────────────────────

test_expect_success '--hash shows only object IDs' '
	(
	cd repo &&
	grit show-ref --hash >actual &&
	while IFS= read -r line; do
		len=$(echo -n "$line" | wc -c) &&
		test "$len" -eq 40 || return 1
	done <actual
	)
'

test_expect_success '--hash output has same line count as full listing' '
	(
	cd repo &&
	grit show-ref >full &&
	grit show-ref --hash >hashes &&
	test $(wc -l <full) -eq $(wc -l <hashes)
	)
'

test_expect_success '--hash=7 abbreviates to 7 characters' '
	(
	cd repo &&
	grit show-ref --hash=7 >actual &&
	while IFS= read -r line; do
		len=$(echo -n "$line" | wc -c) &&
		test "$len" -eq 7 || return 1
	done <actual
	)
'

test_expect_success '--hash=4 abbreviates to minimum 4 characters' '
	(
	cd repo &&
	grit show-ref --hash=4 >actual &&
	while IFS= read -r line; do
		len=$(echo -n "$line" | wc -c) &&
		test "$len" -ge 4 || return 1
	done <actual
	)
'

# ── --abbrev ──────────────────────────────────────────────────────────────

test_expect_success '--abbrev shows abbreviated hashes with ref names' '
	(
	cd repo &&
	grit show-ref --abbrev >actual &&
	while IFS= read -r line; do
		sha=$(echo "$line" | cut -d" " -f1) &&
		len=$(echo -n "$sha" | wc -c) &&
		test "$len" -lt 40 || return 1
	done <actual
	)
'

test_expect_success '--abbrev=4 abbreviates to 4 chars' '
	(
	cd repo &&
	grit show-ref --abbrev=4 >actual &&
	while IFS= read -r line; do
		sha=$(echo "$line" | cut -d" " -f1) &&
		len=$(echo -n "$sha" | wc -c) &&
		test "$len" -ge 4 &&
		test "$len" -le 7 || return 1
	done <actual
	)
'

test_expect_success '--abbrev still shows ref names' '
	(
	cd repo &&
	grit show-ref --abbrev >actual &&
	grep "refs/heads/master" actual
	)
'

# ── --dereference ─────────────────────────────────────────────────────────

test_expect_success '-d shows peeled line for annotated tags' '
	(
	cd repo &&
	grit show-ref -d --tags >actual &&
	grep "\\^{}" actual
	)
'

test_expect_success '-d peeled line points to commit' '
	(
	cd repo &&
	grit show-ref -d --tags >actual &&
	peeled_sha=$(grep "annotated-tag\\^{}" actual | cut -d" " -f1) &&
	type=$(git cat-file -t "$peeled_sha") &&
	test "$type" = "commit"
	)
'

test_expect_success '-d does not add peeled line for lightweight tags' '
	(
	cd repo &&
	grit show-ref -d refs/tags/lightweight-tag >actual &&
	! grep "\\^{}" actual
	)
'

test_expect_success '-d peeled hash matches rev-parse peel' '
	(
	cd repo &&
	grit show-ref -d refs/tags/annotated-tag >actual &&
	peeled_sha=$(grep "\\^{}" actual | cut -d" " -f1) &&
	expected=$(grit rev-parse annotated-tag^{}) &&
	test "$peeled_sha" = "$expected"
	)
'

# ── --head ────────────────────────────────────────────────────────────────

test_expect_success '--head includes HEAD in listing' '
	(
	cd repo &&
	grit show-ref --head >actual &&
	grep "HEAD" actual
	)
'

test_expect_success '--head HEAD line appears first' '
	(
	cd repo &&
	grit show-ref --head >actual &&
	head -1 actual | grep "HEAD"
	)
'

test_expect_success '--head adds exactly one extra line' '
	(
	cd repo &&
	grit show-ref >without &&
	grit show-ref --head >with &&
	without_count=$(wc -l <without) &&
	with_count=$(wc -l <with) &&
	test $((with_count - without_count)) -eq 1
	)
'

# ── patterns ──────────────────────────────────────────────────────────────

test_expect_success 'pattern filters matching refs' '
	(
	cd repo &&
	grit show-ref refs/heads/master >actual &&
	test $(wc -l <actual) -eq 1 &&
	grep "refs/heads/master" actual
	)
'

test_expect_success 'pattern matches prefix' '
	(
	cd repo &&
	grit show-ref refs/heads/topic >actual &&
	test $(wc -l <actual) -ge 1 &&
	grep "refs/heads/topic" actual
	)
'

test_expect_success 'pattern with no match gives empty output and failure' '
	(
	cd repo &&
	test_must_fail grit show-ref refs/heads/nonexistent >actual &&
	test_must_be_empty actual
	)
'

# ── --verify ──────────────────────────────────────────────────────────────

test_expect_success '--verify resolves exact ref' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/master >actual &&
	grep "refs/heads/master" actual
	)
'

test_expect_success '--verify rejects non-existent ref' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify refs/heads/nonexistent 2>err
	)
'

test_expect_success '--verify requires full ref path' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify master 2>err
	)
'

# ── --exists ──────────────────────────────────────────────────────────────

test_expect_success '--exists succeeds for existing ref' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/master
	)
'

test_expect_success '--exists fails for missing ref' '
	(
	cd repo &&
	test_must_fail grit show-ref --exists refs/heads/nonexistent
	)
'

# ── --quiet ───────────────────────────────────────────────────────────────

test_expect_success '-q --verify suppresses output on success' '
	(
	cd repo &&
	grit show-ref -q --verify refs/heads/master >actual &&
	test_must_be_empty actual
	)
'

test_expect_success '-q --verify fails silently for missing ref' '
	(
	cd repo &&
	test_must_fail grit show-ref -q --verify refs/heads/nonexistent >actual 2>err &&
	test_must_be_empty actual
	)
'

test_done
