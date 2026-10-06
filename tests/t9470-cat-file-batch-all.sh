#!/bin/sh
# Tests for grit cat-file --batch, --batch-check, --batch-command modes.

test_description='grit cat-file batch modes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with various objects' '
	(
	grit init repo &&
	cd repo &&
	echo "hello" >hello.txt &&
	mkdir -p sub &&
	echo "nested" >sub/nested.txt &&
	grit add hello.txt sub/nested.txt &&
	blob_oid=$(grit hash-object -w hello.txt) &&
	echo "$blob_oid" >../blob_oid &&
	nested_oid=$(grit hash-object -w sub/nested.txt) &&
	echo "$nested_oid" >../nested_oid &&
	tree_oid=$(grit write-tree) &&
	echo "$tree_oid" >../tree_oid &&
	commit_oid=$(echo "initial commit" | grit commit-tree "$tree_oid") &&
	echo "$commit_oid" >../commit_oid
	)
'

###########################################################################
# Section 2: --batch basic
###########################################################################

test_expect_success 'cat-file --batch prints blob info and content' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "$blob" | grit cat-file --batch >actual &&
	head -1 actual >header &&
	echo "$blob blob 6" >expect_hdr &&
	test_cmp expect_hdr header
	)
'

test_expect_success 'cat-file --batch blob content after header' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "$blob" | grit cat-file --batch >actual &&
	sed -n 2p actual >content &&
	echo "hello" >expect &&
	test_cmp expect content
	)
'

test_expect_success 'cat-file --batch with commit OID' '
	(
	cd repo &&
	commit=$(cat ../commit_oid) &&
	echo "$commit" | grit cat-file --batch >actual &&
	head -1 actual >header &&
	grep "^$commit commit" header
	)
'

test_expect_success 'cat-file --batch with tree OID' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	echo "$tree" | grit cat-file --batch >actual &&
	head -1 actual >header &&
	grep "^$tree tree" header
	)
'

test_expect_success 'cat-file --batch with multiple OIDs' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	tree=$(cat ../tree_oid) &&
	commit=$(cat ../commit_oid) &&
	printf "%s\n%s\n%s\n" "$blob" "$tree" "$commit" | grit cat-file --batch >actual &&
	grep "$blob blob" actual &&
	grep "$tree tree" actual &&
	grep "$commit commit" actual
	)
'

test_expect_success 'cat-file --batch with nonexistent OID reports missing' '
	(
	cd repo &&
	echo "0000000000000000000000000000000000000000" | grit cat-file --batch >actual &&
	grep "missing" actual
	)
'

test_expect_success 'cat-file --batch with empty blob' '
	(
	cd repo &&
	empty_oid=$(printf "" | grit hash-object -w --stdin) &&
	echo "$empty_oid" | grit cat-file --batch >actual &&
	head -1 actual >header &&
	echo "$empty_oid blob 0" >expect &&
	test_cmp expect header
	)
'

###########################################################################
# Section 3: --batch-check
###########################################################################

test_expect_success 'cat-file --batch-check shows type and size for blob' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "$blob" | grit cat-file --batch-check >actual &&
	echo "$blob blob 6" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file --batch-check shows type and size for tree' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	echo "$tree" | grit cat-file --batch-check >actual &&
	grep "^$tree tree" actual
	)
'

test_expect_success 'cat-file --batch-check shows type and size for commit' '
	(
	cd repo &&
	commit=$(cat ../commit_oid) &&
	echo "$commit" | grit cat-file --batch-check >actual &&
	grep "^$commit commit" actual
	)
'

test_expect_success 'cat-file --batch-check with multiple OIDs' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	tree=$(cat ../tree_oid) &&
	printf "%s\n%s\n" "$blob" "$tree" | grit cat-file --batch-check >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'cat-file --batch-check nonexistent OID reports missing' '
	(
	cd repo &&
	echo "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef" | grit cat-file --batch-check >actual &&
	grep "missing" actual
	)
'

test_expect_success 'cat-file --batch-check does NOT print content' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "$blob" | grit cat-file --batch-check >actual &&
	test $(wc -l <actual) -eq 1
	)
'

###########################################################################
# Section 4: --batch-check with custom format
###########################################################################

test_expect_success 'cat-file --batch-check=%(objecttype) shows type only' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "$blob" | grit cat-file "--batch-check=%(objecttype)" >actual &&
	echo "blob" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file --batch-check=%(objectsize) shows size only' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "$blob" | grit cat-file "--batch-check=%(objectsize)" >actual &&
	echo "6" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file --batch-check=%(objectname) shows OID' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "$blob" | grit cat-file "--batch-check=%(objectname)" >actual &&
	echo "$blob" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file --batch-check with combined format' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "$blob" | grit cat-file "--batch-check=%(objecttype) %(objectsize)" >actual &&
	echo "blob 6" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 5: --batch-command
###########################################################################

test_expect_success 'cat-file --batch-command info prints type/size' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "info $blob" | grit cat-file --batch-command >actual &&
	echo "$blob blob 6" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file --batch-command contents prints content' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "contents $blob" | grit cat-file --batch-command >actual &&
	grep "hello" actual
	)
'

test_expect_success 'cat-file --batch-command multiple commands' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	commit=$(cat ../commit_oid) &&
	printf "info %s\ninfo %s\n" "$blob" "$commit" | grit cat-file --batch-command >actual &&
	grep "blob" actual &&
	grep "commit" actual
	)
'

test_expect_success 'cat-file --batch-command info on missing OID' '
	(
	cd repo &&
	echo "info 0000000000000000000000000000000000000000" | grit cat-file --batch-command >actual &&
	grep "missing" actual
	)
'

###########################################################################
# Section 6: Cross-check with real git
###########################################################################

test_expect_success 'batch-check output matches real git for blob' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "$blob" | grit cat-file --batch-check >grit_out &&
	echo "$blob" | $REAL_GIT cat-file --batch-check >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'batch-check output matches real git for tree' '
	(
	cd repo &&
	tree=$(cat ../tree_oid) &&
	echo "$tree" | grit cat-file --batch-check >grit_out &&
	echo "$tree" | $REAL_GIT cat-file --batch-check >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'batch output matches real git for blob content' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "$blob" | grit cat-file --batch >grit_out &&
	echo "$blob" | $REAL_GIT cat-file --batch >git_out &&
	test_cmp grit_out git_out
	)
'

###########################################################################
# Section 7: Edge cases
###########################################################################

test_expect_success 'cat-file --batch with short/invalid OID' '
	(
	cd repo &&
	echo "not-a-valid-oid" | grit cat-file --batch >actual &&
	grep "missing" actual
	)
'

test_expect_success 'cat-file --batch-check with empty input produces no output' '
	(
	cd repo &&
	printf "" | grit cat-file --batch-check >actual &&
	test_must_be_empty actual
	)
'

test_done
