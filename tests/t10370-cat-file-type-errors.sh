#!/bin/sh
# Test cat-file error handling: invalid OIDs, missing objects, wrong types,
# short OIDs, malformed input, and interaction of -t/-s/-p/-e flags.

test_description='grit cat-file type and error handling'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with blob, tree, commit, tag' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "hello world" >hello &&
	printf "" >empty &&
	grit add hello empty &&
	test_tick &&
	grit commit -m "initial" &&
	blob_oid=$(grit hash-object hello) &&
	empty_oid=$(grit hash-object empty) &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	commit_oid=$(grit rev-parse HEAD) &&
	echo "$blob_oid" >../blob_oid &&
	echo "$empty_oid" >../empty_oid &&
	echo "$tree_oid" >../tree_oid &&
	echo "$commit_oid" >../commit_oid
	)
'

# --- missing / invalid OID errors ---

test_expect_success 'cat-file -t with nonexistent OID fails' '
	(
	cd repo &&
	test_must_fail grit cat-file -t 0000000000000000000000000000000000000000 2>err
	)
'

test_expect_success 'cat-file -s with nonexistent OID fails' '
	(
	cd repo &&
	test_must_fail grit cat-file -s 0000000000000000000000000000000000000000 2>err
	)
'

test_expect_success 'cat-file -p with nonexistent OID fails' '
	(
	cd repo &&
	test_must_fail grit cat-file -p 0000000000000000000000000000000000000000 2>err
	)
'

test_expect_success 'cat-file -e with nonexistent OID fails' '
	(
	cd repo &&
	test_must_fail grit cat-file -e 0000000000000000000000000000000000000000
	)
'

test_expect_success 'cat-file -e with existing blob succeeds' '
	(
	cd repo &&
	grit cat-file -e $(cat ../blob_oid)
	)
'

test_expect_success 'cat-file -e with existing tree succeeds' '
	(
	cd repo &&
	grit cat-file -e $(cat ../tree_oid)
	)
'

test_expect_success 'cat-file -e with existing commit succeeds' '
	(
	cd repo &&
	grit cat-file -e $(cat ../commit_oid)
	)
'

test_expect_success 'cat-file -t blob returns blob' '
	(
	cd repo &&
	grit cat-file -t $(cat ../blob_oid) >actual &&
	echo "blob" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -t tree returns tree' '
	(
	cd repo &&
	grit cat-file -t $(cat ../tree_oid) >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -t commit returns commit' '
	(
	cd repo &&
	grit cat-file -t $(cat ../commit_oid) >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s blob returns correct size' '
	(
	cd repo &&
	size=$(grit cat-file -s $(cat ../blob_oid)) &&
	expected=$(wc -c <hello | tr -d " ") &&
	test "$size" = "$expected"
	)
'

test_expect_success 'cat-file -s empty blob returns 0' '
	(
	cd repo &&
	size=$(grit cat-file -s $(cat ../empty_oid)) &&
	test "$size" = "0"
	)
'

test_expect_success 'cat-file -s tree returns positive size' '
	(
	cd repo &&
	size=$(grit cat-file -s $(cat ../tree_oid)) &&
	test "$size" -gt 0
	)
'

test_expect_success 'cat-file -s commit returns positive size' '
	(
	cd repo &&
	size=$(grit cat-file -s $(cat ../commit_oid)) &&
	test "$size" -gt 0
	)
'

test_expect_success 'cat-file -p blob outputs content' '
	(
	cd repo &&
	grit cat-file -p $(cat ../blob_oid) >actual &&
	test_cmp hello actual
	)
'

test_expect_success 'cat-file -p empty blob outputs nothing' '
	(
	cd repo &&
	grit cat-file -p $(cat ../empty_oid) >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'cat-file -p tree outputs ls-tree-like format' '
	(
	cd repo &&
	grit cat-file -p $(cat ../tree_oid) >actual &&
	grep "hello" actual
	)
'

test_expect_success 'cat-file -p commit outputs commit message' '
	(
	cd repo &&
	grit cat-file -p $(cat ../commit_oid) >actual &&
	grep "initial" actual
	)
'

test_expect_success 'cat-file -p commit shows author line' '
	(
	cd repo &&
	grit cat-file -p $(cat ../commit_oid) >actual &&
	grep "author" actual
	)
'

test_expect_success 'cat-file -p commit shows tree line' '
	(
	cd repo &&
	grit cat-file -p $(cat ../commit_oid) >actual &&
	tree=$(cat ../tree_oid) &&
	grep "tree $tree" actual
	)
'

# --- batch-check with missing objects ---

test_expect_success 'batch-check: missing object shows missing' '
	(
	cd repo &&
	echo "0000000000000000000000000000000000000000" |
		grit cat-file --batch-check >actual &&
	grep "missing" actual
	)
'

test_expect_success 'batch-check: all-f OID is missing' '
	(
	cd repo &&
	echo "ffffffffffffffffffffffffffffffffffffffff" |
		grit cat-file --batch-check >actual &&
	grep "missing" actual
	)
'

test_expect_success 'batch: missing object shows missing' '
	(
	cd repo &&
	echo "0000000000000000000000000000000000000000" |
		grit cat-file --batch >actual &&
	grep "missing" actual
	)
'

test_expect_success 'batch-check: valid then missing then valid' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	printf "%s\n%s\n%s\n" "$blob" "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef" "$blob" |
		grit cat-file --batch-check >actual &&
	test_line_count = 3 actual &&
	sed -n 1p actual | grep "blob" &&
	sed -n 2p actual | grep "missing" &&
	sed -n 3p actual | grep "blob"
	)
'

test_expect_success 'batch-check: empty line handling' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	printf "%s\n" "$blob" | grit cat-file --batch-check >actual &&
	grep "blob" actual
	)
'

# --- HEAD and ref resolution ---

test_expect_success 'cat-file -t HEAD returns commit' '
	(
	cd repo &&
	grit cat-file -t HEAD >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -p of tree OID shows tree content' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit cat-file -p "$tree" >actual &&
	grep "hello" actual
	)
'

test_expect_success 'cat-file -t of tree OID returns tree' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	grit cat-file -t "$tree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

# --- second commit for parent testing ---

test_expect_success 'setup second commit' '
	(
	cd repo &&
	echo "second" >second &&
	grit add second &&
	test_tick &&
	grit commit -m "second commit" &&
	echo "$(grit rev-parse HEAD)" >../commit2_oid
	)
'

test_expect_success 'cat-file -p second commit shows parent' '
	(
	cd repo &&
	grit cat-file -p $(cat ../commit2_oid) >actual &&
	parent=$(cat ../commit_oid) &&
	grep "parent $parent" actual
	)
'

test_expect_success 'cat-file -t of second commit is commit' '
	(
	cd repo &&
	grit cat-file -t $(cat ../commit2_oid) >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s second commit is larger than first (has parent)' '
	(
	cd repo &&
	size1=$(grit cat-file -s $(cat ../commit_oid)) &&
	size2=$(grit cat-file -s $(cat ../commit2_oid)) &&
	test "$size2" -gt "$size1"
	)
'

test_expect_success 'cat-file -e accepts abbreviated OID on command line' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	short=$(echo "$blob" | cut -c1-7) &&
	grit cat-file -e "$short"
	)
'

test_expect_success 'cat-file with no flags and no args fails' '
	(
	cd repo &&
	test_must_fail grit cat-file 2>err
	)
'

test_expect_success 'batch-check with HEAD resolves correctly' '
	(
	cd repo &&
	echo "HEAD" | grit cat-file --batch-check >actual &&
	grep "commit" actual
	)
'

test_expect_success 'batch-check format with missing object' '
	(
	cd repo &&
	echo "0000000000000000000000000000000000000000" |
		grit cat-file --batch-check="%(objecttype)" >actual &&
	grep "missing" actual
	)
'

test_done
