#!/bin/sh
# Tests for grit hash-object focusing on blob content and tree interactions.

test_description='grit hash-object blob content and tree interactions'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

###########################################################################
# Section 1: Blob hashing basics
###########################################################################

test_expect_success 'setup: create repository with real git' '
	"$REAL_GIT" init repo &&
	"$REAL_GIT" -C repo config user.name "Test User" &&
	"$REAL_GIT" -C repo config user.email "test@example.com"
'

test_expect_success 'hash-object blob matches real git for simple content' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "hello world" >hw.txt &&
	grit hash-object hw.txt >actual &&
	"$REAL_GIT" hash-object hw.txt >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object blob matches real git for empty file' '
	cd "$TRASH_DIRECTORY/repo" &&
	>empty.txt &&
	grit hash-object empty.txt >actual &&
	"$REAL_GIT" hash-object empty.txt >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object blob matches for binary-like content' '
	cd "$TRASH_DIRECTORY/repo" &&
	printf "\000\001\002\003" >binary.bin &&
	grit hash-object binary.bin >actual &&
	"$REAL_GIT" hash-object binary.bin >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object blob matches for file with only newlines' '
	cd "$TRASH_DIRECTORY/repo" &&
	printf "\n\n\n" >newlines.txt &&
	grit hash-object newlines.txt >actual &&
	"$REAL_GIT" hash-object newlines.txt >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object blob matches for large content' '
	cd "$TRASH_DIRECTORY/repo" &&
	dd if=/dev/urandom bs=1024 count=64 2>/dev/null | base64 >large.txt &&
	grit hash-object large.txt >actual &&
	"$REAL_GIT" hash-object large.txt >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object blob matches for file with trailing spaces' '
	cd "$TRASH_DIRECTORY/repo" &&
	printf "line with trailing spaces   \n" >spaces.txt &&
	grit hash-object spaces.txt >actual &&
	"$REAL_GIT" hash-object spaces.txt >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object blob matches for unicode content' '
	cd "$TRASH_DIRECTORY/repo" &&
	printf "日本語テスト\nÜmlaut\n" >unicode.txt &&
	grit hash-object unicode.txt >actual &&
	"$REAL_GIT" hash-object unicode.txt >expect &&
	test_cmp expect actual
'

###########################################################################
# Section 2: Writing blobs and verifying with cat-file
###########################################################################

test_expect_success 'hash-object -w writes blob retrievable by cat-file' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "write and read" >wr.txt &&
	oid=$(grit hash-object -w wr.txt) &&
	grit cat-file -p "$oid" >actual &&
	test_cmp wr.txt actual
'

test_expect_success 'hash-object -w for empty file produces retrievable blob' '
	cd "$TRASH_DIRECTORY/repo" &&
	>empty_write.txt &&
	oid=$(grit hash-object -w empty_write.txt) &&
	grit cat-file -t "$oid" >actual &&
	echo blob >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object -w blob type confirmed by cat-file -t' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "type check" >tc.txt &&
	oid=$(grit hash-object -w tc.txt) &&
	grit cat-file -t "$oid" >actual &&
	echo blob >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object -w blob size confirmed by cat-file -s' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "size check content" >sc.txt &&
	oid=$(grit hash-object -w sc.txt) &&
	grit cat-file -s "$oid" >actual &&
	wc -c <sc.txt | tr -d " " >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object without -w does not store object' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "no write" >nw.txt &&
	oid=$(grit hash-object nw.txt) &&
	test_must_fail grit cat-file -t "$oid"
'

###########################################################################
# Section 3: Hash-object with --stdin
###########################################################################

test_expect_success 'hash-object --stdin matches file hash' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "stdin test" >st.txt &&
	grit hash-object st.txt >expect &&
	echo "stdin test" | grit hash-object --stdin >actual &&
	test_cmp expect actual
'

test_expect_success 'hash-object --stdin -w writes retrievable blob' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "stdin write" | grit hash-object --stdin -w >oid_file &&
	oid=$(cat oid_file) &&
	grit cat-file -p "$oid" >actual &&
	echo "stdin write" >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object --stdin with empty input' '
	cd "$TRASH_DIRECTORY/repo" &&
	grit hash-object --stdin </dev/null >actual &&
	"$REAL_GIT" hash-object --stdin </dev/null >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object --stdin matches real git for multi-line' '
	cd "$TRASH_DIRECTORY/repo" &&
	printf "line1\nline2\nline3\n" | grit hash-object --stdin >actual &&
	printf "line1\nline2\nline3\n" | "$REAL_GIT" hash-object --stdin >expect &&
	test_cmp expect actual
'

###########################################################################
# Section 4: Blob interaction with trees
###########################################################################

test_expect_success 'blob written by hash-object appears in tree after add' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "for tree" >tree_file.txt &&
	"$REAL_GIT" add tree_file.txt &&
	"$REAL_GIT" commit -m "add tree_file" &&
	blob_oid=$(grit hash-object tree_file.txt) &&
	grit ls-tree HEAD >tree_out &&
	grep "$blob_oid" tree_out
'

test_expect_success 'hash-object of file matches blob in committed tree' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "committed blob" >cb.txt &&
	"$REAL_GIT" add cb.txt &&
	"$REAL_GIT" commit -m "add cb" &&
	expected_oid=$(grit hash-object cb.txt) &&
	tree_oid=$(grit ls-tree HEAD -- cb.txt | awk "{print \$3}") &&
	test "$expected_oid" = "$tree_oid"
'

test_expect_success 'hash-object of modified file differs from tree blob' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "original" >mod.txt &&
	"$REAL_GIT" add mod.txt &&
	"$REAL_GIT" commit -m "add mod" &&
	tree_oid=$(grit ls-tree HEAD -- mod.txt | awk "{print \$3}") &&
	echo "modified" >mod.txt &&
	new_oid=$(grit hash-object mod.txt) &&
	test "$tree_oid" != "$new_oid"
'

test_expect_success 'hash-object of unmodified file matches tree blob' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "unchanged" >unch.txt &&
	"$REAL_GIT" add unch.txt &&
	"$REAL_GIT" commit -m "add unch" &&
	tree_oid=$(grit ls-tree HEAD -- unch.txt | awk "{print \$3}") &&
	file_oid=$(grit hash-object unch.txt) &&
	test "$tree_oid" = "$file_oid"
'

###########################################################################
# Section 5: Multiple blobs and deduplication
###########################################################################

test_expect_success 'identical files produce same OID (content addressable)' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "duplicate content" >dup1.txt &&
	echo "duplicate content" >dup2.txt &&
	oid1=$(grit hash-object dup1.txt) &&
	oid2=$(grit hash-object dup2.txt) &&
	test "$oid1" = "$oid2"
'

test_expect_success 'hash-object -w of same content twice does not error' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "write twice" >wt.txt &&
	grit hash-object -w wt.txt &&
	grit hash-object -w wt.txt
'

test_expect_success 'hash-object multiple files on command line' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "file a" >ma.txt &&
	echo "file b" >mb.txt &&
	grit hash-object ma.txt mb.txt >actual &&
	test $(wc -l <actual) -eq 2
'

test_expect_success 'hash-object multiple files match individual hashes' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "multi a" >mua.txt &&
	echo "multi b" >mub.txt &&
	grit hash-object mua.txt >expect_a &&
	grit hash-object mub.txt >expect_b &&
	grit hash-object mua.txt mub.txt >actual &&
	head -1 actual >actual_a &&
	tail -1 actual >actual_b &&
	test_cmp expect_a actual_a &&
	test_cmp expect_b actual_b
'

###########################################################################
# Section 6: Edge cases
###########################################################################

test_expect_success 'hash-object file with no trailing newline' '
	cd "$TRASH_DIRECTORY/repo" &&
	printf "no newline" >nonl.txt &&
	grit hash-object nonl.txt >actual &&
	"$REAL_GIT" hash-object nonl.txt >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object file with only null bytes' '
	cd "$TRASH_DIRECTORY/repo" &&
	printf "\000\000\000\000" >nulls.bin &&
	grit hash-object nulls.bin >actual &&
	"$REAL_GIT" hash-object nulls.bin >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object file with CR LF line endings' '
	cd "$TRASH_DIRECTORY/repo" &&
	printf "line1\r\nline2\r\n" >crlf.txt &&
	grit hash-object crlf.txt >actual &&
	"$REAL_GIT" hash-object crlf.txt >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object file with mixed line endings' '
	cd "$TRASH_DIRECTORY/repo" &&
	printf "unix\nwindows\r\nold-mac\r" >mixed.txt &&
	grit hash-object mixed.txt >actual &&
	"$REAL_GIT" hash-object mixed.txt >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object very long single line' '
	cd "$TRASH_DIRECTORY/repo" &&
	python3 -c "print(\"x\" * 100000)" >longline.txt &&
	grit hash-object longline.txt >actual &&
	"$REAL_GIT" hash-object longline.txt >expect &&
	test_cmp expect actual
'

test_expect_success 'hash-object nonexistent file fails' '
	cd "$TRASH_DIRECTORY/repo" &&
	test_must_fail grit hash-object does-not-exist.txt
'

test_expect_success 'hash-object -w --stdin blob is findable in ODB' '
	cd "$TRASH_DIRECTORY/repo" &&
	echo "find me in odb" | grit hash-object -w --stdin >oid_file &&
	oid=$(cat oid_file) &&
	grit cat-file -e "$oid"
'

test_done
