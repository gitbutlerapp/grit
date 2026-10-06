#!/bin/sh
# Tests for grit hash-object -w (write to object database), --stdin,
# --stdin-paths, and --literally flag behavior.

test_description='grit hash-object -w write and stdin modes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo
	)
'

###########################################################################
# Section 2: Basic -w (write to ODB)
###########################################################################

test_expect_success 'hash-object -w writes blob to object database' '
	(
	cd repo &&
	echo "write me" >write.txt &&
	oid=$(grit hash-object -w write.txt) &&
	grit cat-file -e "$oid"
	)
'

test_expect_success 'hash-object without -w does NOT write to ODB' '
	(
	cd repo &&
	echo "no write" >nw.txt &&
	oid=$(grit hash-object nw.txt) &&
	test_must_fail grit cat-file -e "$oid"
	)
'

test_expect_success 'hash-object -w returns same OID as without -w' '
	(
	cd repo &&
	echo "same oid" >same.txt &&
	oid_dry=$(grit hash-object same.txt) &&
	oid_write=$(grit hash-object -w same.txt) &&
	test "$oid_dry" = "$oid_write"
	)
'

test_expect_success 'hash-object -w written blob has correct content' '
	(
	cd repo &&
	echo "verify content" >verify.txt &&
	oid=$(grit hash-object -w verify.txt) &&
	grit cat-file -p "$oid" >actual &&
	echo "verify content" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object -w written blob has type blob' '
	(
	cd repo &&
	echo "type check" >tc.txt &&
	oid=$(grit hash-object -w tc.txt) &&
	grit cat-file -t "$oid" >actual &&
	echo blob >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object -w written blob has correct size' '
	(
	cd repo &&
	echo "size check" >sz.txt &&
	oid=$(grit hash-object -w sz.txt) &&
	grit cat-file -s "$oid" >actual &&
	echo 11 >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: --stdin (read object data from stdin)
###########################################################################

test_expect_success 'hash-object --stdin hashes data from stdin' '
	(
	cd repo &&
	echo "stdin data" | grit hash-object --stdin >oid_stdin &&
	echo "stdin data" >file_stdin.txt &&
	grit hash-object file_stdin.txt >oid_file &&
	test_cmp oid_stdin oid_file
	)
'

test_expect_success 'hash-object -w --stdin writes to ODB' '
	(
	cd repo &&
	echo "write stdin" | grit hash-object -w --stdin >oid &&
	oid=$(cat oid) &&
	grit cat-file -e "$oid"
	)
'

test_expect_success 'hash-object -w --stdin content is retrievable' '
	(
	cd repo &&
	echo "readable stdin" | grit hash-object -w --stdin >oid_file &&
	oid=$(cat oid_file) &&
	grit cat-file -p "$oid" >actual &&
	echo "readable stdin" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object --stdin with empty input' '
	(
	cd repo &&
	oid=$(printf "" | grit hash-object --stdin) &&
	test -n "$oid" &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'hash-object -w --stdin empty blob is valid' '
	(
	cd repo &&
	oid=$(printf "" | grit hash-object -w --stdin) &&
	grit cat-file -e "$oid" &&
	grit cat-file -s "$oid" >actual &&
	echo 0 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object --stdin with multi-line content' '
	(
	cd repo &&
	printf "line1\nline2\nline3\n" | grit hash-object -w --stdin >oid_file &&
	oid=$(cat oid_file) &&
	grit cat-file -p "$oid" >actual &&
	printf "line1\nline2\nline3\n" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object --stdin with binary content' '
	(
	cd repo &&
	printf "\000\001\002\377" | grit hash-object -w --stdin >oid_file &&
	oid=$(cat oid_file) &&
	grit cat-file -e "$oid"
	)
'

###########################################################################
# Section 4: --stdin-paths (file paths from stdin)
###########################################################################

test_expect_success 'hash-object --stdin-paths hashes one file' '
	(
	cd repo &&
	echo "alpha" >alpha.txt &&
	echo "alpha.txt" | grit hash-object --stdin-paths >actual &&
	grit hash-object alpha.txt >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object --stdin-paths hashes multiple files' '
	(
	cd repo &&
	echo "one" >one.txt &&
	echo "two" >two.txt &&
	echo "three" >three.txt &&
	printf "one.txt\ntwo.txt\nthree.txt\n" | grit hash-object --stdin-paths >actual &&
	{
		grit hash-object one.txt &&
		grit hash-object two.txt &&
		grit hash-object three.txt
	} >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object -w --stdin-paths writes all objects' '
	(
	cd repo &&
	echo "sp1" >sp1.txt &&
	echo "sp2" >sp2.txt &&
	printf "sp1.txt\nsp2.txt\n" | grit hash-object -w --stdin-paths >oids &&
	oid1=$(sed -n 1p oids) &&
	oid2=$(sed -n 2p oids) &&
	grit cat-file -e "$oid1" &&
	grit cat-file -e "$oid2"
	)
'

test_expect_success 'hash-object --stdin-paths preserves order' '
	(
	cd repo &&
	echo "z" >z.txt &&
	echo "a" >a.txt &&
	printf "z.txt\na.txt\n" | grit hash-object --stdin-paths >actual &&
	{
		grit hash-object z.txt &&
		grit hash-object a.txt
	} >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 5: Multiple files as arguments
###########################################################################

test_expect_success 'hash-object with multiple file args' '
	(
	cd repo &&
	echo "multi1" >m1.txt &&
	echo "multi2" >m2.txt &&
	grit hash-object m1.txt m2.txt >actual &&
	{
		grit hash-object m1.txt &&
		grit hash-object m2.txt
	} >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object -w with multiple file args writes all' '
	(
	cd repo &&
	echo "wm1" >wm1.txt &&
	echo "wm2" >wm2.txt &&
	grit hash-object -w wm1.txt wm2.txt >oids &&
	oid1=$(sed -n 1p oids) &&
	oid2=$(sed -n 2p oids) &&
	grit cat-file -e "$oid1" &&
	grit cat-file -e "$oid2"
	)
'

###########################################################################
# Section 6: hash-object -w matches real git
###########################################################################

test_expect_success 'hash-object -w OID matches real git' '
	(
	cd repo &&
	echo "cross check" >cross.txt &&
	oid_grit=$(grit hash-object cross.txt) &&
	oid_git=$($REAL_GIT hash-object cross.txt) &&
	test "$oid_grit" = "$oid_git"
	)
'

test_expect_success 'hash-object -w --stdin OID matches real git' '
	(
	cd repo &&
	echo "stdin cross" | grit hash-object --stdin >grit_oid &&
	echo "stdin cross" | $REAL_GIT hash-object --stdin >git_oid &&
	test_cmp grit_oid git_oid
	)
'

###########################################################################
# Section 7: Edge cases
###########################################################################

test_expect_success 'hash-object -w large file' '
	(
	cd repo &&
	dd if=/dev/urandom bs=1024 count=128 2>/dev/null >large.bin &&
	oid=$(grit hash-object -w large.bin) &&
	grit cat-file -e "$oid" &&
	grit cat-file -s "$oid" >actual_sz &&
	wc -c <large.bin | tr -d " " >expect_sz &&
	test_cmp expect_sz actual_sz
	)
'

test_expect_success 'hash-object -w file with spaces in name' '
	(
	cd repo &&
	echo "space" >"file with spaces.txt" &&
	oid=$(grit hash-object -w "file with spaces.txt") &&
	grit cat-file -p "$oid" >actual &&
	echo "space" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'hash-object -w same content twice returns same OID' '
	(
	cd repo &&
	echo "dedup" >dup1.txt &&
	echo "dedup" >dup2.txt &&
	oid1=$(grit hash-object -w dup1.txt) &&
	oid2=$(grit hash-object -w dup2.txt) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'hash-object nonexistent file fails' '
	(
	cd repo &&
	test_must_fail grit hash-object nonexistent-file.txt
	)
'

test_expect_success 'hash-object -w with -t blob explicit type' '
	(
	cd repo &&
	echo "typed blob" >typed.txt &&
	oid=$(grit hash-object -w -t blob typed.txt) &&
	grit cat-file -t "$oid" >actual &&
	echo blob >expect &&
	test_cmp expect actual
	)
'

test_done
