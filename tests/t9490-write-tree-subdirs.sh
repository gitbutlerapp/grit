#!/bin/sh
# Tests for grit write-tree with subdirectories, nested trees, and
# various index states.

test_description='grit write-tree with subdirectories'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git
EMPTY_TREE=4b825dc642cb6eb9a060e54bf8d69288fbee4904

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with nested directories' '
	(
	grit init repo &&
	cd repo &&
	echo "root" >root.txt &&
	mkdir -p src/lib src/bin docs/api &&
	echo "main" >src/bin/main.rs &&
	echo "lib" >src/lib/lib.rs &&
	echo "util" >src/lib/util.rs &&
	echo "readme" >docs/README.md &&
	echo "api" >docs/api/index.html &&
	grit add .
	)
'

###########################################################################
# Section 2: Basic write-tree with subdirectories
###########################################################################

test_expect_success 'write-tree with subdirectories produces valid tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	test -n "$tree" &&
	grit cat-file -t "$tree" >actual &&
	echo tree >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'write-tree OID is 40 hex chars' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	echo "$tree" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'write-tree is deterministic' '
	(
	cd repo &&
	tree1=$(grit write-tree) &&
	tree2=$(grit write-tree) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'write-tree top-level shows root.txt' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "root.txt" actual
	)
'

test_expect_success 'write-tree top-level shows src dir' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "^040000 tree.*src$" actual
	)
'

test_expect_success 'write-tree top-level shows docs dir' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "^040000 tree.*docs$" actual
	)
'

test_expect_success 'write-tree recursive lists all files' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >actual &&
	grep "root.txt" actual &&
	grep "src/bin/main.rs" actual &&
	grep "src/lib/lib.rs" actual &&
	grep "src/lib/util.rs" actual &&
	grep "docs/README.md" actual &&
	grep "docs/api/index.html" actual
	)
'

test_expect_success 'write-tree recursive has 6 blob entries' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >actual &&
	test $(wc -l <actual) -eq 6
	)
'

test_expect_success 'write-tree blob modes are 100644' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >actual &&
	while read mode type oid name; do
		test "$mode" = "100644" || return 1
	done <actual
	)
'

###########################################################################
# Section 3: Subdirectory tree structure
###########################################################################

test_expect_success 'write-tree src subtree has bin and lib' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >top &&
	src_oid=$(grep "src$" top | cut -f1 | awk "{print \$3}") &&
	grit ls-tree "$src_oid" >actual &&
	grep "bin" actual &&
	grep "lib" actual
	)
'

test_expect_success 'write-tree src/lib subtree has lib.rs and util.rs' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >top_entries &&
	src_oid=$(awk "/\tsrc$/ {print \$3}" top_entries) &&
	grit ls-tree "$src_oid" >src_entries &&
	lib_oid=$(awk "/\tlib$/ {print \$3}" src_entries) &&
	grit ls-tree "$lib_oid" >actual &&
	grep "lib.rs" actual &&
	grep "util.rs" actual
	)
'

test_expect_success 'write-tree docs subtree has README.md and api' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >top &&
	docs_oid=$(grep "docs$" top | cut -f1 | awk "{print \$3}") &&
	grit ls-tree "$docs_oid" >actual &&
	grep "README.md" actual &&
	grep "api" actual
	)
'

test_expect_success 'write-tree docs/api subtree has index.html' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >top &&
	docs_oid=$(grep "docs$" top | cut -f1 | awk "{print \$3}") &&
	grit ls-tree "$docs_oid" >docs_ls &&
	api_oid=$(grep "api$" docs_ls | cut -f1 | awk "{print \$3}") &&
	grit ls-tree "$api_oid" >actual &&
	grep "index.html" actual &&
	test $(wc -l <actual) -eq 1
	)
'

###########################################################################
# Section 4: write-tree after index modifications
###########################################################################

test_expect_success 'write-tree changes after adding new file' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	echo "new" >new.txt &&
	grit add new.txt &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after"
	)
'

test_expect_success 'write-tree new file appears in ls-tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "new.txt" actual
	)
'

test_expect_success 'write-tree changes after modifying file' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	echo "modified" >root.txt &&
	grit add root.txt &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after"
	)
'

test_expect_success 'write-tree changes after removing file' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	grit rm --cached new.txt &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after"
	)
'

test_expect_success 'write-tree adding file in subdirectory changes tree' '
	(
	cd repo &&
	tree_before=$(grit write-tree) &&
	echo "extra" >src/extra.rs &&
	grit add src/extra.rs &&
	tree_after=$(grit write-tree) &&
	test "$tree_before" != "$tree_after"
	)
'

test_expect_success 'write-tree new subdir file appears recursively' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >actual &&
	grep "src/extra.rs" actual
	)
'

###########################################################################
# Section 5: write-tree matches real git
###########################################################################

test_expect_success 'setup fresh repo for cross-check' '
	(
	$REAL_GIT init cross-repo &&
	cd cross-repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	mkdir -p a/b &&
	echo "one" >one.txt &&
	echo "deep" >a/b/deep.txt &&
	$REAL_GIT add .
	)
'

test_expect_success 'write-tree OID matches real git' '
	(
	cd cross-repo &&
	tree_grit=$(grit write-tree) &&
	tree_git=$($REAL_GIT write-tree) &&
	test "$tree_grit" = "$tree_git"
	)
'

test_expect_success 'write-tree ls-tree -r matches real git' '
	(
	cd cross-repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >grit_out &&
	$REAL_GIT ls-tree -r "$tree" >git_out &&
	test_cmp grit_out git_out
	)
'

###########################################################################
# Section 6: Edge cases
###########################################################################

test_expect_success 'write-tree on empty index gives empty tree' '
	(
	grit init empty-repo &&
	cd empty-repo &&
	tree=$(grit write-tree) &&
	test "$tree" = "$EMPTY_TREE"
	)
'

test_expect_success 'write-tree with deeply nested single file' '
	(
	grit init deep-repo &&
	cd deep-repo &&
	mkdir -p a/b/c/d/e &&
	echo "deep" >a/b/c/d/e/leaf.txt &&
	grit add . &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >actual &&
	grep "a/b/c/d/e/leaf.txt" actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'write-tree tree content is retrievable via cat-file' '
	(
	cd deep-repo &&
	tree=$(grit write-tree) &&
	grit cat-file -p "$tree" >actual &&
	grep "^040000 tree" actual
	)
'

test_expect_success 'write-tree with only directories (no root files)' '
	(
	grit init dirs-only &&
	cd dirs-only &&
	mkdir -p x/y &&
	echo "leaf" >x/y/leaf.txt &&
	grit add . &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	test $(wc -l <actual) -eq 1 &&
	grep "^040000 tree" actual
	)
'

test_expect_success 'write-tree with many files in one directory' '
	(
	grit init many-files &&
	cd many-files &&
	for i in $(seq 1 20); do
		echo "file $i" >"file_$i.txt"
	done &&
	grit add . &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	test $(wc -l <actual) -eq 20
	)
'

test_done
