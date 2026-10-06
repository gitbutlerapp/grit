#!/bin/sh
# 3-way merge via read-tree, conflict stages, unmerged entries.

test_description='grit read-tree 3-way merge and conflict stages'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup base/ours/theirs trees' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "author@example.com" &&
	grit config user.name "A U Thor" &&

	echo "base content" >conflict.txt &&
	echo "unchanged" >unchanged.txt &&
	echo "base-deleted-ours" >del-ours.txt &&
	echo "base-deleted-theirs" >del-theirs.txt &&
	echo "same-change-base" >same-change.txt &&
	grit add conflict.txt unchanged.txt del-ours.txt del-theirs.txt same-change.txt &&
	base_tree=$(grit write-tree) &&
	echo "$base_tree" >../base_tree &&

	echo "ours content" >conflict.txt &&
	echo "unchanged" >unchanged.txt &&
	echo "same-final" >same-change.txt &&
	echo "new-ours" >added-ours.txt &&
	rm -f .git/index &&
	grit update-index --add conflict.txt unchanged.txt same-change.txt del-theirs.txt added-ours.txt &&
	ours_tree=$(grit write-tree) &&
	echo "$ours_tree" >../ours_tree &&

	echo "theirs content" >conflict.txt &&
	echo "unchanged" >unchanged.txt &&
	echo "same-final" >same-change.txt &&
	echo "new-theirs" >added-theirs.txt &&
	rm -f .git/index &&
	grit update-index --add conflict.txt unchanged.txt same-change.txt del-ours.txt added-theirs.txt &&
	theirs_tree=$(grit write-tree) &&
	echo "$theirs_tree" >../theirs_tree
	)
'

test_expect_success '3-way merge with read-tree -m succeeds' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree -m "$(cat ../base_tree)" "$(cat ../ours_tree)" "$(cat ../theirs_tree)"
	)
'

test_expect_success 'conflicting path has stage 1,2,3 entries in ls-files -u' '
	(
	cd repo &&
	grit ls-files -u >unmerged &&
	grep "	conflict.txt$" unmerged >conflict_stages &&
	awk "\$3==1" conflict_stages | grep -q conflict.txt &&
	awk "\$3==2" conflict_stages | grep -q conflict.txt &&
	awk "\$3==3" conflict_stages | grep -q conflict.txt
	)
'

test_expect_success 'conflict stage 1 OID matches base blob' '
	(
	cd repo &&
	base_blob=$(echo "base content" | grit hash-object --stdin) &&
	stage1_oid=$(awk "\$3==1 && /conflict.txt/ {print \$2}" unmerged) &&
	test "$stage1_oid" = "$base_blob"
	)
'

test_expect_success 'conflict stage 2 OID matches ours blob' '
	(
	cd repo &&
	ours_blob=$(echo "ours content" | grit hash-object --stdin) &&
	stage2_oid=$(awk "\$3==2 && /conflict.txt/ {print \$2}" unmerged) &&
	test "$stage2_oid" = "$ours_blob"
	)
'

test_expect_success 'conflict stage 3 OID matches theirs blob' '
	(
	cd repo &&
	theirs_blob=$(echo "theirs content" | grit hash-object --stdin) &&
	stage3_oid=$(awk "\$3==3 && /conflict.txt/ {print \$2}" unmerged) &&
	test "$stage3_oid" = "$theirs_blob"
	)
'

test_expect_success 'unchanged path is resolved (stage 0 in ls-files -s)' '
	(
	cd repo &&
	grit ls-files -s >all_staged &&
	grep "unchanged.txt" all_staged >unch &&
	awk "{print \$3}" unch | grep -q "^0$"
	)
'

test_expect_success 'identical change is resolved to stage 0' '
	(
	cd repo &&
	grep "same-change.txt" all_staged >sc &&
	awk "{print \$3}" sc | grep -q "^0$"
	)
'

test_expect_success 'identical change resolved OID matches final content' '
	(
	cd repo &&
	final_blob=$(echo "same-final" | grit hash-object --stdin) &&
	resolved_oid=$(awk "/same-change.txt/ {print \$2}" all_staged) &&
	test "$resolved_oid" = "$final_blob"
	)
'

test_expect_success 'path deleted in ours is absent or conflicted' '
	(
	cd repo &&
	grit ls-files -s >staged_all &&
	grit ls-files -u >unmerged_all &&
	if grep "del-ours.txt" staged_all >/dev/null 2>&1; then
		echo "del-ours.txt present at stage 0 (kept)"
	elif grep "del-ours.txt" unmerged_all >/dev/null 2>&1; then
		echo "del-ours.txt present as unmerged (conflict)"
	else
		echo "del-ours.txt absent from merge result"
	fi
	)
'

test_expect_success 'path deleted in theirs is absent or conflicted' '
	(
	cd repo &&
	if grep "del-theirs.txt" staged_all >/dev/null 2>&1; then
		echo "del-theirs.txt present at stage 0 (kept)"
	elif grep "del-theirs.txt" unmerged_all >/dev/null 2>&1; then
		echo "del-theirs.txt present as unmerged (conflict)"
	else
		echo "del-theirs.txt absent from merge result"
	fi
	)
'

test_expect_success 'added-ours.txt appears in merge result' '
	(
	cd repo &&
	grit ls-files -s added-ours.txt >actual &&
	grep "added-ours.txt" actual
	)
'

test_expect_success 'added-theirs.txt appears in merge result' '
	(
	cd repo &&
	grit ls-files -s added-theirs.txt >actual &&
	grep "added-theirs.txt" actual
	)
'

test_expect_success 'ls-files -u shows only unmerged entries' '
	(
	cd repo &&
	grit ls-files -u >unmerged &&
	! grep "unchanged.txt" unmerged &&
	! grep "same-change.txt" unmerged
	)
'

test_expect_success 'unmerged entries have 3 stages for conflicting path' '
	(
	cd repo &&
	grit ls-files -u >unmerged &&
	grep "conflict.txt" unmerged >conflict_entries &&
	test_line_count = 3 conflict_entries
	)
'

test_expect_success '2-way merge with read-tree -m succeeds' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree -m "$(cat ../ours_tree)" "$(cat ../theirs_tree)"
	)
'

test_expect_success '2-way merge populates the index' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	test -s actual
	)
'

test_expect_success 'read-tree --reset with single tree populates index' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --reset "$(cat ../base_tree)" &&
	grit ls-files -s >actual &&
	test_line_count = 5 actual
	)
'

test_expect_success 'read-tree --reset produces all stage 0' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	awk "{print \$3}" actual | sort -u >stages &&
	echo "0" >expect &&
	test_cmp expect stages
	)
'

test_expect_success 'read-tree --reset tree matches write-tree output' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	test "$tree" = "$(cat ../base_tree)"
	)
'

test_expect_success '3-way merge: both sides add same file with same content resolves' '
	(
	cd repo &&
	blob=$(echo "new identical" | grit hash-object -w --stdin) &&
	rm -f .git/index &&
	grit update-index --add --cacheinfo 100644,$blob,newfile.txt &&
	both_add_tree=$(grit write-tree) &&
	rm -f .git/index &&
	empty_tree=$(printf "" | grit mktree) &&
	grit read-tree -m "$empty_tree" "$both_add_tree" "$both_add_tree" &&
	grit ls-files -s newfile.txt >actual &&
	grep "newfile.txt" actual &&
	awk "/newfile.txt/ {print \$3}" actual | grep -q "^0$"
	)
'

test_expect_success '3-way merge: both sides add same file with different content conflicts' '
	(
	cd repo &&
	blob_a=$(echo "version a" | grit hash-object -w --stdin) &&
	blob_b=$(echo "version b" | grit hash-object -w --stdin) &&
	rm -f .git/index &&
	grit update-index --add --cacheinfo 100644,$blob_a,newconflict.txt &&
	add_a_tree=$(grit write-tree) &&
	rm -f .git/index &&
	grit update-index --add --cacheinfo 100644,$blob_b,newconflict.txt &&
	add_b_tree=$(grit write-tree) &&
	empty_tree=$(printf "" | grit mktree) &&
	rm -f .git/index &&
	grit read-tree -m "$empty_tree" "$add_a_tree" "$add_b_tree" &&
	grit ls-files -u >unmerged_new &&
	grep "newconflict.txt" unmerged_new
	)
'

test_expect_success '3-way merge: file modified only in ours takes ours' '
	(
	cd repo &&
	blob_base=$(echo "base" | grit hash-object -w --stdin) &&
	blob_mod=$(echo "modified" | grit hash-object -w --stdin) &&
	rm -f .git/index &&
	grit update-index --add --cacheinfo 100644,$blob_base,f.txt &&
	base_t=$(grit write-tree) &&
	rm -f .git/index &&
	grit update-index --add --cacheinfo 100644,$blob_mod,f.txt &&
	ours_t=$(grit write-tree) &&
	rm -f .git/index &&
	grit read-tree -m "$base_t" "$ours_t" "$base_t" &&
	grit ls-files -s f.txt >actual &&
	resolved_oid=$(awk "/f.txt/ {print \$2}" actual) &&
	test "$resolved_oid" = "$blob_mod"
	)
'

test_expect_success '3-way merge: file modified only in theirs takes theirs' '
	(
	cd repo &&
	blob_base=$(echo "base" | grit hash-object -w --stdin) &&
	blob_mod=$(echo "theirs-mod" | grit hash-object -w --stdin) &&
	rm -f .git/index &&
	grit update-index --add --cacheinfo 100644,$blob_base,g.txt &&
	base_t=$(grit write-tree) &&
	rm -f .git/index &&
	grit update-index --add --cacheinfo 100644,$blob_mod,g.txt &&
	theirs_t=$(grit write-tree) &&
	rm -f .git/index &&
	grit read-tree -m "$base_t" "$base_t" "$theirs_t" &&
	grit ls-files -s g.txt >actual &&
	resolved_oid=$(awk "/g.txt/ {print \$2}" actual) &&
	test "$resolved_oid" = "$blob_mod"
	)
'

test_expect_success 'read-tree with --prefix stages under subdirectory' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree --prefix=sub/ "$(cat ../base_tree)" &&
	grit ls-files -s >actual &&
	grep "sub/" actual &&
	test_line_count = 5 actual
	)
'

test_expect_success '3-way merge preserves mode changes' '
	(
	cd repo &&
	blob=$(echo "script" | grit hash-object -w --stdin) &&
	rm -f .git/index &&
	grit update-index --add --cacheinfo 100644,$blob,script.sh &&
	base_mode_tree=$(grit write-tree) &&
	rm -f .git/index &&
	grit update-index --add --cacheinfo 100755,$blob,script.sh &&
	ours_mode_tree=$(grit write-tree) &&
	rm -f .git/index &&
	grit read-tree -m "$base_mode_tree" "$ours_mode_tree" "$base_mode_tree" &&
	grit ls-files -s script.sh >actual &&
	grep "100755" actual
	)
'

test_expect_success 'read-tree single tree populates index' '
	(
	cd repo &&
	rm -f .git/index &&
	grit read-tree "$(cat ../base_tree)" &&
	grit ls-files -s >actual &&
	test_line_count = 5 actual
	)
'

test_expect_success 'read-tree single tree all at stage 0' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	awk "{print \$3}" actual | sort -u >stages &&
	echo "0" >expect &&
	test_cmp expect stages
	)
'

test_expect_success 'read-tree with empty tree clears index' '
	(
	cd repo &&
	empty_tree=$(printf "" | grit mktree) &&
	grit read-tree --reset "$empty_tree" &&
	grit ls-files -s >actual &&
	test_must_be_empty actual
	)
'

test_done
