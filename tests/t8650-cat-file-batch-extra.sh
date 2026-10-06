#!/bin/sh
# Tests for cat-file --batch edge cases, empty input, many objects.

test_description='cat-file --batch extra scenarios'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

# ── Setup ──────────────────────────────────────────────────────────────────

test_expect_success 'setup: repository with various objects' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "hello world" >file.txt &&
	echo "second" >file2.txt &&
	mkdir sub &&
	echo "nested" >sub/deep.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial" &&
	echo "updated" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second" &&
	"$REAL_GIT" tag -a v1.0 -m "version one"
	)
'

# ── --batch basics ─────────────────────────────────────────────────────────

test_expect_success 'cat-file --batch outputs blob content' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	echo "$blob" | git cat-file --batch >actual &&
	head -1 actual >header &&
	grep "$blob blob" header
	)
'

test_expect_success 'cat-file --batch header has oid type size' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	echo "$blob" | git cat-file --batch >actual &&
	head -1 actual | grep -qE "^[0-9a-f]{40} blob [0-9]+$"
	)
'

test_expect_success 'cat-file --batch content matches cat-file -p' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	git cat-file -p "$blob" >expected &&
	echo "$blob" | git cat-file --batch >batch_out &&
	tail -n +2 batch_out | sed "\$d" >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'cat-file --batch with tree object' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	echo "$tree" | git cat-file --batch >actual &&
	head -1 actual | grep -qE "^[0-9a-f]{40} tree [0-9]+$"
	)
'

test_expect_success 'cat-file --batch with commit object' '
	(
	cd repo &&
	commit=$(git rev-parse HEAD) &&
	echo "$commit" | git cat-file --batch >actual &&
	head -1 actual | grep -qE "^[0-9a-f]{40} commit [0-9]+$"
	)
'

test_expect_success 'cat-file --batch with tag object' '
	(
	cd repo &&
	tag=$(git rev-parse v1.0) &&
	echo "$tag" | git cat-file --batch >actual &&
	head -1 actual | grep -qE "^[0-9a-f]{40} tag [0-9]+$"
	)
'

# ── Multiple objects in one batch ──────────────────────────────────────────

test_expect_success 'cat-file --batch with multiple OIDs' '
	(
	cd repo &&
	blob1=$(git rev-parse HEAD:file.txt) &&
	blob2=$(git rev-parse HEAD:file2.txt) &&
	printf "%s\n%s\n" "$blob1" "$blob2" | git cat-file --batch >actual &&
	grep "$blob1 blob" actual &&
	grep "$blob2 blob" actual
	)
'

test_expect_success 'cat-file --batch processes all objects in order' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(git rev-parse HEAD) &&
	printf "%s\n%s\n%s\n" "$blob" "$tree" "$commit" | git cat-file --batch >actual &&
	grep -c "^[0-9a-f]\{40\} " actual >count &&
	test "$(cat count)" -eq 3
	)
'

test_expect_success 'cat-file --batch with many blob objects' '
	(
	cd repo &&
	blob1=$(git rev-parse HEAD:file.txt) &&
	blob2=$(git rev-parse HEAD:file2.txt) &&
	blob3=$(git rev-parse HEAD:sub/deep.txt) &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(git rev-parse HEAD) &&
	printf "%s\n%s\n%s\n%s\n%s\n" "$blob1" "$blob2" "$blob3" "$tree" "$commit" |
		git cat-file --batch >actual &&
	grep -c "^[0-9a-f]\{40\} " actual >count &&
	test "$(cat count)" -eq 5
	)
'

# ── Empty and invalid input ────────────────────────────────────────────────

test_expect_success 'cat-file --batch with empty input produces no output' '
	(
	cd repo &&
	printf "" | git cat-file --batch >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'cat-file --batch with missing object reports error' '
	(
	cd repo &&
	echo "0000000000000000000000000000000000000000" | git cat-file --batch >actual &&
	grep "missing" actual
	)
'

test_expect_success 'cat-file --batch with invalid hex reports error' '
	(
	cd repo &&
	echo "not-a-valid-oid" | git cat-file --batch >actual &&
	grep "missing" actual
	)
'

# ── --batch-check ──────────────────────────────────────────────────────────

test_expect_success 'cat-file --batch-check outputs type and size' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	echo "$blob" | git cat-file --batch-check >actual &&
	grep "$blob blob" actual
	)
'

test_expect_success 'cat-file --batch-check does not output content' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	echo "$blob" | git cat-file --batch-check >actual &&
	test_line_count -eq 1 actual
	)
'

test_expect_success 'cat-file --batch-check with multiple objects' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	commit=$(git rev-parse HEAD) &&
	printf "%s\n%s\n" "$blob" "$commit" | git cat-file --batch-check >actual &&
	test_line_count -eq 2 actual &&
	grep "blob" actual &&
	grep "commit" actual
	)
'

test_expect_success 'cat-file --batch-check with missing object' '
	(
	cd repo &&
	echo "0000000000000000000000000000000000000000" | git cat-file --batch-check >actual &&
	grep "missing" actual
	)
'

test_expect_success 'cat-file --batch-check with empty input' '
	(
	cd repo &&
	printf "" | git cat-file --batch-check >actual &&
	test_must_be_empty actual
	)
'

# ── --batch-check with custom format ──────────────────────────────────────

test_expect_success 'cat-file --batch-check with %(objecttype) format' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	echo "$blob" | git cat-file "--batch-check=%(objecttype)" >actual &&
	test "$(cat actual)" = "blob"
	)
'

test_expect_success 'cat-file --batch-check with %(objectname) format' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	echo "$blob" | git cat-file "--batch-check=%(objectname)" >actual &&
	test "$(cat actual)" = "$blob"
	)
'

test_expect_success 'cat-file --batch-check with %(objectsize) format' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	expected_size=$(git cat-file -s "$blob") &&
	echo "$blob" | git cat-file "--batch-check=%(objectsize)" >actual &&
	test "$(cat actual)" = "$expected_size"
	)
'

test_expect_success 'cat-file --batch-check with combined format' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	echo "$blob" | git cat-file "--batch-check=%(objecttype) %(objectsize)" >actual &&
	grep "^blob [0-9]*$" actual
	)
'

# ── Size consistency ───────────────────────────────────────────────────────

test_expect_success 'cat-file --batch size matches -s output' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	size_s=$(git cat-file -s "$blob") &&
	echo "$blob" | git cat-file --batch >batch_out &&
	size_batch=$(head -1 batch_out | awk "{print \$3}") &&
	test "$size_s" = "$size_batch"
	)
'

test_expect_success 'cat-file --batch-check size matches -s output' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	size_s=$(git cat-file -s "$blob") &&
	echo "$blob" | git cat-file --batch-check >check_out &&
	size_check=$(awk "{print \$3}" check_out) &&
	test "$size_s" = "$size_check"
	)
'

# ── --batch with all object types ──────────────────────────────────────────

test_expect_success 'cat-file --batch-check sees blob, tree, commit, tag types' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	tree=$(git rev-parse HEAD^{tree}) &&
	commit=$(git rev-parse HEAD) &&
	tag=$(git rev-parse v1.0) &&
	printf "%s\n%s\n%s\n%s\n" "$blob" "$tree" "$commit" "$tag" |
		git cat-file --batch-check >actual &&
	grep "blob" actual &&
	grep "tree" actual &&
	grep "commit" actual &&
	grep "tag" actual
	)
'

test_expect_success 'cat-file --batch output is deterministic' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	echo "$blob" | git cat-file --batch >out1 &&
	echo "$blob" | git cat-file --batch >out2 &&
	test_cmp out1 out2
	)
'

test_expect_success 'cat-file --batch-check output is deterministic' '
	(
	cd repo &&
	blob=$(git rev-parse HEAD:file.txt) &&
	echo "$blob" | git cat-file --batch-check >out1 &&
	echo "$blob" | git cat-file --batch-check >out2 &&
	test_cmp out1 out2
	)
'

test_done
