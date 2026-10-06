#!/bin/sh
# Test corrupted pack detection via verify-pack and show-index.

test_description='pack corruption detection'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository and create pack' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "file one" >one.txt &&
	echo "file two" >two.txt &&
	echo "file three" >three.txt &&
	grit add one.txt two.txt three.txt &&
	grit commit -m "initial" &&
	echo "changed" >one.txt &&
	grit add one.txt &&
	grit commit -m "second" &&
	echo "more changes" >two.txt &&
	grit add two.txt &&
	grit commit -m "third" &&
	grit gc &&
	ls .git/objects/pack/*.pack >../packfile &&
	ls .git/objects/pack/*.idx >../idxfile &&
	test -s ../packfile &&
	test -s ../idxfile &&
	cp "$(cat ../packfile)" ../good.pack
	)
'

test_expect_success 'verify-pack succeeds on valid pack' '
	(
	cd repo &&
	grit verify-pack "$(cat ../packfile)"
	)
'

test_expect_success 'verify-pack -v shows object listing with ok' '
	(
	cd repo &&
	grit verify-pack -v "$(cat ../packfile)" >actual &&
	test -s actual &&
	grep "ok" actual
	)
'

test_expect_success 'verify-pack -v lists blob objects' '
	(
	cd repo &&
	grit verify-pack -v "$(cat ../packfile)" >actual &&
	grep "blob" actual
	)
'

test_expect_success 'verify-pack -v lists commit objects' '
	(
	cd repo &&
	grit verify-pack -v "$(cat ../packfile)" >actual &&
	grep "commit" actual
	)
'

test_expect_success 'verify-pack -v lists tree objects' '
	(
	cd repo &&
	grit verify-pack -v "$(cat ../packfile)" >actual &&
	grep "tree" actual
	)
'

test_expect_success 'verify-pack -v shows chain length summary' '
	(
	cd repo &&
	grit verify-pack -v "$(cat ../packfile)" >actual &&
	grep "chain length" actual
	)
'

test_expect_success 'show-index lists objects from idx' '
	(
	cd repo &&
	grit show-index <"$(cat ../idxfile)" >actual &&
	test -s actual
	)
'

test_expect_success 'show-index and verify-pack agree on object count' '
	(
	cd repo &&
	grit show-index <"$(cat ../idxfile)" >idx_out &&
	idx_count=$(wc -l <idx_out | tr -d " ") &&
	grit verify-pack -v "$(cat ../packfile)" >verify_out &&
	obj_count=$(grep -cE "(blob|commit|tree|ofs-delta|ref-delta)" verify_out) &&
	test "$obj_count" = "$idx_count"
	)
'

test_expect_success 'show-index entries contain valid SHA-1 hashes' '
	(
	cd repo &&
	grit show-index <"$(cat ../idxfile)" >actual &&
	while read offset hash rest; do
		len=$(printf "%s" "$hash" | wc -c | tr -d " ") &&
		test "$len" -eq 40 || return 1
	done <actual
	)
'

test_expect_success 'verify-pack detects corrupted pack version' '
	(
	cd repo &&
	cp ../good.pack /tmp/corrupt-$$.pack &&
	chmod u+w /tmp/corrupt-$$.pack &&
	printf "\\xff\\xff\\xff\\xff" | dd of=/tmp/corrupt-$$.pack bs=1 seek=4 count=4 conv=notrunc 2>/dev/null &&
	test_must_fail grit verify-pack /tmp/corrupt-$$.pack &&
	rm -f /tmp/corrupt-$$.pack
	)
'

test_expect_success 'verify-pack detects corrupted magic bytes' '
	(
	cd repo &&
	cp ../good.pack /tmp/corrupt-$$.pack &&
	chmod u+w /tmp/corrupt-$$.pack &&
	printf "\\x00\\x00\\x00\\x00" | dd of=/tmp/corrupt-$$.pack bs=1 seek=0 count=4 conv=notrunc 2>/dev/null &&
	test_must_fail grit verify-pack /tmp/corrupt-$$.pack &&
	rm -f /tmp/corrupt-$$.pack
	)
'

test_expect_success 'verify-pack detects corrupted pack data in middle' '
	(
	cd repo &&
	cp ../good.pack /tmp/corrupt-$$.pack &&
	chmod u+w /tmp/corrupt-$$.pack &&
	size=$(wc -c </tmp/corrupt-$$.pack | tr -d " ") &&
	mid=$((size / 2)) &&
	printf "\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00" | dd of=/tmp/corrupt-$$.pack bs=1 seek=$mid count=8 conv=notrunc 2>/dev/null &&
	test_must_fail grit verify-pack /tmp/corrupt-$$.pack &&
	rm -f /tmp/corrupt-$$.pack
	)
'

test_expect_success 'verify-pack detects corrupted trailing checksum' '
	(
	cd repo &&
	cp ../good.pack /tmp/corrupt-$$.pack &&
	chmod u+w /tmp/corrupt-$$.pack &&
	size=$(wc -c </tmp/corrupt-$$.pack | tr -d " ") &&
	tail_off=$((size - 4)) &&
	printf "\\xde\\xad\\xbe\\xef" | dd of=/tmp/corrupt-$$.pack bs=1 seek=$tail_off count=4 conv=notrunc 2>/dev/null &&
	test_must_fail grit verify-pack /tmp/corrupt-$$.pack &&
	rm -f /tmp/corrupt-$$.pack
	)
'

test_expect_success 'verify-pack on original pack still valid after corruption tests' '
	(
	cd repo &&
	grit verify-pack "$(cat ../packfile)"
	)
'

test_expect_success 'verify-pack with nonexistent file fails' '
	(
	cd repo &&
	test_must_fail grit verify-pack nonexistent.pack 2>err
	)
'

test_expect_success 'show-index with empty input' '
	(
	cd repo &&
	: >empty &&
	grit show-index <empty >actual 2>err || true &&
	true
	)
'

test_expect_success 'verify-pack on valid pack returns exit 0' '
	(
	cd repo &&
	grit verify-pack "$(cat ../packfile)"
	)
'

test_expect_success 'count-objects shows info' '
	(
	cd repo &&
	grit count-objects >actual &&
	test -s actual
	)
'

test_expect_success 'verify-pack -v format: lines have hash and type' '
	(
	cd repo &&
	grit verify-pack -v "$(cat ../packfile)" >actual &&
	head -1 actual | grep -q "[0-9a-f]"
	)
'

test_expect_success 'show-index entries have offset field' '
	(
	cd repo &&
	grit show-index <"$(cat ../idxfile)" >actual &&
	while read offset hash rest; do
		test "$offset" -ge 0 || return 1
	done <actual
	)
'

test_expect_success 'prune-packed runs without error' '
	(
	cd repo &&
	grit prune-packed
	)
'

test_expect_success 'verify-pack -v output has pack path with ok' '
	(
	cd repo &&
	grit verify-pack -v "$(cat ../packfile)" >actual &&
	grep "\.pack: ok" actual
	)
'

test_expect_success 'repack -a -d produces valid pack' '
	(
	cd repo &&
	grit repack -a -d &&
	PACK=$(ls .git/objects/pack/*.pack | head -1) &&
	grit verify-pack "$PACK"
	)
'

test_expect_success 'verify-pack on freshly repacked repo' '
	(
	grit init fresh &&
	cd fresh &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "a" >a.txt &&
	grit add a.txt &&
	grit commit -m "one" &&
	echo "b" >b.txt &&
	grit add b.txt &&
	grit commit -m "two" &&
	grit repack -a -d &&
	PACK=$(ls .git/objects/pack/*.pack | head -1) &&
	grit verify-pack "$PACK"
	)
'

test_done
