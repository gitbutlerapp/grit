#!/bin/sh
# Test rev-list with --all, --count, --max-count, --reverse, --first-parent.

test_description='grit rev-list --all --count and related options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# Real git for merge (grit does not have merge subcommand).
REAL_GIT=/usr/bin/git

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository with linear history' '
	(
	grit init --initial-branch=master repo &&
	cd repo &&
	$REAL_GIT config user.email "test@test.com" &&
	$REAL_GIT config user.name "Tester" &&
	echo "first" >file.txt &&
	grit add file.txt &&
	grit commit -m "first" &&
	grit rev-parse HEAD >../SHA1 &&
	echo "second" >>file.txt &&
	grit add file.txt &&
	grit commit -m "second" &&
	grit rev-parse HEAD >../SHA2 &&
	echo "third" >>file.txt &&
	grit add file.txt &&
	grit commit -m "third" &&
	grit rev-parse HEAD >../SHA3
	)
'

###########################################################################
# Section 1: Basic rev-list
###########################################################################

test_expect_success 'rev-list HEAD lists all reachable commits' '
	(
	cd repo &&
	grit rev-list HEAD >out &&
	test_line_count = 3 out
	)
'

test_expect_success 'rev-list HEAD output is full 40-char hashes' '
	(
	cd repo &&
	grit rev-list HEAD >out &&
	hash=$(head -1 out) &&
	test ${#hash} -eq 40
	)
'

test_expect_success 'rev-list HEAD includes all known commits' '
	(
	cd repo &&
	grit rev-list HEAD >out &&
	grep $(cat ../SHA1) out &&
	grep $(cat ../SHA2) out &&
	grep $(cat ../SHA3) out
	)
'

###########################################################################
# Section 2: --count
###########################################################################

test_expect_success 'rev-list --count HEAD returns correct count' '
	(
	cd repo &&
	count=$(grit rev-list --count HEAD) &&
	test "$count" = "3"
	)
'

test_expect_success 'rev-list --count with single commit' '
	(
	cd repo &&
	count=$(grit rev-list --count $(cat ../SHA1)) &&
	test "$count" = "1"
	)
'

test_expect_success 'rev-list --count with two commits reachable' '
	(
	cd repo &&
	count=$(grit rev-list --count $(cat ../SHA2)) &&
	test "$count" = "2"
	)
'

###########################################################################
# Section 3: --all
###########################################################################

test_expect_success 'rev-list --all lists commits from all refs' '
	(
	cd repo &&
	grit rev-list --all >out &&
	test_line_count = 3 out
	)
'

test_expect_success 'rev-list --count --all counts all commits' '
	(
	cd repo &&
	count=$(grit rev-list --count --all) &&
	test "$count" = "3"
	)
'

###########################################################################
# Section 4: --max-count / -n
###########################################################################

test_expect_success 'rev-list --max-count=1 HEAD shows one commit' '
	(
	cd repo &&
	grit rev-list --max-count=1 HEAD >out &&
	test_line_count = 1 out
	)
'

test_expect_success 'rev-list --max-count=2 HEAD shows two commits' '
	(
	cd repo &&
	grit rev-list --max-count=2 HEAD >out &&
	test_line_count = 2 out
	)
'

test_expect_success 'rev-list -n 1 HEAD shows one commit' '
	(
	cd repo &&
	grit rev-list -n 1 HEAD >out &&
	test_line_count = 1 out
	)
'

test_expect_success 'rev-list --max-count larger than total shows all' '
	(
	cd repo &&
	grit rev-list --max-count=100 HEAD >out &&
	test_line_count = 3 out
	)
'

###########################################################################
# Section 5: --reverse
###########################################################################

test_expect_success 'rev-list --reverse HEAD reverses output order' '
	(
	cd repo &&
	grit rev-list HEAD >forward &&
	grit rev-list --reverse HEAD >reverse &&
	tail -1 forward >last_forward &&
	head -1 reverse >first_reverse &&
	test_cmp last_forward first_reverse
	)
'

test_expect_success 'rev-list --reverse has same count as forward' '
	(
	cd repo &&
	grit rev-list HEAD >forward &&
	grit rev-list --reverse HEAD >reverse &&
	fwd_count=$(wc -l <forward | tr -d " ") &&
	rev_count=$(wc -l <reverse | tr -d " ") &&
	test "$fwd_count" = "$rev_count"
	)
'

###########################################################################
# Section 6: --skip
###########################################################################

test_expect_success 'rev-list --skip=1 HEAD skips first commit' '
	(
	cd repo &&
	grit rev-list --skip=1 HEAD >out &&
	test_line_count = 2 out
	)
'

test_expect_success 'rev-list --skip=2 HEAD skips two commits' '
	(
	cd repo &&
	grit rev-list --skip=2 HEAD >out &&
	test_line_count = 1 out
	)
'

test_expect_success 'rev-list --skip=3 HEAD gives empty output' '
	(
	cd repo &&
	grit rev-list --skip=3 HEAD >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 7: Commit ranges
###########################################################################

test_expect_success 'rev-list HEAD ^SHA1 excludes SHA1 and ancestors' '
	(
	cd repo &&
	sha1=$(cat ../SHA1) &&
	grit rev-list HEAD ^$sha1 >out &&
	test_line_count = 2 out
	)
'

test_expect_success 'rev-list SHA2..HEAD shows commits after SHA2' '
	(
	cd repo &&
	sha2=$(cat ../SHA2) &&
	grit rev-list $sha2..HEAD >out &&
	test_line_count = 1 out
	)
'

test_expect_success 'rev-list HEAD..HEAD gives empty output' '
	(
	cd repo &&
	grit rev-list HEAD..HEAD >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 8: Branches and --all
###########################################################################

test_expect_success 'setup branch with extra commit' '
	(
	cd repo &&
	$REAL_GIT checkout -b feature &&
	echo "feature" >feature.txt &&
	grit add feature.txt &&
	grit commit -m "feature commit" &&
	grit rev-parse HEAD >../SHA_FEAT &&
	$REAL_GIT checkout master
	)
'

test_expect_success 'rev-list --all includes commits from all branches' '
	(
	cd repo &&
	grit rev-list --all >out &&
	grep $(cat ../SHA_FEAT) out &&
	grep $(cat ../SHA3) out
	)
'

test_expect_success 'rev-list --count --all counts across branches' '
	(
	cd repo &&
	count=$(grit rev-list --count --all) &&
	test "$count" = "4"
	)
'

test_expect_success 'rev-list master does not include feature-only commit' '
	(
	cd repo &&
	grit rev-list master >out &&
	! grep $(cat ../SHA_FEAT) out
	)
'

test_expect_success 'rev-list feature includes shared history' '
	(
	cd repo &&
	grit rev-list feature >out &&
	grep $(cat ../SHA3) out &&
	grep $(cat ../SHA_FEAT) out
	)
'

###########################################################################
# Section 9: With merges
###########################################################################

test_expect_success 'add commit on master before merge' '
	(
	cd repo &&
	echo "master extra" >master_extra.txt &&
	grit add master_extra.txt &&
	grit commit -m "master extra"
	)
'

test_expect_success 'setup merge commit' '
	(
	cd repo &&
	$REAL_GIT merge feature --no-edit &&
	grit rev-parse HEAD >../SHA_MERGE
	)
'

test_expect_success 'rev-list --count HEAD after merge counts all reachable' '
	(
	cd repo &&
	count=$(grit rev-list --count HEAD) &&
	test "$count" = "6"
	)
'

test_expect_success 'rev-list --first-parent HEAD follows only first parent' '
	(
	cd repo &&
	grit rev-list --first-parent HEAD >out &&
	! grep $(cat ../SHA_FEAT) out
	)
'

test_expect_success 'rev-list --first-parent has fewer commits than full walk' '
	(
	cd repo &&
	grit rev-list HEAD >full &&
	grit rev-list --first-parent HEAD >fp &&
	full_count=$(wc -l <full | tr -d " ") &&
	fp_count=$(wc -l <fp | tr -d " ") &&
	test "$fp_count" -lt "$full_count"
	)
'

###########################################################################
# Section 10: Edge cases
###########################################################################

test_expect_success 'rev-list with explicit commit SHA works' '
	(
	cd repo &&
	sha=$(cat ../SHA2) &&
	grit rev-list $sha >out &&
	test_line_count = 2 out
	)
'

test_expect_success 'rev-list --count with range' '
	(
	cd repo &&
	sha1=$(cat ../SHA1) &&
	count=$(grit rev-list --count HEAD ^$sha1) &&
	test "$count" -ge 2
	)
'

test_done
