#!/bin/sh
# Test cat-file --batch and --batch-check with custom format strings.

test_description='grit cat-file batch format'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

echo_without_newline() {
	printf '%s' "$*"
}

strlen() {
	echo_without_newline "$1" | wc -c | sed -e 's/^ *//'
}

test_expect_success 'setup repository with blob, tree, commit' '
	(
	grit init repo &&
	cd repo &&
	echo_without_newline "Hello World" >hello &&
	grit add hello &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	test_tick &&
	grit commit -m "initial" &&
	blob_oid=$(grit hash-object hello) &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	commit_oid=$(grit rev-parse HEAD) &&
	echo "$blob_oid" >../blob_oid &&
	echo "$tree_oid" >../tree_oid &&
	echo "$commit_oid" >../commit_oid &&
	echo "11" >../blob_size
	)
'

# --- batch-check default format ---

test_expect_success 'batch-check default: blob' '
	(
	cd repo &&
	oid=$(cat ../blob_oid) &&
	echo "$oid blob 11" >expect &&
	echo "$oid" | grit cat-file --batch-check >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'batch-check default: tree' '
	(
	cd repo &&
	oid=$(cat ../tree_oid) &&
	size=$(grit cat-file -s "$oid") &&
	echo "$oid tree $size" >expect &&
	echo "$oid" | grit cat-file --batch-check >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'batch-check default: commit' '
	(
	cd repo &&
	oid=$(cat ../commit_oid) &&
	size=$(grit cat-file -s "$oid") &&
	echo "$oid commit $size" >expect &&
	echo "$oid" | grit cat-file --batch-check >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'batch-check: missing object reports error' '
	(
	cd repo &&
	echo "0000000000000000000000000000000000000000" | grit cat-file --batch-check >actual &&
	grep "missing" actual
	)
'

# --- batch-check custom format strings ---

test_expect_success 'batch-check format: %(objecttype)' '
	(
	cd repo &&
	oid=$(cat ../blob_oid) &&
	echo "blob" >expect &&
	echo "$oid" | grit cat-file --batch-check="%(objecttype)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'batch-check format: %(objectname)' '
	(
	cd repo &&
	oid=$(cat ../blob_oid) &&
	echo "$oid" >expect &&
	echo "$oid" | grit cat-file --batch-check="%(objectname)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'batch-check format: %(objectsize)' '
	(
	cd repo &&
	oid=$(cat ../blob_oid) &&
	echo "11" >expect &&
	echo "$oid" | grit cat-file --batch-check="%(objectsize)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'batch-check format: %(objecttype) %(objectsize)' '
	(
	cd repo &&
	oid=$(cat ../blob_oid) &&
	echo "blob 11" >expect &&
	echo "$oid" | grit cat-file --batch-check="%(objecttype) %(objectsize)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'batch-check format: %(objectname) %(objecttype)' '
	(
	cd repo &&
	oid=$(cat ../blob_oid) &&
	echo "$oid blob" >expect &&
	echo "$oid" | grit cat-file --batch-check="%(objectname) %(objecttype)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'batch-check format: literal text around placeholders' '
	(
	cd repo &&
	oid=$(cat ../blob_oid) &&
	echo "type=blob size=11" >expect &&
	echo "$oid" | grit cat-file --batch-check="type=%(objecttype) size=%(objectsize)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'batch-check format works for tree objects' '
	(
	cd repo &&
	oid=$(cat ../tree_oid) &&
	echo "tree" >expect &&
	echo "$oid" | grit cat-file --batch-check="%(objecttype)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'batch-check format works for commit objects' '
	(
	cd repo &&
	oid=$(cat ../commit_oid) &&
	echo "commit" >expect &&
	echo "$oid" | grit cat-file --batch-check="%(objecttype)" >actual &&
	test_cmp expect actual
	)
'

# --- batch default format ---

test_expect_success 'batch default: blob header and content' '
	(
	cd repo &&
	oid=$(cat ../blob_oid) &&
	{
		echo "$oid blob 11" &&
		echo_without_newline "Hello World" &&
		echo
	} >expect &&
	echo "$oid" | grit cat-file --batch >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'batch: tree object header is correct' '
	(
	cd repo &&
	oid=$(cat ../tree_oid) &&
	size=$(grit cat-file -s "$oid") &&
	echo "$oid" | grit cat-file --batch >actual &&
	head -1 actual >header &&
	echo "$oid tree $size" >expect_header &&
	test_cmp expect_header header
	)
'

test_expect_success 'batch: commit object header is correct' '
	(
	cd repo &&
	oid=$(cat ../commit_oid) &&
	size=$(grit cat-file -s "$oid") &&
	echo "$oid" | grit cat-file --batch >actual &&
	head -1 actual >header &&
	echo "$oid commit $size" >expect_header &&
	test_cmp expect_header header
	)
'

# --- multiple objects in batch ---

test_expect_success 'batch-check: multiple objects in one invocation' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	tree=$(cat ../tree_oid) &&
	commit=$(cat ../commit_oid) &&
	printf "%s\n%s\n%s\n" "$blob" "$tree" "$commit" | grit cat-file --batch-check >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'batch-check: multiple objects types are correct' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	tree=$(cat ../tree_oid) &&
	commit=$(cat ../commit_oid) &&
	printf "%s\n%s\n%s\n" "$blob" "$tree" "$commit" |
		grit cat-file --batch-check="%(objecttype)" >actual &&
	printf "blob\ntree\ncommit\n" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'batch: multiple objects produce correct headers' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	tree=$(cat ../tree_oid) &&
	printf "%s\n%s\n" "$blob" "$tree" | grit cat-file --batch >actual &&
	grep "^$blob blob" actual &&
	grep "^$tree tree" actual
	)
'

# --- second commit, more objects ---

test_expect_success 'setup second commit with more files' '
	(
	cd repo &&
	echo "second file" >second &&
	echo "third file content here" >third &&
	grit add second third &&
	test_tick &&
	grit commit -m "second commit" &&
	second_blob=$(grit hash-object second) &&
	third_blob=$(grit hash-object third) &&
	echo "$second_blob" >../second_oid &&
	echo "$third_blob" >../third_oid
	)
'

test_expect_success 'batch-check: five objects in one stream' '
	(
	cd repo &&
	cat ../blob_oid ../tree_oid ../commit_oid ../second_oid ../third_oid |
		grit cat-file --batch-check >actual &&
	test_line_count = 5 actual
	)
'

test_expect_success 'batch-check format: %(objectsize) correct for various blobs' '
	(
	cd repo &&
	second=$(cat ../second_oid) &&
	third=$(cat ../third_oid) &&
	printf "%s\n%s\n" "$second" "$third" |
		grit cat-file --batch-check="%(objectsize)" >actual &&
	size1=$(grit cat-file -s "$second") &&
	size2=$(grit cat-file -s "$third") &&
	printf "%s\n%s\n" "$size1" "$size2" >expect &&
	test_cmp expect actual
	)
'

# --- missing / invalid object handling ---

test_expect_success 'batch-check: interleaved valid and missing objects' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	printf "%s\n%s\n%s\n" "$blob" "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef" "$blob" |
		grit cat-file --batch-check >actual &&
	test_line_count = 3 actual &&
	sed -n 2p actual | grep "missing"
	)
'

test_expect_success 'batch: missing object does not crash' '
	(
	cd repo &&
	echo "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef" |
		grit cat-file --batch >actual &&
	grep "missing" actual
	)
'

# --- blob content verification through batch ---

test_expect_success 'batch: blob content matches cat-file -p' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	grit cat-file -p "$blob" >expect &&
	echo "$blob" | grit cat-file --batch >batch_out &&
	sed 1d batch_out | head -c $(wc -c <expect | tr -d " ") >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'batch: second blob content is correct' '
	(
	cd repo &&
	second=$(cat ../second_oid) &&
	grit cat-file -p "$second" >expect &&
	echo "$second" | grit cat-file --batch >batch_out &&
	sed 1d batch_out | head -c $(wc -c <expect | tr -d " ") >actual &&
	test_cmp expect actual
	)
'

# --- format edge cases ---

test_expect_success 'batch-check format: just objecttype placeholder' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "blob" >expect &&
	echo "$blob" | grit cat-file --batch-check="%(objecttype)" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'batch-check format: only literal text' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "hello" >expect &&
	echo "$blob" | grit cat-file --batch-check="hello" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'batch-check format: repeated placeholder' '
	(
	cd repo &&
	blob=$(cat ../blob_oid) &&
	echo "blob blob" >expect &&
	echo "$blob" | grit cat-file --batch-check="%(objecttype) %(objecttype)" >actual &&
	test_cmp expect actual
	)
'

# --- using HEAD/refs with batch ---

test_expect_success 'batch-check with HEAD ref' '
	(
	cd repo &&
	echo "HEAD" | grit cat-file --batch-check >actual &&
	grep "commit" actual
	)
'

test_expect_success 'batch-check with tree oid directly' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	echo "$tree" | grit cat-file --batch-check >actual &&
	grep "tree" actual
	)
'

test_expect_success 'batch with HEAD produces commit content' '
	(
	cd repo &&
	echo "HEAD" | grit cat-file --batch >actual &&
	head -1 actual | grep "commit"
	)
'

test_expect_success 'batch-check format: %(objectname) matches rev-parse' '
	(
	cd repo &&
	expected=$(grit rev-parse HEAD) &&
	echo "HEAD" | grit cat-file --batch-check="%(objectname)" >actual &&
	echo "$expected" >expect &&
	test_cmp expect actual
	)
'

test_done
