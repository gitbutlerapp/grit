#!/bin/sh
# Test cat-file output for commit and tree objects: formatting, fields,
# type/size reporting, and round-trip correctness.

test_description='grit cat-file commit and tree inspection'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with history' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test User" &&
	echo "file1" >a.txt &&
	mkdir -p sub &&
	echo "file2" >sub/b.txt &&
	grit add a.txt sub/b.txt &&
	grit commit -m "first commit" &&
	echo "modified" >a.txt &&
	echo "new" >c.txt &&
	grit add a.txt c.txt &&
	grit commit -m "second commit"
	)
'

###########################################################################
# Section 2: cat-file -t on commits and trees
###########################################################################

test_expect_success 'cat-file -t HEAD is commit' '
	(
	cd repo &&
	type=$(grit cat-file -t HEAD) &&
	test "$type" = "commit"
	)
'

test_expect_success 'cat-file -t on tree OID is tree' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	type=$(grit cat-file -t "$tree_oid") &&
	test "$type" = "tree"
	)
'

test_expect_success 'cat-file -t parent commit is commit' '
	(
	cd repo &&
	parent_oid=$(grit cat-file -p HEAD | grep "^parent " | awk "{print \$2}") &&
	type=$(grit cat-file -t "$parent_oid") &&
	test "$type" = "commit"
	)
'

test_expect_success 'cat-file -t on blob OID is blob' '
	(
	cd repo &&
	blob_oid=$(grit hash-object a.txt) &&
	grit hash-object -w a.txt >/dev/null &&
	type=$(grit cat-file -t "$blob_oid") &&
	test "$type" = "blob"
	)
'

###########################################################################
# Section 3: cat-file -s sizes
###########################################################################

test_expect_success 'cat-file -s on commit returns positive size' '
	(
	cd repo &&
	size=$(grit cat-file -s HEAD) &&
	test "$size" -gt 0
	)
'

test_expect_success 'cat-file -s on tree returns positive size' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	size=$(grit cat-file -s "$tree_oid") &&
	test "$size" -gt 0
	)
'

test_expect_success 'cat-file -s matches git for commit' '
	(
	cd repo &&
	grit_size=$(grit cat-file -s HEAD) &&
	git_size=$(git cat-file -s HEAD) &&
	test "$grit_size" = "$git_size"
	)
'

test_expect_success 'cat-file -s matches git for tree' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	grit_size=$(grit cat-file -s "$tree_oid") &&
	git_size=$(git cat-file -s "$tree_oid") &&
	test "$grit_size" = "$git_size"
	)
'

###########################################################################
# Section 4: cat-file -p commit content
###########################################################################

test_expect_success 'cat-file -p HEAD shows tree line' '
	(
	cd repo &&
	grit cat-file -p HEAD >out &&
	grep "^tree [0-9a-f]\{40\}" out
	)
'

test_expect_success 'cat-file -p HEAD shows author line' '
	(
	cd repo &&
	grit cat-file -p HEAD >out &&
	grep "^author " out
	)
'

test_expect_success 'cat-file -p HEAD shows committer line' '
	(
	cd repo &&
	grit cat-file -p HEAD >out &&
	grep "^committer " out
	)
'

test_expect_success 'cat-file -p HEAD shows commit message' '
	(
	cd repo &&
	grit cat-file -p HEAD >out &&
	grep "second commit" out
	)
'

test_expect_success 'cat-file -p parent shows first commit message' '
	(
	cd repo &&
	parent_oid=$(grit cat-file -p HEAD | grep "^parent " | awk "{print \$2}") &&
	grit cat-file -p "$parent_oid" >out &&
	grep "first commit" out
	)
'

test_expect_success 'second commit has parent line' '
	(
	cd repo &&
	grit cat-file -p HEAD >out &&
	grep "^parent [0-9a-f]\{40\}" out
	)
'

test_expect_success 'first commit has no parent line' '
	(
	cd repo &&
	parent_oid=$(grit cat-file -p HEAD | grep "^parent " | awk "{print \$2}") &&
	grit cat-file -p "$parent_oid" >out &&
	! grep "^parent " out
	)
'

test_expect_success 'parent OID from cat-file matches log' '
	(
	cd repo &&
	parent=$(grit cat-file -p HEAD | grep "^parent " | awk "{print \$2}") &&
	test -n "$parent" &&
	grit cat-file -e "$parent"
	)
'

test_expect_success 'tree in commit matches HEAD^{tree}' '
	(
	cd repo &&
	tree_from_commit=$(grit cat-file -p HEAD | grep "^tree " | awk "{print \$2}") &&
	tree_direct=$(grit rev-parse HEAD^{tree}) &&
	test "$tree_from_commit" = "$tree_direct"
	)
'

test_expect_success 'cat-file -p commit output matches git' '
	(
	cd repo &&
	grit cat-file -p HEAD >grit_out &&
	git cat-file -p HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'cat-file -p first commit matches git' '
	(
	cd repo &&
	parent_oid=$(grit cat-file -p HEAD | grep "^parent " | awk "{print \$2}") &&
	grit cat-file -p "$parent_oid" >grit_out &&
	git cat-file -p "$parent_oid" >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 5: cat-file -p tree content
###########################################################################

test_expect_success 'cat-file -p tree lists entries' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	grit cat-file -p "$tree_oid" >out &&
	test_line_count -ge 2 out
	)
'

test_expect_success 'tree listing shows a.txt' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	grit cat-file -p "$tree_oid" >out &&
	grep "a.txt" out
	)
'

test_expect_success 'tree listing shows sub directory' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	grit cat-file -p "$tree_oid" >out &&
	grep "sub" out
	)
'

test_expect_success 'tree entry for a.txt is blob mode 100644' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	grit cat-file -p "$tree_oid" >out &&
	grep "100644 blob" out | grep "a.txt"
	)
'

test_expect_success 'tree entry for sub is tree mode 040000' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	grit cat-file -p "$tree_oid" >out &&
	grep "040000 tree" out | grep "sub"
	)
'

test_expect_success 'cat-file -p tree matches git' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	grit cat-file -p "$tree_oid" >grit_out &&
	git cat-file -p "$tree_oid" >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'subtree has blob entry for b.txt' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	sub_oid=$(grit cat-file -p "$tree_oid" | grep "sub" | awk "{print \$3}") &&
	grit cat-file -p "$sub_oid" >out &&
	grep "b.txt" out
	)
'

###########################################################################
# Section 6: cat-file -e existence checks
###########################################################################

test_expect_success 'cat-file -e succeeds for HEAD commit' '
	(
	cd repo &&
	grit cat-file -e HEAD
	)
'

test_expect_success 'cat-file -e succeeds for tree' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	grit cat-file -e "$tree_oid"
	)
'

test_expect_success 'cat-file -e fails for nonexistent OID' '
	(
	cd repo &&
	test_must_fail grit cat-file -e 0000000000000000000000000000000000000099
	)
'

###########################################################################
# Section 7: Round-trip blob through commit
###########################################################################

test_expect_success 'blob in tree matches file content' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	blob_oid=$(grit cat-file -p "$tree_oid" | grep "a.txt" | awk "{print \$3}") &&
	grit cat-file -p "$blob_oid" >actual &&
	test_cmp a.txt actual
	)
'

test_expect_success 'blob OID in tree matches hash-object' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	blob_oid=$(grit cat-file -p "$tree_oid" | grep "a.txt" | awk "{print \$3}") &&
	expected=$(grit hash-object a.txt) &&
	test "$blob_oid" = "$expected"
	)
'

test_expect_success 'c.txt appears in HEAD tree' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	grit cat-file -p "$tree_oid" >out &&
	grep "c.txt" out
	)
'

test_expect_success 'c.txt blob content round-trips correctly' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	blob_oid=$(grit cat-file -p "$tree_oid" | grep "c.txt" | awk "{print \$3}") &&
	grit cat-file -p "$blob_oid" >actual &&
	test_cmp c.txt actual
	)
'

test_done
