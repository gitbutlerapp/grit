#!/bin/sh
# Tests for grit hash-object with empty files, binary data, and edge cases.

test_description='grit hash-object: empty files, binary content, stdin, and flags'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com"
	)
'

###########################################################################
# Section 2: Empty file hashing
###########################################################################

test_expect_success 'hash-object of empty file matches git' '
	(
	cd repo &&
	>empty &&
	grit hash-object empty >grit_out &&
	"$REAL_GIT" hash-object empty >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'hash-object -w of empty file creates object' '
	(
	cd repo &&
	>empty &&
	hash=$(grit hash-object -w empty) &&
	grit cat-file -t "$hash" >actual &&
	echo "blob" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object -w empty file content is empty' '
	(
	cd repo &&
	>empty &&
	hash=$(grit hash-object -w empty) &&
	grit cat-file -p "$hash" >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'hash-object empty file produces known SHA' '
	(
	cd repo &&
	>empty &&
	hash=$(grit hash-object empty) &&
	echo "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391" >expect &&
	echo "$hash" >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object --stdin with empty input' '
	(
	cd repo &&
	hash=$(printf "" | grit hash-object --stdin) &&
	echo "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391" >expect &&
	echo "$hash" >actual &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: Binary content
###########################################################################

test_expect_success 'hash-object of binary file matches git' '
	(
	cd repo &&
	printf "\000\001\002\003\377\376" >binary &&
	grit hash-object binary >grit_out &&
	"$REAL_GIT" hash-object binary >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'hash-object -w of binary file roundtrips' '
	(
	cd repo &&
	printf "\000\001\002\003\377\376" >binary &&
	hash=$(grit hash-object -w binary) &&
	grit cat-file -p "$hash" >actual &&
	printf "\000\001\002\003\377\376" >expect &&
	cmp expect actual
	)
'

test_expect_success 'hash-object binary with embedded newlines' '
	(
	cd repo &&
	printf "line1\nline2\n\000middle\nline3" >binmix &&
	grit hash-object binmix >grit_out &&
	"$REAL_GIT" hash-object binmix >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'hash-object binary with NUL bytes via stdin' '
	(
	cd repo &&
	printf "\000\000\000" | grit hash-object --stdin >grit_out &&
	printf "\000\000\000" | "$REAL_GIT" hash-object --stdin >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'hash-object -w binary via stdin creates valid object' '
	(
	cd repo &&
	printf "\000\001\002" | grit hash-object -w --stdin >hash_file &&
	hash=$(cat hash_file) &&
	grit cat-file -t "$hash" >actual &&
	echo "blob" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 4: Large-ish content
###########################################################################

test_expect_success 'hash-object of 1MB file matches git' '
	(
	cd repo &&
	dd if=/dev/urandom bs=1024 count=1024 of=large 2>/dev/null &&
	grit hash-object large >grit_out &&
	"$REAL_GIT" hash-object large >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'hash-object -w of 1MB file roundtrips via cat-file' '
	(
	cd repo &&
	dd if=/dev/urandom bs=1024 count=1024 of=large2 2>/dev/null &&
	hash=$(grit hash-object -w large2) &&
	grit cat-file -p "$hash" >actual &&
	cmp large2 actual
	)
'

###########################################################################
# Section 5: Multiple files
###########################################################################

test_expect_success 'hash-object with multiple files' '
	(
	cd repo &&
	echo "aaa" >a.txt &&
	echo "bbb" >b.txt &&
	echo "ccc" >c.txt &&
	grit hash-object a.txt b.txt c.txt >grit_out &&
	"$REAL_GIT" hash-object a.txt b.txt c.txt >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'hash-object -w with multiple files writes all' '
	(
	cd repo &&
	echo "one" >f1.txt &&
	echo "two" >f2.txt &&
	grit hash-object -w f1.txt f2.txt >hashes &&
	h1=$(sed -n 1p hashes) &&
	h2=$(sed -n 2p hashes) &&
	grit cat-file -t "$h1" >actual1 &&
	grit cat-file -t "$h2" >actual2 &&
	echo "blob" >expect &&
	test_cmp expect actual1 &&
	test_cmp expect actual2
	)
'

###########################################################################
# Section 6: --stdin-paths
###########################################################################

test_expect_success 'hash-object --stdin-paths reads paths from stdin' '
	(
	cd repo &&
	echo "alpha" >alpha.txt &&
	echo "beta" >beta.txt &&
	printf "alpha.txt\nbeta.txt\n" | grit hash-object --stdin-paths >grit_out &&
	printf "alpha.txt\nbeta.txt\n" | "$REAL_GIT" hash-object --stdin-paths >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'hash-object -w --stdin-paths writes objects' '
	(
	cd repo &&
	echo "gamma" >gamma.txt &&
	printf "gamma.txt\n" | grit hash-object -w --stdin-paths >hashes &&
	hash=$(cat hashes) &&
	grit cat-file -p "$hash" >actual &&
	echo "gamma" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 7: Type flag
###########################################################################

test_expect_success 'hash-object -t blob is default' '
	(
	cd repo &&
	echo "data" >typed.txt &&
	h1=$(grit hash-object typed.txt) &&
	h2=$(grit hash-object -t blob typed.txt) &&
	test "$h1" = "$h2"
	)
'

test_expect_success 'hash-object -t blob matches git -t blob' '
	(
	cd repo &&
	echo "content" >tb.txt &&
	grit hash-object -t blob tb.txt >grit_out &&
	"$REAL_GIT" hash-object -t blob tb.txt >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 8: Special content patterns
###########################################################################

test_expect_success 'hash-object file with only newlines' '
	(
	cd repo &&
	printf "\n\n\n" >newlines &&
	grit hash-object newlines >grit_out &&
	"$REAL_GIT" hash-object newlines >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'hash-object file with only spaces' '
	(
	cd repo &&
	printf "   " >spaces &&
	grit hash-object spaces >grit_out &&
	"$REAL_GIT" hash-object spaces >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'hash-object file with trailing newline' '
	(
	cd repo &&
	printf "hello\n" >trailing &&
	grit hash-object trailing >grit_out &&
	"$REAL_GIT" hash-object trailing >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'hash-object file without trailing newline' '
	(
	cd repo &&
	printf "hello" >notrailing &&
	grit hash-object notrailing >grit_out &&
	"$REAL_GIT" hash-object notrailing >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'hash-object file with unicode content' '
	(
	cd repo &&
	printf "héllo wörld 你好" >unicode &&
	grit hash-object unicode >grit_out &&
	"$REAL_GIT" hash-object unicode >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'hash-object file with CRLF line endings' '
	(
	cd repo &&
	printf "line1\r\nline2\r\n" >crlf &&
	grit hash-object crlf >grit_out &&
	"$REAL_GIT" hash-object crlf >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'hash-object very long single line' '
	(
	cd repo &&
	dd if=/dev/zero bs=1 count=10000 2>/dev/null | tr "\000" "A" >longline &&
	grit hash-object longline >grit_out &&
	"$REAL_GIT" hash-object longline >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 9: Error handling
###########################################################################

test_expect_success 'hash-object nonexistent file fails' '
	(
	cd repo &&
	test_must_fail grit hash-object does-not-exist 2>err
	)
'

test_expect_success 'hash-object --stdin with -w stores object' '
	(
	cd repo &&
	echo "stdin stored" | grit hash-object -w --stdin >hash_file &&
	hash=$(cat hash_file) &&
	grit cat-file -p "$hash" >actual &&
	echo "stdin stored" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 10: Idempotency
###########################################################################

test_expect_success 'hash-object is idempotent for same content' '
	(
	cd repo &&
	echo "same" >same1 &&
	echo "same" >same2 &&
	h1=$(grit hash-object same1) &&
	h2=$(grit hash-object same2) &&
	test "$h1" = "$h2"
	)
'

test_expect_success 'hash-object -w same content twice yields same hash' '
	(
	cd repo &&
	echo "duplicate" >dup.txt &&
	h1=$(grit hash-object -w dup.txt) &&
	h2=$(grit hash-object -w dup.txt) &&
	test "$h1" = "$h2"
	)
'

test_expect_success 'hash-object different content yields different hash' '
	(
	cd repo &&
	echo "content A" >diffA &&
	echo "content B" >diffB &&
	hA=$(grit hash-object diffA) &&
	hB=$(grit hash-object diffB) &&
	test "$hA" != "$hB"
	)
'

test_done
