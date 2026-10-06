#!/bin/sh

test_description='grit rev-parse with tags, refs, peeling, and revision syntax'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo && cd repo &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo A >a.txt && grit add a.txt && grit commit -m "first" &&
	echo B >b.txt && grit add b.txt && grit commit -m "second" &&
	echo C >c.txt && grit add c.txt && grit commit -m "third" &&
	echo D >d.txt && grit add d.txt && grit commit -m "fourth" &&
	grit tag v1.0 &&
	grit tag -a v2.0 -m "annotated tag"
	)
'

test_expect_success 'rev-parse v1.0 returns full hash' '
	(cd repo && grit rev-parse v1.0 >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'rev-parse v1.0 matches HEAD (lightweight tag)' '
	(cd repo && grit rev-parse v1.0 >../tag_hash) &&
	(cd repo && grit rev-parse HEAD >../head_hash) &&
	test_cmp tag_hash head_hash
'

test_expect_success 'rev-parse v2.0 returns annotated tag object' '
	(cd repo && grit rev-parse v2.0 >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'rev-parse v2.0 differs from HEAD (annotated tag)' '
	(cd repo && grit rev-parse v2.0 >../tag_hash) &&
	(cd repo && grit rev-parse HEAD >../head_hash) &&
	! test_cmp tag_hash head_hash
'

test_expect_success 'rev-parse v2.0^{commit} peels to commit' '
	(cd repo && grit rev-parse "v2.0^{commit}" >../actual) &&
	(cd repo && grit rev-parse HEAD >../head_hash) &&
	test_cmp actual head_hash
'

test_expect_success 'rev-parse v1.0^{commit} same as v1.0 for lightweight' '
	(cd repo && grit rev-parse v1.0 >../lw) &&
	(cd repo && grit rev-parse "v1.0^{commit}" >../peeled) &&
	test_cmp lw peeled
'

test_expect_success 'rev-parse v1.0^{tree} returns tree hash' '
	(cd repo && grit rev-parse "v1.0^{tree}" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'rev-parse v2.0^{tree} returns tree hash' '
	(cd repo && grit rev-parse "v2.0^{tree}" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'rev-parse v1.0^{tree} same as HEAD^{tree}' '
	(cd repo && grit rev-parse "v1.0^{tree}" >../tag_tree) &&
	(cd repo && grit rev-parse "HEAD^{tree}" >../head_tree) &&
	test_cmp tag_tree head_tree
'

test_expect_success 'rev-parse HEAD^ returns parent of HEAD' '
	(cd repo && grit rev-parse "HEAD^" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40 &&
	(cd repo && grit rev-parse HEAD >../head_hash) &&
	test "$(cat actual)" != "$(cat head_hash)"
'

test_expect_success 'rev-parse HEAD~1 same as HEAD^' '
	(cd repo && grit rev-parse "HEAD^" >../caret) &&
	(cd repo && grit rev-parse "HEAD~1" >../tilde) &&
	test_cmp caret tilde
'

test_expect_success 'rev-parse HEAD^^ is grandparent' '
	(cd repo && grit rev-parse "HEAD^^" >../actual) &&
	(cd repo && grit rev-parse "HEAD^" >../parent) &&
	test "$(cat actual)" != "$(cat parent)"
'

test_expect_success 'rev-parse HEAD~2 same as HEAD^^' '
	(cd repo && grit rev-parse "HEAD^^" >../dblcaret) &&
	(cd repo && grit rev-parse "HEAD~2" >../tilde2) &&
	test_cmp dblcaret tilde2
'

test_expect_success 'rev-parse HEAD~3 is great-grandparent' '
	(cd repo && grit rev-parse "HEAD~3" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'rev-parse HEAD~3 is the root commit' '
	(cd repo && grit rev-parse "HEAD~3" >../actual) &&
	(cd repo && grit log --reverse --format="%H" | head -1 >../root_hash) &&
	test_cmp actual root_hash
'

test_expect_success 'rev-parse with invalid ref fails' '
	(cd repo && ! grit rev-parse nonexistent 2>../err) &&
	test -s err
'

test_expect_success 'rev-parse --verify with invalid ref fails' '
	(cd repo && ! grit rev-parse --verify nonexistent 2>../err) &&
	test -s err
'

test_expect_success 'setup feature branch' '
	(cd repo && git checkout -b feature &&
	 echo E >e.txt && grit add e.txt && grit commit -m "feature-1" &&
	 echo F >f.txt && grit add f.txt && grit commit -m "feature-2")
'

test_expect_success 'rev-parse feature returns full hash' '
	(cd repo && grit rev-parse feature >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'rev-parse feature differs from main' '
	(cd repo && grit rev-parse feature >../feat) &&
	(cd repo && grit rev-parse main >../mast) &&
	! test_cmp feat mast
'

test_expect_success 'rev-parse HEAD matches current branch' '
	(cd repo && grit rev-parse HEAD >../head_hash) &&
	(cd repo && grit rev-parse feature >../feat_hash) &&
	test_cmp head_hash feat_hash
'

test_expect_success 'rev-parse feature^ is parent of feature tip' '
	(cd repo && grit rev-parse "feature^" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40 &&
	(cd repo && grit rev-parse feature >../feat_hash) &&
	test "$(cat actual)" != "$(cat feat_hash)"
'

test_expect_success 'rev-parse feature~2 goes back two from feature' '
	(cd repo && grit rev-parse "feature~2" >../actual) &&
	(cd repo && grit rev-parse main >../mast) &&
	test_cmp actual mast
'

test_expect_success 'rev-parse --short v1.0 returns short hash' '
	(cd repo && grit rev-parse --short v1.0 >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 7
'

test_expect_success 'rev-parse --short feature returns short hash' '
	(cd repo && grit rev-parse --short feature >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 7
'

test_expect_success 'rev-parse --verify HEAD^ succeeds' '
	(cd repo && grit rev-parse --verify "HEAD^" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'rev-parse v1.0^ returns parent of tagged commit' '
	(cd repo && grit rev-parse "v1.0^" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40 &&
	(cd repo && grit rev-parse v1.0 >../v1_hash) &&
	test "$(cat actual)" != "$(cat v1_hash)"
'

test_expect_success 'rev-parse main from feature branch context' '
	(cd repo && grit rev-parse main >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'rev-parse --is-bare-repository from feature' '
	(cd repo && grit rev-parse --is-bare-repository >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'setup: switch back to main' '
	(cd repo && git checkout main)
'

test_expect_success 'rev-parse HEAD after checkout matches main' '
	(cd repo && grit rev-parse HEAD >../head_hash) &&
	(cd repo && grit rev-parse main >../main_hash) &&
	test_cmp head_hash main_hash
'

test_expect_success 'rev-parse multiple refs at once' '
	(cd repo && grit rev-parse HEAD main >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'rev-parse multiple refs gives same hash twice for HEAD and main' '
	(cd repo && grit rev-parse HEAD main >../actual) &&
	head_h=$(head -1 actual) &&
	main_h=$(tail -1 actual) &&
	test "$head_h" = "$main_h"
'

test_expect_success 'rev-parse HEAD feature gives different hashes' '
	(cd repo && grit rev-parse HEAD feature >../actual) &&
	head_h=$(head -1 actual) &&
	feat_h=$(tail -1 actual) &&
	test "$head_h" != "$feat_h"
'

test_done
