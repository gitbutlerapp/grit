#!/bin/sh
# Tests for cat-file with -t (type), -s (size), -e (existence check),
# and -p (pretty-print) across all object types.

test_description='cat-file type/size filtering and existence check'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

SYS_GIT=$(command -v git)

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup: repository with various objects' '
	(
	"$SYS_GIT" init repo &&
	cd repo &&
	"$SYS_GIT" config user.name "Test User" &&
	"$SYS_GIT" config user.email "test@example.com" &&
	echo "hello world" >file.txt &&
	echo "second" >file2.txt &&
	mkdir -p sub &&
	echo "nested" >sub/nested.txt &&
	"$SYS_GIT" add . &&
	"$SYS_GIT" commit -m "initial commit" &&
	"$SYS_GIT" tag -a v1.0 -m "version 1.0" &&
	echo "updated" >file.txt &&
	"$SYS_GIT" add file.txt &&
	"$SYS_GIT" commit -m "update file"
	)
'

# ── -t (type) for blob ──────────────────────────────────────────────────

test_expect_success 'cat-file -t: blob type' '
	(
	cd repo &&
	blob_oid=$("$SYS_GIT" rev-parse HEAD:file.txt) &&
	type=$(git cat-file -t "$blob_oid") &&
	test "$type" = "blob"
	)
'

test_expect_success 'cat-file -t: second blob type' '
	(
	cd repo &&
	blob_oid=$("$SYS_GIT" rev-parse HEAD:file2.txt) &&
	type=$(git cat-file -t "$blob_oid") &&
	test "$type" = "blob"
	)
'

# ── -t (type) for tree ──────────────────────────────────────────────────

test_expect_success 'cat-file -t: tree type' '
	(
	cd repo &&
	tree_oid=$("$SYS_GIT" rev-parse HEAD^{tree}) &&
	type=$(git cat-file -t "$tree_oid") &&
	test "$type" = "tree"
	)
'

# ── -t (type) for commit ────────────────────────────────────────────────

test_expect_success 'cat-file -t: commit type' '
	(
	cd repo &&
	commit_oid=$("$SYS_GIT" rev-parse HEAD) &&
	type=$(git cat-file -t "$commit_oid") &&
	test "$type" = "commit"
	)
'

# ── -t (type) for tag ───────────────────────────────────────────────────

test_expect_success 'cat-file -t: annotated tag type' '
	(
	cd repo &&
	tag_oid=$("$SYS_GIT" rev-parse v1.0) &&
	type=$(git cat-file -t "$tag_oid") &&
	test "$type" = "tag"
	)
'

# ── -s (size) for blob ──────────────────────────────────────────────────

test_expect_success 'cat-file -s: blob size matches content length' '
	(
	cd repo &&
	blob_oid=$("$SYS_GIT" rev-parse HEAD:file.txt) &&
	size=$(git cat-file -s "$blob_oid") &&
	test "$size" = "8"
	)
'

test_expect_success 'cat-file -s: empty blob has size 0' '
	(
	cd repo &&
	empty_oid=$(git hash-object -w --stdin </dev/null) &&
	size=$(git cat-file -s "$empty_oid") &&
	test "$size" = "0"
	)
'

test_expect_success 'cat-file -s: larger blob has correct size' '
	(
	cd repo &&
	blob_oid=$("$SYS_GIT" rev-parse HEAD:file2.txt) &&
	size=$(git cat-file -s "$blob_oid") &&
	test "$size" -gt 0
	)
'

# ── -s (size) for tree ──────────────────────────────────────────────────

test_expect_success 'cat-file -s: tree size is positive' '
	(
	cd repo &&
	tree_oid=$("$SYS_GIT" rev-parse HEAD^{tree}) &&
	size=$(git cat-file -s "$tree_oid") &&
	test "$size" -gt 0
	)
'

# ── -s (size) for commit ────────────────────────────────────────────────

test_expect_success 'cat-file -s: commit size is positive' '
	(
	cd repo &&
	commit_oid=$("$SYS_GIT" rev-parse HEAD) &&
	size=$(git cat-file -s "$commit_oid") &&
	test "$size" -gt 0
	)
'

# ── -s (size) for tag ───────────────────────────────────────────────────

test_expect_success 'cat-file -s: tag size is positive' '
	(
	cd repo &&
	tag_oid=$("$SYS_GIT" rev-parse v1.0) &&
	size=$(git cat-file -s "$tag_oid") &&
	test "$size" -gt 0
	)
'

# ── -e (existence check) ────────────────────────────────────────────────

test_expect_success 'cat-file -e: existing blob returns 0' '
	(
	cd repo &&
	blob_oid=$("$SYS_GIT" rev-parse HEAD:file.txt) &&
	git cat-file -e "$blob_oid"
	)
'

test_expect_success 'cat-file -e: existing commit returns 0' '
	(
	cd repo &&
	commit_oid=$("$SYS_GIT" rev-parse HEAD) &&
	git cat-file -e "$commit_oid"
	)
'

test_expect_success 'cat-file -e: existing tree returns 0' '
	(
	cd repo &&
	tree_oid=$("$SYS_GIT" rev-parse HEAD^{tree}) &&
	git cat-file -e "$tree_oid"
	)
'

test_expect_success 'cat-file -e: existing tag returns 0' '
	(
	cd repo &&
	tag_oid=$("$SYS_GIT" rev-parse v1.0) &&
	git cat-file -e "$tag_oid"
	)
'

test_expect_success 'cat-file -e: nonexistent object returns non-zero' '
	(
	cd repo &&
	test_must_fail git cat-file -e 0000000000000000000000000000000000000000
	)
'

test_expect_success 'cat-file -e: bogus hex returns non-zero' '
	(
	cd repo &&
	test_must_fail git cat-file -e aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
	)
'

test_expect_success 'cat-file -e: produces no stdout on success' '
	(
	cd repo &&
	blob_oid=$("$SYS_GIT" rev-parse HEAD:file.txt) &&
	git cat-file -e "$blob_oid" >../ce_out &&
	test_must_be_empty ../ce_out
	)
'

# ── -p (pretty-print) ───────────────────────────────────────────────────

test_expect_success 'cat-file -p: blob prints content' '
	(
	cd repo &&
	blob_oid=$("$SYS_GIT" rev-parse HEAD:file.txt) &&
	content=$(git cat-file -p "$blob_oid") &&
	test "$content" = "updated"
	)
'

test_expect_success 'cat-file -p: commit shows tree and parent' '
	(
	cd repo &&
	commit_oid=$("$SYS_GIT" rev-parse HEAD) &&
	git cat-file -p "$commit_oid" >../pp_out &&
	grep "^tree" ../pp_out &&
	grep "^parent" ../pp_out &&
	grep "^author" ../pp_out
	)
'

test_expect_success 'cat-file -p: root commit has no parent line' '
	(
	cd repo &&
	root_oid=$("$SYS_GIT" rev-parse HEAD~1) &&
	git cat-file -p "$root_oid" >../pp_out &&
	grep "^tree" ../pp_out &&
	! grep "^parent" ../pp_out
	)
'

test_expect_success 'cat-file -p: tree shows entries' '
	(
	cd repo &&
	tree_oid=$("$SYS_GIT" rev-parse HEAD^{tree}) &&
	git cat-file -p "$tree_oid" >../pp_out &&
	grep "file.txt" ../pp_out &&
	grep "file2.txt" ../pp_out &&
	grep "sub" ../pp_out
	)
'

test_expect_success 'cat-file -p: tag shows object and tagger' '
	(
	cd repo &&
	tag_oid=$("$SYS_GIT" rev-parse v1.0) &&
	git cat-file -p "$tag_oid" >../pp_out &&
	grep "^object" ../pp_out &&
	grep "^type commit" ../pp_out &&
	grep "^tag v1.0" ../pp_out &&
	grep "^tagger" ../pp_out
	)
'

# ── -t with ref names ───────────────────────────────────────────────────

test_expect_success 'cat-file -t HEAD: resolves ref to commit' '
	(
	cd repo &&
	type=$(git cat-file -t HEAD) &&
	test "$type" = "commit"
	)
'

test_expect_success 'cat-file -t v1.0: resolves tag ref' '
	(
	cd repo &&
	type=$(git cat-file -t v1.0) &&
	test "$type" = "tag"
	)
'

# ── -s with ref names ───────────────────────────────────────────────────

test_expect_success 'cat-file -s HEAD: size of commit via ref' '
	(
	cd repo &&
	size=$(git cat-file -s HEAD) &&
	test "$size" -gt 0
	)
'

# ── -e with ref names ───────────────────────────────────────────────────

test_expect_success 'cat-file -e HEAD: ref resolves and exists' '
	(
	cd repo &&
	git cat-file -e HEAD
	)
'

# ── Previous commit blob ────────────────────────────────────────────────

test_expect_success 'cat-file -p: blob from first commit via rev-parse' '
	(
	cd repo &&
	first=$($SYS_GIT rev-parse HEAD~1) &&
	old_tree=$(git cat-file -p "$first" | grep "^tree" | awk "{print \$2}") &&
	old_blob=$(git cat-file -p "$old_tree" | grep "file.txt" | awk "{print \$3}") &&
	content=$(git cat-file -p "$old_blob") &&
	test "$content" = "hello world"
	)
'

test_expect_success 'cat-file -s: different blobs have different sizes' '
	(
	cd repo &&
	first=$($SYS_GIT rev-parse HEAD~1) &&
	old_tree=$(git cat-file -p "$first" | grep "^tree" | awk "{print \$2}") &&
	old_blob=$(git cat-file -p "$old_tree" | grep "file.txt" | awk "{print \$3}") &&
	new_blob=$($SYS_GIT rev-parse HEAD:file.txt) &&
	old_size=$(git cat-file -s "$old_blob") &&
	new_size=$(git cat-file -s "$new_blob") &&
	test "$old_size" != "$new_size"
	)
'

# ── Error cases ──────────────────────────────────────────────────────────

test_expect_success 'cat-file -t: nonexistent object fails' '
	(
	cd repo &&
	test_must_fail git cat-file -t 0000000000000000000000000000000000000000
	)
'

test_expect_success 'cat-file -s: nonexistent object fails' '
	(
	cd repo &&
	test_must_fail git cat-file -s 0000000000000000000000000000000000000000
	)
'

test_expect_success 'cat-file -p: nonexistent object fails' '
	(
	cd repo &&
	test_must_fail git cat-file -p 0000000000000000000000000000000000000000
	)
'

test_done
