#!/bin/sh
# Tests for commit-tree output verification and parent chain.

test_description='commit-tree output verification and parent chain'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ──────────────────────────────────────────────────────────────────

test_expect_success 'setup: repository with a tree' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "hello" >file.txt &&
	echo "world" >file2.txt &&
	mkdir sub &&
	echo "nested" >sub/deep.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial"
	)
'

# ── Basic commit-tree ──────────────────────────────────────────────────────

test_expect_success 'commit-tree produces a valid OID' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(echo "test message" | git commit-tree "$tree") &&
	echo "$commit" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'commit-tree output is a commit object' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(echo "msg" | git commit-tree "$tree") &&
	type=$(git cat-file -t "$commit") &&
	test "$type" = "commit"
	)
'

test_expect_success 'commit-tree with -m flag' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(git commit-tree "$tree" -m "inline message") &&
	echo "$commit" | grep -qE "^[0-9a-f]{40}$"
	)
'

# ── Commit content verification ───────────────────────────────────────────

test_expect_success 'commit-tree output contains tree line' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(echo "test" | git commit-tree "$tree") &&
	git cat-file -p "$commit" >content &&
	grep "^tree $tree" content
	)
'

test_expect_success 'commit-tree output contains author line' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(echo "test" | git commit-tree "$tree") &&
	git cat-file -p "$commit" >content &&
	grep "^author " content
	)
'

test_expect_success 'commit-tree output contains committer line' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(echo "test" | git commit-tree "$tree") &&
	git cat-file -p "$commit" >content &&
	grep "^committer " content
	)
'

test_expect_success 'commit-tree message appears in commit content' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(echo "unique test message 12345" | git commit-tree "$tree") &&
	git cat-file -p "$commit" >content &&
	grep "unique test message 12345" content
	)
'

test_expect_success 'commit-tree -m message appears in commit content' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(git commit-tree "$tree" -m "inline msg xyz") &&
	git cat-file -p "$commit" >content &&
	grep "inline msg xyz" content
	)
'

# ── Root commit (no parents) ──────────────────────────────────────────────

test_expect_success 'commit-tree without -p creates root commit' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(echo "root" | git commit-tree "$tree") &&
	git cat-file -p "$commit" >content &&
	! grep "^parent " content
	)
'

# ── Single parent ─────────────────────────────────────────────────────────

test_expect_success 'commit-tree -p sets parent commit' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	commit=$(echo "child" | git commit-tree "$tree" -p "$parent") &&
	git cat-file -p "$commit" >content &&
	grep "^parent $parent" content
	)
'

test_expect_success 'commit-tree -p parent is exactly one line' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	commit=$(echo "child" | git commit-tree "$tree" -p "$parent") &&
	git cat-file -p "$commit" >content &&
	grep -c "^parent " content >count &&
	test "$(cat count)" -eq 1
	)
'

# ── Multiple parents (merge commit) ───────────────────────────────────────

test_expect_success 'setup: create second branch for merge' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent1=$(echo "branch1" | git commit-tree "$tree") &&
	parent2=$(echo "branch2" | git commit-tree "$tree") &&
	echo "$parent1" >../parent1 &&
	echo "$parent2" >../parent2
	)
'

test_expect_success 'commit-tree with two parents creates merge commit' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	p1=$(cat ../parent1) &&
	p2=$(cat ../parent2) &&
	commit=$(echo "merge" | git commit-tree "$tree" -p "$p1" -p "$p2") &&
	git cat-file -p "$commit" >content &&
	grep -c "^parent " content >count &&
	test "$(cat count)" -eq 2
	)
'

test_expect_success 'commit-tree merge commit lists both parents' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	p1=$(cat ../parent1) &&
	p2=$(cat ../parent2) &&
	commit=$(echo "merge" | git commit-tree "$tree" -p "$p1" -p "$p2") &&
	git cat-file -p "$commit" >content &&
	grep "^parent $p1" content &&
	grep "^parent $p2" content
	)
'

test_expect_success 'commit-tree with three parents (octopus)' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	p1=$(cat ../parent1) &&
	p2=$(cat ../parent2) &&
	p3=$(echo "branch3" | git commit-tree "$tree") &&
	commit=$(echo "octopus" | git commit-tree "$tree" -p "$p1" -p "$p2" -p "$p3") &&
	git cat-file -p "$commit" >content &&
	grep -c "^parent " content >count &&
	test "$(cat count)" -eq 3
	)
'

# ── Parent chain verification ─────────────────────────────────────────────

test_expect_success 'commit chain: child → parent → grandparent' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	grandparent=$(echo "gp" | git commit-tree "$tree") &&
	parent=$(echo "p" | git commit-tree "$tree" -p "$grandparent") &&
	child=$(echo "c" | git commit-tree "$tree" -p "$parent") &&
	git cat-file -p "$child" >child_content &&
	grep "^parent $parent" child_content &&
	git cat-file -p "$parent" >parent_content &&
	grep "^parent $grandparent" parent_content
	)
'

test_expect_success 'long commit chain (5 deep)' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	prev=$(echo "gen0" | git commit-tree "$tree") &&
	for i in 1 2 3 4; do
		prev=$(echo "gen$i" | git commit-tree "$tree" -p "$prev")
	done &&
	git cat-file -p "$prev" >content &&
	grep "^parent " content
	)
'

# ── Author/committer from environment ─────────────────────────────────────

test_expect_success 'commit-tree respects GIT_AUTHOR_NAME' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	export GIT_AUTHOR_NAME="Custom Author" &&
	export GIT_AUTHOR_EMAIL="custom@example.com" &&
	commit=$(echo "env test" | git commit-tree "$tree") &&
	git cat-file -p "$commit" >content &&
	grep "^author Custom Author <custom@example.com>" content
	)
'

test_expect_success 'commit-tree respects GIT_COMMITTER_NAME' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	export GIT_COMMITTER_NAME="Custom Committer" &&
	export GIT_COMMITTER_EMAIL="committer@example.com" &&
	commit=$(echo "env test" | git commit-tree "$tree") &&
	git cat-file -p "$commit" >content &&
	grep "^committer Custom Committer <committer@example.com>" content
	)
'

# ── Determinism and uniqueness ─────────────────────────────────────────────

test_expect_success 'same tree+message+parent produces same commit with fixed dates' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	GIT_AUTHOR_DATE="1234567890 +0000" \
	GIT_COMMITTER_DATE="1234567890 +0000" \
	c1=$(echo "fixed" | git commit-tree "$tree" -p "$parent") &&
	GIT_AUTHOR_DATE="1234567890 +0000" \
	GIT_COMMITTER_DATE="1234567890 +0000" \
	c2=$(echo "fixed" | git commit-tree "$tree" -p "$parent") &&
	test "$c1" = "$c2"
	)
'

test_expect_success 'different messages produce different OIDs' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	GIT_AUTHOR_DATE="1234567890 +0000" \
	GIT_COMMITTER_DATE="1234567890 +0000" \
	c1=$(echo "msg1" | git commit-tree "$tree") &&
	GIT_AUTHOR_DATE="1234567890 +0000" \
	GIT_COMMITTER_DATE="1234567890 +0000" \
	c2=$(echo "msg2" | git commit-tree "$tree") &&
	test "$c1" != "$c2"
	)
'

test_expect_success 'different trees produce different OIDs' '
	(
	cd repo &&
	tree1=$(git rev-parse HEAD^{tree}) &&
	blob=$(echo "extra" | git hash-object -w --stdin) &&
	tree2=$(printf "100644 blob %s\textra.txt\n" "$blob" | git mktree) &&
	GIT_AUTHOR_DATE="1234567890 +0000" \
	GIT_COMMITTER_DATE="1234567890 +0000" \
	c1=$(echo "same" | git commit-tree "$tree1") &&
	GIT_AUTHOR_DATE="1234567890 +0000" \
	GIT_COMMITTER_DATE="1234567890 +0000" \
	c2=$(echo "same" | git commit-tree "$tree2") &&
	test "$c1" != "$c2"
	)
'

# ── commit-tree with -F (file) ────────────────────────────────────────────

test_expect_success 'commit-tree -F reads message from file' '
	(
	cd repo &&
	echo "message from file" >msg.txt &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(git commit-tree "$tree" -F msg.txt) &&
	git cat-file -p "$commit" >content &&
	grep "message from file" content
	)
'

test_expect_success 'commit-tree -F with multi-line message' '
	(
	cd repo &&
	printf "line one\nline two\nline three\n" >msg_multi.txt &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(git commit-tree "$tree" -F msg_multi.txt) &&
	git cat-file -p "$commit" >content &&
	grep "line one" content &&
	grep "line two" content &&
	grep "line three" content
	)
'

# ── Commit size ────────────────────────────────────────────────────────────

test_expect_success 'commit-tree commit has non-zero size' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(echo "size test" | git commit-tree "$tree") &&
	size=$(git cat-file -s "$commit") &&
	test "$size" -gt 0
	)
'

test_done
