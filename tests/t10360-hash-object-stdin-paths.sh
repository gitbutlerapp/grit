#!/bin/sh
# Test hash-object --stdin-paths: feeding file paths via stdin,
# edge cases around paths, whitespace, missing files, and interaction
# with -w flag.

test_description='grit hash-object --stdin-paths'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo &&
	cd repo &&
	echo "content alpha" >alpha &&
	echo "content beta" >beta &&
	echo "content gamma" >gamma &&
	echo "content delta" >delta &&
	printf "no newline" >nonewline &&
	printf "" >empty &&
	mkdir -p sub/deep &&
	echo "nested" >sub/nested &&
	echo "deep" >sub/deep/leaf &&
	dd if=/dev/urandom bs=512 count=4 2>/dev/null >binary &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh
	)
'

test_expect_success 'single path via --stdin-paths' '
	(
	cd repo &&
	echo alpha | grit hash-object --stdin-paths >actual &&
	grit hash-object alpha >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'multiple paths via --stdin-paths' '
	(
	cd repo &&
	printf "alpha\nbeta\ngamma\n" | grit hash-object --stdin-paths >actual &&
	grit hash-object alpha beta gamma >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'order of paths is preserved in output' '
	(
	cd repo &&
	printf "gamma\nalpha\nbeta\n" | grit hash-object --stdin-paths >actual &&
	grit hash-object gamma alpha beta >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'two paths via --stdin-paths' '
	(
	cd repo &&
	printf "alpha\nbeta\n" | grit hash-object --stdin-paths >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'five paths produce five OIDs' '
	(
	cd repo &&
	printf "alpha\nbeta\ngamma\ndelta\nnonewline\n" |
		grit hash-object --stdin-paths >actual &&
	test_line_count = 5 actual
	)
'

test_expect_success 'each OID is 40 hex chars' '
	(
	cd repo &&
	printf "alpha\nbeta\n" | grit hash-object --stdin-paths >actual &&
	while read oid; do
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" || return 1
	done <actual
	)
'

test_expect_success '--stdin-paths matches individual hash-object calls' '
	(
	cd repo &&
	oid_a=$(grit hash-object alpha) &&
	oid_b=$(grit hash-object beta) &&
	oid_g=$(grit hash-object gamma) &&
	printf "alpha\nbeta\ngamma\n" | grit hash-object --stdin-paths >actual &&
	printf "%s\n%s\n%s\n" "$oid_a" "$oid_b" "$oid_g" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin-paths with -w writes all objects' '
	(
	cd repo &&
	printf "alpha\nbeta\n" | grit hash-object -w --stdin-paths >oids &&
	oid1=$(sed -n 1p oids) &&
	oid2=$(sed -n 2p oids) &&
	grit cat-file -e "$oid1" &&
	grit cat-file -e "$oid2"
	)
'

test_expect_success '-w --stdin-paths: content is retrievable' '
	(
	cd repo &&
	printf "alpha\n" | grit hash-object -w --stdin-paths >oid_file &&
	oid=$(cat oid_file) &&
	grit cat-file -p "$oid" >actual &&
	test_cmp alpha actual
	)
'

test_expect_success '-w --stdin-paths: multiple files all retrievable' '
	(
	cd repo &&
	printf "alpha\nbeta\ngamma\n" | grit hash-object -w --stdin-paths >oids &&
	sed -n 1p oids | xargs -I{} grit cat-file -p {} >actual_a &&
	sed -n 2p oids | xargs -I{} grit cat-file -p {} >actual_b &&
	sed -n 3p oids | xargs -I{} grit cat-file -p {} >actual_g &&
	test_cmp alpha actual_a &&
	test_cmp beta actual_b &&
	test_cmp gamma actual_g
	)
'

test_expect_success '--stdin-paths with empty file' '
	(
	cd repo &&
	echo empty | grit hash-object --stdin-paths >actual &&
	grit hash-object empty >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin-paths with binary file' '
	(
	cd repo &&
	echo binary | grit hash-object --stdin-paths >actual &&
	grit hash-object binary >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin-paths with nested path' '
	(
	cd repo &&
	echo sub/nested | grit hash-object --stdin-paths >actual &&
	grit hash-object sub/nested >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin-paths with deeply nested path' '
	(
	cd repo &&
	echo sub/deep/leaf | grit hash-object --stdin-paths >actual &&
	grit hash-object sub/deep/leaf >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin-paths with mixed top-level and nested paths' '
	(
	cd repo &&
	printf "alpha\nsub/nested\nsub/deep/leaf\nbeta\n" |
		grit hash-object --stdin-paths >actual &&
	grit hash-object alpha sub/nested sub/deep/leaf beta >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin-paths: same file listed twice gives same OID twice' '
	(
	cd repo &&
	printf "alpha\nalpha\n" | grit hash-object --stdin-paths >actual &&
	test_line_count = 2 actual &&
	oid1=$(sed -n 1p actual) &&
	oid2=$(sed -n 2p actual) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success '--stdin-paths: missing file causes error' '
	(
	cd repo &&
	echo "no_such_file" | test_must_fail grit hash-object --stdin-paths 2>err &&
	test -s err
	)
'

test_expect_success '--stdin-paths cannot be combined with --stdin' '
	(
	cd repo &&
	echo alpha | test_must_fail grit hash-object --stdin --stdin-paths 2>err
	)
'

test_expect_success '--stdin-paths: file with no-newline content' '
	(
	cd repo &&
	echo nonewline | grit hash-object --stdin-paths >actual &&
	grit hash-object nonewline >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin-paths: empty stdin produces no output' '
	(
	cd repo &&
	printf "" | grit hash-object --stdin-paths >actual &&
	test_must_be_empty actual
	)
'

test_expect_success '--stdin-paths -w then cat-file -t shows blob' '
	(
	cd repo &&
	echo alpha | grit hash-object -w --stdin-paths >oid_file &&
	oid=$(cat oid_file) &&
	grit cat-file -t "$oid" >actual &&
	echo blob >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin-paths -w then cat-file -s shows correct size' '
	(
	cd repo &&
	echo alpha | grit hash-object -w --stdin-paths >oid_file &&
	oid=$(cat oid_file) &&
	size=$(grit cat-file -s "$oid") &&
	expected=$(wc -c <alpha | tr -d " ") &&
	test "$size" = "$expected"
	)
'

test_expect_success '--stdin-paths idempotent: hash same files twice' '
	(
	cd repo &&
	printf "alpha\nbeta\n" | grit hash-object --stdin-paths >run1 &&
	printf "alpha\nbeta\n" | grit hash-object --stdin-paths >run2 &&
	test_cmp run1 run2
	)
'

test_expect_success '--stdin-paths with all test files' '
	(
	cd repo &&
	printf "alpha\nbeta\ngamma\ndelta\nnonewline\nempty\nbinary\nsub/nested\nsub/deep/leaf\nscript.sh\n" |
		grit hash-object --stdin-paths >actual &&
	test_line_count = 10 actual
	)
'

test_expect_success '--stdin-paths: all OIDs are unique for unique content' '
	(
	cd repo &&
	printf "alpha\nbeta\ngamma\ndelta\n" |
		grit hash-object --stdin-paths >oids &&
	sort -u oids >unique &&
	test_line_count = 4 unique
	)
'

test_expect_success '--stdin-paths with -w: idempotent writes' '
	(
	cd repo &&
	printf "alpha\n" | grit hash-object -w --stdin-paths >oid1 &&
	printf "alpha\n" | grit hash-object -w --stdin-paths >oid2 &&
	test_cmp oid1 oid2
	)
'

test_expect_success '--stdin-paths: script.sh hashes same as direct' '
	(
	cd repo &&
	echo script.sh | grit hash-object --stdin-paths >actual &&
	grit hash-object script.sh >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin-paths with -w writes objects that cat-file -e accepts' '
	(
	cd repo &&
	printf "alpha\nbeta\ngamma\ndelta\n" |
		grit hash-object -w --stdin-paths >oids &&
	while read oid; do
		grit cat-file -e "$oid" || return 1
	done <oids
	)
'

test_expect_success '--stdin-paths: large number of files' '
	(
	cd repo &&
	for i in $(seq 1 20); do
		echo "file content $i" >"batch_$i"
	done &&
	for i in $(seq 1 20); do
		echo "batch_$i"
	done | grit hash-object --stdin-paths >actual &&
	test_line_count = 20 actual
	)
'

test_expect_success '--stdin-paths -w: large batch all written' '
	(
	cd repo &&
	for i in $(seq 1 20); do
		echo "batch_$i"
	done | grit hash-object -w --stdin-paths >oids &&
	while read oid; do
		grit cat-file -e "$oid" || return 1
	done <oids
	)
'

test_expect_success '--stdin-paths: relative path with ./' '
	(
	cd repo &&
	echo ./alpha | grit hash-object --stdin-paths >actual &&
	grit hash-object alpha >expect &&
	test_cmp expect actual
	)
'

test_done
