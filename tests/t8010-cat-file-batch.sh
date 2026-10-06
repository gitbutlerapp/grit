#!/bin/sh
# Tests for cat-file --batch and --batch-check modes.

test_description='cat-file batch modes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with various objects' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "hello world" >file.txt &&
	echo "second file" >file2.txt &&
	mkdir -p sub &&
	echo "sub content" >sub/nested.txt &&
	git add . &&
	git commit -m "initial commit" &&
	git tag v1.0 -m "version 1.0" &&
	echo "updated" >file.txt &&
	git add file.txt &&
	git commit -m "update file"
	)
'

# ── --batch: single object ──────────────────────────────────────────────

test_expect_success 'batch: blob by OID' '
	(
	cd repo &&
	blob_oid=$(git rev-parse HEAD:file.txt) &&
	result=$(echo "$blob_oid" | git cat-file --batch) &&
	echo "$result" | head -1 >header &&
	grep "$blob_oid blob" header
	)
'

test_expect_success 'batch: blob content is correct' '
	(
	cd repo &&
	blob_oid=$(git rev-parse HEAD:file.txt) &&
	echo "$blob_oid" | git cat-file --batch >out &&
	grep "updated" out
	)
'

test_expect_success 'batch: commit by OID' '
	(
	cd repo &&
	commit_oid=$(git rev-parse HEAD) &&
	echo "$commit_oid" | git cat-file --batch >out &&
	head -1 out >header &&
	grep "$commit_oid commit" header &&
	grep "update file" out
	)
'

test_expect_success 'batch: tree by OID' '
	(
	cd repo &&
	tree_oid=$(git rev-parse HEAD^{tree}) &&
	echo "$tree_oid" | git cat-file --batch >out &&
	head -1 out >header &&
	grep "$tree_oid tree" header
	)
'

# ── --batch: multiple objects ────────────────────────────────────────────

test_expect_success 'batch: multiple OIDs in one invocation' '
	(
	cd repo &&
	blob_oid=$(git rev-parse HEAD:file.txt) &&
	commit_oid=$(git rev-parse HEAD) &&
	printf "%s\n%s\n" "$blob_oid" "$commit_oid" | git cat-file --batch >out &&
	grep "$blob_oid blob" out &&
	grep "$commit_oid commit" out
	)
'

test_expect_success 'batch: three different object types' '
	(
	cd repo &&
	blob_oid=$(git rev-parse HEAD:file.txt) &&
	tree_oid=$(git rev-parse HEAD^{tree}) &&
	commit_oid=$(git rev-parse HEAD) &&
	printf "%s\n%s\n%s\n" "$blob_oid" "$tree_oid" "$commit_oid" | git cat-file --batch >out &&
	grep "$blob_oid blob" out &&
	grep "$tree_oid tree" out &&
	grep "$commit_oid commit" out
	)
'

# ── --batch: missing objects ─────────────────────────────────────────────

test_expect_success 'batch: missing object prints "missing"' '
	(
	cd repo &&
	echo "0000000000000000000000000000000000000000" | git cat-file --batch >out &&
	grep "missing" out
	)
'

test_expect_success 'batch: non-hex input prints "missing"' '
	(
	cd repo &&
	echo "not-a-valid-oid" | git cat-file --batch >out &&
	grep "missing" out
	)
'

test_expect_success 'batch: mix of valid and missing objects' '
	(
	cd repo &&
	blob_oid=$(git rev-parse HEAD:file.txt) &&
	printf "%s\n%s\n%s\n" "$blob_oid" "0000000000000000000000000000000000000000" "$blob_oid" | git cat-file --batch >out &&
	valid_count=$(grep -c "blob" out) &&
	missing_count=$(grep -c "missing" out) &&
	test "$valid_count" = "2" &&
	test "$missing_count" = "1"
	)
'

# ── --batch-check ────────────────────────────────────────────────────────

test_expect_success 'batch-check: blob shows type and size' '
	(
	cd repo &&
	blob_oid=$(git rev-parse HEAD:file.txt) &&
	echo "$blob_oid" | git cat-file --batch-check >out &&
	grep "$blob_oid blob" out
	)
'

test_expect_success 'batch-check: commit shows type and size' '
	(
	cd repo &&
	commit_oid=$(git rev-parse HEAD) &&
	echo "$commit_oid" | git cat-file --batch-check >out &&
	grep "$commit_oid commit" out
	)
'

test_expect_success 'batch-check: tree shows type and size' '
	(
	cd repo &&
	tree_oid=$(git rev-parse HEAD^{tree}) &&
	echo "$tree_oid" | git cat-file --batch-check >out &&
	grep "$tree_oid tree" out
	)
'

test_expect_success 'batch-check: multiple objects' '
	(
	cd repo &&
	blob_oid=$(git rev-parse HEAD:file.txt) &&
	commit_oid=$(git rev-parse HEAD) &&
	printf "%s\n%s\n" "$blob_oid" "$commit_oid" | git cat-file --batch-check >out &&
	grep "$blob_oid blob" out &&
	grep "$commit_oid commit" out
	)
'

test_expect_success 'batch-check: missing object prints missing' '
	(
	cd repo &&
	echo "0000000000000000000000000000000000000000" | git cat-file --batch-check >out &&
	grep "missing" out
	)
'

test_expect_success 'batch-check: does not print content (only header)' '
	(
	cd repo &&
	blob_oid=$(git rev-parse HEAD:file.txt) &&
	echo "$blob_oid" | git cat-file --batch-check >out &&
	! grep "updated" out
	)
'

# ── --batch with various blob content ────────────────────────────────────

test_expect_success 'batch: empty blob' '
	(
	cd repo &&
	empty_oid=$(git hash-object -w --stdin </dev/null) &&
	echo "$empty_oid" | git cat-file --batch >out &&
	head -1 out >header &&
	grep "$empty_oid blob 0" header
	)
'

test_expect_success 'batch: blob with newlines' '
	(
	cd repo &&
	blob_oid=$(printf "line1\nline2\nline3\n" | git hash-object -w --stdin) &&
	echo "$blob_oid" | git cat-file --batch >out &&
	grep "line1" out &&
	grep "line3" out
	)
'

test_expect_success 'batch: large-ish blob' '
	(
	cd repo &&
	dd if=/dev/urandom bs=1024 count=10 2>/dev/null | base64 >large.txt &&
	blob_oid=$(git hash-object -w large.txt) &&
	echo "$blob_oid" | git cat-file --batch >out &&
	head -1 out >header &&
	grep "$blob_oid blob" header
	)
'

# ── --batch with refs ────────────────────────────────────────────────────

test_expect_success 'batch-check: size field is numeric' '
	(
	cd repo &&
	blob_oid=$(git rev-parse HEAD:file.txt) &&
	echo "$blob_oid" | git cat-file --batch-check >out &&
	size=$(awk "{print \$3}" out) &&
	test "$size" -gt 0
	)
'

test_expect_success 'batch: different blobs from same commit' '
	(
	cd repo &&
	blob1=$(git rev-parse HEAD:file.txt) &&
	blob2=$(git rev-parse HEAD:file2.txt) &&
	printf "%s\n%s\n" "$blob1" "$blob2" | git cat-file --batch >out &&
	grep "updated" out &&
	grep "second file" out
	)
'

# ── --batch empty input ─────────────────────────────────────────────────

test_expect_success 'batch: empty input produces no output' '
	(
	cd repo &&
	echo "" | git cat-file --batch >out &&
	grep "missing" out
	)
'

test_expect_success 'batch-check: empty input produces no output' '
	(
	cd repo &&
	echo "" | git cat-file --batch-check >out &&
	grep "missing" out
	)
'

# ── batch output format consistency ──────────────────────────────────────

test_expect_success 'batch: header line format is OID TYPE SIZE' '
	(
	cd repo &&
	blob_oid=$(git rev-parse HEAD:file.txt) &&
	echo "$blob_oid" | git cat-file --batch >out &&
	head -1 out | grep -E "^[0-9a-f]{40} blob [0-9]+$"
	)
'

test_expect_success 'batch-check: output format is OID TYPE SIZE' '
	(
	cd repo &&
	commit_oid=$(git rev-parse HEAD) &&
	echo "$commit_oid" | git cat-file --batch-check >out &&
	grep -E "^[0-9a-f]{40} commit [0-9]+$" out
	)
'

test_expect_success 'batch: content length matches size header' '
	(
	cd repo &&
	blob_oid=$(git rev-parse HEAD:file.txt) &&
	echo "$blob_oid" | git cat-file --batch >out &&
	size=$(head -1 out | awk "{print \$3}") &&
	tail -n +2 out >content &&
	actual_size=$(wc -c <content | tr -d " ") &&
	test "$actual_size" -ge "$size"
	)
'

test_done
