#!/bin/sh
# Tests for grit show-ref with --dereference, --hash, --abbrev, --head, --verify.

test_description='grit show-ref dereference and extra options'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with tags' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "first" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "first commit" &&
	"$REAL_GIT" tag lightweight-tag &&
	"$REAL_GIT" tag -a -m "annotated v1" annotated-v1 &&
	echo "second" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit" &&
	"$REAL_GIT" tag -a -m "annotated v2" annotated-v2 &&
	"$REAL_GIT" branch side-branch HEAD~1
	)
'

###########################################################################
# Section 2: Basic show-ref
###########################################################################

test_expect_success 'show-ref lists all refs' '
	(
	cd repo &&
	grit show-ref >actual &&
	"$REAL_GIT" show-ref >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref output is non-empty' '
	(
	cd repo &&
	grit show-ref >actual &&
	test -s actual
	)
'

test_expect_success 'show-ref with pattern main' '
	(
	cd repo &&
	grit show-ref main >actual &&
	"$REAL_GIT" show-ref main >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref with pattern annotated-v1' '
	(
	cd repo &&
	grit show-ref annotated-v1 >actual &&
	"$REAL_GIT" show-ref annotated-v1 >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: --dereference
###########################################################################

test_expect_success 'show-ref --dereference shows peeled tags' '
	(
	cd repo &&
	grit show-ref --dereference >actual &&
	"$REAL_GIT" show-ref --dereference >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref -d is alias for --dereference' '
	(
	cd repo &&
	grit show-ref -d >actual &&
	"$REAL_GIT" show-ref -d >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'dereference shows ^{} entries for annotated tags' '
	(
	cd repo &&
	grit show-ref -d >actual &&
	grep "\\^{}" actual >peeled &&
	test -s peeled
	)
'

test_expect_success 'dereference ^{} lines point to commit objects' '
	(
	cd repo &&
	grit show-ref -d >actual &&
	grep "\\^{}" actual | while read oid ref; do
		type=$("$REAL_GIT" cat-file -t "$oid") &&
		test "$type" = "commit" || return 1
	done
	)
'

test_expect_success 'dereference with tag pattern' '
	(
	cd repo &&
	grit show-ref -d annotated-v1 >actual &&
	"$REAL_GIT" show-ref -d annotated-v1 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'lightweight tag has no ^{} line with -d' '
	(
	cd repo &&
	grit show-ref -d lightweight-tag >actual &&
	! grep "\\^{}" actual
	)
'

test_expect_success 'annotated tag has ^{} line with -d' '
	(
	cd repo &&
	grit show-ref -d annotated-v1 >actual &&
	grep "\\^{}" actual
	)
'

test_expect_success 'dereference with annotated-v2 pattern' '
	(
	cd repo &&
	grit show-ref -d annotated-v2 >actual &&
	"$REAL_GIT" show-ref -d annotated-v2 >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 4: --hash
###########################################################################

test_expect_success 'show-ref --hash shows only OIDs' '
	(
	cd repo &&
	grit show-ref --hash >actual &&
	"$REAL_GIT" show-ref --hash >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --hash output has no refnames' '
	(
	cd repo &&
	grit show-ref --hash >actual &&
	! grep "refs/" actual
	)
'

test_expect_success 'show-ref --hash lines are 40 hex chars' '
	(
	cd repo &&
	grit show-ref --hash >actual &&
	while read line; do
		echo "$line" | grep -qE "^[0-9a-f]{40}$" || return 1
	done <actual
	)
'

test_expect_success 'show-ref -s is alias for --hash' '
	(
	cd repo &&
	grit show-ref -s >actual &&
	"$REAL_GIT" show-ref -s >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 5: --head
###########################################################################

test_expect_success 'show-ref --head includes HEAD' '
	(
	cd repo &&
	grit show-ref --head >actual &&
	"$REAL_GIT" show-ref --head >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --head has HEAD as first line' '
	(
	cd repo &&
	grit show-ref --head >actual &&
	head -1 actual | grep "HEAD"
	)
'

test_expect_success 'show-ref without --head does not show HEAD' '
	(
	cd repo &&
	grit show-ref >actual &&
	! grep "HEAD" actual
	)
'

test_expect_success 'show-ref --head --hash' '
	(
	cd repo &&
	grit show-ref --head --hash >actual &&
	"$REAL_GIT" show-ref --head --hash >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 6: --verify
###########################################################################

test_expect_success 'show-ref --verify with full refname' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/main >actual &&
	"$REAL_GIT" show-ref --verify refs/heads/main >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --verify with nonexistent ref fails' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify refs/heads/nonexistent
	)
'

test_expect_success 'show-ref --verify refs/tags/annotated-v1' '
	(
	cd repo &&
	grit show-ref --verify refs/tags/annotated-v1 >actual &&
	"$REAL_GIT" show-ref --verify refs/tags/annotated-v1 >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --verify HEAD' '
	(
	cd repo &&
	grit show-ref --verify HEAD >actual &&
	"$REAL_GIT" show-ref --verify HEAD >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 7: --exists
###########################################################################

test_expect_success 'show-ref --exists for existing ref succeeds' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/main
	)
'

test_expect_success 'show-ref --exists for nonexistent ref fails' '
	(
	cd repo &&
	test_must_fail grit show-ref --exists refs/heads/nonexistent
	)
'

test_expect_success 'show-ref --exists for tag ref succeeds' '
	(
	cd repo &&
	grit show-ref --exists refs/tags/annotated-v1
	)
'

###########################################################################
# Section 8: --abbrev
###########################################################################

test_expect_success 'show-ref --abbrev shortens OIDs' '
	(
	cd repo &&
	grit show-ref --abbrev >actual &&
	"$REAL_GIT" show-ref --abbrev >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --abbrev=7 uses 7-char OIDs' '
	(
	cd repo &&
	grit show-ref --abbrev=7 >actual &&
	"$REAL_GIT" show-ref --abbrev=7 >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 9: --tags and --heads filters
###########################################################################

test_expect_success 'show-ref --tags only shows tags' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	"$REAL_GIT" show-ref --tags >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --tags output has no heads' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	! grep "refs/heads/" actual
	)
'

test_expect_success 'show-ref --heads only shows branches' '
	(
	cd repo &&
	grit show-ref --heads >actual &&
	"$REAL_GIT" show-ref --heads >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --heads output has no tags' '
	(
	cd repo &&
	grit show-ref --heads >actual &&
	! grep "refs/tags/" actual
	)
'

###########################################################################
# Section 10: --quiet
###########################################################################

test_expect_success 'show-ref --quiet --verify existing ref exits 0' '
	(
	cd repo &&
	grit show-ref --quiet --verify refs/heads/main
	)
'

test_expect_success 'show-ref --quiet --verify existing ref produces no output' '
	(
	cd repo &&
	grit show-ref --quiet --verify refs/heads/main >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'show-ref --quiet --verify nonexistent ref fails' '
	(
	cd repo &&
	test_must_fail grit show-ref --quiet --verify refs/heads/nope
	)
'

###########################################################################
# Section 11: Combined flags
###########################################################################

test_expect_success 'show-ref --head --dereference combined' '
	(
	cd repo &&
	grit show-ref --head --dereference >actual &&
	"$REAL_GIT" show-ref --head --dereference >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --tags --dereference combined' '
	(
	cd repo &&
	grit show-ref --tags --dereference >actual &&
	"$REAL_GIT" show-ref --tags --dereference >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --hash --dereference shows extra peeled lines' '
	(
	cd repo &&
	grit show-ref --hash --dereference >actual &&
	grit show-ref --hash >hash_only &&
	test $(wc -l <actual) -gt $(wc -l <hash_only)
	)
'

test_done
