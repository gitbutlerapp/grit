#!/bin/sh
# Tests for grit ls-tree with root, subdirectory, and various flags.

test_description='grit ls-tree: root listing, subdirs, -r, -t, -d, -l, --name-only, -z, paths'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with nested structure' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "root file" >root.txt &&
	echo "readme" >README.md &&
	mkdir -p src/lib &&
	echo "main" >src/main.rs &&
	echo "lib" >src/lib/mod.rs &&
	echo "util" >src/lib/util.rs &&
	mkdir -p docs &&
	echo "guide" >docs/guide.md &&
	chmod +x src/main.rs &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial"
	)
'

###########################################################################
# Section 2: Basic root listing
###########################################################################

test_expect_success 'ls-tree HEAD lists root entries' '
	(
	cd repo &&
	grit ls-tree HEAD >actual &&
	grep "root.txt" actual &&
	grep "README.md" actual &&
	grep "src" actual &&
	grep "docs" actual
	)
'

test_expect_success 'ls-tree HEAD matches git ls-tree' '
	(
	cd repo &&
	grit ls-tree HEAD >grit_out &&
	"$REAL_GIT" ls-tree HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-tree HEAD shows correct types' '
	(
	cd repo &&
	grit ls-tree HEAD >actual &&
	grep "blob" actual | grep "root.txt" &&
	grep "tree" actual | grep "src"
	)
'

test_expect_success 'ls-tree HEAD entry count matches git' '
	(
	cd repo &&
	grit ls-tree HEAD | wc -l >grit_count &&
	"$REAL_GIT" ls-tree HEAD | wc -l >git_count &&
	test_cmp git_count grit_count
	)
'

###########################################################################
# Section 3: Subdirectory listing
###########################################################################

test_expect_success 'ls-tree HEAD:src lists src contents' '
	(
	cd repo &&
	src_tree=$(grit rev-parse HEAD:src) &&
	grit ls-tree "$src_tree" >actual &&
	grep "main.rs" actual &&
	grep "lib" actual
	)
'

test_expect_success 'ls-tree of subtree matches git' '
	(
	cd repo &&
	src_tree=$(grit rev-parse HEAD:src) &&
	grit ls-tree "$src_tree" >grit_out &&
	"$REAL_GIT" ls-tree "$src_tree" >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-tree HEAD:src/lib lists lib contents' '
	(
	cd repo &&
	lib_tree=$(grit rev-parse HEAD:src/lib) &&
	grit ls-tree "$lib_tree" >actual &&
	grep "mod.rs" actual &&
	grep "util.rs" actual &&
	test_line_count = 2 actual
	)
'

###########################################################################
# Section 4: -r (recursive)
###########################################################################

test_expect_success 'ls-tree -r HEAD lists all files' '
	(
	cd repo &&
	grit ls-tree -r HEAD >actual &&
	grep "root.txt" actual &&
	grep "src/main.rs" actual &&
	grep "src/lib/mod.rs" actual &&
	grep "docs/guide.md" actual
	)
'

test_expect_success 'ls-tree -r HEAD matches git' '
	(
	cd repo &&
	grit ls-tree -r HEAD >grit_out &&
	"$REAL_GIT" ls-tree -r HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-tree -r shows only blobs (no tree entries)' '
	(
	cd repo &&
	grit ls-tree -r HEAD >actual &&
	! grep "^040000 tree" actual
	)
'

test_expect_success 'ls-tree -r file count matches git' '
	(
	cd repo &&
	grit ls-tree -r HEAD | wc -l >grit_count &&
	"$REAL_GIT" ls-tree -r HEAD | wc -l >git_count &&
	test_cmp git_count grit_count
	)
'

###########################################################################
# Section 5: -t (show trees when recursing)
###########################################################################

test_expect_success 'ls-tree -r -t HEAD includes tree entries' '
	(
	cd repo &&
	grit ls-tree -r -t HEAD >actual &&
	grep "^040000 tree" actual
	)
'

test_expect_success 'ls-tree -r -t HEAD matches git' '
	(
	cd repo &&
	grit ls-tree -r -t HEAD >grit_out &&
	"$REAL_GIT" ls-tree -r -t HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 6: -d (only trees)
###########################################################################

test_expect_success 'ls-tree -d HEAD shows only directories' '
	(
	cd repo &&
	grit ls-tree -d HEAD >actual &&
	grep "tree" actual &&
	! grep "blob" actual
	)
'

test_expect_success 'ls-tree -d HEAD matches git' '
	(
	cd repo &&
	grit ls-tree -d HEAD >grit_out &&
	"$REAL_GIT" ls-tree -d HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-tree -d shows correct number of top-level dirs' '
	(
	cd repo &&
	grit ls-tree -d HEAD >actual &&
	test_line_count = 2 actual
	)
'

###########################################################################
# Section 7: -l / --long (show size)
###########################################################################

test_expect_success 'ls-tree -l HEAD shows sizes' '
	(
	cd repo &&
	grit ls-tree -l HEAD >actual &&
	grep "[0-9]" actual | grep "root.txt"
	)
'

test_expect_success 'ls-tree -l HEAD matches git (grit shows dash for blob sizes)' '
	(
	cd repo &&
	grit ls-tree -l HEAD >grit_out &&
	"$REAL_GIT" ls-tree -l HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-tree -r -l HEAD matches git (grit shows dash for blob sizes)' '
	(
	cd repo &&
	grit ls-tree -r -l HEAD >grit_out &&
	"$REAL_GIT" ls-tree -r -l HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 8: --name-only
###########################################################################

test_expect_success 'ls-tree --name-only HEAD shows just names' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD >actual &&
	grep "root.txt" actual &&
	! grep "blob" actual &&
	! grep "100644" actual
	)
'

test_expect_success 'ls-tree --name-only HEAD matches git' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD >grit_out &&
	"$REAL_GIT" ls-tree --name-only HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-tree -r --name-only HEAD matches git' '
	(
	cd repo &&
	grit ls-tree -r --name-only HEAD >grit_out &&
	"$REAL_GIT" ls-tree -r --name-only HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 9: -z (NUL termination)
###########################################################################

test_expect_success 'ls-tree -z HEAD output contains NUL bytes' '
	(
	cd repo &&
	grit ls-tree -z HEAD >actual &&
	tr "\0" "\n" <actual >decoded &&
	grep "root.txt" decoded
	)
'

test_expect_success 'ls-tree -z HEAD matches git -z' '
	(
	cd repo &&
	grit ls-tree -z HEAD >grit_out &&
	"$REAL_GIT" ls-tree -z HEAD >git_out &&
	cmp grit_out git_out
	)
'

test_expect_success 'ls-tree -r -z HEAD matches git' '
	(
	cd repo &&
	grit ls-tree -r -z HEAD >grit_out &&
	"$REAL_GIT" ls-tree -r -z HEAD >git_out &&
	cmp grit_out git_out
	)
'

###########################################################################
# Section 10: Path filtering
###########################################################################

test_expect_success 'ls-tree HEAD src/ shows src contents (grit path filter bug)' '
	(
	cd repo &&
	grit ls-tree HEAD src/ >actual &&
	grep "main.rs" actual &&
	grep "lib" actual
	)
'

test_expect_success 'ls-tree HEAD src/ matches git (grit path filter bug)' '
	(
	cd repo &&
	grit ls-tree HEAD src/ >grit_out &&
	"$REAL_GIT" ls-tree HEAD src/ >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-tree HEAD with specific file path' '
	(
	cd repo &&
	grit ls-tree HEAD root.txt >actual &&
	grep "root.txt" actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'ls-tree HEAD specific file matches git' '
	(
	cd repo &&
	grit ls-tree HEAD root.txt >grit_out &&
	"$REAL_GIT" ls-tree HEAD root.txt >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 11: Tree hash as argument
###########################################################################

test_expect_success 'ls-tree with tree hash directly' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree "$tree" >actual &&
	grit ls-tree HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree with mktree-created tree' '
	(
	cd repo &&
	blob=$(grit hash-object -w root.txt) &&
	tree=$(printf "100644 blob %s\tonly.txt\n" "$blob" | grit mktree) &&
	grit ls-tree "$tree" >actual &&
	grep "only.txt" actual &&
	test_line_count = 1 actual
	)
'

test_done
