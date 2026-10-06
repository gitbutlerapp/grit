#!/bin/sh
# Tests for grit show-ref with --hash, --abbrev, --verify, --exists,
# --head, --tags, --branches, -d (dereference), and -q options.

test_description='grit show-ref hash abbreviation and filtering options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with branches and tags' '
	(
	grit init --initial-branch=master repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo one >one.txt &&
	grit add . &&
	grit commit -m "first" &&
	grit branch alpha &&
	grit branch beta &&
	echo two >two.txt &&
	grit add . &&
	grit commit -m "second" &&
	grit tag v1.0 &&
	grit tag -a v2.0 -m "annotated tag" &&
	grit tag v3.0
	)
'

###########################################################################
# Section 2: Basic show-ref
###########################################################################

test_expect_success 'show-ref lists all refs' '
	(
	cd repo &&
	grit show-ref >actual &&
	test $(wc -l <actual) -ge 5
	)
'

test_expect_success 'show-ref output format is OID SP refname' '
	(
	cd repo &&
	grit show-ref >actual &&
	while IFS= read -r line; do
		echo "$line" | grep -qE "^[0-9a-f]{40} refs/" || return 1
	done <actual
	)
'

test_expect_success 'show-ref includes branches and tags' '
	(
	cd repo &&
	grit show-ref >actual &&
	grep "refs/heads/" actual &&
	grep "refs/tags/" actual
	)
'

###########################################################################
# Section 3: --hash
###########################################################################

test_expect_success 'show-ref --hash shows only full OIDs' '
	(
	cd repo &&
	grit show-ref --hash >actual &&
	while IFS= read -r line; do
		echo "$line" | grep -qE "^[0-9a-f]{40}$" || return 1
	done <actual
	)
'

test_expect_success 'show-ref --hash line count matches show-ref' '
	(
	cd repo &&
	grit show-ref >full &&
	grit show-ref --hash >hashes &&
	test $(wc -l <full) -eq $(wc -l <hashes)
	)
'

test_expect_success 'show-ref --hash=7 shows abbreviated OIDs' '
	(
	cd repo &&
	grit show-ref --hash=7 >actual &&
	while IFS= read -r line; do
		len=$(echo "$line" | wc -c) &&
		# wc -c includes newline, so 7 chars + newline = 8
		test "$len" -eq 8 || return 1
	done <actual
	)
'

test_expect_success 'show-ref --hash=4 shows 4-char OIDs' '
	(
	cd repo &&
	grit show-ref --hash=4 >actual &&
	while IFS= read -r line; do
		len=$(echo "$line" | wc -c) &&
		test "$len" -eq 5 || return 1
	done <actual
	)
'

test_expect_success 'show-ref -s is alias for --hash' '
	(
	cd repo &&
	grit show-ref --hash >hash_out &&
	grit show-ref -s >s_out &&
	test_cmp hash_out s_out
	)
'

test_expect_success 'show-ref --hash=7 produces consistent output' '
	(
	cd repo &&
	grit show-ref --hash=7 >hash7a &&
	grit show-ref --hash=7 >hash7b &&
	test_cmp hash7a hash7b
	)
'

###########################################################################
# Section 4: --abbrev
###########################################################################

test_expect_success 'show-ref --abbrev shows abbreviated OIDs with refnames' '
	(
	cd repo &&
	grit show-ref --abbrev >actual &&
	grep "refs/heads/master" actual &&
	head -1 actual | cut -d" " -f1 >oid &&
	len=$(wc -c <oid) &&
	test "$len" -le 41
	)
'

test_expect_success 'show-ref --abbrev=7 shows 7-char OIDs with refnames' '
	(
	cd repo &&
	grit show-ref --abbrev=7 >actual &&
	head -1 actual | cut -d" " -f1 >oid &&
	len=$(wc -c <oid) &&
	test "$len" -eq 8
	)
'

test_expect_success 'show-ref --abbrev line count matches show-ref' '
	(
	cd repo &&
	grit show-ref >full &&
	grit show-ref --abbrev >abbr &&
	test $(wc -l <full) -eq $(wc -l <abbr)
	)
'

###########################################################################
# Section 5: --tags and --branches
###########################################################################

test_expect_success 'show-ref --tags shows only tag refs' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	grep "refs/tags/" actual &&
	! grep "refs/heads/" actual
	)
'

test_expect_success 'show-ref --branches shows only branch refs' '
	(
	cd repo &&
	grit show-ref --branches >actual &&
	grep "refs/heads/" actual &&
	! grep "refs/tags/" actual
	)
'

test_expect_success 'show-ref --tags has correct count' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	test $(wc -l <actual) -ge 3
	)
'

test_expect_success 'show-ref --branches has correct count' '
	(
	cd repo &&
	grit show-ref --branches >actual &&
	test $(wc -l <actual) -ge 3
	)
'

###########################################################################
# Section 6: --head
###########################################################################

test_expect_success 'show-ref --head includes HEAD line' '
	(
	cd repo &&
	grit show-ref --head >actual &&
	grep "HEAD" actual
	)
'

test_expect_success 'show-ref --head has one more line than plain' '
	(
	cd repo &&
	grit show-ref >plain &&
	grit show-ref --head >with_head &&
	plain_lines=$(wc -l <plain) &&
	head_lines=$(wc -l <with_head) &&
	test $head_lines -eq $((plain_lines + 1))
	)
'

###########################################################################
# Section 7: --verify
###########################################################################

test_expect_success 'show-ref --verify with valid full ref succeeds' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/master >actual &&
	grep "refs/heads/master" actual
	)
'

test_expect_success 'show-ref --verify with nonexistent ref fails' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify refs/heads/nonexistent
	)
'

test_expect_success 'show-ref --verify with tag ref succeeds' '
	(
	cd repo &&
	grit show-ref --verify refs/tags/v1.0 >actual &&
	grep "refs/tags/v1.0" actual
	)
'

test_expect_success 'show-ref --verify returns single line' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/master >actual &&
	test $(wc -l <actual) -eq 1
	)
'

###########################################################################
# Section 8: --exists
###########################################################################

test_expect_success 'show-ref --exists with valid ref exits 0' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/master
	)
'

test_expect_success 'show-ref --exists with nonexistent ref exits non-zero' '
	(
	cd repo &&
	test_must_fail grit show-ref --exists refs/heads/nonexistent
	)
'

test_expect_success 'show-ref --exists with tag ref exits 0' '
	(
	cd repo &&
	grit show-ref --exists refs/tags/v1.0
	)
'

test_expect_success 'show-ref --exists produces no output on success' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/master >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 9: -d (dereference)
###########################################################################

test_expect_success 'show-ref -d shows peeled entries for annotated tags' '
	(
	cd repo &&
	grit show-ref -d >actual &&
	grep "\\^{}" actual
	)
'

test_expect_success 'show-ref -d peeled entry for v2.0 points to commit' '
	(
	cd repo &&
	grit show-ref -d >actual &&
	grep "refs/tags/v2.0\\^{}" actual
	)
'

test_expect_success 'show-ref -d has more lines than plain show-ref' '
	(
	cd repo &&
	grit show-ref >plain &&
	grit show-ref -d >deref &&
	test $(wc -l <deref) -gt $(wc -l <plain)
	)
'

###########################################################################
# Section 10: -q (quiet)
###########################################################################

test_expect_success 'show-ref -q --verify produces no output on success' '
	(
	cd repo &&
	grit show-ref -q --verify refs/heads/master >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'show-ref -q --verify exits non-zero for missing ref' '
	(
	cd repo &&
	test_must_fail grit show-ref -q --verify refs/heads/nope
	)
'

###########################################################################
# Section 11: Pattern matching
###########################################################################

test_expect_success 'show-ref with pattern filters refs' '
	(
	cd repo &&
	grit show-ref refs/tags/v1.0 >actual &&
	test $(wc -l <actual) -eq 1 &&
	grep "refs/tags/v1.0" actual
	)
'

###########################################################################
# Section 12: Cross-check with real git
###########################################################################

test_expect_success 'setup cross-check repo' '
	(
	$REAL_GIT init --initial-branch=master cross &&
	cd cross &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo abc >abc.txt &&
	$REAL_GIT add . &&
	$REAL_GIT commit -m "init" &&
	$REAL_GIT branch br1 &&
	$REAL_GIT tag t1 &&
	$REAL_GIT tag -a t2 -m "annotated"
	)
'

test_expect_success 'show-ref output matches real git' '
	(
	cd cross &&
	grit show-ref >grit_out &&
	$REAL_GIT show-ref >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'show-ref --hash output matches real git' '
	(
	cd cross &&
	grit show-ref --hash >grit_out &&
	$REAL_GIT show-ref --hash >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'show-ref --hash=7 output matches real git' '
	(
	cd cross &&
	grit show-ref --hash=7 >grit_out &&
	$REAL_GIT show-ref --hash=7 >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'show-ref --tags output matches real git' '
	(
	cd cross &&
	grit show-ref --tags >grit_out &&
	$REAL_GIT show-ref --tags >git_out &&
	test_cmp grit_out git_out
	)
'

test_done
