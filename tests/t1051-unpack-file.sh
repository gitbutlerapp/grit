#!/bin/sh
# Tests for grit unpack-file.

test_description='grit unpack-file'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo &&
	cd repo
	)
'

test_expect_success 'unpack-file writes blob content to a temp file' '
	(
	cd repo &&
	echo "hello unpack" >src.txt &&
	oid=$(grit hash-object -w src.txt) &&
	tmppath=$(grit unpack-file "$oid") &&
	test -f "$tmppath" &&
	test "$(cat "$tmppath")" = "hello unpack" &&
	rm -f "$tmppath"
	)
'

test_expect_success 'temp file is named .merge_file_*' '
	(
	cd repo &&
	echo "test content" >blob.txt &&
	oid=$(grit hash-object -w blob.txt) &&
	tmppath=$(grit unpack-file "$oid") &&
	case "$tmppath" in
		*/.merge_file_*) : ;;
		*) echo "unexpected temp file name: $tmppath"; false ;;
	esac &&
	rm -f "$tmppath"
	)
'

test_expect_success 'unpack-file fails for unknown OID' '
	(
	cd repo &&
	test_must_fail grit unpack-file 0000000000000000000000000000000000000000
	)
'

test_expect_success 'unpack-file with empty blob' '
	(
	cd repo &&
	oid=$(echo -n "" | grit hash-object -w --stdin) &&
	tmppath=$(grit unpack-file "$oid") &&
	test -f "$tmppath" &&
	test_must_be_empty "$tmppath" &&
	rm -f "$tmppath"
	)
'

test_expect_success 'unpack-file with large blob' '
	(
	cd repo &&
	dd if=/dev/urandom bs=1024 count=100 2>/dev/null | base64 >large.txt &&
	oid=$(grit hash-object -w large.txt) &&
	tmppath=$(grit unpack-file "$oid") &&
	test -f "$tmppath" &&
	test_cmp large.txt "$tmppath" &&
	rm -f "$tmppath"
	)
'

test_expect_success 'unpack-file with binary content' '
	(
	cd repo &&
	printf "\x00\x01\x02\xff" >binary.dat &&
	oid=$(grit hash-object -w binary.dat) &&
	tmppath=$(grit unpack-file "$oid") &&
	test -f "$tmppath" &&
	cmp binary.dat "$tmppath" &&
	rm -f "$tmppath"
	)
'

test_expect_success 'unpack-file creates unique temp files' '
	(
	cd repo &&
	echo "blob A" >a.txt &&
	echo "blob B" >b.txt &&
	oid_a=$(grit hash-object -w a.txt) &&
	oid_b=$(grit hash-object -w b.txt) &&
	tmp_a=$(grit unpack-file "$oid_a") &&
	tmp_b=$(grit unpack-file "$oid_b") &&
	test "$tmp_a" != "$tmp_b" &&
	test "$(cat "$tmp_a")" = "blob A" &&
	test "$(cat "$tmp_b")" = "blob B" &&
	rm -f "$tmp_a" "$tmp_b"
	)
'

test_expect_success 'unpack-file with multiline content preserves lines' '
	(
	cd repo &&
	printf "line1\nline2\nline3\n" >multi.txt &&
	oid=$(grit hash-object -w multi.txt) &&
	tmppath=$(grit unpack-file "$oid") &&
	test_cmp multi.txt "$tmppath" &&
	rm -f "$tmppath"
	)
'

test_expect_success 'unpack-file with abbreviated OID succeeds' '
	(
	cd repo &&
	echo "abbrev test" >abbr.txt &&
	oid=$(grit hash-object -w abbr.txt) &&
	short=$(echo "$oid" | cut -c1-7) &&
	tmppath=$(grit unpack-file "$short") &&
	test -f "$tmppath" &&
	test "$(cat "$tmppath")" = "abbrev test" &&
	rm -f "$tmppath"
	)
'

test_expect_success 'unpack-file with no arguments fails' '
	(
	cd repo &&
	test_must_fail grit unpack-file 2>err
	)
'

test_expect_success 'unpack-file same blob twice gives same content' '
	(
	cd repo &&
	echo "double check" >double.txt &&
	oid=$(grit hash-object -w double.txt) &&
	tmp1=$(grit unpack-file "$oid") &&
	tmp2=$(grit unpack-file "$oid") &&
	test_cmp "$tmp1" "$tmp2" &&
	rm -f "$tmp1" "$tmp2"
	)
'

test_done
