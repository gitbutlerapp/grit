#!/bin/sh
# Tests for grit ls-tree with path filters, -d, -t, -l, --name-only,
# --format, and -z options.

test_description='grit ls-tree path filtering and output options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with varied structure' '
	(
	grit init repo &&
	cd repo &&
	echo "root" >root.txt &&
	echo "readme" >README.md &&
	mkdir -p src/core src/util docs images &&
	echo "main" >src/main.rs &&
	echo "core" >src/core/engine.rs &&
	echo "helper" >src/core/helper.rs &&
	echo "util" >src/util/parse.rs &&
	echo "guide" >docs/guide.md &&
	echo "changelog" >docs/CHANGELOG.md &&
	printf "\x89PNG" >images/logo.png &&
	grit add . &&
	tree=$(grit write-tree) &&
	echo "$tree" >../tree_oid
	)
'

###########################################################################
# Section 2: Path filtering
###########################################################################

test_expect_success 'ls-tree with path filter shows only matching file' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree "$tree" root.txt >actual &&
	grep "root.txt" actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'ls-tree with path filter for directory' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree "$tree" src >actual &&
	grep "src" actual
	)
'

test_expect_success 'ls-tree path filter non-existent path returns empty' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree "$tree" nonexistent >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'ls-tree with multiple path filters' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree "$tree" root.txt README.md >actual &&
	grep "root.txt" actual &&
	grep "README.md" actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'ls-tree -r with path filter for top-level dir' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r "$tree" src >actual &&
	grep "src/core/engine.rs" actual &&
	grep "src/core/helper.rs" actual &&
	grep "src/main.rs" actual &&
	! grep "docs" actual
	)
'

test_expect_success 'ls-tree -r path filter has correct count for src' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r "$tree" src >actual &&
	test $(wc -l <actual) -eq 4
	)
'

test_expect_success 'ls-tree -r with multiple path filters' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r "$tree" src docs >actual &&
	grep "src/core/engine.rs" actual &&
	grep "docs/guide.md" actual &&
	! grep "root.txt" actual
	)
'

###########################################################################
# Section 3: -d (show only trees)
###########################################################################

test_expect_success 'ls-tree -d shows only tree entries' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -d "$tree" >actual &&
	grep "src" actual &&
	grep "docs" actual &&
	grep "images" actual &&
	! grep "root.txt" actual &&
	! grep "README.md" actual
	)
'

test_expect_success 'ls-tree -d entries all have tree type' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -d "$tree" >actual &&
	while read mode type oid name; do
		test "$type" = "tree" || return 1
	done <actual
	)
'

###########################################################################
# Section 4: -t (show trees when recursing)
###########################################################################

test_expect_success 'ls-tree -r -t shows trees alongside blobs' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r -t "$tree" >actual &&
	grep "^040000 tree" actual &&
	grep "^100644 blob" actual
	)
'

test_expect_success 'ls-tree -r -t includes more lines than -r alone' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r "$tree" >r_only &&
	grit ls-tree -r -t "$tree" >r_t &&
	test $(wc -l <r_t) -gt $(wc -l <r_only)
	)
'

###########################################################################
# Section 5: -l / --long (show sizes)
###########################################################################

test_expect_success 'ls-tree -l shows object sizes' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -l "$tree" >actual &&
	grep "root.txt" actual
	)
'

test_expect_success 'ls-tree --long is same as -l' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -l "$tree" >l_out &&
	grit ls-tree --long "$tree" >long_out &&
	test_cmp l_out long_out
	)
'

test_expect_success 'ls-tree -r -l shows sizes for all recursive entries' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r -l "$tree" >actual &&
	grep "engine.rs" actual
	)
'

###########################################################################
# Section 6: --name-only
###########################################################################

test_expect_success 'ls-tree --name-only shows just filenames' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree --name-only "$tree" >actual &&
	grep "root.txt" actual &&
	! grep "100644" actual &&
	! grep "blob" actual
	)
'

test_expect_success 'ls-tree -r --name-only shows full paths' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r --name-only "$tree" >actual &&
	grep "src/core/engine.rs" actual &&
	grep "docs/guide.md" actual
	)
'

test_expect_success 'ls-tree --name-only entry count matches default' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree "$tree" >full &&
	grit ls-tree --name-only "$tree" >names &&
	test $(wc -l <full) -eq $(wc -l <names)
	)
'

###########################################################################
# Section 7: -z (NUL-terminated output)
###########################################################################

test_expect_success 'ls-tree -z uses NUL terminators' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -z "$tree" >actual &&
	tr "\0" "\n" <actual >decoded &&
	grep "root.txt" decoded
	)
'

test_expect_success 'ls-tree -r -z has NUL after each entry' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree -r -z "$tree" | tr "\0" "\n" >decoded &&
	grep "src/core/engine.rs" decoded
	)
'

###########################################################################
# Section 8: --format
###########################################################################

test_expect_success 'ls-tree --format=%(objectname) shows only OIDs' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree "--format=%(objectname)" "$tree" >actual &&
	while read line; do
		echo "$line" | grep -qE "^[0-9a-f]{40}$" || return 1
	done <actual
	)
'

test_expect_success 'ls-tree --format=%(objecttype) shows types' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree "--format=%(objecttype)" "$tree" >actual &&
	grep "blob" actual &&
	grep "tree" actual
	)
'

test_expect_success 'ls-tree --format=%(path) shows paths' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit ls-tree "--format=%(path)" "$tree" >actual &&
	grep "root.txt" actual &&
	grep "src" actual
	)
'

###########################################################################
# Section 9: Cross-check with real git
###########################################################################

test_expect_success 'ls-tree output matches real git for simple tree' '
	(
	$REAL_GIT init cross-repo &&
	cd cross-repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "a" >a.txt &&
	mkdir sub &&
	echo "b" >sub/b.txt &&
	$REAL_GIT add . &&
	tree=$($REAL_GIT write-tree) &&
	grit ls-tree "$tree" >grit_out &&
	$REAL_GIT ls-tree "$tree" >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'ls-tree -r output matches real git' '
	(
	cd cross-repo &&
	tree=$($REAL_GIT write-tree) &&
	grit ls-tree -r "$tree" >grit_out &&
	$REAL_GIT ls-tree -r "$tree" >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'ls-tree --name-only output matches real git' '
	(
	cd cross-repo &&
	tree=$($REAL_GIT write-tree) &&
	grit ls-tree --name-only "$tree" >grit_out &&
	$REAL_GIT ls-tree --name-only "$tree" >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'ls-tree -d output matches real git' '
	(
	cd cross-repo &&
	tree=$($REAL_GIT write-tree) &&
	grit ls-tree -d "$tree" >grit_out &&
	$REAL_GIT ls-tree -d "$tree" >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'ls-tree path filter for single file matches real git' '
	(
	cd cross-repo &&
	tree=$($REAL_GIT write-tree) &&
	grit ls-tree "$tree" a.txt >grit_out &&
	$REAL_GIT ls-tree "$tree" a.txt >git_out &&
	test_cmp grit_out git_out
	)
'

test_done
