#!/bin/sh
# Tests for commit-tree: orphan (no parents), single parent, multiple parents,
# -m message, -F file, stdin message, --encoding, and edge cases.

test_description='commit-tree with orphan, single, and multiple parents'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with multiple commits' '
	(
	grit init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo file1 >a.txt &&
	git add a.txt &&
	grit commit -m "commit A" &&
	echo file2 >b.txt &&
	git add b.txt &&
	grit commit -m "commit B" &&
	git checkout -b side HEAD~1 &&
	echo file3 >c.txt &&
	git add c.txt &&
	grit commit -m "commit C" &&
	git checkout main
	)
'

# ── orphan commit (no parents) ───────────────────────────────────────────

test_expect_success 'commit-tree with no parent creates orphan commit' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	oid=$(echo "orphan" | grit commit-tree "$tree") &&
	test -n "$oid" &&
	git cat-file -p "$oid" >out &&
	! grep "^parent " out
	)
'

test_expect_success 'orphan commit is a valid commit object' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	oid=$(echo "orphan check" | grit commit-tree "$tree") &&
	type=$(git cat-file -t "$oid") &&
	test "$type" = "commit"
	)
'

test_expect_success 'orphan commit references correct tree' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	oid=$(echo "tree check" | grit commit-tree "$tree") &&
	git cat-file -p "$oid" >out &&
	grep "^tree $tree" out
	)
'

test_expect_success 'orphan commit with -m sets message' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	oid=$(grit commit-tree "$tree" -m "orphan message") &&
	git cat-file -p "$oid" >out &&
	grep "orphan message" out
	)
'

test_expect_success 'orphan commit with -F file sets message' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	echo "file based orphan" >../orphan-msg.txt &&
	oid=$(grit commit-tree "$tree" -F ../orphan-msg.txt) &&
	git cat-file -p "$oid" >out &&
	grep "file based orphan" out
	)
'

test_expect_success 'orphan commit message from stdin' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	oid=$(echo "stdin orphan" | grit commit-tree "$tree") &&
	git cat-file -p "$oid" >out &&
	grep "stdin orphan" out
	)
'

test_expect_success 'multiple orphan commits are independent' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	oid1=$(echo "orphan 1" | grit commit-tree "$tree") &&
	oid2=$(echo "orphan 2" | grit commit-tree "$tree") &&
	test "$oid1" != "$oid2"
	)
'

# ── single parent ────────────────────────────────────────────────────────

test_expect_success 'commit-tree with -p sets single parent' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	parent=$(grit rev-parse HEAD) &&
	oid=$(echo "child" | grit commit-tree "$tree" -p "$parent") &&
	git cat-file -p "$oid" >out &&
	grep "^parent $parent" out
	)
'

test_expect_success 'commit-tree with -p has exactly one parent line' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	parent=$(grit rev-parse HEAD) &&
	oid=$(echo "one parent" | grit commit-tree "$tree" -p "$parent") &&
	git cat-file -p "$oid" >out &&
	test $(grep -c "^parent " out) -eq 1
	)
'

test_expect_success 'child commit references correct parent' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	parent=$(grit rev-parse HEAD) &&
	oid=$(echo "verify parent" | grit commit-tree "$tree" -p "$parent") &&
	git cat-file -p "$oid" >out &&
	parent_from_commit=$(sed -n "s/^parent //p" out) &&
	test "$parent_from_commit" = "$parent"
	)
'

# ── multiple parents (merge commit) ──────────────────────────────────────

test_expect_success 'commit-tree with two -p creates merge commit' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	p1=$(grit rev-parse main) &&
	p2=$(grit rev-parse side) &&
	oid=$(echo "merge" | grit commit-tree "$tree" -p "$p1" -p "$p2") &&
	git cat-file -p "$oid" >out &&
	test $(grep -c "^parent " out) -eq 2
	)
'

test_expect_success 'merge commit lists parents in order' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	p1=$(grit rev-parse main) &&
	p2=$(grit rev-parse side) &&
	oid=$(echo "ordered merge" | grit commit-tree "$tree" -p "$p1" -p "$p2") &&
	git cat-file -p "$oid" >out &&
	sed -n "s/^parent //p" out >parents &&
	head -1 parents >first &&
	tail -1 parents >second &&
	echo "$p1" >expect_first &&
	echo "$p2" >expect_second &&
	test_cmp expect_first first &&
	test_cmp expect_second second
	)
'

test_expect_success 'commit-tree with three parents (octopus)' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	p1=$(grit rev-parse main) &&
	p2=$(grit rev-parse side) &&
	p3=$(grit rev-parse main~1) &&
	oid=$(echo "octopus" | grit commit-tree "$tree" -p "$p1" -p "$p2" -p "$p3") &&
	git cat-file -p "$oid" >out &&
	test $(grep -c "^parent " out) -eq 3
	)
'

# ── message variations ───────────────────────────────────────────────────

test_expect_success 'commit-tree -m with empty message' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	oid=$(grit commit-tree "$tree" -m "") &&
	type=$(git cat-file -t "$oid") &&
	test "$type" = "commit"
	)
'

test_expect_success 'commit-tree -m with multiline message' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	oid=$(grit commit-tree "$tree" -m "line one
line two
line three") &&
	git cat-file -p "$oid" >out &&
	grep "line one" out &&
	grep "line two" out &&
	grep "line three" out
	)
'

test_expect_success 'commit-tree -F with multiline file' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	printf "subject\n\nbody line 1\nbody line 2\n" >../multi-msg.txt &&
	oid=$(grit commit-tree "$tree" -F ../multi-msg.txt) &&
	git cat-file -p "$oid" >out &&
	grep "subject" out &&
	grep "body line 1" out
	)
'

# ── tree variations ──────────────────────────────────────────────────────

test_expect_success 'commit-tree with different trees produces different commits' '
	(
	cd repo &&
	tree1=$(grit rev-parse main^{tree}) &&
	tree2=$(grit rev-parse side^{tree}) &&
	test "$tree1" != "$tree2" &&
	oid1=$(echo "tree1" | grit commit-tree "$tree1") &&
	oid2=$(echo "tree1" | grit commit-tree "$tree2") &&
	test "$oid1" != "$oid2"
	)
'

test_expect_success 'commit-tree with empty tree' '
	(
	cd repo &&
	empty_tree=$(git mktree </dev/null) &&
	oid=$(echo "empty tree" | grit commit-tree "$empty_tree") &&
	type=$(git cat-file -t "$oid") &&
	test "$type" = "commit"
	)
'

# ── author/committer from environment ─────────────────────────────────────

test_expect_success 'commit-tree uses GIT_AUTHOR_NAME from env' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	oid=$(echo "env author" | GIT_AUTHOR_NAME="Env Author" \
	      GIT_AUTHOR_EMAIL="env@example.com" \
	      grit commit-tree "$tree") &&
	git cat-file -p "$oid" >out &&
	grep "Env Author" out
	)
'

test_expect_success 'commit-tree uses GIT_COMMITTER_NAME from env' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	oid=$(echo "env committer" | GIT_COMMITTER_NAME="Env Committer" \
	      GIT_COMMITTER_EMAIL="committer@example.com" \
	      grit commit-tree "$tree") &&
	git cat-file -p "$oid" >out &&
	grep "Env Committer" out
	)
'

# ── chaining commits ─────────────────────────────────────────────────────

test_expect_success 'chain of commit-tree calls builds a linear history' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	c1=$(echo "chain 1" | grit commit-tree "$tree") &&
	c2=$(echo "chain 2" | grit commit-tree "$tree" -p "$c1") &&
	c3=$(echo "chain 3" | grit commit-tree "$tree" -p "$c2") &&
	git cat-file -p "$c3" >out &&
	grep "^parent $c2" out &&
	git cat-file -p "$c2" >out2 &&
	grep "^parent $c1" out2
	)
'

test_expect_success 'commit-tree result is usable as update-ref target' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	oid=$(echo "for ref" | grit commit-tree "$tree") &&
	grit update-ref refs/heads/from-commit-tree "$oid" &&
	resolved=$(grit rev-parse refs/heads/from-commit-tree) &&
	test "$resolved" = "$oid"
	)
'

# ── error cases ──────────────────────────────────────────────────────────

test_expect_success 'commit-tree with same tree and message yields same hash' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	oid1=$(GIT_AUTHOR_DATE="2020-01-01T00:00:00+0000" \
	       GIT_COMMITTER_DATE="2020-01-01T00:00:00+0000" \
	       grit commit-tree "$tree" -m "deterministic") &&
	oid2=$(GIT_AUTHOR_DATE="2020-01-01T00:00:00+0000" \
	       GIT_COMMITTER_DATE="2020-01-01T00:00:00+0000" \
	       grit commit-tree "$tree" -m "deterministic") &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'commit-tree output is exactly 40 hex chars' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	oid=$(echo "length check" | grit commit-tree "$tree") &&
	len=$(echo -n "$oid" | wc -c) &&
	test "$len" -eq 40
	)
'

test_done
