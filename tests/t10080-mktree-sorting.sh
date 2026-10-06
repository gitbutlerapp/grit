#!/bin/sh
# Test that mktree sorts entries correctly and handles various input orderings.

test_description='grit mktree sorting'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

EMPTY_TREE='4b825dc642cb6eb9a060e54bf8d69288fbee4904'

test_expect_success 'setup repository with files and directories' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	mkdir -p dir_a dir_b dir_c &&
	echo "file a" >a_file &&
	echo "file b" >b_file &&
	echo "file z" >z_file &&
	echo "in dir_a" >dir_a/one &&
	echo "in dir_b" >dir_b/two &&
	echo "in dir_c" >dir_c/three &&
	grit add . &&
	test_tick &&
	grit commit -m "initial" &&
	grit rev-parse HEAD^{tree} >../canonical_tree &&
	grit ls-tree HEAD >../canonical_ls
	)
'

test_expect_success 'mktree from ls-tree output reproduces same tree' '
	(
	cd repo &&
	grit ls-tree HEAD >ls_out &&
	grit mktree <ls_out >actual &&
	test_cmp ../canonical_tree actual
	)
'

test_expect_success 'mktree from reverse-sorted input produces same tree' '
	(
	cd repo &&
	grit ls-tree HEAD | sort -r >reversed &&
	grit mktree <reversed >actual &&
	test_cmp ../canonical_tree actual
	)
'

test_expect_success 'mktree from randomly shuffled input produces same tree' '
	(
	cd repo &&
	grit ls-tree HEAD | sort -t/ -k2 >shuffled &&
	grit mktree <shuffled >actual &&
	test_cmp ../canonical_tree actual
	)
'

test_expect_success 'mktree with empty input produces empty tree' '
	(
	cd repo &&
	printf "" | grit mktree >actual &&
	echo "$EMPTY_TREE" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree empty tree is valid object' '
	(
	cd repo &&
	oid=$(printf "" | grit mktree) &&
	grit cat-file -t "$oid" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree empty tree has zero size' '
	(
	cd repo &&
	oid=$(printf "" | grit mktree) &&
	grit cat-file -s "$oid" >actual &&
	echo "0" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree: single blob entry' '
	(
	cd repo &&
	blob_oid=$(grit hash-object -w a_file) &&
	printf "100644 blob %s\ta_file\n" "$blob_oid" | grit mktree >tree_oid &&
	grit cat-file -t $(cat tree_oid) >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree: single blob ls-tree round-trip' '
	(
	cd repo &&
	blob_oid=$(grit hash-object -w a_file) &&
	printf "100644 blob %s\ta_file\n" "$blob_oid" | grit mktree >tree_oid &&
	grit ls-tree $(cat tree_oid) >actual &&
	printf "100644 blob %s\ta_file\n" "$blob_oid" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree: two blobs sorted alphabetically' '
	(
	cd repo &&
	oid_a=$(grit hash-object -w a_file) &&
	oid_z=$(grit hash-object -w z_file) &&
	printf "100644 blob %s\ta_file\n100644 blob %s\tz_file\n" "$oid_a" "$oid_z" |
		grit mktree >tree1 &&
	printf "100644 blob %s\tz_file\n100644 blob %s\ta_file\n" "$oid_z" "$oid_a" |
		grit mktree >tree2 &&
	test_cmp tree1 tree2
	)
'

test_expect_success 'mktree: blobs and trees mixed sort correctly' '
	(
	cd repo &&
	grit ls-tree HEAD >ls_out &&
	grit mktree <ls_out >tree1 &&
	sort -r <ls_out | grit mktree >tree2 &&
	test_cmp tree1 tree2
	)
'

test_expect_success 'mktree output matches write-tree for same content' '
	(
	cd repo &&
	grit ls-tree HEAD >ls_out &&
	tree_from_mktree=$(grit mktree <ls_out) &&
	tree_from_head=$(grit rev-parse HEAD^{tree}) &&
	test "$tree_from_mktree" = "$tree_from_head"
	)
'

test_expect_success 'mktree: tree entries sort with trailing slash convention' '
	(
	cd repo &&
	grit ls-tree HEAD >ls_out &&
	grep "^040000" ls_out >trees_only &&
	grep "^100644" ls_out >blobs_only &&
	cat blobs_only trees_only | grit mktree >tree1 &&
	cat trees_only blobs_only | grit mktree >tree2 &&
	test_cmp tree1 tree2
	)
'

test_expect_success 'setup: create more complex tree' '
	(
	cd repo &&
	mkdir -p sub/deep &&
	echo "nested" >sub/deep/file &&
	echo "top" >top &&
	echo "aaa" >aaa &&
	echo "zzz" >zzz &&
	grit add . &&
	test_tick &&
	grit commit -m "complex" &&
	grit rev-parse HEAD^{tree} >../complex_tree &&
	grit ls-tree HEAD >../complex_ls
	)
'

test_expect_success 'mktree: complex tree from forward-sorted ls-tree' '
	(
	cd repo &&
	grit ls-tree HEAD | sort >sorted_fwd &&
	grit mktree <sorted_fwd >actual &&
	test_cmp ../complex_tree actual
	)
'

test_expect_success 'mktree: complex tree from reverse-sorted ls-tree' '
	(
	cd repo &&
	grit ls-tree HEAD | sort -r >sorted_rev &&
	grit mktree <sorted_rev >actual &&
	test_cmp ../complex_tree actual
	)
'

test_expect_success 'mktree: idempotent - feeding output back in' '
	(
	cd repo &&
	tree1=$(grit ls-tree HEAD | grit mktree) &&
	grit ls-tree "$tree1" | grit mktree >actual &&
	echo "$tree1" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree: duplicate entries produce valid tree' '
	(
	cd repo &&
	blob_oid=$(grit hash-object -w a_file) &&
	printf "100644 blob %s\tsame\n100644 blob %s\tsame\n" "$blob_oid" "$blob_oid" |
		grit mktree >tree_oid &&
	grit cat-file -t $(cat tree_oid) >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree --missing allows nonexistent objects' '
	(
	cd repo &&
	printf "100644 blob %s\tghost\n" "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" |
		grit mktree --missing >actual &&
	test -s actual
	)
'

test_expect_success 'mktree without --missing rejects nonexistent objects' '
	(
	cd repo &&
	printf "100644 blob %s\tghost\n" "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" |
		test_must_fail grit mktree 2>err
	)
'

test_expect_success 'mktree: executable blob (100755) is preserved' '
	(
	cd repo &&
	echo "#!/bin/sh" >script &&
	blob_oid=$(grit hash-object -w script) &&
	printf "100755 blob %s\tscript\n" "$blob_oid" | grit mktree >tree_oid &&
	grit ls-tree $(cat tree_oid) >actual &&
	grep "100755" actual
	)
'

test_expect_success 'mktree: symlink (120000) is preserved' '
	(
	cd repo &&
	printf "target" | grit hash-object -w --stdin >link_oid &&
	printf "120000 blob %s\tmy_link\n" "$(cat link_oid)" | grit mktree >tree_oid &&
	grit ls-tree $(cat tree_oid) >actual &&
	grep "120000" actual
	)
'

test_expect_success 'mktree: mixed modes sort correctly' '
	(
	cd repo &&
	blob1=$(grit hash-object -w a_file) &&
	blob2=$(echo "#!/bin/sh" | grit hash-object -w --stdin) &&
	printf "100755 blob %s\texec_file\n100644 blob %s\tnormal_file\n" "$blob2" "$blob1" |
		grit mktree >tree1 &&
	printf "100644 blob %s\tnormal_file\n100755 blob %s\texec_file\n" "$blob1" "$blob2" |
		grit mktree >tree2 &&
	test_cmp tree1 tree2
	)
'

test_expect_success 'mktree: filenames with special characters' '
	(
	cd repo &&
	blob_oid=$(grit hash-object -w a_file) &&
	printf "100644 blob %s\tfile with spaces\n" "$blob_oid" | grit mktree >tree_oid &&
	grit ls-tree $(cat tree_oid) >actual &&
	grep "file with spaces" actual
	)
'

test_expect_success 'mktree: filenames with dots sort correctly' '
	(
	cd repo &&
	blob=$(grit hash-object -w a_file) &&
	printf "100644 blob %s\t.gitignore\n100644 blob %s\tREADME\n100644 blob %s\ta.txt\n" \
		"$blob" "$blob" "$blob" | grit mktree >tree1 &&
	printf "100644 blob %s\ta.txt\n100644 blob %s\t.gitignore\n100644 blob %s\tREADME\n" \
		"$blob" "$blob" "$blob" | grit mktree >tree2 &&
	test_cmp tree1 tree2
	)
'

test_expect_success 'mktree: subtree entries have mode 040000' '
	(
	cd repo &&
	grit ls-tree HEAD >ls_out &&
	grep "^040000" ls_out | grit mktree >tree_oid &&
	grit ls-tree $(cat tree_oid) >actual &&
	while read mode type oid name; do
		test "$mode" = "040000" || return 1
	done <actual
	)
'

test_expect_success 'mktree: 10 blobs all sort correctly' '
	(
	cd repo &&
	blob=$(grit hash-object -w a_file) &&
	for i in 01 02 03 04 05 06 07 08 09 10; do
		printf "100644 blob %s\tfile_%s\n" "$blob" "$i"
	done | grit mktree >tree_oid &&
	grit ls-tree $(cat tree_oid) >actual &&
	test_line_count = 10 actual &&
	# verify sorted
	awk -F"\t" "{print \$2}" actual >names &&
	sort names >names_sorted &&
	test_cmp names names_sorted
	)
'

test_expect_success 'mktree: --batch creates multiple trees' '
	(
	cd repo &&
	blob=$(grit hash-object -w a_file) &&
	printf "100644 blob %s\tfile1\n\n100644 blob %s\tfile2\n" "$blob" "$blob" |
		grit mktree --batch >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'mktree --batch: each tree is distinct' '
	(
	cd repo &&
	blob=$(grit hash-object -w a_file) &&
	blob2=$(grit hash-object -w b_file) &&
	printf "100644 blob %s\tfile_a\n\n100644 blob %s\tfile_b\n" "$blob" "$blob2" |
		grit mktree --batch >actual &&
	tree1=$(sed -n 1p actual) &&
	tree2=$(sed -n 2p actual) &&
	test "$tree1" != "$tree2"
	)
'

test_expect_success 'mktree --batch: each tree is valid' '
	(
	cd repo &&
	blob=$(grit hash-object -w a_file) &&
	printf "100644 blob %s\tx\n\n100644 blob %s\ty\n" "$blob" "$blob" |
		grit mktree --batch >oids &&
	while read oid; do
		grit cat-file -t "$oid" >type &&
		echo "tree" >expect_type &&
		test_cmp expect_type type || return 1
	done <oids
	)
'

test_expect_success 'mktree refuses recursive ls-tree -r output' '
	(
	cd repo &&
	grit ls-tree -r HEAD >recursive &&
	test_must_fail grit mktree <recursive
	)
'

test_expect_success 'mktree: tree with single subtree' '
	(
	cd repo &&
	inner_blob=$(grit hash-object -w a_file) &&
	inner_tree=$(printf "100644 blob %s\tinner\n" "$inner_blob" | grit mktree) &&
	printf "040000 tree %s\tsubdir\n" "$inner_tree" | grit mktree >outer_oid &&
	grit ls-tree $(cat outer_oid) >actual &&
	grep "040000 tree.*subdir" actual
	)
'

test_expect_success 'mktree: nested tree round-trip via ls-tree' '
	(
	cd repo &&
	inner_blob=$(grit hash-object -w a_file) &&
	inner_tree=$(printf "100644 blob %s\tinner\n" "$inner_blob" | grit mktree) &&
	outer_tree=$(printf "040000 tree %s\tsub\n100644 blob %s\ttop\n" "$inner_tree" "$inner_blob" | grit mktree) &&
	grit ls-tree "$outer_tree" | grit mktree >actual &&
	echo "$outer_tree" >expect &&
	test_cmp expect actual
	)
'

test_done
