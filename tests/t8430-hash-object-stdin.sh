#!/bin/sh
# Tests for hash-object --stdin, --stdin-paths, piped input, and
# combinations with -w and -t.

test_description='hash-object --stdin, --stdin-paths, piped input'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

SYS_GIT=$(command -v git)

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup: repository' '
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

# ── --stdin basics ───────────────────────────────────────────────────────

test_expect_success 'hash-object --stdin: produces 40-char hex' '
	(
	cd repo &&
	oid=$(echo "test" | git hash-object --stdin) &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'hash-object --stdin: deterministic' '
	(
	cd repo &&
	oid1=$(echo "same content" | git hash-object --stdin) &&
	oid2=$(echo "same content" | git hash-object --stdin) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'hash-object --stdin: different content gives different OID' '
	(
	cd repo &&
	oid1=$(echo "content A" | git hash-object --stdin) &&
	oid2=$(echo "content B" | git hash-object --stdin) &&
	test "$oid1" != "$oid2"
	)
'

test_expect_success 'hash-object --stdin: matches file hash' '
	(
	cd repo &&
	echo "file vs stdin" >compare.txt &&
	oid_file=$(git hash-object compare.txt) &&
	oid_stdin=$(echo "file vs stdin" | git hash-object --stdin) &&
	test "$oid_file" = "$oid_stdin"
	)
'

test_expect_success 'hash-object --stdin: empty input gives empty blob OID' '
	(
	cd repo &&
	oid=$(git hash-object --stdin </dev/null) &&
	test "$oid" = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
	)
'

test_expect_success 'hash-object --stdin: single newline' '
	(
	cd repo &&
	oid=$(printf "\n" | git hash-object --stdin) &&
	test -n "$oid" &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'hash-object --stdin: multiline content' '
	(
	cd repo &&
	oid=$(printf "line1\nline2\nline3\n" | git hash-object --stdin) &&
	test -n "$oid"
	)
'

test_expect_success 'hash-object --stdin: binary-ish content' '
	(
	cd repo &&
	oid=$(printf "\x00\x01\x02\x03" | git hash-object --stdin) &&
	test -n "$oid" &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

# ── --stdin -w (write) ──────────────────────────────────────────────────

test_expect_success 'hash-object --stdin -w: writes to object store' '
	(
	cd repo &&
	oid=$(echo "written via stdin" | git hash-object -w --stdin) &&
	git cat-file -t "$oid" >../ho_type &&
	test "$(cat ../ho_type)" = "blob"
	)
'

test_expect_success 'hash-object --stdin -w: content is retrievable' '
	(
	cd repo &&
	oid=$(echo "retrieve me" | git hash-object -w --stdin) &&
	content=$(git cat-file -p "$oid") &&
	test "$content" = "retrieve me"
	)
'

test_expect_success 'hash-object --stdin without -w: does not write' '
	(
	cd repo &&
	oid=$(echo "do not write me" | git hash-object --stdin) &&
	test_must_fail git cat-file -t "$oid"
	)
'

# ── --stdin-paths ────────────────────────────────────────────────────────

test_expect_success 'hash-object --stdin-paths: single file' '
	(
	cd repo &&
	echo "path content" >pathfile.txt &&
	oid_direct=$(git hash-object pathfile.txt) &&
	oid_paths=$(echo "pathfile.txt" | git hash-object --stdin-paths) &&
	test "$oid_direct" = "$oid_paths"
	)
'

test_expect_success 'hash-object --stdin-paths: multiple files' '
	(
	cd repo &&
	echo "aaa" >p1.txt &&
	echo "bbb" >p2.txt &&
	echo "ccc" >p3.txt &&
	oid1=$(git hash-object p1.txt) &&
	oid2=$(git hash-object p2.txt) &&
	oid3=$(git hash-object p3.txt) &&
	printf "p1.txt\np2.txt\np3.txt\n" | git hash-object --stdin-paths >../ho_out &&
	test_line_count = 3 ../ho_out &&
	sed -n 1p ../ho_out >../l1 &&
	sed -n 2p ../ho_out >../l2 &&
	sed -n 3p ../ho_out >../l3 &&
	test "$(cat ../l1)" = "$oid1" &&
	test "$(cat ../l2)" = "$oid2" &&
	test "$(cat ../l3)" = "$oid3"
	)
'

test_expect_success 'hash-object --stdin-paths -w: writes all objects' '
	(
	cd repo &&
	echo "wp1" >wp1.txt &&
	echo "wp2" >wp2.txt &&
	printf "wp1.txt\nwp2.txt\n" | git hash-object -w --stdin-paths >../ho_out &&
	oid1=$(sed -n 1p ../ho_out) &&
	oid2=$(sed -n 2p ../ho_out) &&
	git cat-file -t "$oid1" >../t1 &&
	git cat-file -t "$oid2" >../t2 &&
	test "$(cat ../t1)" = "blob" &&
	test "$(cat ../t2)" = "blob"
	)
'

test_expect_success 'hash-object --stdin-paths: file in subdirectory' '
	(
	cd repo &&
	mkdir -p subdir &&
	echo "sub content" >subdir/subfile.txt &&
	oid_direct=$(git hash-object subdir/subfile.txt) &&
	oid_paths=$(echo "subdir/subfile.txt" | git hash-object --stdin-paths) &&
	test "$oid_direct" = "$oid_paths"
	)
'

# ── Piped input (file argument via pipe redirection) ─────────────────────

test_expect_success 'hash-object with file arg: same as --stdin pipe' '
	(
	cd repo &&
	echo "pipe test" >pipe.txt &&
	oid_arg=$(git hash-object pipe.txt) &&
	oid_pipe=$(cat pipe.txt | git hash-object --stdin) &&
	test "$oid_arg" = "$oid_pipe"
	)
'

test_expect_success 'hash-object: multiple file args produce multiple OIDs' '
	(
	cd repo &&
	echo "m1" >m1.txt &&
	echo "m2" >m2.txt &&
	git hash-object m1.txt m2.txt >../ho_out &&
	test_line_count = 2 ../ho_out
	)
'

test_expect_success 'hash-object -w: multiple file args write all' '
	(
	cd repo &&
	echo "mw1" >mw1.txt &&
	echo "mw2" >mw2.txt &&
	git hash-object -w mw1.txt mw2.txt >../ho_out &&
	oid1=$(sed -n 1p ../ho_out) &&
	oid2=$(sed -n 2p ../ho_out) &&
	git cat-file -e "$oid1" &&
	git cat-file -e "$oid2"
	)
'

# ── Known SHA1 values ────────────────────────────────────────────────────

test_expect_success 'hash-object --stdin: known SHA1 for "hello\\n"' '
	(
	cd repo &&
	oid=$(printf "hello\n" | git hash-object --stdin) &&
	test "$oid" = "ce013625030ba8dba906f756967f9e9ca394464a"
	)
'

test_expect_success 'hash-object --stdin: known SHA1 for empty' '
	(
	cd repo &&
	oid=$(git hash-object --stdin </dev/null) &&
	test "$oid" = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
	)
'

# ── -t with --stdin ──────────────────────────────────────────────────────

test_expect_success 'hash-object -t blob --stdin: same as default' '
	(
	cd repo &&
	oid1=$(echo "typed" | git hash-object --stdin) &&
	oid2=$(echo "typed" | git hash-object -t blob --stdin) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'hash-object -t commit --stdin --literally: different OID' '
	(
	cd repo &&
	blob_oid=$(echo "data" | git hash-object -t blob --stdin --literally) &&
	commit_oid=$(echo "data" | git hash-object -t commit --stdin --literally) &&
	test "$blob_oid" != "$commit_oid"
	)
'

test_expect_success 'hash-object -t tree -w --stdin --literally: stores as tree' '
	(
	cd repo &&
	oid=$(echo "fake tree data" | git hash-object -t tree -w --stdin --literally) &&
	type=$(git cat-file -t "$oid") &&
	test "$type" = "tree"
	)
'

# ── Large stdin content ──────────────────────────────────────────────────

test_expect_success 'hash-object --stdin: large content (100KB)' '
	(
	cd repo &&
	dd if=/dev/zero bs=1024 count=100 2>/dev/null | git hash-object --stdin >../ho_out &&
	oid=$(cat ../ho_out) &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'hash-object --stdin -w: large content stored correctly' '
	(
	cd repo &&
	dd if=/dev/zero bs=1024 count=100 2>/dev/null | git hash-object -w --stdin >../ho_out &&
	oid=$(cat ../ho_out) &&
	size=$(git cat-file -s "$oid") &&
	test "$size" = "102400"
	)
'

# ── Error cases ──────────────────────────────────────────────────────────

test_expect_success 'hash-object: nonexistent file fails' '
	(
	cd repo &&
	test_must_fail git hash-object nosuchfile.txt 2>../ho_err
	)
'

test_expect_success 'hash-object --stdin-paths: nonexistent file in list fails' '
	(
	cd repo &&
	echo "nosuchfile.txt" | test_must_fail git hash-object --stdin-paths 2>../ho_err
	)
'

test_done
