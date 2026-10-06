#!/bin/sh
# Test mktree --batch mode and --missing flag: multiple trees per invocation,
# empty tree separators, missing object references, mode handling, and
# round-trip verification via ls-tree.

test_description='grit mktree --batch and --missing'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

EMPTY_TREE='4b825dc642cb6eb9a060e54bf8d69288fbee4904'

test_expect_success 'setup blobs' '
	(
	grit init repo &&
	cd repo &&
	echo "alpha" >alpha &&
	echo "beta" >beta &&
	echo "gamma" >gamma &&
	echo "delta" >delta &&
	echo "epsilon" >epsilon &&
	printf "" >empty_file &&
	echo "#!/bin/sh" >script &&
	blob_a=$(grit hash-object -w alpha) &&
	blob_b=$(grit hash-object -w beta) &&
	blob_g=$(grit hash-object -w gamma) &&
	blob_d=$(grit hash-object -w delta) &&
	blob_e=$(grit hash-object -w epsilon) &&
	blob_empty=$(grit hash-object -w empty_file) &&
	blob_script=$(grit hash-object -w script) &&
	echo "$blob_a" >../blob_a &&
	echo "$blob_b" >../blob_b &&
	echo "$blob_g" >../blob_g &&
	echo "$blob_d" >../blob_d &&
	echo "$blob_e" >../blob_e &&
	echo "$blob_empty" >../blob_empty &&
	echo "$blob_script" >../blob_script
	)
'

# --- --batch basics ---

test_expect_success 'mktree --batch: two trees separated by blank line' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	bb=$(cat ../blob_b) &&
	printf "100644 blob %s\tfile_a\n\n100644 blob %s\tfile_b\n" "$ba" "$bb" |
		grit mktree --batch >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'mktree --batch: each tree is a valid tree object' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	bb=$(cat ../blob_b) &&
	printf "100644 blob %s\tfile_a\n\n100644 blob %s\tfile_b\n" "$ba" "$bb" |
		grit mktree --batch >oids &&
	while read oid; do
		grit cat-file -t "$oid" >type &&
		echo "tree" >expect &&
		test_cmp expect type || return 1
	done <oids
	)
'

test_expect_success 'mktree --batch: trees have different OIDs for different content' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	bb=$(cat ../blob_b) &&
	printf "100644 blob %s\tfile_a\n\n100644 blob %s\tfile_b\n" "$ba" "$bb" |
		grit mktree --batch >oids &&
	tree1=$(sed -n 1p oids) &&
	tree2=$(sed -n 2p oids) &&
	test "$tree1" != "$tree2"
	)
'

test_expect_success 'mktree --batch: three trees' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	bb=$(cat ../blob_b) &&
	bg=$(cat ../blob_g) &&
	printf "100644 blob %s\ta\n\n100644 blob %s\tb\n\n100644 blob %s\tc\n" \
		"$ba" "$bb" "$bg" |
		grit mktree --batch >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'mktree --batch: first empty section yields empty tree' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	printf "\n100644 blob %s\tfile\n" "$ba" |
		grit mktree --batch >oids &&
	tree1=$(sed -n 1p oids) &&
	test "$tree1" = "$EMPTY_TREE"
	)
'

test_expect_success 'mktree --batch: single tree (no separator needed)' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	printf "100644 blob %s\tonly\n" "$ba" | grit mktree --batch >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'mktree --batch: tree with multiple entries' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	bb=$(cat ../blob_b) &&
	bg=$(cat ../blob_g) &&
	printf "100644 blob %s\ta\n100644 blob %s\tb\n100644 blob %s\tc\n" \
		"$ba" "$bb" "$bg" | grit mktree --batch >actual &&
	test_line_count = 1 actual &&
	oid=$(cat actual) &&
	grit ls-tree "$oid" >ls_out &&
	test_line_count = 3 ls_out
	)
'

test_expect_success 'mktree --batch: ls-tree round-trip for each tree' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	bb=$(cat ../blob_b) &&
	printf "100644 blob %s\tfile_a\n\n100644 blob %s\tfile_b\n" "$ba" "$bb" |
		grit mktree --batch >oids &&
	tree1=$(sed -n 1p oids) &&
	tree2=$(sed -n 2p oids) &&
	grit ls-tree "$tree1" | grit mktree >re1 &&
	grit ls-tree "$tree2" | grit mktree >re2 &&
	echo "$tree1" >expect1 &&
	echo "$tree2" >expect2 &&
	test_cmp expect1 re1 &&
	test_cmp expect2 re2
	)
'

test_expect_success 'mktree --batch: same content in two batches yields same OID' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	printf "100644 blob %s\tfile_a\n\n100644 blob %s\tfile_a\n" "$ba" "$ba" |
		grit mktree --batch >oids &&
	tree1=$(sed -n 1p oids) &&
	tree2=$(sed -n 2p oids) &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'mktree --batch: five trees' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	bb=$(cat ../blob_b) &&
	bg=$(cat ../blob_g) &&
	bd=$(cat ../blob_d) &&
	be=$(cat ../blob_e) &&
	printf "100644 blob %s\t1\n\n100644 blob %s\t2\n\n100644 blob %s\t3\n\n100644 blob %s\t4\n\n100644 blob %s\t5\n" \
		"$ba" "$bb" "$bg" "$bd" "$be" |
		grit mktree --batch >actual &&
	test_line_count = 5 actual
	)
'

# --- --missing flag ---

test_expect_success 'mktree without --missing rejects nonexistent blob' '
	(
	cd repo &&
	printf "100644 blob %s\tghost\n" "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" |
		test_must_fail grit mktree 2>err
	)
'

test_expect_success 'mktree --missing allows nonexistent blob' '
	(
	cd repo &&
	printf "100644 blob %s\tghost\n" "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" |
		grit mktree --missing >actual &&
	test -s actual
	)
'

test_expect_success 'mktree --missing: result is a valid tree object' '
	(
	cd repo &&
	printf "100644 blob %s\tghost\n" "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" |
		grit mktree --missing >oid_file &&
	oid=$(cat oid_file) &&
	grit cat-file -t "$oid" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree --missing: mix of real and fake blobs' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	printf "100644 blob %s\treal\n100644 blob %s\tfake\n" \
		"$ba" "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" |
		grit mktree --missing >oid_file &&
	oid=$(cat oid_file) &&
	grit ls-tree "$oid" >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'mktree --missing: multiple fake entries' '
	(
	cd repo &&
	printf "100644 blob %s\tfake1\n100644 blob %s\tfake2\n100644 blob %s\tfake3\n" \
		"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" \
		"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" \
		"cccccccccccccccccccccccccccccccccccccccc" |
		grit mktree --missing >actual &&
	test -s actual
	)
'

test_expect_success 'mktree --missing with --batch' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	printf "100644 blob %s\treal\n\n100644 blob %s\tghost\n" \
		"$ba" "dddddddddddddddddddddddddddddddddddddddd" |
		grit mktree --missing --batch >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'mktree --missing --batch: each tree is valid' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	printf "100644 blob %s\treal\n\n100644 blob %s\tghost\n" \
		"$ba" "dddddddddddddddddddddddddddddddddddddddd" |
		grit mktree --missing --batch >oids &&
	while read oid; do
		grit cat-file -t "$oid" >type &&
		echo "tree" >expect &&
		test_cmp expect type || return 1
	done <oids
	)
'

# --- modes ---

test_expect_success 'mktree: 100644 mode preserved' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	printf "100644 blob %s\tnormal\n" "$ba" | grit mktree >oid_file &&
	grit ls-tree $(cat oid_file) >actual &&
	grep "100644" actual
	)
'

test_expect_success 'mktree: 100755 mode preserved' '
	(
	cd repo &&
	bs=$(cat ../blob_script) &&
	printf "100755 blob %s\texec\n" "$bs" | grit mktree >oid_file &&
	grit ls-tree $(cat oid_file) >actual &&
	grep "100755" actual
	)
'

test_expect_success 'mktree: 120000 symlink mode preserved' '
	(
	cd repo &&
	link_oid=$(printf "target" | grit hash-object -w --stdin) &&
	printf "120000 blob %s\tlink\n" "$link_oid" | grit mktree >oid_file &&
	grit ls-tree $(cat oid_file) >actual &&
	grep "120000" actual
	)
'

test_expect_success 'mktree: 040000 tree mode for subtree' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	inner=$(printf "100644 blob %s\tinner\n" "$ba" | grit mktree) &&
	printf "040000 tree %s\tsub\n" "$inner" | grit mktree >oid_file &&
	grit ls-tree $(cat oid_file) >actual &&
	grep "040000 tree.*sub" actual
	)
'

test_expect_success 'mktree: mixed modes all preserved' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	bs=$(cat ../blob_script) &&
	link_oid=$(printf "link_target" | grit hash-object -w --stdin) &&
	printf "100644 blob %s\tnormal\n100755 blob %s\texec\n120000 blob %s\tlink\n" \
		"$ba" "$bs" "$link_oid" | grit mktree >oid_file &&
	grit ls-tree $(cat oid_file) >actual &&
	grep "100644" actual &&
	grep "100755" actual &&
	grep "120000" actual
	)
'

# --- empty tree ---

test_expect_success 'mktree: empty input yields well-known empty tree' '
	(
	cd repo &&
	printf "" | grit mktree >actual &&
	echo "$EMPTY_TREE" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree --batch: empty input produces no output' '
	(
	cd repo &&
	printf "" | grit mktree --batch >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'mktree --batch: single blank line yields one empty tree' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	printf "\n100644 blob %s\tfile\n" "$ba" | grit mktree --batch >actual &&
	test_line_count = 2 actual &&
	tree1=$(sed -n 1p actual) &&
	test "$tree1" = "$EMPTY_TREE"
	)
'

# --- large batch ---

test_expect_success 'mktree --batch: ten trees' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	input="" &&
	for i in $(seq 1 10); do
		input="${input}100644 blob ${ba}	file_${i}
"
		if test "$i" -lt 10; then
			input="${input}
"
		fi
	done &&
	printf "%s" "$input" | grit mktree --batch >actual &&
	test_line_count = 10 actual
	)
'

test_expect_success 'mktree --batch: all ten trees are valid' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	input="" &&
	for i in $(seq 1 10); do
		input="${input}100644 blob ${ba}	file_${i}
"
		if test "$i" -lt 10; then
			input="${input}
"
		fi
	done &&
	printf "%s" "$input" | grit mktree --batch >oids &&
	while read oid; do
		grit cat-file -e "$oid" || return 1
	done <oids
	)
'

# --- idempotency ---

test_expect_success 'mktree is idempotent for same input' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	bb=$(cat ../blob_b) &&
	input="100644 blob ${ba}	a
100644 blob ${bb}	b
" &&
	oid1=$(printf "%s" "$input" | grit mktree) &&
	oid2=$(printf "%s" "$input" | grit mktree) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'mktree --batch is idempotent across invocations' '
	(
	cd repo &&
	ba=$(cat ../blob_a) &&
	bb=$(cat ../blob_b) &&
	input="100644 blob ${ba}	x
100644 blob ${bb}	y
" &&
	printf "%s" "$input" | grit mktree --batch >run1 &&
	printf "%s" "$input" | grit mktree --batch >run2 &&
	test_cmp run1 run2
	)
'

test_expect_success 'mktree --missing is idempotent' '
	(
	cd repo &&
	printf "100644 blob %s\tghost\n" "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" |
		grit mktree --missing >run1 &&
	printf "100644 blob %s\tghost\n" "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" |
		grit mktree --missing >run2 &&
	test_cmp run1 run2
	)
'

test_done
