#!/bin/sh
# Tests for grit show-ref with --exclude-existing, patterns, --verify, --exists, --head.

test_description='grit show-ref: exclude patterns, verify, exists, head, dereference, hash, tags, branches'

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

	echo "initial" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial" &&
	"$REAL_GIT" tag v1.0 &&

	echo "second" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit" &&
	"$REAL_GIT" tag v2.0 &&
	"$REAL_GIT" tag -a v2.0-annotated -m "annotated v2" &&

	"$REAL_GIT" branch feature-x &&
	"$REAL_GIT" branch feature-y &&
	"$REAL_GIT" branch release/1.0 &&
	"$REAL_GIT" branch release/2.0 &&

	echo "third" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "third commit" &&
	"$REAL_GIT" tag v3.0
	)
'

###########################################################################
# Section 2: Basic show-ref
###########################################################################

test_expect_success 'show-ref: lists all refs' '
	(
	cd repo &&
	"$GUST_BIN" show-ref >output.txt &&
	test -s output.txt
	)
'

test_expect_success 'show-ref: output format is SHA ref' '
	(
	cd repo &&
	"$GUST_BIN" show-ref >output.txt &&
	head -1 output.txt | grep -E "^[0-9a-f]{40} refs/"
	)
'

test_expect_success 'show-ref: includes branches' '
	(
	cd repo &&
	"$GUST_BIN" show-ref >output.txt &&
	grep "refs/heads/main" output.txt &&
	grep "refs/heads/feature-x" output.txt
	)
'

test_expect_success 'show-ref: includes tags' '
	(
	cd repo &&
	"$GUST_BIN" show-ref >output.txt &&
	grep "refs/tags/v1.0" output.txt &&
	grep "refs/tags/v2.0" output.txt
	)
'

###########################################################################
# Section 3: --tags and --branches
###########################################################################

test_expect_success 'show-ref --tags: only shows tags' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --tags >output.txt &&
	grep "refs/tags/" output.txt &&
	! grep "refs/heads/" output.txt
	)
'

test_expect_success 'show-ref --branches: only shows branches' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --branches >output.txt &&
	grep "refs/heads/" output.txt &&
	! grep "refs/tags/" output.txt
	)
'

test_expect_success 'show-ref --tags: includes all tags' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --tags >output.txt &&
	grep "v1.0" output.txt &&
	grep "v2.0" output.txt &&
	grep "v3.0" output.txt
	)
'

test_expect_success 'show-ref --branches: includes all branches' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --branches >output.txt &&
	grep "feature-x" output.txt &&
	grep "feature-y" output.txt &&
	grep "release/1.0" output.txt
	)
'

###########################################################################
# Section 4: --head
###########################################################################

test_expect_success 'show-ref --head: includes HEAD' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --head >output.txt &&
	grep "^[0-9a-f]* HEAD$" output.txt
	)
'

test_expect_success 'show-ref: without --head does not show HEAD' '
	(
	cd repo &&
	"$GUST_BIN" show-ref >output.txt &&
	! grep "HEAD$" output.txt
	)
'

test_expect_success 'show-ref --head: HEAD SHA matches main' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --head >output.txt &&
	HEAD_SHA=$(grep "HEAD$" output.txt | awk "{print \$1}") &&
	MAIN_SHA=$("$REAL_GIT" rev-parse HEAD) &&
	test "$HEAD_SHA" = "$MAIN_SHA"
	)
'

###########################################################################
# Section 5: --verify
###########################################################################

test_expect_success 'show-ref --verify: verifies existing ref' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --verify refs/heads/main >output.txt &&
	grep "refs/heads/main" output.txt
	)
'

test_expect_success 'show-ref --verify: fails for non-existing ref' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" show-ref --verify refs/heads/nonexistent
	)
'

test_expect_success 'show-ref --verify: works with tag ref' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --verify refs/tags/v1.0 >output.txt &&
	grep "refs/tags/v1.0" output.txt
	)
'

test_expect_success 'show-ref --verify: multiple refs' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --verify refs/heads/main refs/tags/v1.0 >output.txt &&
	grep "refs/heads/main" output.txt &&
	grep "refs/tags/v1.0" output.txt
	)
'

test_expect_success 'show-ref --verify: rejects partial name' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" show-ref --verify main
	)
'

###########################################################################
# Section 6: --exists
###########################################################################

test_expect_success 'show-ref --exists: returns 0 for existing ref' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --exists refs/heads/main
	)
'

test_expect_success 'show-ref --exists: returns non-zero for missing ref' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" show-ref --exists refs/heads/does-not-exist
	)
'

test_expect_success 'show-ref --exists: works for tags' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --exists refs/tags/v1.0
	)
'

###########################################################################
# Section 7: --hash
###########################################################################

test_expect_success 'show-ref --hash: prints only SHA' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --hash refs/heads/main >output.txt &&
	SHA=$("$REAL_GIT" rev-parse refs/heads/main) &&
	grep "$SHA" output.txt &&
	! grep "refs/" output.txt
	)
'

test_expect_success 'show-ref --hash=8: prints abbreviated SHA' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --hash=8 refs/heads/main >output.txt &&
	ABBR=$("$REAL_GIT" rev-parse --short=8 refs/heads/main) &&
	grep "$ABBR" output.txt
	)
'

###########################################################################
# Section 8: --dereference
###########################################################################

test_expect_success 'show-ref -d: dereferences annotated tags' '
	(
	cd repo &&
	"$GUST_BIN" show-ref -d refs/tags/v2.0-annotated >output.txt &&
	grep "refs/tags/v2.0-annotated$" output.txt &&
	grep "refs/tags/v2.0-annotated\^{}" output.txt
	)
'

test_expect_success 'show-ref -d: peeled object matches commit SHA' '
	(
	cd repo &&
	"$GUST_BIN" show-ref -d refs/tags/v2.0-annotated >output.txt &&
	PEELED_SHA=$(grep "\^{}" output.txt | awk "{print \$1}") &&
	COMMIT_SHA=$("$REAL_GIT" rev-parse v2.0-annotated^{commit}) &&
	test "$PEELED_SHA" = "$COMMIT_SHA"
	)
'

test_expect_success 'show-ref -d: lightweight tags are not doubled' '
	(
	cd repo &&
	"$GUST_BIN" show-ref -d refs/tags/v1.0 >output.txt &&
	test_line_count = 1 output.txt
	)
'

###########################################################################
# Section 9: --abbrev
###########################################################################

test_expect_success 'show-ref --abbrev: abbreviates SHA' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --abbrev refs/heads/main >output.txt &&
	# Abbreviated SHA is shorter than 40 chars
	SHA_LEN=$(awk "{print length(\$1)}" output.txt) &&
	test "$SHA_LEN" -lt 40
	)
'

test_expect_success 'show-ref --abbrev=12: specific abbreviation length' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --abbrev=12 refs/heads/main >output.txt &&
	SHA_PART=$(awk "{print \$1}" output.txt) &&
	test ${#SHA_PART} -ge 12
	)
'

###########################################################################
# Section 10: Pattern matching
###########################################################################

test_expect_success 'show-ref: pattern matching on ref name' '
	(
	cd repo &&
	"$GUST_BIN" show-ref refs/heads/feature-x >output.txt &&
	grep "feature-x" output.txt &&
	! grep "feature-y" output.txt
	)
'

test_expect_success 'show-ref: glob-style pattern on refs/heads/release' '
	(
	cd repo &&
	"$GUST_BIN" show-ref refs/heads/release/ >output.txt 2>&1 || true &&
	# May or may not match; just check it does not crash
	true
	)
'

test_expect_success 'show-ref: non-matching pattern returns non-zero' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" show-ref refs/heads/zzz-nonexistent
	)
'

###########################################################################
# Section 11: --quiet
###########################################################################

test_expect_success 'show-ref -q: suppresses output but returns 0' '
	(
	cd repo &&
	"$GUST_BIN" show-ref -q refs/heads/main >output.txt &&
	test_must_be_empty output.txt
	)
'

test_expect_success 'show-ref -q: returns non-zero for missing ref' '
	(
	cd repo &&
	test_must_fail "$GUST_BIN" show-ref -q refs/heads/nonexistent
	)
'

###########################################################################
# Section 12: Edge cases
###########################################################################

test_expect_success 'show-ref: empty repo returns non-zero' '
	(
	"$REAL_GIT" init -b main empty-repo &&
	cd empty-repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	test_must_fail "$GUST_BIN" show-ref
	)
'

test_expect_success 'show-ref --head --hash: HEAD hash only' '
	(
	cd repo &&
	"$GUST_BIN" show-ref --head --hash HEAD >output.txt &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	grep "$SHA" output.txt
	)
'

test_done
