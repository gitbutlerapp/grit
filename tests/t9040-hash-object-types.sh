#!/bin/sh
# Tests for grit hash-object with various object types and edge cases.

test_description='grit hash-object type handling and edge cases'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# Helper: create a repo and cd into it
setup_repo () {
	grit init repo &&
	cd repo
}

###########################################################################
# Section 1: Basic type handling
###########################################################################

test_expect_success 'setup test repository' '
	(
	setup_repo
	)
'

test_expect_success 'hash-object defaults to blob type' '
	(
	cd repo &&
	echo "blob content" >file.txt &&
	oid=$(grit hash-object -w file.txt) &&
	grit cat-file -t "$oid" >actual &&
	echo blob >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object -t blob is explicit default' '
	(
	cd repo &&
	echo "explicit blob" >explicit.txt &&
	oid_default=$(grit hash-object explicit.txt) &&
	oid_typed=$(grit hash-object -t blob explicit.txt) &&
	test "$oid_default" = "$oid_typed"
	)
'

test_expect_success 'hash-object produces consistent OID for same content' '
	(
	cd repo &&
	echo "deterministic" >d1.txt &&
	echo "deterministic" >d2.txt &&
	oid1=$(grit hash-object d1.txt) &&
	oid2=$(grit hash-object d2.txt) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'hash-object produces different OID for different content' '
	(
	cd repo &&
	echo "content A" >a.txt &&
	echo "content B" >b.txt &&
	oid_a=$(grit hash-object a.txt) &&
	oid_b=$(grit hash-object b.txt) &&
	test "$oid_a" != "$oid_b"
	)
'

test_expect_success 'hash-object OID is 40 hex characters' '
	(
	cd repo &&
	echo "length check" >len.txt &&
	oid=$(grit hash-object len.txt) &&
	test $(echo "$oid" | wc -c | tr -d " ") -eq 41 &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

###########################################################################
# Section 2: stdin handling
###########################################################################

test_expect_success 'hash-object --stdin hashes stdin content' '
	(
	cd repo &&
	echo "from stdin" | grit hash-object --stdin >actual_oid &&
	echo "from stdin" >fromstdin.txt &&
	grit hash-object fromstdin.txt >expect_oid &&
	test_cmp expect_oid actual_oid
	)
'

test_expect_success 'hash-object --stdin -w writes to ODB' '
	(
	cd repo &&
	echo "stdin written" | grit hash-object --stdin -w >oid_file &&
	oid=$(cat oid_file) &&
	grit cat-file -e "$oid"
	)
'

test_expect_success 'hash-object -w --stdin also writes (flag order)' '
	(
	cd repo &&
	echo "order test" | grit hash-object -w --stdin >oid_file &&
	oid=$(cat oid_file) &&
	grit cat-file -e "$oid"
	)
'

test_expect_success 'hash-object --stdin with empty input' '
	(
	cd repo &&
	oid=$(printf "" | grit hash-object --stdin) &&
	test "$oid" = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
	)
'

###########################################################################
# Section 3: Write mode
###########################################################################

test_expect_success 'hash-object without -w does not write to ODB' '
	(
	cd repo &&
	echo "no write content" >nw.txt &&
	oid=$(grit hash-object nw.txt) &&
	test_must_fail grit cat-file -e "$oid"
	)
'

test_expect_success 'hash-object -w writes blob to ODB' '
	(
	cd repo &&
	echo "write me" >wr.txt &&
	oid=$(grit hash-object -w wr.txt) &&
	grit cat-file -e "$oid"
	)
'

test_expect_success 'hash-object -w written blob has correct content' '
	(
	cd repo &&
	echo "verify content" >vc.txt &&
	oid=$(grit hash-object -w vc.txt) &&
	grit cat-file -p "$oid" >actual &&
	test_cmp vc.txt actual
	)
'

test_expect_success 'hash-object -w is idempotent' '
	(
	cd repo &&
	echo "idempotent" >idem.txt &&
	oid1=$(grit hash-object -w idem.txt) &&
	oid2=$(grit hash-object -w idem.txt) &&
	test "$oid1" = "$oid2"
	)
'

###########################################################################
# Section 4: Multiple files
###########################################################################

test_expect_success 'hash-object with multiple files' '
	(
	cd repo &&
	echo "file one" >one.txt &&
	echo "file two" >two.txt &&
	grit hash-object one.txt two.txt >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'hash-object -w with multiple files writes all' '
	(
	cd repo &&
	echo "multi write 1" >mw1.txt &&
	echo "multi write 2" >mw2.txt &&
	grit hash-object -w mw1.txt mw2.txt >oids &&
	oid1=$(sed -n 1p oids) &&
	oid2=$(sed -n 2p oids) &&
	grit cat-file -e "$oid1" &&
	grit cat-file -e "$oid2"
	)
'

test_expect_success 'hash-object multiple files produce correct individual OIDs' '
	(
	cd repo &&
	echo "individual A" >indA.txt &&
	echo "individual B" >indB.txt &&
	oid_a=$(grit hash-object indA.txt) &&
	oid_b=$(grit hash-object indB.txt) &&
	grit hash-object indA.txt indB.txt >multi_oids &&
	echo "$oid_a" >expect &&
	echo "$oid_b" >>expect &&
	test_cmp expect multi_oids
	)
'

###########################################################################
# Section 5: stdin-paths
###########################################################################

test_expect_success 'hash-object --stdin-paths reads paths from stdin' '
	(
	cd repo &&
	echo "path file 1" >pf1.txt &&
	echo "path file 2" >pf2.txt &&
	printf "pf1.txt\npf2.txt\n" | grit hash-object --stdin-paths >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'hash-object --stdin-paths -w writes all objects' '
	(
	cd repo &&
	echo "sp write 1" >sp1.txt &&
	echo "sp write 2" >sp2.txt &&
	printf "sp1.txt\nsp2.txt\n" | grit hash-object --stdin-paths -w >oids &&
	oid1=$(sed -n 1p oids) &&
	oid2=$(sed -n 2p oids) &&
	grit cat-file -e "$oid1" &&
	grit cat-file -e "$oid2"
	)
'

test_expect_success 'hash-object --stdin-paths OIDs match direct hashing' '
	(
	cd repo &&
	echo "match content X" >mx.txt &&
	echo "match content Y" >my.txt &&
	direct_x=$(grit hash-object mx.txt) &&
	direct_y=$(grit hash-object my.txt) &&
	printf "mx.txt\nmy.txt\n" | grit hash-object --stdin-paths >stdin_oids &&
	echo "$direct_x" >expect &&
	echo "$direct_y" >>expect &&
	test_cmp expect stdin_oids
	)
'

###########################################################################
# Section 6: Edge cases and special content
###########################################################################

test_expect_success 'hash-object with binary content' '
	(
	cd repo &&
	printf "\000\001\002\003" >binary.dat &&
	oid=$(grit hash-object -w binary.dat) &&
	grit cat-file -p "$oid" >actual &&
	test_cmp binary.dat actual
	)
'

test_expect_success 'hash-object with large content' '
	(
	cd repo &&
	dd if=/dev/zero bs=1024 count=64 2>/dev/null >large.bin &&
	oid=$(grit hash-object -w large.bin) &&
	grit cat-file -s "$oid" >actual_size &&
	echo 65536 >expect_size &&
	test_cmp expect_size actual_size
	)
'

test_expect_success 'hash-object with newline-only content' '
	(
	cd repo &&
	printf "\n\n\n" >newlines.txt &&
	oid=$(grit hash-object -w newlines.txt) &&
	grit cat-file -p "$oid" >actual &&
	test_cmp newlines.txt actual
	)
'

test_expect_success 'hash-object with unicode content' '
	(
	cd repo &&
	printf "héllo wörld 你好" >unicode.txt &&
	oid=$(grit hash-object -w unicode.txt) &&
	grit cat-file -p "$oid" >actual &&
	test_cmp unicode.txt actual
	)
'

test_expect_success 'hash-object --literally with -t blob' '
	(
	cd repo &&
	echo "literally blob" >lit.txt &&
	oid_normal=$(grit hash-object lit.txt) &&
	oid_literal=$(grit hash-object --literally -t blob lit.txt) &&
	test "$oid_normal" = "$oid_literal"
	)
'

test_expect_success 'hash-object rejects nonexistent file' '
	(
	cd repo &&
	test_must_fail grit hash-object nonexistent-file.txt
	)
'

test_expect_success 'hash-object --stdin with filename processes both' '
	(
	cd repo &&
	echo "stdin and file" >sf.txt &&
	echo "stdin and file" | grit hash-object --stdin sf.txt >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'hash-object known SHA1 for empty blob' '
	(
	cd repo &&
	oid=$(printf "" | grit hash-object --stdin) &&
	test "$oid" = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
	)
'

test_done
