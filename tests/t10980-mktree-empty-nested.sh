#!/bin/sh
# Tests for grit mktree with empty trees, nested trees, and various modes.

test_description='grit mktree: empty trees, nested trees, -z, --batch, --missing'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with blobs and trees' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "hello" >file.txt &&
	mkdir -p dir/sub &&
	echo "nested" >dir/nested.txt &&
	echo "deep" >dir/sub/deep.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial"
	)
'

###########################################################################
# Section 2: Empty tree
###########################################################################

test_expect_success 'mktree with empty input creates empty tree' '
	(
	cd repo &&
	tree=$(printf "" | grit mktree) &&
	echo "$tree" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'mktree empty tree matches git mktree empty' '
	(
	cd repo &&
	tree=$(printf "" | grit mktree) &&
	git_tree=$(printf "" | "$REAL_GIT" mktree) &&
	test "$tree" = "$git_tree"
	)
'

test_expect_success 'mktree empty tree matches git mktree' '
	(
	cd repo &&
	gtree=$(printf "" | grit mktree) &&
	rgit_tree=$(printf "" | "$REAL_GIT" mktree) &&
	test "$gtree" = "$rgit_tree"
	)
'

test_expect_success 'cat-file -t of empty tree shows tree' '
	(
	cd repo &&
	tree=$(printf "" | grit mktree) &&
	grit cat-file -t "$tree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file -s of empty tree is 0' '
	(
	cd repo &&
	tree=$(printf "" | grit mktree) &&
	grit cat-file -s "$tree" >actual &&
	echo "0" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: Single blob entry
###########################################################################

test_expect_success 'mktree with one blob entry' '
	(
	cd repo &&
	blob=$(grit hash-object -w file.txt) &&
	tree=$(printf "100644 blob %s\tfile.txt\n" "$blob" | grit mktree) &&
	grit cat-file -t "$tree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree with one blob entry matches git' '
	(
	cd repo &&
	blob=$(grit hash-object -w file.txt) &&
	gtree=$(printf "100644 blob %s\tfile.txt\n" "$blob" | grit mktree) &&
	rgit_tree=$(printf "100644 blob %s\tfile.txt\n" "$blob" | "$REAL_GIT" mktree) &&
	test "$gtree" = "$rgit_tree"
	)
'

test_expect_success 'mktree single entry ls-tree roundtrip' '
	(
	cd repo &&
	blob=$(grit hash-object -w file.txt) &&
	tree=$(printf "100644 blob %s\tfile.txt\n" "$blob" | grit mktree) &&
	grit ls-tree "$tree" >actual &&
	grep "file.txt" actual &&
	grep "$blob" actual
	)
'

###########################################################################
# Section 4: Multiple entries
###########################################################################

test_expect_success 'mktree with multiple blob entries' '
	(
	cd repo &&
	echo "aaa" >a.txt && echo "bbb" >b.txt &&
	ha=$(grit hash-object -w a.txt) &&
	hb=$(grit hash-object -w b.txt) &&
	tree=$(printf "100644 blob %s\ta.txt\n100644 blob %s\tb.txt\n" "$ha" "$hb" | grit mktree) &&
	grit ls-tree "$tree" >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'mktree multiple entries matches git' '
	(
	cd repo &&
	echo "xxx" >x.txt && echo "yyy" >y.txt &&
	hx=$(grit hash-object -w x.txt) &&
	hy=$(grit hash-object -w y.txt) &&
	input=$(printf "100644 blob %s\tx.txt\n100644 blob %s\ty.txt\n" "$hx" "$hy") &&
	gtree=$(echo "$input" | grit mktree) &&
	rgit_tree=$(echo "$input" | "$REAL_GIT" mktree) &&
	test "$gtree" = "$rgit_tree"
	)
'

###########################################################################
# Section 5: Nested trees (tree entry in mktree)
###########################################################################

test_expect_success 'mktree with subtree entry' '
	(
	cd repo &&
	blob=$(grit hash-object -w file.txt) &&
	inner=$(printf "100644 blob %s\tinner.txt\n" "$blob" | grit mktree) &&
	outer=$(printf "040000 tree %s\tsubdir\n" "$inner" | grit mktree) &&
	grit ls-tree "$outer" >actual &&
	grep "subdir" actual &&
	grep "040000" actual
	)
'

test_expect_success 'mktree nested tree roundtrips with ls-tree -r' '
	(
	cd repo &&
	blob=$(grit hash-object -w file.txt) &&
	inner=$(printf "100644 blob %s\tleaf.txt\n" "$blob" | grit mktree) &&
	outer=$(printf "040000 tree %s\tdir\n" "$inner" | grit mktree) &&
	grit ls-tree -r "$outer" >actual &&
	grep "dir/leaf.txt" actual
	)
'

test_expect_success 'mktree nested tree matches git' '
	(
	cd repo &&
	echo "data" >d.txt &&
	hd=$(grit hash-object -w d.txt) &&
	inner_input=$(printf "100644 blob %s\td.txt\n" "$hd") &&
	grit_inner=$(echo "$inner_input" | grit mktree) &&
	git_inner=$(echo "$inner_input" | "$REAL_GIT" mktree) &&
	test "$grit_inner" = "$git_inner" &&
	outer_input=$(printf "040000 tree %s\tsub\n" "$grit_inner") &&
	grit_outer=$(echo "$outer_input" | grit mktree) &&
	git_outer=$(echo "$outer_input" | "$REAL_GIT" mktree) &&
	test "$grit_outer" = "$git_outer"
	)
'

###########################################################################
# Section 6: -z (NUL-terminated input)
###########################################################################

test_expect_success 'mktree -z with NUL-terminated input' '
	(
	cd repo &&
	blob=$(grit hash-object -w file.txt) &&
	tree=$(printf "100644 blob %s\tfile.txt\0" "$blob" | grit mktree -z) &&
	grit cat-file -t "$tree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree -z matches newline version' '
	(
	cd repo &&
	echo "content" >c.txt &&
	hc=$(grit hash-object -w c.txt) &&
	t_nl=$(printf "100644 blob %s\tc.txt\n" "$hc" | grit mktree) &&
	t_z=$(printf "100644 blob %s\tc.txt\0" "$hc" | grit mktree -z) &&
	test "$t_nl" = "$t_z"
	)
'

test_expect_success 'mktree -z with multiple entries' '
	(
	cd repo &&
	echo "p" >p.txt && echo "q" >q.txt &&
	hp=$(grit hash-object -w p.txt) &&
	hq=$(grit hash-object -w q.txt) &&
	printf "100644 blob %s\tp.txt\0" "$hp" >input_z &&
	printf "100644 blob %s\tq.txt\0" "$hq" >>input_z &&
	tree=$(grit mktree -z <input_z) &&
	grit ls-tree "$tree" >actual &&
	test_line_count = 2 actual
	)
'

###########################################################################
# Section 7: --batch mode
###########################################################################

test_expect_success 'mktree --batch creates multiple trees' '
	(
	cd repo &&
	echo "m1" >m1.txt && echo "m2" >m2.txt &&
	hm1=$(grit hash-object -w m1.txt) &&
	hm2=$(grit hash-object -w m2.txt) &&
	trees=$(printf "100644 blob %s\tm1.txt\n\n100644 blob %s\tm2.txt\n" "$hm1" "$hm2" | grit mktree --batch) &&
	echo "$trees" | wc -l | tr -d " " >actual &&
	echo "2" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree --batch first tree is valid' '
	(
	cd repo &&
	echo "b1" >b1.txt && echo "b2" >b2.txt &&
	hb1=$(grit hash-object -w b1.txt) &&
	hb2=$(grit hash-object -w b2.txt) &&
	printf "100644 blob %s\tb1.txt\n\n100644 blob %s\tb2.txt\n" "$hb1" "$hb2" | grit mktree --batch >trees &&
	t1=$(sed -n 1p trees) &&
	grit cat-file -t "$t1" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'mktree --batch second tree is valid' '
	(
	cd repo &&
	echo "c1" >c1.txt && echo "c2" >c2.txt &&
	hc1=$(grit hash-object -w c1.txt) &&
	hc2=$(grit hash-object -w c2.txt) &&
	printf "100644 blob %s\tc1.txt\n\n100644 blob %s\tc2.txt\n" "$hc1" "$hc2" | grit mktree --batch >trees &&
	t2=$(sed -n 2p trees) &&
	grit cat-file -t "$t2" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 8: Executable mode
###########################################################################

test_expect_success 'mktree with 100755 executable entry' '
	(
	cd repo &&
	echo "#!/bin/sh" >script.sh &&
	hs=$(grit hash-object -w script.sh) &&
	tree=$(printf "100755 blob %s\tscript.sh\n" "$hs" | grit mktree) &&
	grit ls-tree "$tree" >actual &&
	grep "100755" actual
	)
'

test_expect_success 'mktree with 120000 symlink entry' '
	(
	cd repo &&
	printf "target" >link_content &&
	hl=$(grit hash-object -w link_content) &&
	tree=$(printf "120000 blob %s\tmy-link\n" "$hl" | grit mktree) &&
	grit ls-tree "$tree" >actual &&
	grep "120000" actual
	)
'

###########################################################################
# Section 9: Roundtrip with real tree
###########################################################################

test_expect_success 'ls-tree piped to mktree reproduces tree' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	reproduced=$(grit ls-tree "$tree" | grit mktree) &&
	test "$tree" = "$reproduced"
	)
'

test_expect_success 'ls-tree -z piped to mktree -z reproduces tree' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	reproduced=$(grit ls-tree -z "$tree" | grit mktree -z) &&
	test "$tree" = "$reproduced"
	)
'

###########################################################################
# Section 10: --missing flag
###########################################################################

test_expect_success 'mktree --missing allows nonexistent object' '
	(
	cd repo &&
	fake_hash="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" &&
	tree=$(printf "100644 blob %s\tghost.txt\n" "$fake_hash" | grit mktree --missing) &&
	echo "$tree" | grep -qE "^[0-9a-f]{40}$"
	)
'

###########################################################################
# Section 11: Deeply nested
###########################################################################

test_expect_success 'mktree three levels deep' '
	(
	cd repo &&
	echo "leaf" >leaf.txt &&
	hl=$(grit hash-object -w leaf.txt) &&
	t1=$(printf "100644 blob %s\tleaf.txt\n" "$hl" | grit mktree) &&
	t2=$(printf "040000 tree %s\tlevel2\n" "$t1" | grit mktree) &&
	t3=$(printf "040000 tree %s\tlevel1\n" "$t2" | grit mktree) &&
	grit ls-tree -r "$t3" >actual &&
	grep "level1/level2/leaf.txt" actual
	)
'

test_expect_success 'mktree mixed blobs and subtrees' '
	(
	cd repo &&
	echo "root" >root.txt &&
	hr=$(grit hash-object -w root.txt) &&
	echo "child" >child.txt &&
	hc=$(grit hash-object -w child.txt) &&
	subtree=$(printf "100644 blob %s\tchild.txt\n" "$hc" | grit mktree) &&
	top=$(printf "100644 blob %s\troot.txt\n040000 tree %s\tsubdir\n" "$hr" "$subtree" | grit mktree) &&
	grit ls-tree "$top" >actual &&
	grep "root.txt" actual &&
	grep "subdir" actual &&
	test_line_count = 2 actual
	)
'

test_done
