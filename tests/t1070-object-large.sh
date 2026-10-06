#!/bin/sh
# Tests for large objects: big blobs, big trees, many entries

test_description='large objects: big blobs, big trees, many entries'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com"
	)
'

# ---------------------------------------------------------------------------
# Large blobs
# ---------------------------------------------------------------------------
test_expect_success 'hash-object with 1MB blob' '
	(
	cd repo &&
	dd if=/dev/urandom bs=1024 count=1024 2>/dev/null | base64 >large1m &&
	sha=$(git hash-object -w large1m) &&
	echo "$sha" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'cat-file -s reports correct size for large blob' '
	(
	cd repo &&
	expected=$(wc -c <large1m | tr -d " ") &&
	sha=$(git hash-object large1m) &&
	git cat-file -s "$sha" >actual_size &&
	test "$expected" = "$(cat actual_size)"
	)
'

test_expect_success 'cat-file -p retrieves large blob content' '
	(
	cd repo &&
	sha=$(git hash-object -w large1m) &&
	git cat-file -p "$sha" >retrieved &&
	test_cmp large1m retrieved
	)
'

test_expect_success 'hash-object with 5MB blob' '
	(
	cd repo &&
	dd if=/dev/zero bs=1024 count=5120 2>/dev/null >large5m &&
	sha=$(git hash-object -w large5m) &&
	git cat-file -e "$sha"
	)
'

test_expect_success 'add and commit large file' '
	(
	cd repo &&
	git add large1m &&
	git commit -m "add 1MB file" &&
	git rev-parse HEAD >../commit1
	)
'

test_expect_success 'diff-tree shows large file in commit' '
	(
	cd repo &&
	sha=$(git rev-parse HEAD) &&
	git diff-tree --name-only -r --root "$sha" >out &&
	grep -q "large1m" out
	)
'

test_expect_success 'cat-file -t on large blob is blob' '
	(
	cd repo &&
	sha=$(git hash-object large1m) &&
	git cat-file -t "$sha" >out &&
	echo "blob" >expected &&
	test_cmp expected out
	)
'

# ---------------------------------------------------------------------------
# Many files in a single tree
# ---------------------------------------------------------------------------
test_expect_success 'create 100 files' '
	(
	cd repo &&
	for i in $(seq 1 100); do
		echo "file content $i" >"file_$i.txt"
	done &&
	git add file_*.txt &&
	git commit -m "add 100 files"
	)
'

test_expect_success 'ls-tree lists all 100 files' '
	(
	cd repo &&
	git ls-tree HEAD >out &&
	count=$(grep -c "file_" out) &&
	test "$count" -eq 100
	)
'

test_expect_success 'ls-files lists all 100 files' '
	(
	cd repo &&
	git ls-files >out &&
	count=$(grep -c "file_" out) &&
	test "$count" -eq 100
	)
'

test_expect_success 'tree object size is substantial' '
	(
	cd repo &&
	tree_sha=$(git rev-parse HEAD^{tree}) &&
	size=$(git cat-file -s "$tree_sha") &&
	test "$size" -gt 1000
	)
'

# ---------------------------------------------------------------------------
# Many files in subdirectories
# ---------------------------------------------------------------------------
test_expect_success 'create nested directory structure with many files' '
	(
	cd repo &&
	for d in a b c d e; do
		mkdir -p "dir_$d" &&
		for i in $(seq 1 20); do
			echo "nested $d $i" >"dir_$d/file_$i.txt"
		done
	done &&
	git add dir_* &&
	git commit -m "add nested dirs with 100 files"
	)
'

test_expect_success 'ls-tree -r lists all nested files' '
	(
	cd repo &&
	git ls-tree -r HEAD >out &&
	count=$(grep -c "dir_" out) &&
	test "$count" -eq 100
	)
'

test_expect_success 'ls-tree without -r shows subtrees' '
	(
	cd repo &&
	git ls-tree HEAD >out &&
	tree_count=$(grep -c "^[0-9].*tree" out) &&
	test "$tree_count" -ge 5
	)
'

# ---------------------------------------------------------------------------
# Large number of commits
# ---------------------------------------------------------------------------
test_expect_success 'create 50 sequential commits' '
	(
	cd repo &&
	for i in $(seq 1 50); do
		echo "iteration $i" >counter.txt &&
		git add counter.txt &&
		git commit -m "commit $i" >/dev/null
	done
	)
'

test_expect_success 'rev-list counts all commits' '
	(
	cd repo &&
	git rev-list HEAD >out &&
	count=$(wc -l <out | tr -d " ") &&
	test "$count" -ge 52
	)
'

test_expect_success 'log shows commits' '
	(
	cd repo &&
	git log --oneline >out &&
	count=$(wc -l <out | tr -d " ") &&
	test "$count" -ge 52
	)
'

# ---------------------------------------------------------------------------
# Large blob with specific content patterns
# ---------------------------------------------------------------------------
test_expect_success 'blob with many repeated lines' '
	(
	cd repo &&
	for i in $(seq 1 10000); do
		echo "repeated line number $i"
	done >repeated.txt &&
	sha=$(git hash-object -w repeated.txt) &&
	git cat-file -e "$sha"
	)
'

test_expect_success 'blob with binary-like content' '
	(
	cd repo &&
	dd if=/dev/urandom bs=1024 count=100 2>/dev/null >binary.bin &&
	sha=$(git hash-object -w binary.bin) &&
	git cat-file -e "$sha"
	)
'

test_expect_success 'add and commit binary blob' '
	(
	cd repo &&
	git add binary.bin &&
	git commit -m "add binary file"
	)
'

test_expect_success 'diff detects binary file' '
	(
	cd repo &&
	dd if=/dev/urandom bs=1024 count=100 2>/dev/null >binary.bin &&
	git diff --stat >out &&
	grep -q "Bin\|binary" out || grep -q "binary.bin" out
	)
'

# ---------------------------------------------------------------------------
# Empty and single-byte blobs
# ---------------------------------------------------------------------------
test_expect_success 'empty blob has consistent SHA1' '
	(
	cd repo &&
	: >empty1 &&
	: >empty2 &&
	sha1=$(git hash-object empty1) &&
	sha2=$(git hash-object empty2) &&
	test "$sha1" = "$sha2"
	)
'

test_expect_success 'empty blob SHA1 is the well-known value' '
	(
	cd repo &&
	: >empty &&
	sha=$(git hash-object empty) &&
	test "$sha" = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
	)
'

test_expect_success 'single-byte blob' '
	(
	cd repo &&
	printf "x" >single &&
	sha=$(git hash-object -w single) &&
	git cat-file -s "$sha" >out &&
	test "$(cat out)" = "1"
	)
'

# ---------------------------------------------------------------------------
# Wide tree (many entries at top level)
# ---------------------------------------------------------------------------
test_expect_success 'create 200 files at top level' '
	(
	cd repo &&
	for i in $(seq 200 400); do
		echo "wide $i" >"w_$i.txt"
	done &&
	git add w_*.txt &&
	git commit -m "wide tree"
	)
'

test_expect_success 'ls-tree HEAD lists 200+ w_ entries' '
	(
	cd repo &&
	git ls-tree HEAD >out &&
	count=$(grep -c "w_" out) &&
	test "$count" -ge 200
	)
'

test_expect_success 'write-tree with many entries succeeds' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	echo "$tree" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'cat-file -t on wide tree is tree' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	git cat-file -t "$tree" >out &&
	echo "tree" >expected &&
	test_cmp expected out
	)
'

test_done
