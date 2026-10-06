#!/bin/sh
# Test that grit hash-object produces identical SHAs to git for known content,
# covering blobs, type flags, various sizes, and content edge cases.

test_description='grit hash-object produces known/correct SHAs'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test"
	)
'

###########################################################################
# Section 2: Known empty blob
###########################################################################

test_expect_success 'empty blob has well-known SHA' '
	(
	cd repo &&
	oid=$(printf "" | grit hash-object --stdin) &&
	test "$oid" = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
	)
'

test_expect_success 'empty file hashes to empty blob SHA' '
	(
	cd repo &&
	>empty &&
	oid=$(grit hash-object empty) &&
	test "$oid" = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
	)
'

###########################################################################
# Section 3: Known content SHAs match git
###########################################################################

test_expect_success 'hello world blob matches git' '
	(
	cd repo &&
	echo "hello world" >hw.txt &&
	grit_oid=$(grit hash-object hw.txt) &&
	git_oid=$(git hash-object hw.txt) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'single character file matches git' '
	(
	cd repo &&
	printf "a" >single.txt &&
	grit_oid=$(grit hash-object single.txt) &&
	git_oid=$(git hash-object single.txt) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'single newline matches git' '
	(
	cd repo &&
	printf "\n" >newline.txt &&
	grit_oid=$(grit hash-object newline.txt) &&
	git_oid=$(git hash-object newline.txt) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'binary content matches git' '
	(
	cd repo &&
	printf "\x00\x01\x02\xff" >binary.dat &&
	grit_oid=$(grit hash-object binary.dat) &&
	git_oid=$(git hash-object binary.dat) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'null bytes in content match git' '
	(
	cd repo &&
	printf "hello\x00world" >nullbyte.dat &&
	grit_oid=$(grit hash-object nullbyte.dat) &&
	git_oid=$(git hash-object nullbyte.dat) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'large file (64KB) matches git' '
	(
	cd repo &&
	dd if=/dev/urandom bs=1024 count=64 2>/dev/null >large.bin &&
	grit_oid=$(grit hash-object large.bin) &&
	git_oid=$(git hash-object large.bin) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success '1MB file matches git' '
	(
	cd repo &&
	dd if=/dev/urandom bs=1024 count=1024 2>/dev/null >onemb.bin &&
	grit_oid=$(grit hash-object onemb.bin) &&
	git_oid=$(git hash-object onemb.bin) &&
	test "$grit_oid" = "$git_oid"
	)
'

###########################################################################
# Section 4: --stdin content matching
###########################################################################

test_expect_success '--stdin matches git for simple text' '
	(
	cd repo &&
	grit_oid=$(echo "test content" | grit hash-object --stdin) &&
	git_oid=$(echo "test content" | git hash-object --stdin) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success '--stdin matches git for multiline text' '
	(
	cd repo &&
	grit_oid=$(printf "line1\nline2\nline3\n" | grit hash-object --stdin) &&
	git_oid=$(printf "line1\nline2\nline3\n" | git hash-object --stdin) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success '--stdin matches git for empty input' '
	(
	cd repo &&
	grit_oid=$(printf "" | grit hash-object --stdin) &&
	git_oid=$(printf "" | git hash-object --stdin) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success '--stdin matches git for binary data' '
	(
	cd repo &&
	grit_oid=$(dd if=/dev/urandom bs=256 count=1 2>/dev/null | grit hash-object --stdin) &&
	dd if=/dev/urandom bs=256 count=1 2>/dev/null >tmpbin &&
	grit_oid2=$(grit hash-object tmpbin) &&
	git_oid2=$(git hash-object tmpbin) &&
	test "$grit_oid2" = "$git_oid2"
	)
'

###########################################################################
# Section 5: -t type flag
###########################################################################

test_expect_success 'hash-object -t blob matches default' '
	(
	cd repo &&
	echo "typed blob" >typed.txt &&
	oid_default=$(grit hash-object typed.txt) &&
	oid_blob=$(grit hash-object -t blob typed.txt) &&
	test "$oid_default" = "$oid_blob"
	)
'

test_expect_success 'hash-object -t blob --stdin matches git' '
	(
	cd repo &&
	grit_oid=$(echo "typed stdin" | grit hash-object -t blob --stdin) &&
	git_oid=$(echo "typed stdin" | git hash-object -t blob --stdin) &&
	test "$grit_oid" = "$git_oid"
	)
'

###########################################################################
# Section 6: -w writes and SHA is correct
###########################################################################

test_expect_success '-w produces same SHA as without -w' '
	(
	cd repo &&
	echo "write test" >wtest.txt &&
	oid_dry=$(grit hash-object wtest.txt) &&
	oid_write=$(grit hash-object -w wtest.txt) &&
	test "$oid_dry" = "$oid_write"
	)
'

test_expect_success '-w object content matches original' '
	(
	cd repo &&
	echo "verify content" >verify.txt &&
	oid=$(grit hash-object -w verify.txt) &&
	grit cat-file -p "$oid" >actual &&
	test_cmp verify.txt actual
	)
'

test_expect_success '-w --stdin produces same SHA as file' '
	(
	cd repo &&
	echo "stdin write" >stdinw.txt &&
	oid_file=$(grit hash-object -w stdinw.txt) &&
	oid_stdin=$(echo "stdin write" | grit hash-object -w --stdin) &&
	test "$oid_file" = "$oid_stdin"
	)
'

###########################################################################
# Section 7: Repeated hashing is stable
###########################################################################

test_expect_success 'hashing same content twice gives same SHA' '
	(
	cd repo &&
	echo "deterministic" >det.txt &&
	oid1=$(grit hash-object det.txt) &&
	oid2=$(grit hash-object det.txt) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'hashing same content via file and stdin gives same SHA' '
	(
	cd repo &&
	echo "same both ways" >sameway.txt &&
	oid_file=$(grit hash-object sameway.txt) &&
	oid_stdin=$(echo "same both ways" | grit hash-object --stdin) &&
	test "$oid_file" = "$oid_stdin"
	)
'

###########################################################################
# Section 8: Different content gives different SHAs
###########################################################################

test_expect_success 'different content gives different SHAs' '
	(
	cd repo &&
	echo "content A" >ca.txt &&
	echo "content B" >cb.txt &&
	oid_a=$(grit hash-object ca.txt) &&
	oid_b=$(grit hash-object cb.txt) &&
	test "$oid_a" != "$oid_b"
	)
'

test_expect_success 'trailing newline matters for SHA' '
	(
	cd repo &&
	printf "no newline" >nonl.txt &&
	printf "no newline\n" >withnl.txt &&
	oid_no=$(grit hash-object nonl.txt) &&
	oid_yes=$(grit hash-object withnl.txt) &&
	test "$oid_no" != "$oid_yes"
	)
'

###########################################################################
# Section 9: Various content patterns
###########################################################################

test_expect_success 'all-spaces content matches git' '
	(
	cd repo &&
	printf "     " >spaces.txt &&
	grit_oid=$(grit hash-object spaces.txt) &&
	git_oid=$(git hash-object spaces.txt) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'tabs and mixed whitespace matches git' '
	(
	cd repo &&
	printf "a\tb\t\tc\n" >tabs.txt &&
	grit_oid=$(grit hash-object tabs.txt) &&
	git_oid=$(git hash-object tabs.txt) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'very long single line matches git' '
	(
	cd repo &&
	python3 -c "print(\"x\" * 10000)" >longline.txt &&
	grit_oid=$(grit hash-object longline.txt) &&
	git_oid=$(git hash-object longline.txt) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'unicode content matches git' '
	(
	cd repo &&
	printf "héllo wörld 日本語\n" >unicode.txt &&
	grit_oid=$(grit hash-object unicode.txt) &&
	git_oid=$(git hash-object unicode.txt) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'CR LF line endings match git' '
	(
	cd repo &&
	printf "line1\r\nline2\r\n" >crlf.txt &&
	grit_oid=$(grit hash-object crlf.txt) &&
	git_oid=$(git hash-object crlf.txt) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'many empty lines match git' '
	(
	cd repo &&
	printf "\n\n\n\n\n\n\n\n\n\n" >blanks.txt &&
	grit_oid=$(grit hash-object blanks.txt) &&
	git_oid=$(git hash-object blanks.txt) &&
	test "$grit_oid" = "$git_oid"
	)
'

###########################################################################
# Section 10: Multiple files at once
###########################################################################

test_expect_success 'multiple file args each match git individually' '
	(
	cd repo &&
	echo "multi1" >m1.txt &&
	echo "multi2" >m2.txt &&
	echo "multi3" >m3.txt &&
	grit hash-object m1.txt m2.txt m3.txt >grit_out &&
	git hash-object m1.txt m2.txt m3.txt >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'OID format is 40 lowercase hex chars' '
	(
	cd repo &&
	echo "format test" >fmt.txt &&
	oid=$(grit hash-object fmt.txt) &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'hash of file not in repo matches git' '
	(
	cd repo &&
	echo "outside content" >../outside.txt &&
	grit_oid=$(grit hash-object ../outside.txt) &&
	git_oid=$(git hash-object ../outside.txt) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'symlink target content is hashed (matches git)' '
	(
	cd repo &&
	echo "link target" >real.txt &&
	ln -sf real.txt link.txt &&
	grit_oid=$(grit hash-object link.txt) &&
	git_oid=$(git hash-object link.txt) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_done
