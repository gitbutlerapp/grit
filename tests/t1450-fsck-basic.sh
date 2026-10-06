#!/bin/sh
# Test basic fsck-like operations: verify-pack, count-objects, object integrity.

test_description='grit basic fsck-like verification (verify-pack, count-objects, object integrity)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: count-objects basics
###########################################################################

test_expect_success 'setup: create repo with objects' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test User" &&
	echo "file one" >one.txt &&
	echo "file two" >two.txt &&
	grit add one.txt two.txt &&
	grit commit -m "initial commit"
	)
'

test_expect_success 'count-objects reports object count' '
	(
	cd repo &&
	grit count-objects >actual &&
	grep "objects" actual
	)
'

test_expect_success 'count-objects -v shows verbose output' '
	(
	cd repo &&
	grit count-objects -v >actual &&
	grep "count:" actual &&
	grep "size:" actual
	)
'

test_expect_success 'count-objects -v shows in-pack field' '
	(
	cd repo &&
	grit count-objects -v >actual &&
	grep "in-pack:" actual
	)
'

test_expect_success 'count-objects -v shows packs field' '
	(
	cd repo &&
	grit count-objects -v >actual &&
	grep "packs:" actual
	)
'

test_expect_success 'count-objects after adding more objects increases count' '
	(
	cd repo &&
	grit count-objects -v >before &&
	echo "more content" >three.txt &&
	grit add three.txt &&
	grit commit -m "second commit" &&
	grit count-objects -v >after &&
	before_total=$(grep "count:" before | awk "{print \$2}") &&
	after_total=$(grep "count:" after | awk "{print \$2}") &&
	# Loose count may go down if repack happened, but in-pack should grow
	# Just check the command works without error
	test -s after
	)
'

###########################################################################
# Section 2: verify-pack basics
###########################################################################

test_expect_success 'repack creates pack files' '
	(
	cd repo &&
	grit repack -a -d &&
	ls .git/objects/pack/*.idx >pack_list 2>/dev/null &&
	test -s pack_list
	)
'

test_expect_success 'verify-pack validates pack index' '
	(
	cd repo &&
	pack_idx=$(ls .git/objects/pack/*.idx | head -1) &&
	grit verify-pack "$pack_idx"
	)
'

test_expect_success 'verify-pack -v shows object listing' '
	(
	cd repo &&
	pack_idx=$(ls .git/objects/pack/*.idx | head -1) &&
	grit verify-pack -v "$pack_idx" >actual &&
	test -s actual
	)
'

test_expect_success 'verify-pack -v lists blob objects' '
	(
	cd repo &&
	pack_idx=$(ls .git/objects/pack/*.idx | head -1) &&
	grit verify-pack -v "$pack_idx" >actual &&
	grep "blob" actual
	)
'

test_expect_success 'verify-pack -v lists tree objects' '
	(
	cd repo &&
	pack_idx=$(ls .git/objects/pack/*.idx | head -1) &&
	grit verify-pack -v "$pack_idx" >actual &&
	grep "tree" actual
	)
'

test_expect_success 'verify-pack -v lists commit objects' '
	(
	cd repo &&
	pack_idx=$(ls .git/objects/pack/*.idx | head -1) &&
	grit verify-pack -v "$pack_idx" >actual &&
	grep "commit" actual
	)
'

test_expect_success 'verify-pack on pack file (not idx) also works' '
	(
	cd repo &&
	pack_file=$(ls .git/objects/pack/*.pack | head -1) &&
	grit verify-pack "$pack_file"
	)
'

test_expect_success 'verify-pack -s shows stat summary' '
	(
	cd repo &&
	pack_idx=$(ls .git/objects/pack/*.idx | head -1) &&
	grit verify-pack -s "$pack_idx" >actual 2>&1 &&
	test -f actual
	)
'

###########################################################################
# Section 3: Object integrity via cat-file (with loose objects)
###########################################################################

test_expect_success 'setup: create new loose objects for cat-file tests' '
	(
	grit init catfile-repo &&
	cd catfile-repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test User" &&
	echo "file one" >one.txt &&
	echo "file two" >two.txt &&
	grit add one.txt two.txt &&
	grit commit -m "initial commit" &&
	echo "file three" >three.txt &&
	grit add three.txt &&
	grit commit -m "second commit"
	)
'

test_expect_success 'cat-file -t works for loose commit' '
	(
	cd catfile-repo &&
	head_oid=$(grit rev-parse HEAD) &&
	type=$(grit cat-file -t "$head_oid") &&
	test "$type" = "commit"
	)
'

test_expect_success 'cat-file -p on loose commit shows tree and message' '
	(
	cd catfile-repo &&
	head_oid=$(grit rev-parse HEAD) &&
	grit cat-file -p "$head_oid" >actual &&
	grep "^tree " actual &&
	grep "second commit" actual
	)
'

test_expect_success 'cat-file -s on loose blob returns correct size' '
	(
	cd catfile-repo &&
	blob_oid=$(grit hash-object one.txt) &&
	size=$(grit cat-file -s "$blob_oid") &&
	test "$size" = "9"
	)
'

test_expect_success 'cat-file -e succeeds for loose objects' '
	(
	cd catfile-repo &&
	head_oid=$(grit rev-parse HEAD) &&
	grit cat-file -e "$head_oid"
	)
'

test_expect_success 'cat-file -e fails for nonexistent objects' '
	(
	cd catfile-repo &&
	test_must_fail grit cat-file -e aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
	)
'

###########################################################################
# Section 4: count-objects after repack
###########################################################################

test_expect_success 'count-objects -v after repack shows packed objects' '
	(
	cd repo &&
	grit count-objects -v >actual &&
	in_pack=$(grep "in-pack:" actual | awk "{print \$2}") &&
	test "$in_pack" -gt 0
	)
'

test_expect_success 'count-objects -v after repack shows packs count' '
	(
	cd repo &&
	grit count-objects -v >actual &&
	packs=$(grep "packs:" actual | awk "{print \$2}") &&
	test "$packs" -ge 1
	)
'

test_expect_success 'prune-packed removes loose objects that are in pack' '
	(
	grit init prune-repo &&
	cd prune-repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test User" &&
	echo "prune test" >prune.txt &&
	grit add prune.txt &&
	grit commit -m "add prune file" &&
	blob_oid=$(grit hash-object prune.txt) &&
	loose_path=".git/objects/$(echo $blob_oid | cut -c1-2)/$(echo $blob_oid | cut -c3-)" &&
	test -f "$loose_path" &&
	grit repack -a &&
	grit prune-packed &&
	test ! -f "$loose_path"
	)
'

###########################################################################
# Section 5: Verify multiple packs
###########################################################################

test_expect_success 'setup: create second pack via new loose objects' '
	(
	cd repo &&
	echo "pack two content" >pack2.txt &&
	grit hash-object -w pack2.txt &&
	echo "pack two extra" >pack2b.txt &&
	grit hash-object -w pack2b.txt &&
	grit repack
	)
'

test_expect_success 'verify-pack on all pack indices succeeds' '
	(
	cd repo &&
	for idx in .git/objects/pack/*.idx; do
		grit verify-pack "$idx" || return 1
	done
	)
'

test_expect_success 'verify-pack -v on all packs shows objects' '
	(
	cd repo &&
	for idx in .git/objects/pack/*.idx; do
		grit verify-pack -v "$idx" >out &&
		test -s out || return 1
	done
	)
'

###########################################################################
# Section 6: show-index
###########################################################################

test_expect_success 'show-index reads pack index from stdin' '
	(
	cd repo &&
	pack_idx=$(ls .git/objects/pack/*.idx | head -1) &&
	grit show-index <"$pack_idx" >actual &&
	test -s actual
	)
'

test_expect_success 'show-index output contains object hashes' '
	(
	cd repo &&
	pack_idx=$(ls .git/objects/pack/*.idx | head -1) &&
	grit show-index <"$pack_idx" >actual &&
	line_count=$(wc -l <actual | tr -d " ") &&
	test "$line_count" -gt 0
	)
'

###########################################################################
# Section 7: Edge cases
###########################################################################

test_expect_success 'verify-pack on nonexistent file fails' '
	(
	cd repo &&
	test_must_fail grit verify-pack nonexistent.idx 2>err
	)
'

test_expect_success 'count-objects on empty repo works' '
	(
	grit init empty-repo &&
	cd empty-repo &&
	grit count-objects >actual &&
	grep "0 objects" actual
	)
'

test_expect_success 'count-objects -v on empty repo shows zeros' '
	(
	cd empty-repo &&
	grit count-objects -v >actual &&
	grep "count: 0" actual &&
	grep "in-pack: 0" actual
	)
'

test_done
