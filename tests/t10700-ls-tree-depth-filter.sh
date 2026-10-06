#!/bin/sh
# Test ls-tree with recursive listing, depth, path filtering, and various
# output format options on nested tree structures.

test_description='grit ls-tree depth and path filtering'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with nested structure' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "root file" >root.txt &&
	mkdir -p a/b/c &&
	echo "a file" >a/file_a.txt &&
	echo "b file" >a/b/file_b.txt &&
	echo "c file" >a/b/c/file_c.txt &&
	mkdir -p x/y &&
	echo "x file" >x/file_x.txt &&
	echo "y file" >x/y/file_y.txt &&
	echo "#!/bin/sh" >a/exec.sh &&
	chmod +x a/exec.sh &&
	grit add . &&
	grit commit -m "nested structure"
	)
'

###########################################################################
# Section 2: Basic ls-tree
###########################################################################

test_expect_success 'ls-tree HEAD shows top-level entries' '
	(
	cd repo &&
	grit ls-tree HEAD >out &&
	grep "root.txt" out &&
	grep "a" out &&
	grep "x" out
	)
'

test_expect_success 'ls-tree HEAD top-level entry count' '
	(
	cd repo &&
	grit ls-tree HEAD >out &&
	test_line_count = 3 out
	)
'

test_expect_success 'ls-tree shows correct types at top level' '
	(
	cd repo &&
	grit ls-tree HEAD >out &&
	grep "blob.*root.txt" out &&
	grep "tree.*a$" out
	)
'

test_expect_success 'ls-tree HEAD matches git' '
	(
	cd repo &&
	grit ls-tree HEAD >grit_out &&
	git ls-tree HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 3: Recursive ls-tree
###########################################################################

test_expect_success 'ls-tree -r lists all files recursively' '
	(
	cd repo &&
	grit ls-tree -r HEAD >out &&
	grep "root.txt" out &&
	grep "a/file_a.txt" out &&
	grep "a/b/file_b.txt" out &&
	grep "a/b/c/file_c.txt" out &&
	grep "x/file_x.txt" out &&
	grep "x/y/file_y.txt" out
	)
'

test_expect_success 'ls-tree -r shows all 7 files' '
	(
	cd repo &&
	grit ls-tree -r HEAD >out &&
	test_line_count = 7 out
	)
'

test_expect_success 'ls-tree -r matches git' '
	(
	cd repo &&
	grit ls-tree -r HEAD >grit_out &&
	git ls-tree -r HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-tree -r only shows blobs (no trees)' '
	(
	cd repo &&
	grit ls-tree -r HEAD >out &&
	! grep "	a$" out &&
	! grep "	x$" out
	)
'

test_expect_success 'ls-tree -r shows executable mode' '
	(
	cd repo &&
	grit ls-tree -r HEAD >out &&
	grep "100755.*a/exec.sh" out
	)
'

###########################################################################
# Section 4: ls-tree -rt (recursive with trees)
###########################################################################

test_expect_success 'ls-tree -rt shows both trees and blobs' '
	(
	cd repo &&
	grit ls-tree -r -t HEAD >out &&
	grep "tree.*a$" out &&
	grep "blob.*root.txt" out
	)
'

test_expect_success 'ls-tree -rt matches git' '
	(
	cd repo &&
	grit ls-tree -r -t HEAD >grit_out &&
	git ls-tree -r -t HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-tree -rt includes intermediate trees' '
	(
	cd repo &&
	grit ls-tree -r -t HEAD >out &&
	grep "tree.*a/b$" out &&
	grep "tree.*a/b/c$" out
	)
'

###########################################################################
# Section 5: Path filtering
###########################################################################

test_expect_success 'ls-tree with path filter a shows tree entry' '
	(
	cd repo &&
	grit ls-tree HEAD -- a >out &&
	grep "tree.*a$" out &&
	test_line_count = 1 out
	)
'

test_expect_success 'ls-tree HEAD -- a matches git' '
	(
	cd repo &&
	grit ls-tree HEAD -- a >grit_out &&
	git ls-tree HEAD -- a >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-tree with path filter root.txt shows blob' '
	(
	cd repo &&
	grit ls-tree HEAD -- root.txt >out &&
	grep "blob.*root.txt" out &&
	test_line_count = 1 out
	)
'

test_expect_success 'ls-tree HEAD -- root.txt matches git' '
	(
	cd repo &&
	grit ls-tree HEAD -- root.txt >grit_out &&
	git ls-tree HEAD -- root.txt >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-tree -r with path filter -- a shows subtree files' '
	(
	cd repo &&
	grit ls-tree -r HEAD -- a >out &&
	grep "a/file_a.txt" out &&
	grep "a/exec.sh" out
	)
'

test_expect_success 'ls-tree -r HEAD -- a matches git' '
	(
	cd repo &&
	grit ls-tree -r HEAD -- a >grit_out &&
	git ls-tree -r HEAD -- a >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 6: Name-only output
###########################################################################

test_expect_success 'ls-tree --name-only shows just names' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD >out &&
	grep "^root.txt$" out &&
	! grep "blob" out &&
	! grep "100644" out
	)
'

test_expect_success 'ls-tree --name-only matches git' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD >grit_out &&
	git ls-tree --name-only HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-tree -r --name-only shows full paths' '
	(
	cd repo &&
	grit ls-tree -r --name-only HEAD >out &&
	grep "^a/b/c/file_c.txt$" out
	)
'

test_expect_success 'ls-tree -r --name-only matches git' '
	(
	cd repo &&
	grit ls-tree -r --name-only HEAD >grit_out &&
	git ls-tree -r --name-only HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 7: ls-tree -d (only trees)
###########################################################################

test_expect_success 'ls-tree -d shows only tree entries' '
	(
	cd repo &&
	grit ls-tree -d HEAD >out &&
	! grep "blob" out
	)
'

test_expect_success 'ls-tree -d matches git' '
	(
	cd repo &&
	grit ls-tree -d HEAD >grit_out &&
	git ls-tree -d HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 8: ls-tree with tree OID directly
###########################################################################

test_expect_success 'ls-tree works with tree OID' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree "$tree_oid" >out &&
	grep "root.txt" out
	)
'

test_expect_success 'ls-tree with tree OID matches HEAD' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree "$tree_oid" >tree_out &&
	grit ls-tree HEAD >head_out &&
	test_cmp head_out tree_out
	)
'

###########################################################################
# Section 9: Edge cases
###########################################################################

test_expect_success 'ls-tree on empty tree' '
	(
	cd repo &&
	empty_tree=$(printf "" | grit mktree) &&
	grit ls-tree "$empty_tree" >out &&
	test_must_be_empty out
	)
'

test_expect_success 'ls-tree -r on single-file tree' '
	(
	cd repo &&
	blob_oid=$(echo "single" | grit hash-object -w --stdin) &&
	tree_oid=$(printf "100644 blob %s\tonly.txt\n" "$blob_oid" | grit mktree) &&
	grit ls-tree -r "$tree_oid" >out &&
	test_line_count = 1 out &&
	grep "only.txt" out
	)
'

test_expect_success 'ls-tree nonexistent path gives empty output' '
	(
	cd repo &&
	grit ls-tree HEAD -- nosuchpath >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 10: Recursive with path filter
###########################################################################

test_expect_success 'ls-tree -r -- a shows all files under a/' '
	(
	cd repo &&
	grit ls-tree -r HEAD -- a >out &&
	grep "a/file_a.txt" out &&
	grep "a/b/file_b.txt" out &&
	grep "a/b/c/file_c.txt" out &&
	grep "a/exec.sh" out
	)
'

test_expect_success 'ls-tree -r -- a count is 4' '
	(
	cd repo &&
	grit ls-tree -r HEAD -- a >out &&
	test_line_count = 4 out
	)
'

test_expect_success 'ls-tree -r -- x matches git' '
	(
	cd repo &&
	grit ls-tree -r HEAD -- x >grit_out &&
	git ls-tree -r HEAD -- x >git_out &&
	test_cmp git_out grit_out
	)
'

test_done
