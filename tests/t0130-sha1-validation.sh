#!/bin/sh
# Tests for SHA1 validation in various commands

test_description='SHA1 validation in rev-parse, cat-file, update-ref'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup repository with commits' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "content" >file.txt &&
	git add file.txt &&
	git commit -m "initial" &&
	git rev-parse HEAD >../valid_sha
	)
'

# ---------------------------------------------------------------------------
# rev-parse validation
# ---------------------------------------------------------------------------
test_expect_success 'rev-parse accepts valid full SHA1' '
	(
	cd repo &&
	sha=$(cat ../valid_sha) &&
	git rev-parse "$sha" >out &&
	test "$sha" = "$(cat out)"
	)
'

test_expect_success 'rev-parse accepts abbreviated SHA1' '
	(
	cd repo &&
	sha=$(cat ../valid_sha) &&
	abbrev=$(echo "$sha" | cut -c1-7) &&
	git rev-parse "$abbrev" >out &&
	test "$sha" = "$(cat out)"
	)
'

test_expect_success 'rev-parse rejects invalid hex characters' '
	(
	cd repo &&
	test_must_fail git rev-parse "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz" 2>err
	)
'

test_expect_success 'rev-parse rejects too-short SHA1' '
	(
	cd repo &&
	test_must_fail git rev-parse "abc" 2>err
	)
'

test_expect_success 'rev-parse HEAD is valid SHA1 format' '
	(
	cd repo &&
	sha=$(git rev-parse HEAD) &&
	echo "$sha" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'rev-parse HEAD^{tree} returns valid SHA1' '
	(
	cd repo &&
	sha=$(git rev-parse HEAD^{tree}) &&
	echo "$sha" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'rev-parse nonexistent ref fails' '
	(
	cd repo &&
	test_must_fail git rev-parse refs/heads/nonexistent 2>err
	)
'

test_expect_success 'rev-parse HEAD~1 fails on root commit' '
	(
	cd repo &&
	test_must_fail git rev-parse HEAD~1 2>err
	)
'

# ---------------------------------------------------------------------------
# cat-file validation
# ---------------------------------------------------------------------------
test_expect_success 'cat-file -t with valid SHA1' '
	(
	cd repo &&
	sha=$(cat ../valid_sha) &&
	git cat-file -t "$sha" >out &&
	echo "commit" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'cat-file -s with valid SHA1 returns a size' '
	(
	cd repo &&
	sha=$(cat ../valid_sha) &&
	git cat-file -s "$sha" >out &&
	size=$(cat out) &&
	test "$size" -gt 0
	)
'

test_expect_success 'cat-file -e with valid SHA1 succeeds' '
	(
	cd repo &&
	sha=$(cat ../valid_sha) &&
	git cat-file -e "$sha"
	)
'

test_expect_success 'cat-file -e with invalid SHA1 fails' '
	(
	cd repo &&
	test_must_fail git cat-file -e "0000000000000000000000000000000000000000" 2>err
	)
'

test_expect_success 'cat-file -e with malformed SHA1 fails' '
	(
	cd repo &&
	test_must_fail git cat-file -e "not-a-sha" 2>err
	)
'

test_expect_success 'cat-file -t blob returns blob' '
	(
	cd repo &&
	blob_sha=$(git hash-object -w file.txt) &&
	git cat-file -t "$blob_sha" >out &&
	echo "blob" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'cat-file -t tree returns tree' '
	(
	cd repo &&
	tree_sha=$(git rev-parse HEAD^{tree}) &&
	git cat-file -t "$tree_sha" >out &&
	echo "tree" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'cat-file -p with full SHA1 from blob' '
	(
	cd repo &&
	blob_sha=$(git hash-object -w file.txt) &&
	git cat-file -p "$blob_sha" >out &&
	echo "content" >expected &&
	test_cmp expected out
	)
'

# ---------------------------------------------------------------------------
# update-ref validation
# ---------------------------------------------------------------------------
test_expect_success 'update-ref creates a ref with valid SHA1' '
	(
	cd repo &&
	sha=$(git rev-parse HEAD) &&
	git update-ref refs/test/valid "$sha" &&
	stored=$(cat .git/refs/test/valid) &&
	test "$sha" = "$stored"
	)
'

test_expect_success 'update-ref rejects invalid SHA1' '
	(
	cd repo &&
	test_must_fail git update-ref refs/test/bad "not-a-sha" 2>err
	)
'

test_expect_success 'update-ref overwrites existing ref' '
	(
	cd repo &&
	sha=$(git rev-parse HEAD) &&
	echo "new content" >new-file &&
	git add new-file &&
	git commit -m "new commit" &&
	new_sha=$(git rev-parse HEAD) &&
	git update-ref refs/test/overwrite "$sha" &&
	git update-ref refs/test/overwrite "$new_sha" &&
	stored=$(cat .git/refs/test/overwrite) &&
	test "$new_sha" = "$stored"
	)
'

test_expect_success 'update-ref -d deletes a ref' '
	(
	cd repo &&
	sha=$(git rev-parse HEAD) &&
	git update-ref refs/test/to-delete "$sha" &&
	test_path_is_file .git/refs/test/to-delete &&
	git update-ref -d refs/test/to-delete &&
	test_path_is_missing .git/refs/test/to-delete
	)
'

test_expect_success 'update-ref with old-value check succeeds when matching' '
	(
	cd repo &&
	sha=$(git rev-parse HEAD) &&
	git update-ref refs/test/checked "$sha" &&
	git update-ref refs/test/checked "$sha" "$sha"
	)
'

test_expect_success 'update-ref with wrong old-value fails' '
	(
	cd repo &&
	sha=$(git rev-parse HEAD) &&
	git update-ref refs/test/checked2 "$sha" &&
	test_must_fail git update-ref refs/test/checked2 "$sha" "0000000000000000000000000000000000000001" 2>err
	)
'

# ---------------------------------------------------------------------------
# hash-object validation
# ---------------------------------------------------------------------------
test_expect_success 'hash-object produces valid SHA1' '
	(
	cd repo &&
	echo "test content" >test-blob &&
	sha=$(git hash-object test-blob) &&
	echo "$sha" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'hash-object -w stores object retrievable by SHA1' '
	(
	cd repo &&
	echo "stored content" >store-blob &&
	sha=$(git hash-object -w store-blob) &&
	git cat-file -e "$sha"
	)
'

test_expect_success 'hash-object same content same SHA1' '
	(
	cd repo &&
	echo "deterministic" >blob1 &&
	echo "deterministic" >blob2 &&
	sha1=$(git hash-object blob1) &&
	sha2=$(git hash-object blob2) &&
	test "$sha1" = "$sha2"
	)
'

test_expect_success 'hash-object different content different SHA1' '
	(
	cd repo &&
	echo "content A" >blobA &&
	echo "content B" >blobB &&
	sha1=$(git hash-object blobA) &&
	sha2=$(git hash-object blobB) &&
	test "$sha1" != "$sha2"
	)
'

# ---------------------------------------------------------------------------
# Additional rev-parse / symbolic
# ---------------------------------------------------------------------------
test_expect_success 'rev-parse --verify HEAD succeeds' '
	(
	cd repo &&
	git rev-parse --verify HEAD >out &&
	cat out | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'rev-parse --verify nonexistent fails' '
	(
	cd repo &&
	test_must_fail git rev-parse --verify nonexistent 2>err
	)
'

test_expect_success 'symbolic-ref HEAD returns branch ref' '
	(
	cd repo &&
	git symbolic-ref HEAD >out &&
	grep -q "refs/heads/" out
	)
'

test_done
