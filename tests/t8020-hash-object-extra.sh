#!/bin/sh
# Extended tests for hash-object: -t types, --stdin-paths, --literally, large files.

test_description='hash-object extra'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "initial" >file.txt &&
	git add file.txt &&
	git commit -m "initial"
	)
'

# ── Basic hash-object ────────────────────────────────────────────────────

test_expect_success 'hash-object produces 40-char hex OID' '
	(
	cd repo &&
	echo "test content" >test.txt &&
	oid=$(git hash-object test.txt) &&
	len=$(printf "%s" "$oid" | wc -c | tr -d " ") &&
	test "$len" = "40" &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'hash-object is deterministic' '
	(
	cd repo &&
	echo "deterministic" >det.txt &&
	oid1=$(git hash-object det.txt) &&
	oid2=$(git hash-object det.txt) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'hash-object --stdin reads from stdin' '
	(
	cd repo &&
	oid1=$(echo "stdin test" | git hash-object --stdin) &&
	echo "stdin test" >stdin.txt &&
	oid2=$(git hash-object stdin.txt) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'hash-object -w writes to object store' '
	(
	cd repo &&
	echo "written" >written.txt &&
	oid=$(git hash-object -w written.txt) &&
	git cat-file -t "$oid" >type &&
	test "$(cat type)" = "blob"
	)
'

test_expect_success 'hash-object without -w does not write' '
	(
	cd repo &&
	echo "nowrite unique content" >nowrite.txt &&
	oid=$(git hash-object nowrite.txt) &&
	test_must_fail git cat-file -t "$oid" 2>err
	)
'

# ── -t types ─────────────────────────────────────────────────────────────

test_expect_success 'hash-object -t blob (default)' '
	(
	cd repo &&
	oid=$(echo "blob content" | git hash-object --stdin) &&
	oid_explicit=$(echo "blob content" | git hash-object -t blob --stdin) &&
	test "$oid" = "$oid_explicit"
	)
'

test_expect_success 'hash-object -t commit --literally' '
	(
	cd repo &&
	oid=$(echo "fake commit" | git hash-object -t commit --stdin --literally) &&
	test -n "$oid" &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'hash-object -t tree --literally' '
	(
	cd repo &&
	oid=$(echo "fake tree" | git hash-object -t tree --stdin --literally) &&
	test -n "$oid" &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'hash-object -t tag --literally' '
	(
	cd repo &&
	oid=$(echo "fake tag" | git hash-object -t tag --stdin --literally) &&
	test -n "$oid" &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'hash-object -t commit -w --literally stores object' '
	(
	cd repo &&
	oid=$(echo "stored commit" | git hash-object -t commit -w --stdin --literally) &&
	type=$(git cat-file -t "$oid") &&
	test "$type" = "commit"
	)
'

test_expect_success 'hash-object -t tree -w --literally stores object' '
	(
	cd repo &&
	oid=$(echo "stored tree" | git hash-object -t tree -w --stdin --literally) &&
	type=$(git cat-file -t "$oid") &&
	test "$type" = "tree"
	)
'

test_expect_success 'hash-object -t tag -w --literally stores object' '
	(
	cd repo &&
	oid=$(echo "stored tag" | git hash-object -t tag -w --stdin --literally) &&
	type=$(git cat-file -t "$oid") &&
	test "$type" = "tag"
	)
'

test_expect_success 'different -t values produce different OIDs for same content' '
	(
	cd repo &&
	blob_oid=$(echo "same" | git hash-object -t blob --stdin --literally) &&
	commit_oid=$(echo "same" | git hash-object -t commit --stdin --literally) &&
	tree_oid=$(echo "same" | git hash-object -t tree --stdin --literally) &&
	tag_oid=$(echo "same" | git hash-object -t tag --stdin --literally) &&
	test "$blob_oid" != "$commit_oid" &&
	test "$commit_oid" != "$tree_oid" &&
	test "$tree_oid" != "$tag_oid"
	)
'

# ── --stdin-paths ────────────────────────────────────────────────────────

test_expect_success 'hash-object --stdin-paths hashes one file' '
	(
	cd repo &&
	echo "path1 content" >path1.txt &&
	oid_direct=$(git hash-object path1.txt) &&
	oid_paths=$(echo "path1.txt" | git hash-object --stdin-paths) &&
	test "$oid_direct" = "$oid_paths"
	)
'

test_expect_success 'hash-object --stdin-paths hashes multiple files' '
	(
	cd repo &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	echo "c" >c.txt &&
	oid_a=$(git hash-object a.txt) &&
	oid_b=$(git hash-object b.txt) &&
	oid_c=$(git hash-object c.txt) &&
	printf "a.txt\nb.txt\nc.txt\n" | git hash-object --stdin-paths >out &&
	test_line_count = 3 out &&
	sed -n 1p out >line1 &&
	sed -n 2p out >line2 &&
	sed -n 3p out >line3 &&
	test "$(cat line1)" = "$oid_a" &&
	test "$(cat line2)" = "$oid_b" &&
	test "$(cat line3)" = "$oid_c"
	)
'

test_expect_success 'hash-object --stdin-paths -w writes all objects' '
	(
	cd repo &&
	echo "write-p1" >wp1.txt &&
	echo "write-p2" >wp2.txt &&
	printf "wp1.txt\nwp2.txt\n" | git hash-object -w --stdin-paths >out &&
	oid1=$(sed -n 1p out) &&
	oid2=$(sed -n 2p out) &&
	git cat-file -t "$oid1" >type1 &&
	git cat-file -t "$oid2" >type2 &&
	test "$(cat type1)" = "blob" &&
	test "$(cat type2)" = "blob"
	)
'

# ── Empty content ────────────────────────────────────────────────────────

test_expect_success 'hash-object of empty file' '
	(
	cd repo &&
	>empty.txt &&
	oid=$(git hash-object empty.txt) &&
	test "$oid" = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
	)
'

test_expect_success 'hash-object --stdin with empty input' '
	(
	cd repo &&
	oid=$(git hash-object --stdin </dev/null) &&
	test "$oid" = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
	)
'

# ── Large files ──────────────────────────────────────────────────────────

test_expect_success 'hash-object on 100KB file' '
	(
	cd repo &&
	dd if=/dev/zero bs=1024 count=100 2>/dev/null >large100k.bin &&
	oid=$(git hash-object large100k.bin) &&
	test -n "$oid" &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'hash-object -w on 100KB file and verify' '
	(
	cd repo &&
	oid=$(git hash-object -w large100k.bin) &&
	size=$(git cat-file -s "$oid") &&
	test "$size" = "102400"
	)
'

test_expect_success 'hash-object on 1MB file' '
	(
	cd repo &&
	dd if=/dev/zero bs=1024 count=1024 2>/dev/null >large1m.bin &&
	oid=$(git hash-object large1m.bin) &&
	test -n "$oid"
	)
'

# ── Multiple file arguments ──────────────────────────────────────────────

test_expect_success 'hash-object with multiple file args' '
	(
	cd repo &&
	echo "f1" >f1.txt &&
	echo "f2" >f2.txt &&
	git hash-object f1.txt f2.txt >out &&
	test_line_count = 2 out
	)
'

test_expect_success 'hash-object -w with multiple file args writes all' '
	(
	cd repo &&
	echo "mw1" >mw1.txt &&
	echo "mw2" >mw2.txt &&
	git hash-object -w mw1.txt mw2.txt >out &&
	oid1=$(sed -n 1p out) &&
	oid2=$(sed -n 2p out) &&
	git cat-file -t "$oid1" >t1 &&
	git cat-file -t "$oid2" >t2 &&
	test "$(cat t1)" = "blob" &&
	test "$(cat t2)" = "blob"
	)
'

# ── Consistency with git ─────────────────────────────────────────────────

test_expect_success 'hash-object matches known SHA1 for "hello\n"' '
	(
	cd repo &&
	oid=$(printf "hello\n" | git hash-object --stdin) &&
	test "$oid" = "ce013625030ba8dba906f756967f9e9ca394464a"
	)
'

test_expect_success 'hash-object matches known SHA1 for empty blob' '
	(
	cd repo &&
	oid=$(git hash-object --stdin </dev/null) &&
	test "$oid" = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
	)
'

# ── Error cases ──────────────────────────────────────────────────────────

test_expect_success 'hash-object fails on nonexistent file' '
	(
	cd repo &&
	test_must_fail git hash-object nonexistent-file.txt 2>err
	)
'

test_done
