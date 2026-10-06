#!/bin/sh
# Test hash-object determinism: identical content always produces the same OID,
# regardless of how the content is fed (file, stdin, --stdin-paths, etc.).

test_description='grit hash-object determinism'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# Known SHA-1 for "Hello World" (no newline) as a blob
HELLO_OID='5e1c309dae7f45e0f39b1bf3ac3cd9db12e7d689'

test_expect_success 'setup' '
	(
	grit init repo &&
	cd repo &&
	printf "Hello World" >hello &&
	printf "This is a test\n" >test_file &&
	printf "" >empty_file &&
	dd if=/dev/urandom bs=1024 count=64 2>/dev/null >binary_blob
	)
'

test_expect_success 'hash known content produces expected OID' '
	(
	cd repo &&
	oid=$(grit hash-object hello) &&
	test "$oid" = "$HELLO_OID"
	)
'

test_expect_success 'hash via file and stdin produce same OID' '
	(
	cd repo &&
	oid_file=$(grit hash-object hello) &&
	oid_stdin=$(grit hash-object --stdin <hello) &&
	test "$oid_file" = "$oid_stdin"
	)
'

test_expect_success 'hash via --stdin-paths matches direct file hash' '
	(
	cd repo &&
	oid_file=$(grit hash-object hello) &&
	echo hello | grit hash-object --stdin-paths >actual &&
	echo "$oid_file" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hashing same file twice yields identical OID' '
	(
	cd repo &&
	oid1=$(grit hash-object hello) &&
	oid2=$(grit hash-object hello) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'hashing identical content in different files yields same OID' '
	(
	cd repo &&
	printf "Hello World" >hello_copy &&
	oid1=$(grit hash-object hello) &&
	oid2=$(grit hash-object hello_copy) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'hashing different content yields different OIDs' '
	(
	cd repo &&
	oid1=$(grit hash-object hello) &&
	oid2=$(grit hash-object test_file) &&
	test "$oid1" != "$oid2"
	)
'

test_expect_success 'hash-object -w writes object, cat-file retrieves it' '
	(
	cd repo &&
	oid=$(grit hash-object -w hello) &&
	grit cat-file -e "$oid" &&
	grit cat-file -t "$oid" >actual_type &&
	echo blob >expect_type &&
	test_cmp expect_type actual_type
	)
'

test_expect_success 'hash-object -w then hash-object (no -w) same OID' '
	(
	cd repo &&
	oid_w=$(grit hash-object -w hello) &&
	oid_nw=$(grit hash-object hello) &&
	test "$oid_w" = "$oid_nw"
	)
'

test_expect_success 'hash empty file produces consistent OID' '
	(
	cd repo &&
	oid1=$(grit hash-object empty_file) &&
	oid2=$(printf "" | grit hash-object --stdin) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'empty file OID is the well-known empty blob' '
	(
	cd repo &&
	oid=$(grit hash-object empty_file) &&
	test "$oid" = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
	)
'

test_expect_success 'hash binary content is deterministic' '
	(
	cd repo &&
	oid1=$(grit hash-object binary_blob) &&
	oid2=$(grit hash-object binary_blob) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'binary content: file vs stdin produces same OID' '
	(
	cd repo &&
	oid_file=$(grit hash-object binary_blob) &&
	oid_stdin=$(grit hash-object --stdin <binary_blob) &&
	test "$oid_file" = "$oid_stdin"
	)
'

test_expect_success 'hash-object -w idempotent on repeated writes' '
	(
	cd repo &&
	oid1=$(grit hash-object -w hello) &&
	oid2=$(grit hash-object -w hello) &&
	test "$oid1" = "$oid2" &&
	grit cat-file -e "$oid1"
	)
'

test_expect_success 'hash-object with -t blob (explicit) matches default' '
	(
	cd repo &&
	oid_default=$(grit hash-object hello) &&
	oid_explicit=$(grit hash-object -t blob hello) &&
	test "$oid_default" = "$oid_explicit"
	)
'

test_expect_success 'hash-object multiple files via --stdin-paths' '
	(
	cd repo &&
	oid_hello=$(grit hash-object hello) &&
	oid_test=$(grit hash-object test_file) &&
	printf "hello\ntest_file\n" | grit hash-object --stdin-paths >actual &&
	printf "%s\n%s\n" "$oid_hello" "$oid_test" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object multiple files on command line' '
	(
	cd repo &&
	oid_hello=$(grit hash-object hello) &&
	oid_test=$(grit hash-object test_file) &&
	grit hash-object hello test_file >actual &&
	printf "%s\n%s\n" "$oid_hello" "$oid_test" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object -w multiple files writes all' '
	(
	cd repo &&
	grit hash-object -w hello test_file >oids &&
	oid1=$(sed -n 1p oids) &&
	oid2=$(sed -n 2p oids) &&
	grit cat-file -e "$oid1" &&
	grit cat-file -e "$oid2"
	)
'

test_expect_success 'content round-trip: hash-object -w then cat-file -p' '
	(
	cd repo &&
	oid=$(grit hash-object -w hello) &&
	grit cat-file -p "$oid" >actual &&
	test_cmp hello actual
	)
'

test_expect_success 'content round-trip for file with newline' '
	(
	cd repo &&
	oid=$(grit hash-object -w test_file) &&
	grit cat-file -p "$oid" >actual &&
	test_cmp test_file actual
	)
'

test_expect_success 'content round-trip for empty file' '
	(
	cd repo &&
	oid=$(grit hash-object -w empty_file) &&
	grit cat-file -p "$oid" >actual &&
	test_cmp empty_file actual
	)
'

test_expect_success 'content round-trip for binary file' '
	(
	cd repo &&
	oid=$(grit hash-object -w binary_blob) &&
	grit cat-file -p "$oid" >actual &&
	test_cmp binary_blob actual
	)
'

test_expect_success 'hash-object OID is exactly 40 hex chars' '
	(
	cd repo &&
	oid=$(grit hash-object hello) &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'hash one-byte file is deterministic' '
	(
	cd repo &&
	printf "x" >onebyte &&
	oid1=$(grit hash-object onebyte) &&
	oid2=$(printf "x" | grit hash-object --stdin) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'hash file with only newlines' '
	(
	cd repo &&
	printf "\n\n\n" >newlines &&
	oid1=$(grit hash-object newlines) &&
	oid2=$(printf "\n\n\n" | grit hash-object --stdin) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'hash file with trailing whitespace is deterministic' '
	(
	cd repo &&
	printf "hello   \n" >trailing &&
	oid1=$(grit hash-object trailing) &&
	oid2=$(grit hash-object trailing) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'hash large file is deterministic via file and stdin' '
	(
	cd repo &&
	oid_file=$(grit hash-object binary_blob) &&
	oid_stdin=$(cat binary_blob | grit hash-object --stdin) &&
	test "$oid_file" = "$oid_stdin"
	)
'

test_expect_success 'hash-object -w followed by second init preserves objects' '
	(
	cd repo &&
	oid=$(grit hash-object -w hello) &&
	grit cat-file -e "$oid"
	)
'

test_expect_success 'hash-object --literally with -t blob matches default' '
	(
	cd repo &&
	oid1=$(grit hash-object hello) &&
	oid2=$(grit hash-object --literally -t blob hello) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'different type flag changes OID' '
	(
	cd repo &&
	oid_blob=$(grit hash-object -t blob hello) &&
	oid_commit=$(grit hash-object --literally -t commit hello) &&
	test "$oid_blob" != "$oid_commit"
	)
'

test_expect_success 'hash-object size is reported correctly by cat-file -s' '
	(
	cd repo &&
	oid=$(grit hash-object -w hello) &&
	size=$(grit cat-file -s "$oid") &&
	expected=$(wc -c <hello | tr -d " ") &&
	test "$size" = "$expected"
	)
'

test_expect_success 'hash-object with --stdin reads all of stdin' '
	(
	cd repo &&
	seq 1 1000 >bigstdin &&
	oid1=$(grit hash-object bigstdin) &&
	oid2=$(grit hash-object --stdin <bigstdin) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'hash-object --stdin with file arg uses stdin not file' '
	(
	cd repo &&
	printf "different content" >other &&
	oid_stdin=$(printf "Hello World" | grit hash-object --stdin) &&
	oid_other=$(grit hash-object other) &&
	test "$oid_stdin" != "$oid_other"
	)
'

test_expect_success 'hash-object rejects --stdin combined with --stdin-paths' '
	(
	cd repo &&
	echo hello | test_must_fail grit hash-object --stdin --stdin-paths 2>err
	)
'

test_done
