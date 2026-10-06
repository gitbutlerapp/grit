#!/bin/sh
# Tests for commit-tree: multiple parents, mktree input, encoding, author override.

test_description='commit-tree advanced usage'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with initial commit' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "initial" >file.txt &&
	git add file.txt &&
	git commit -m "initial commit"
	)
'

# ── Basic commit-tree ──────────────────────────────────────────────────────

test_expect_success 'commit-tree creates a commit from a tree' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(echo "basic" | git commit-tree "$tree") &&
	test -n "$oid" &&
	type=$(git cat-file -t "$oid") &&
	test "$type" = "commit"
	)
'

test_expect_success 'commit-tree with -m sets the message' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(git commit-tree "$tree" -m "hello world") &&
	git cat-file -p "$oid" >out &&
	grep "hello world" out
	)
'

test_expect_success 'commit-tree with no parent creates root commit' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(echo "root" | git commit-tree "$tree") &&
	git cat-file -p "$oid" >out &&
	! grep "^parent " out
	)
'

# ── Single parent ──────────────────────────────────────────────────────────

test_expect_success 'commit-tree with one -p sets single parent' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	oid=$(echo "child" | git commit-tree "$tree" -p "$parent") &&
	git cat-file -p "$oid" >out &&
	grep "^parent $parent" out
	)
'

# ── Multiple parents (merge commits) ──────────────────────────────────────

test_expect_success 'setup branches for multi-parent test' '
	(
	cd repo &&
	echo "branch-a" >a.txt &&
	git add a.txt &&
	git commit -m "branch a" &&
	A=$(git rev-parse HEAD) &&
	git checkout -b branch-b HEAD~1 &&
	echo "branch-b" >b.txt &&
	git add b.txt &&
	git commit -m "branch b" &&
	B=$(git rev-parse HEAD) &&
	git checkout main
	)
'

test_expect_success 'commit-tree with two parents' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	A=$(git rev-parse main) &&
	B=$(git rev-parse branch-b) &&
	oid=$(echo "merge" | git commit-tree "$tree" -p "$A" -p "$B") &&
	git cat-file -p "$oid" >out &&
	grep "^parent $A" out &&
	grep "^parent $B" out
	)
'

test_expect_success 'commit-tree with three parents' '
	(
	cd repo &&
	git checkout -b branch-c HEAD~1 &&
	echo "branch-c" >c.txt &&
	git add c.txt &&
	git commit -m "branch c" &&
	C=$(git rev-parse HEAD) &&
	git checkout main &&
	tree=$(git rev-parse HEAD^{tree}) &&
	A=$(git rev-parse main) &&
	B=$(git rev-parse branch-b) &&
	oid=$(echo "octopus" | git commit-tree "$tree" -p "$A" -p "$B" -p "$C") &&
	git cat-file -p "$oid" >out &&
	count=$(grep -c "^parent " out) &&
	test "$count" = "3"
	)
'

test_expect_success 'multi-parent commit preserves parent order' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	A=$(git rev-parse main) &&
	B=$(git rev-parse branch-b) &&
	C=$(git rev-parse branch-c) &&
	oid=$(echo "ordered" | git commit-tree "$tree" -p "$C" -p "$A" -p "$B") &&
	git cat-file -p "$oid" >out &&
	sed -n "s/^parent //p" out >parents &&
	head -1 parents >first &&
	test "$(cat first)" = "$C"
	)
'

# ── Tree from mktree ──────────────────────────────────────────────────────

test_expect_success 'commit-tree with tree from mktree' '
	(
	cd repo &&
	blob_oid=$(echo "mktree content" | git hash-object -w --stdin) &&
	mktree_out=$(printf "100644 blob %s\tmktree-file\n" "$blob_oid" | git mktree) &&
	oid=$(echo "from mktree" | git commit-tree "$mktree_out") &&
	git cat-file -p "$oid" >out &&
	grep "^tree $mktree_out" out
	)
'

test_expect_success 'commit-tree with empty tree from mktree' '
	(
	cd repo &&
	empty_tree=$(git mktree </dev/null) &&
	test "$empty_tree" = "4b825dc642cb6eb9a060e54bf8d69288fbee4904" &&
	oid=$(echo "empty tree" | git commit-tree "$empty_tree") &&
	git cat-file -p "$oid" >out &&
	grep "^tree $empty_tree" out
	)
'

test_expect_success 'mktree with multiple entries used in commit-tree' '
	(
	cd repo &&
	b1=$(echo "one" | git hash-object -w --stdin) &&
	b2=$(echo "two" | git hash-object -w --stdin) &&
	tree=$(printf "100644 blob %s\tone.txt\n100644 blob %s\ttwo.txt\n" "$b1" "$b2" | git mktree) &&
	oid=$(echo "two files" | git commit-tree "$tree") &&
	git cat-file -p "$oid" >out &&
	grep "^tree $tree" out
	)
'

test_expect_success 'mktree with subdirectory tree' '
	(
	cd repo &&
	b1=$(echo "sub-content" | git hash-object -w --stdin) &&
	subtree=$(printf "100644 blob %s\tfile-in-sub\n" "$b1" | git mktree) &&
	top=$(printf "040000 tree %s\tsubdir\n" "$subtree" | git mktree) &&
	oid=$(echo "nested" | git commit-tree "$top") &&
	type=$(git cat-file -t "$oid") &&
	test "$type" = "commit"
	)
'

# ── Encoding ──────────────────────────────────────────────────────────────

test_expect_success 'commit-tree --encoding adds encoding header' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(echo "encoded" | git commit-tree "$tree" --encoding ISO-8859-1) &&
	git cat-file -p "$oid" >out &&
	grep "^encoding ISO-8859-1" out
	)
'

test_expect_success 'commit-tree --encoding UTF-8 adds encoding header' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(echo "utf8" | git commit-tree "$tree" --encoding UTF-8) &&
	git cat-file -p "$oid" >out &&
	grep "^encoding UTF-8" out
	)
'

test_expect_success 'commit-tree --encoding EUC-JP adds encoding header' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(echo "jp" | git commit-tree "$tree" --encoding EUC-JP) &&
	git cat-file -p "$oid" >out &&
	grep "^encoding EUC-JP" out
	)
'

test_expect_success 'commit-tree without --encoding has no encoding header' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(echo "no enc" | git commit-tree "$tree") &&
	git cat-file -p "$oid" >out &&
	! grep "^encoding" out
	)
'

# ── Author/committer override via env ────────────────────────────────────

test_expect_success 'GIT_AUTHOR_NAME overrides author' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(GIT_AUTHOR_NAME="Custom Author" \
		GIT_AUTHOR_EMAIL="custom@example.com" \
		GIT_AUTHOR_DATE="2005-04-07T22:13:13+0000" \
		git commit-tree "$tree" -m "author override") &&
	git cat-file -p "$oid" >out &&
	grep "^author Custom Author <custom@example.com>" out
	)
'

test_expect_success 'GIT_COMMITTER_NAME overrides committer' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(GIT_COMMITTER_NAME="Custom Committer" \
		GIT_COMMITTER_EMAIL="committer@example.com" \
		GIT_COMMITTER_DATE="2005-04-07T22:13:13+0000" \
		git commit-tree "$tree" -m "committer override") &&
	git cat-file -p "$oid" >out &&
	grep "^committer Custom Committer <committer@example.com>" out
	)
'

test_expect_success 'author and committer can differ' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(GIT_AUTHOR_NAME="Author A" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="Committer B" GIT_COMMITTER_EMAIL="b@e.com" \
		git commit-tree "$tree" -m "different author/committer") &&
	git cat-file -p "$oid" >out &&
	grep "^author Author A <a@e.com>" out &&
	grep "^committer Committer B <b@e.com>" out
	)
'

# ── Message from file (-F) ──────────────────────────────────────────────

test_expect_success 'commit-tree -F reads message from file' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	echo "message from file" >msg.txt &&
	oid=$(git commit-tree "$tree" -F msg.txt) &&
	git cat-file -p "$oid" >out &&
	grep "message from file" out
	)
'

test_expect_success 'commit-tree -F with multiline message' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	printf "line one\n\nline three\n" >msg.txt &&
	oid=$(git commit-tree "$tree" -F msg.txt) &&
	git cat-file -p "$oid" >out &&
	grep "line one" out &&
	grep "line three" out
	)
'

# ── Error cases ──────────────────────────────────────────────────────────

test_expect_success 'commit-tree with non-hex garbage fails' '
	(
	cd repo &&
	test_must_fail git commit-tree not-a-valid-oid -m "bad" 2>err
	)
'

test_expect_success 'commit-tree requires a tree argument' '
	test_must_fail git commit-tree 2>err
'

# ── Combined features ────────────────────────────────────────────────────

test_expect_success 'commit-tree with parent + encoding + custom author' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	oid=$(GIT_AUTHOR_NAME="Encoded Author" GIT_AUTHOR_EMAIL="enc@e.com" \
		git commit-tree "$tree" -p "$parent" --encoding ISO-8859-1 -m "combined") &&
	git cat-file -p "$oid" >out &&
	grep "^parent $parent" out &&
	grep "^encoding ISO-8859-1" out &&
	grep "^author Encoded Author" out &&
	grep "combined" out
	)
'

test_expect_success 'commit-tree output is a valid 40-char hex OID' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(echo "hex check" | git commit-tree "$tree") &&
	len=$(printf "%s" "$oid" | wc -c | tr -d " ") &&
	test "$len" = "40" &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_done
