#!/bin/sh
# Tests for grit show-ref with --heads, --tags, --verify, --exists, --hash, --dereference, --abbrev.

test_description='grit show-ref: heads, tags, verify, exists, hash, dereference, abbrev'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with branches and tags' '
	(
	"$REAL_GIT" init -b main repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "hello" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial commit" &&
	"$REAL_GIT" branch alpha &&
	"$REAL_GIT" branch beta &&
	echo "world" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit" &&
	"$REAL_GIT" branch gamma &&
	"$REAL_GIT" tag v1.0 alpha &&
	"$REAL_GIT" tag -a -m "annotated v2.0" v2.0 beta &&
	"$REAL_GIT" tag -a -m "annotated v3.0" v3.0 gamma &&
	"$REAL_GIT" tag v4.0 main
	)
'

###########################################################################
# Section 2: Basic listing
###########################################################################

test_expect_success 'show-ref: lists all refs' '
	(
	cd repo &&
	"$GUST_BIN" show-ref >actual &&
	"$REAL_GIT" show-ref >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'show-ref: output has hash and refname' '
	(
	cd repo &&
	"$GUST_BIN" show-ref >actual &&
	head -1 actual | grep -E "^[0-9a-f]{40} refs/"
	)
'

test_expect_success 'show-ref: lists more than one ref' '
	(
	cd repo &&
	"$GUST_BIN" show-ref >actual &&
	test $(wc -l <actual) -gt 1
	)
'

###########################################################################
# Section 3: --branches (same as --heads)
###########################################################################

test_expect_success 'show-ref --branches: only shows branches' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --branches >actual &&
	"$REAL_GIT" show-ref --heads >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'show-ref --branches: no tags in output' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --branches >actual &&
	! grep "refs/tags" actual
	)
'

test_expect_success 'show-ref --branches: all entries are heads' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --branches >actual &&
	! grep -v "refs/heads/" actual
	)
'

###########################################################################
# Section 4: --tags
###########################################################################

test_expect_success 'show-ref --tags: only shows tags' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --tags >actual &&
	"$REAL_GIT" show-ref --tags >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'show-ref --tags: no branches in output' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --tags >actual &&
	! grep "refs/heads" actual
	)
'

test_expect_success 'show-ref --tags: all entries are tags' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --tags >actual &&
	! grep -v "refs/tags/" actual
	)
'

###########################################################################
# Section 5: --verify
###########################################################################

test_expect_success 'show-ref --verify: exact ref lookup' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --verify refs/heads/main >actual &&
	"$REAL_GIT" show-ref --verify refs/heads/main >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'show-ref --verify: nonexistent ref fails' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" show-ref --verify refs/heads/nonexistent
	)
'

test_expect_success 'show-ref --verify: tag ref' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --verify refs/tags/v1.0 >actual &&
	"$REAL_GIT" show-ref --verify refs/tags/v1.0 >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'show-ref --verify: annotated tag ref' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --verify refs/tags/v2.0 >actual &&
	"$REAL_GIT" show-ref --verify refs/tags/v2.0 >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'show-ref --verify: multiple refs' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --verify refs/heads/main refs/heads/alpha >actual &&
	"$REAL_GIT" show-ref --verify refs/heads/main refs/heads/alpha >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 6: --exists
###########################################################################

test_expect_success 'show-ref --exists: existing ref succeeds' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --exists refs/heads/main
	)
'

test_expect_success 'show-ref --exists: nonexistent ref fails' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" show-ref --exists refs/heads/nonexistent
	)
'

test_expect_success 'show-ref --exists: tag ref succeeds' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --exists refs/tags/v1.0
	)
'

test_expect_success 'show-ref --exists: produces no output' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --exists refs/heads/main >actual &&
	test ! -s actual
	)
'

###########################################################################
# Section 7: --hash
###########################################################################

test_expect_success 'show-ref --hash: only shows hashes' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --hash refs/heads/main >actual &&
	"$REAL_GIT" show-ref --hash refs/heads/main >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'show-ref --hash: output is 40-char hex' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --hash refs/heads/main >actual &&
	grep -E "^[0-9a-f]{40}$" actual
	)
'

test_expect_success 'show-ref -s: short for --hash' '
	(
	cd repo &&
	"$GUST_BIN" show-ref -s refs/heads/main >actual &&
	"$REAL_GIT" show-ref -s refs/heads/main >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'show-ref --hash --branches: hash-only heads' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --hash --branches >actual &&
	"$REAL_GIT" show-ref --hash --heads >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 8: --dereference / -d
###########################################################################

test_expect_success 'show-ref -d: derefs annotated tags' '
	(
	cd repo &&
	"$GUST_BIN" show-ref -d --tags >actual &&
	"$REAL_GIT" show-ref -d --tags >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'show-ref --dereference: shows ^{} entries' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --dereference refs/tags/v2.0 >actual &&
	grep "\\^{}" actual
	)
'

test_expect_success 'show-ref -d: lightweight tag no ^{} extra' '
	(
	cd repo &&
	"$GUST_BIN" show-ref -d refs/tags/v1.0 >actual &&
	! grep "\\^{}" actual
	)
'

###########################################################################
# Section 9: --abbrev
###########################################################################

test_expect_success 'show-ref --abbrev: abbreviates hashes' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --abbrev refs/heads/main >actual &&
	"$REAL_GIT" show-ref --abbrev refs/heads/main >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'show-ref --abbrev=8: custom length' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --abbrev=8 refs/heads/main >actual &&
	"$REAL_GIT" show-ref --abbrev=8 refs/heads/main >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 10: --head
###########################################################################

test_expect_success 'show-ref --head: includes HEAD' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --head >actual &&
	"$REAL_GIT" show-ref --head >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'show-ref --head: HEAD is first line' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --head >actual &&
	head -1 actual | grep "HEAD"
	)
'

###########################################################################
# Section 11: Pattern matching
###########################################################################

test_expect_success 'show-ref: pattern filters refs' '
	(
	cd repo &&
	"$GUST_BIN" show-ref alpha >actual &&
	"$REAL_GIT" show-ref alpha >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'show-ref: pattern with no match returns failure' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" show-ref nonexistent-pattern
	)
'

###########################################################################
# Section 12: Quiet mode
###########################################################################

test_expect_success 'show-ref --quiet --verify: no output on success' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --quiet --verify refs/heads/main >actual &&
	test ! -s actual
	)
'

test_expect_success 'show-ref -q --verify: nonexistent ref fails quietly' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" show-ref -q --verify refs/heads/nonexistent 2>err &&
	test ! -s err
	)
'

test_done
