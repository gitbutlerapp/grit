#!/bin/sh
# Test rev-list --parents, --first-parent, --reverse, --count, --format,
# ordering flags, range exclusions, and related options.

test_description='grit rev-list parent traversal and output options'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup — linear history
###########################################################################

test_expect_success 'setup: create repo with linear history' '
	(
	grit init rl-repo &&
	cd rl-repo &&
	grit config user.email "test@test.com" &&
	grit config user.name "Test" &&
	echo "a" >a.txt &&
	grit add a.txt &&
	grit commit -m "c1" &&
	echo "b" >b.txt &&
	grit add b.txt &&
	grit commit -m "c2" &&
	echo "c" >c.txt &&
	grit add c.txt &&
	grit commit -m "c3" &&
	grit tag v1.0
	)
'

###########################################################################
# Section 2: Basic rev-list
###########################################################################

test_expect_success 'rev-list HEAD lists all commits' '
	(
	cd rl-repo &&
	grit rev-list HEAD >out &&
	test_line_count = 3 out
	)
'

test_expect_success 'rev-list outputs valid 40-char hex hashes' '
	(
	cd rl-repo &&
	grit rev-list HEAD >out &&
	while read hash; do
		len=$(printf "%s" "$hash" | wc -c) &&
		test "$len" -eq 40 || { echo "bad hash len $len: $hash"; exit 1; }
	done <out
	)
'

test_expect_success 'rev-list HEAD contains HEAD commit' '
	(
	cd rl-repo &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	grit rev-list HEAD >out &&
	grep "$HEAD_OID" out
	)
'

test_expect_success 'rev-list with tag name works like commit' '
	(
	cd rl-repo &&
	grit rev-list v1.0 >tag_out &&
	grit rev-list HEAD >head_out &&
	test_cmp tag_out head_out
	)
'

###########################################################################
# Section 3: --topo-order gives HEAD-first output
###########################################################################

test_expect_success 'rev-list --topo-order starts with HEAD' '
	(
	cd rl-repo &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	grit rev-list --topo-order HEAD >out &&
	FIRST=$(head -1 out) &&
	test "$FIRST" = "$HEAD_OID"
	)
'

test_expect_success 'rev-list --date-order starts with HEAD' '
	(
	cd rl-repo &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	grit rev-list --date-order HEAD >out &&
	FIRST=$(head -1 out) &&
	test "$FIRST" = "$HEAD_OID"
	)
'

test_expect_success 'rev-list --topo-order ends with root commit' '
	(
	cd rl-repo &&
	grit rev-list --topo-order HEAD >out &&
	LAST=$(tail -1 out) &&
	grit rev-list --topo-order --reverse HEAD >rev &&
	FIRST_REV=$(head -1 rev) &&
	test "$LAST" = "$FIRST_REV"
	)
'

###########################################################################
# Section 4: --parents
###########################################################################

test_expect_success 'rev-list --parents shows parent hashes' '
	(
	cd rl-repo &&
	grit rev-list --parents HEAD >out &&
	test_line_count = 3 out
	)
'

test_expect_success 'rev-list --parents: root commit has no parent listed' '
	(
	cd rl-repo &&
	grit rev-list --parents HEAD >out &&
	# Find the root commit (the one with only 1 field = no parent)
	found_root=false &&
	while read line; do
		set -- $line &&
		if test $# -eq 1; then
			found_root=true
		fi
	done <out &&
	test "$found_root" = "true"
	)
'

test_expect_success 'rev-list --parents: non-root commits have exactly one parent' '
	(
	cd rl-repo &&
	grit rev-list --parents HEAD >out &&
	non_root_count=0 &&
	while read line; do
		set -- $line &&
		if test $# -eq 2; then
			non_root_count=$(($non_root_count + 1))
		fi
	done <out &&
	test "$non_root_count" -eq 2
	)
'

test_expect_success 'rev-list --parents: parent chain is consistent with rev-parse' '
	(
	cd rl-repo &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	PARENT_OID=$(grit rev-parse HEAD~1) &&
	grit rev-list --parents HEAD >out &&
	grep "$HEAD_OID $PARENT_OID" out
	)
'

###########################################################################
# Section 5: --count
###########################################################################

test_expect_success 'rev-list --count HEAD gives commit count' '
	(
	cd rl-repo &&
	COUNT=$(grit rev-list --count HEAD) &&
	test "$COUNT" = "3"
	)
'

test_expect_success 'rev-list --count with ^ exclusion' '
	(
	cd rl-repo &&
	COUNT=$(grit rev-list --count HEAD ^HEAD~1) &&
	test "$COUNT" = "1"
	)
'

test_expect_success 'rev-list --count HEAD ^HEAD is zero' '
	(
	cd rl-repo &&
	COUNT=$(grit rev-list --count HEAD ^HEAD) &&
	test "$COUNT" = "0"
	)
'

###########################################################################
# Section 6: --reverse
###########################################################################

test_expect_success 'rev-list --reverse reverses the output' '
	(
	cd rl-repo &&
	grit rev-list HEAD >normal &&
	grit rev-list --reverse HEAD >reversed &&
	test_line_count = 3 reversed &&
	FIRST_NORMAL=$(head -1 normal) &&
	LAST_REVERSED=$(tail -1 reversed) &&
	test "$FIRST_NORMAL" = "$LAST_REVERSED"
	)
'

test_expect_success 'rev-list --reverse preserves commit count' '
	(
	cd rl-repo &&
	grit rev-list --reverse HEAD >out &&
	test_line_count = 3 out
	)
'

###########################################################################
# Section 7: --max-count
###########################################################################

test_expect_success 'rev-list --max-count=1 shows one commit' '
	(
	cd rl-repo &&
	grit rev-list --max-count=1 HEAD >out &&
	test_line_count = 1 out
	)
'

test_expect_success 'rev-list --max-count=2 shows two commits' '
	(
	cd rl-repo &&
	grit rev-list --max-count=2 HEAD >out &&
	test_line_count = 2 out
	)
'

test_expect_success 'rev-list --max-count=0 shows nothing' '
	(
	cd rl-repo &&
	grit rev-list --max-count=0 HEAD >out &&
	test_line_count = 0 out
	)
'

test_expect_success 'rev-list --max-count larger than history shows all' '
	(
	cd rl-repo &&
	grit rev-list --max-count=100 HEAD >out &&
	test_line_count = 3 out
	)
'

###########################################################################
# Section 8: --skip
###########################################################################

test_expect_success 'rev-list --skip=1 skips one commit' '
	(
	cd rl-repo &&
	grit rev-list --skip=1 HEAD >out &&
	test_line_count = 2 out
	)
'

test_expect_success 'rev-list --skip=N with N >= count gives empty' '
	(
	cd rl-repo &&
	grit rev-list --skip=10 HEAD >out &&
	test_line_count = 0 out
	)
'

test_expect_success 'rev-list --skip combined with --max-count' '
	(
	cd rl-repo &&
	grit rev-list --skip=1 --max-count=1 HEAD >out &&
	test_line_count = 1 out
	)
'

###########################################################################
# Section 9: Range exclusion with ^
###########################################################################

test_expect_success 'rev-list HEAD ^HEAD~1 shows one commit' '
	(
	cd rl-repo &&
	grit rev-list HEAD ^HEAD~1 >out &&
	test_line_count = 1 out
	)
'

test_expect_success 'rev-list HEAD ^HEAD shows nothing' '
	(
	cd rl-repo &&
	grit rev-list HEAD ^HEAD >out &&
	test_line_count = 0 out
	)
'

test_expect_success 'rev-list with ^ exclusion only shows reachable-but-excluded set' '
	(
	cd rl-repo &&
	grit rev-list HEAD ^HEAD~2 >out &&
	test_line_count = 2 out
	)
'

###########################################################################
# Section 10: --format
###########################################################################

test_expect_success 'rev-list --format="%H" shows full hashes' '
	(
	cd rl-repo &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	grit rev-list --format="%H" HEAD >out &&
	grep "$HEAD_OID" out
	)
'

test_expect_success 'rev-list --format="%h" shows abbreviated hashes' '
	(
	cd rl-repo &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	SHORT=$(echo "$HEAD_OID" | cut -c1-7) &&
	grit rev-list --format="%h" HEAD >out &&
	grep "$SHORT" out
	)
'

test_expect_success 'rev-list --format="%s" shows commit subjects' '
	(
	cd rl-repo &&
	grit rev-list --format="%s" HEAD >out &&
	grep "c1" out &&
	grep "c2" out &&
	grep "c3" out
	)
'

test_expect_success 'rev-list --format="%H %s" shows hash and subject' '
	(
	cd rl-repo &&
	grit rev-list --format="%H %s" HEAD >out &&
	HEAD_OID=$(grit rev-parse HEAD) &&
	grep "$HEAD_OID c3" out
	)
'

test_expect_success 'rev-list --format output includes commit header lines' '
	(
	cd rl-repo &&
	grit rev-list --format="%H" HEAD >out &&
	grep "^commit " out
	)
'

###########################################################################
# Section 11: --first-parent with merge
###########################################################################

test_expect_success 'setup: create merge commit' '
	(
	cd rl-repo &&
	grit checkout -b side HEAD~1 &&
	echo "side" >side.txt &&
	grit add side.txt &&
	grit commit -m "side commit" &&
	grit checkout master &&
	/usr/bin/git merge side -m "merge" 2>/dev/null &&
	grit rev-list HEAD >all_after_merge &&
	test_line_count = 5 all_after_merge
	)
'

test_expect_success 'rev-list --first-parent has fewer commits than full traversal' '
	(
	cd rl-repo &&
	grit rev-list --first-parent HEAD >fp_out &&
	grit rev-list HEAD >full_out &&
	fp_count=$(wc -l <fp_out) &&
	full_count=$(wc -l <full_out) &&
	test "$fp_count" -lt "$full_count"
	)
'

test_expect_success 'rev-list --first-parent shows 4 commits (skips side)' '
	(
	cd rl-repo &&
	grit rev-list --first-parent HEAD >out &&
	test_line_count = 4 out
	)
'

test_expect_success 'rev-list --parents on merge shows two parents' '
	(
	cd rl-repo &&
	MERGE_OID=$(grit rev-parse HEAD) &&
	grit rev-list --parents HEAD >out &&
	MERGE_LINE=$(grep "^$MERGE_OID" out) &&
	set -- $MERGE_LINE &&
	test $# -eq 3
	)
'

test_expect_success 'rev-list --first-parent --count' '
	(
	cd rl-repo &&
	COUNT=$(grit rev-list --first-parent --count HEAD) &&
	test "$COUNT" = "4"
	)
'

test_expect_success 'rev-list --first-parent --reverse' '
	(
	cd rl-repo &&
	grit rev-list --first-parent HEAD >normal &&
	grit rev-list --first-parent --reverse HEAD >reversed &&
	test_line_count = 4 reversed &&
	FIRST_NORMAL=$(head -1 normal) &&
	LAST_REVERSED=$(tail -1 reversed) &&
	test "$FIRST_NORMAL" = "$LAST_REVERSED"
	)
'

test_done
