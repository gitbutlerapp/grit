#!/bin/sh
# Tests for mktree/ls-tree roundtrip, reproducibility, and mode handling.

test_description='mktree/ls-tree roundtrip, reproducibility, modes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with various file types' '
	grit init repo &&
	(cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&

	echo "regular file" >regular.txt &&
	echo "another" >another.txt &&
	mkdir -p dir/nested &&
	echo "in dir" >dir/file.txt &&
	echo "deep" >dir/nested/deep.txt &&
	echo "exec content" >run.sh &&
	chmod +x run.sh &&

	grit add . &&
	grit commit -m "initial"
	)
'

# ── Basic roundtrip ─────────────────────────────────────────────────────────

test_expect_success 'ls-tree | mktree produces same tree SHA' '
	(cd repo &&
	TREE=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree "$TREE" >ls_out &&
	REBUILT=$(grit mktree <ls_out) &&
	test "$TREE" = "$REBUILT"
	)
'

test_expect_success 'ls-tree of rebuilt tree matches original' '
	(cd repo &&
	TREE=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree "$TREE" >original &&
	REBUILT=$(grit mktree <original) &&
	grit ls-tree "$REBUILT" >roundtrip &&
	test_cmp original roundtrip
	)
'

test_expect_success 'multiple roundtrips produce identical results' '
	(cd repo &&
	TREE=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree "$TREE" >pass1_ls &&
	T1=$(grit mktree <pass1_ls) &&
	grit ls-tree "$T1" >pass2_ls &&
	T2=$(grit mktree <pass2_ls) &&
	grit ls-tree "$T2" >pass3_ls &&
	T3=$(grit mktree <pass3_ls) &&
	test "$T1" = "$T2" &&
	test "$T2" = "$T3" &&
	test_cmp pass1_ls pass3_ls
	)
'

# ── Sorted order ────────────────────────────────────────────────────────────

test_expect_success 'mktree with reversed input produces same tree' '
	(cd repo &&
	TREE=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree "$TREE" >sorted &&
	sort -r <sorted >reversed &&
	REBUILT=$(grit mktree <reversed) &&
	test "$TREE" = "$REBUILT"
	)
'

test_expect_success 'mktree with shuffled input produces same tree' '
	(cd repo &&
	TREE=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree "$TREE" >sorted &&
	sort -t "	" -k2,2r <sorted >shuffled &&
	REBUILT=$(grit mktree <shuffled) &&
	test "$TREE" = "$REBUILT"
	)
'

# ── Mode handling ───────────────────────────────────────────────────────────

test_expect_success 'mktree preserves 100644 mode' '
	(cd repo &&
	BLOB=$(echo "test" | grit hash-object -w --stdin) &&
	printf "100644 blob %s\tfile644\n" "$BLOB" | grit mktree >sha &&
	grit ls-tree "$(cat sha)" >out &&
	grep "^100644" out
	)
'

test_expect_success 'mktree preserves 100755 mode' '
	(cd repo &&
	BLOB=$(echo "#!/bin/sh" | grit hash-object -w --stdin) &&
	printf "100755 blob %s\texecfile\n" "$BLOB" | grit mktree >sha &&
	grit ls-tree "$(cat sha)" >out &&
	grep "^100755" out
	)
'

test_expect_success 'mktree preserves 120000 mode (symlink)' '
	(cd repo &&
	BLOB=$(printf "target" | grit hash-object -w --stdin) &&
	printf "120000 blob %s\tlink\n" "$BLOB" | grit mktree >sha &&
	grit ls-tree "$(cat sha)" >out &&
	grep "^120000" out
	)
'

test_expect_success 'mktree preserves 040000 mode (tree)' '
	(cd repo &&
	INNER_BLOB=$(echo "inner" | grit hash-object -w --stdin) &&
	printf "100644 blob %s\tinner.txt\n" "$INNER_BLOB" | grit mktree >inner_sha &&
	INNER_TREE=$(cat inner_sha) &&
	printf "040000 tree %s\tsubdir\n" "$INNER_TREE" | grit mktree >outer_sha &&
	grit ls-tree "$(cat outer_sha)" >out &&
	grep "^040000" out &&
	grep "subdir" out
	)
'

test_expect_success 'mktree with mixed modes roundtrips' '
	(cd repo &&
	BLOB1=$(echo "regular" | grit hash-object -w --stdin) &&
	BLOB2=$(echo "exec" | grit hash-object -w --stdin) &&
	BLOB3=$(printf "link-target" | grit hash-object -w --stdin) &&
	{
		printf "100644 blob %s\treg.txt\n" "$BLOB1"
		printf "100755 blob %s\texec.sh\n" "$BLOB2"
		printf "120000 blob %s\tlink\n" "$BLOB3"
	} >mixed_input &&
	SHA=$(grit mktree <mixed_input) &&
	grit ls-tree "$SHA" >mixed_out &&
	grep "100644" mixed_out &&
	grep "100755" mixed_out &&
	grep "120000" mixed_out &&
	REBUILT=$(grit mktree <mixed_out) &&
	test "$SHA" = "$REBUILT"
	)
'

# ── Nested tree roundtrip ───────────────────────────────────────────────────

test_expect_success 'nested tree roundtrip via ls-tree and mktree' '
	(cd repo &&
	TREE=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree "$TREE" >top &&
	grep "^040000" top >trees_only &&
	if test -s trees_only; then
		REBUILT=$(grit mktree <top) &&
		test "$TREE" = "$REBUILT"
	fi
	)
'

test_expect_success 'ls-tree -r shows all files recursively' '
	(cd repo &&
	TREE=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree -r "$TREE" >recursive &&
	grep "regular.txt" recursive &&
	grep "dir/file.txt" recursive &&
	grep "dir/nested/deep.txt" recursive
	)
'

test_expect_success 'mktree rejects ls-tree -r recursive output' '
	(cd repo &&
	TREE=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree -r "$TREE" >recursive &&
	test_must_fail grit mktree <recursive
	)
'

# ── Empty tree ──────────────────────────────────────────────────────────────

test_expect_success 'mktree with empty input creates well-known empty tree SHA' '
	(cd repo &&
	EMPTY=$(printf "" | grit mktree) &&
	test "$EMPTY" = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
	)
'

test_expect_success 'empty tree roundtrips through ls-tree and mktree' '
	(cd repo &&
	EMPTY=$(printf "" | grit mktree) &&
	grit ls-tree "$EMPTY" >empty_ls &&
	test_must_be_empty empty_ls &&
	REBUILT=$(grit mktree <empty_ls) &&
	test "$EMPTY" = "$REBUILT"
	)
'

# ── Single-entry trees ──────────────────────────────────────────────────────

test_expect_success 'single blob entry roundtrip' '
	(cd repo &&
	BLOB=$(echo "solo" | grit hash-object -w --stdin) &&
	printf "100644 blob %s\tsolo.txt\n" "$BLOB" >single &&
	SHA=$(grit mktree <single) &&
	grit ls-tree "$SHA" >ls_single &&
	test_cmp single ls_single
	)
'

test_expect_success 'single tree entry roundtrip' '
	(cd repo &&
	BLOB=$(echo "inner" | grit hash-object -w --stdin) &&
	printf "100644 blob %s\tfile\n" "$BLOB" | grit mktree >inner_sha &&
	printf "040000 tree %s\tsub\n" "$(cat inner_sha)" >tree_entry &&
	SHA=$(grit mktree <tree_entry) &&
	grit ls-tree "$SHA" >ls_tree_entry &&
	test_cmp tree_entry ls_tree_entry
	)
'

# ── NUL-terminated mode (-z) ────────────────────────────────────────────────

test_expect_success 'ls-tree -z | mktree -z roundtrip' '
	(cd repo &&
	TREE=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree -z "$TREE" >ls_z &&
	REBUILT=$(grit mktree -z <ls_z) &&
	test "$TREE" = "$REBUILT"
	)
'

test_expect_success 'ls-tree -z output has NUL terminators' '
	(cd repo &&
	TREE=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree -z "$TREE" >ls_z &&
	# Should contain NUL bytes
	NULS=$(tr -cd "\0" <ls_z | wc -c) &&
	test "$NULS" -gt 0
	)
'

# ── Determinism ─────────────────────────────────────────────────────────────

test_expect_success 'same content always produces same tree SHA' '
	(cd repo &&
	BLOB=$(echo "deterministic" | grit hash-object -w --stdin) &&
	printf "100644 blob %s\tdet.txt\n" "$BLOB" >det_input &&
	SHA1=$(grit mktree <det_input) &&
	SHA2=$(grit mktree <det_input) &&
	SHA3=$(grit mktree <det_input) &&
	test "$SHA1" = "$SHA2" &&
	test "$SHA2" = "$SHA3"
	)
'

test_expect_success 'different content produces different tree SHA' '
	(cd repo &&
	BLOB1=$(echo "content1" | grit hash-object -w --stdin) &&
	BLOB2=$(echo "content2" | grit hash-object -w --stdin) &&
	printf "100644 blob %s\tfile.txt\n" "$BLOB1" | grit mktree >sha1 &&
	printf "100644 blob %s\tfile.txt\n" "$BLOB2" | grit mktree >sha2 &&
	! test_cmp sha1 sha2
	)
'

test_expect_success 'different filename produces different tree SHA' '
	(cd repo &&
	BLOB=$(echo "same content" | grit hash-object -w --stdin) &&
	printf "100644 blob %s\tname1.txt\n" "$BLOB" | grit mktree >sha1 &&
	printf "100644 blob %s\tname2.txt\n" "$BLOB" | grit mktree >sha2 &&
	! test_cmp sha1 sha2
	)
'

test_expect_success 'different mode produces different tree SHA' '
	(cd repo &&
	BLOB=$(echo "mode test" | grit hash-object -w --stdin) &&
	printf "100644 blob %s\tscript\n" "$BLOB" | grit mktree >sha644 &&
	printf "100755 blob %s\tscript\n" "$BLOB" | grit mktree >sha755 &&
	! test_cmp sha644 sha755
	)
'

# ── --missing flag ──────────────────────────────────────────────────────────

test_expect_success 'mktree --missing allows non-existent blob' '
	(cd repo &&
	FAKE="0000000000000000000000000000000000000000" &&
	printf "100644 blob %s\tghost.txt\n" "$FAKE" | grit mktree --missing >sha &&
	test -n "$(cat sha)"
	)
'

test_expect_success 'mktree without --missing rejects non-existent blob' '
	(cd repo &&
	FAKE="0000000000000000000000000000000000000000" &&
	printf "100644 blob %s\tghost.txt\n" "$FAKE" | test_must_fail grit mktree
	)
'

test_done
