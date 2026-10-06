#!/bin/sh

test_description='grit rev-parse: revision expressions, tags, ranges, tilde/caret chains'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup history with tags and branches' '
	(
	grit init repo && cd repo &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo a >a.txt && grit add a.txt && grit commit -m "commit-A" &&
	git tag tagA &&
	echo b >b.txt && grit add b.txt && grit commit -m "commit-B" &&
	git tag tagB &&
	echo c >c.txt && grit add c.txt && grit commit -m "commit-C" &&
	git tag tagC &&
	echo d >d.txt && grit add d.txt && grit commit -m "commit-D" &&
	git tag tagD &&
	echo e >e.txt && grit add e.txt && grit commit -m "commit-E" &&
	git tag tagE &&
	base=$(grit rev-parse HEAD~2) &&
	git branch side "$base" &&
	git checkout side &&
	echo s1 >s1.txt && grit add s1.txt && grit commit -m "side-1" &&
	echo s2 >s2.txt && grit add s2.txt && grit commit -m "side-2" &&
	git tag tagS2 &&
	git checkout main
	)
'

test_expect_success 'rev-parse tagA resolves to commit-A hash' '
	(cd repo && grit rev-parse tagA >../actual) &&
	grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'rev-parse tagE resolves to HEAD' '
	(cd repo && grit rev-parse tagE >../tag_hash) &&
	(cd repo && grit rev-parse HEAD >../head_hash) &&
	test_cmp head_hash tag_hash
'

test_expect_success 'rev-parse tagA differs from tagB' '
	(cd repo && grit rev-parse tagA >../a_hash) &&
	(cd repo && grit rev-parse tagB >../b_hash) &&
	! test_cmp a_hash b_hash
'

test_expect_success 'rev-parse HEAD~0 equals HEAD' '
	(cd repo && grit rev-parse HEAD~0 >../actual) &&
	(cd repo && grit rev-parse HEAD >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse HEAD~1 equals tagD' '
	(cd repo && grit rev-parse HEAD~1 >../actual) &&
	(cd repo && grit rev-parse tagD >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse HEAD~2 equals tagC' '
	(cd repo && grit rev-parse HEAD~2 >../actual) &&
	(cd repo && grit rev-parse tagC >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse HEAD~3 equals tagB' '
	(cd repo && grit rev-parse HEAD~3 >../actual) &&
	(cd repo && grit rev-parse tagB >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse HEAD~4 equals tagA' '
	(cd repo && grit rev-parse HEAD~4 >../actual) &&
	(cd repo && grit rev-parse tagA >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse HEAD^ equals HEAD~1' '
	(cd repo && grit rev-parse "HEAD^" >../caret) &&
	(cd repo && grit rev-parse HEAD~1 >../tilde) &&
	test_cmp tilde caret
'

test_expect_success 'rev-parse HEAD^^ equals HEAD~2' '
	(cd repo && grit rev-parse "HEAD^^" >../caret2) &&
	(cd repo && grit rev-parse HEAD~2 >../tilde2) &&
	test_cmp tilde2 caret2
'

test_expect_success 'rev-parse tag~1 resolves to parent of tagged commit' '
	(cd repo && grit rev-parse tagC~1 >../actual) &&
	(cd repo && grit rev-parse tagB >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse tag^{commit} resolves same as tag' '
	(cd repo && grit rev-parse "tagC^{commit}" >../actual) &&
	(cd repo && grit rev-parse tagC >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse side branch resolves' '
	(cd repo && grit rev-parse side >../actual) &&
	grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'rev-parse side differs from main' '
	(cd repo && grit rev-parse side >../side_hash) &&
	(cd repo && grit rev-parse main >../main_hash) &&
	! test_cmp side_hash main_hash
'

test_expect_success 'rev-parse side~1 is side parent' '
	(cd repo && grit rev-parse side~1 >../actual) &&
	grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'rev-parse side~2 is shared ancestor' '
	(cd repo && grit rev-parse side~2 >../actual) &&
	(cd repo && grit rev-parse tagC >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --verify valid tag succeeds' '
	(cd repo && grit rev-parse --verify tagA >../actual) &&
	grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'rev-parse --verify invalid ref fails' '
	(cd repo && ! grit rev-parse --verify nosuchref 2>/dev/null)
'

test_expect_success 'rev-parse --short tag' '
	(cd repo && grit rev-parse --short tagA >../actual) &&
	grep "^[0-9a-f]\{7\}$" actual
'

test_expect_success 'rev-parse --short tag is prefix of full' '
	(cd repo && grit rev-parse --short tagA >../short) &&
	(cd repo && grit rev-parse tagA >../full) &&
	short=$(cat short) &&
	grep "^$short" full
'

test_expect_success 'rev-parse multiple revs on command line' '
	(cd repo && grit rev-parse tagA tagB tagC >../actual) &&
	test_line_count = 3 actual
'

test_expect_success 'rev-parse multiple revs are all different' '
	(cd repo && grit rev-parse tagA tagB tagC >../actual) &&
	sort actual >sorted &&
	sort -u actual >uniq &&
	test_cmp sorted uniq
'

test_expect_success 'rev-parse chained tilde from tag' '
	(cd repo && grit rev-parse tagE~3 >../actual) &&
	(cd repo && grit rev-parse tagB >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse hash directly' '
	(cd repo && hash=$(grit rev-parse HEAD) && grit rev-parse "$hash" >../actual) &&
	(cd repo && grit rev-parse HEAD >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse abbreviated hash' '
	(cd repo && short=$(grit rev-parse --short HEAD) && grit rev-parse "$short" >../actual) &&
	(cd repo && grit rev-parse HEAD >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --is-bare-repository in bare init' '
	grit init --bare bare-repo.git &&
	(cd bare-repo.git && grit rev-parse --is-bare-repository >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-work-tree false in bare repo' '
	(cd bare-repo.git && grit rev-parse --is-inside-work-tree >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse tagS2 resolves to side tip' '
	(cd repo && grit rev-parse tagS2 >../actual) &&
	(cd repo && grit rev-parse side >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse tagS2~1 resolves to side parent' '
	(cd repo && grit rev-parse tagS2~1 >../actual) &&
	(cd repo && grit rev-parse side~1 >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse HEAD from subdirectory' '
	mkdir -p repo/sub/deep &&
	(cd repo/sub/deep && grit rev-parse HEAD >../../../head_sub 2>&1) &&
	(cd repo && grit rev-parse HEAD >../head_root) &&
	test_cmp head_root head_sub
'

test_expect_success 'rev-parse --git-dir from subdirectory contains .git' '
	(cd repo/sub && grit rev-parse --git-dir >../../actual) &&
	test -s actual
'

test_expect_success 'rev-parse tag with tilde beyond root fails' '
	(cd repo && ! grit rev-parse tagA~1 2>/dev/null)
'

test_done
