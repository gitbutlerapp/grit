#!/bin/sh
# Tests for show-ref pattern matching, --verify, --exists, --hash, --dereference, etc.

test_description='show-ref pattern matching and options'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup -----------------------------------------------------------------

test_expect_success 'setup: repo with branches and tags' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "base" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "first" &&
	$REAL_GIT tag v1.0 &&
	$REAL_GIT tag -a ann-v1.0 -m "annotated v1" &&
	echo "second" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "second" &&
	$REAL_GIT tag v2.0 &&
	$REAL_GIT branch feature &&
	$REAL_GIT branch bugfix &&
	echo "third" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "third" &&
	$REAL_GIT tag v3.0
	)
'

# -- basic show-ref ---------------------------------------------------------

test_expect_success 'show-ref lists all refs' '
	(
	cd repo &&
	grit show-ref >actual &&
	test $(wc -l <actual) -gt 0
	)
'

test_expect_success 'show-ref output matches git (sorted)' '
	(
	cd repo &&
	grit show-ref | sort >actual &&
	$REAL_GIT show-ref | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref each line has SHA and refname' '
	(
	cd repo &&
	grit show-ref >actual &&
	while IFS= read -r line; do
		sha=$(echo "$line" | awk "{print \$1}") &&
		ref=$(echo "$line" | awk "{print \$2}") &&
		test $(echo "$sha" | tr -d "\n" | wc -c) = 40 &&
		case "$ref" in refs/*) true ;; *) false ;; esac
	done <actual
	)
'

# -- pattern matching -------------------------------------------------------

test_expect_success 'show-ref refs/heads/ lists only branches' '
	(
	cd repo &&
	grit show-ref refs/heads/ | sort >actual &&
	$REAL_GIT show-ref refs/heads/ | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref refs/tags/ lists only tags' '
	(
	cd repo &&
	grit show-ref refs/tags/ | sort >actual &&
	$REAL_GIT show-ref refs/tags/ | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref with full ref name matches exactly' '
	(
	cd repo &&
	grit show-ref refs/heads/master >actual &&
	test $(wc -l <actual) = 1 &&
	grep refs/heads/master actual
	)
'

test_expect_success 'show-ref with non-matching pattern returns non-zero' '
	(
	cd repo &&
	test_must_fail grit show-ref refs/remotes/ >actual 2>/dev/null &&
	test ! -s actual
	)
'

# -- --tags and --branches ---------------------------------------------------

test_expect_success 'show-ref --tags shows only tags' '
	(
	cd repo &&
	grit show-ref --tags | sort >actual &&
	$REAL_GIT show-ref --tags | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --branches shows only branches' '
	(
	cd repo &&
	grit show-ref --branches | sort >actual &&
	# git uses --heads but grit uses --branches
	$REAL_GIT show-ref --heads | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'show-ref --tags does not include branches' '
	(
	cd repo &&
	grit show-ref --tags >actual &&
	! grep refs/heads/ actual
	)
'

test_expect_success 'show-ref --branches does not include tags' '
	(
	cd repo &&
	grit show-ref --branches >actual &&
	! grep refs/tags/ actual
	)
'

# -- --verify ---------------------------------------------------------------

test_expect_success 'show-ref --verify with exact ref succeeds' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/master >actual &&
	grep refs/heads/master actual
	)
'

test_expect_success 'show-ref --verify with non-existent ref fails' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify refs/heads/nonexistent 2>err
	)
'

test_expect_success 'show-ref --verify requires full refname' '
	(
	cd repo &&
	test_must_fail grit show-ref --verify master 2>err
	)
'

test_expect_success 'show-ref --verify with tag ref' '
	(
	cd repo &&
	grit show-ref --verify refs/tags/v1.0 >actual &&
	grep refs/tags/v1.0 actual
	)
'

# -- --exists ---------------------------------------------------------------

test_expect_success 'show-ref --exists for existing ref succeeds' '
	(
	cd repo &&
	grit show-ref --exists refs/heads/master
	)
'

test_expect_success 'show-ref --exists for missing ref fails' '
	(
	cd repo &&
	test_must_fail grit show-ref --exists refs/heads/nonexistent
	)
'

# -- --hash / -s ------------------------------------------------------------

test_expect_success 'show-ref --hash shows only SHAs' '
	(
	cd repo &&
	grit show-ref --hash refs/heads/master >actual &&
	sha=$(tr -d "\n" <actual) &&
	len=$(printf "%s" "$sha" | wc -c) &&
	test "$len" = 40
	)
'

test_expect_success 'show-ref -s shows only SHAs (same as --hash)' '
	(
	cd repo &&
	grit show-ref -s refs/heads/master >actual_s &&
	grit show-ref --hash refs/heads/master >actual_hash &&
	test_cmp actual_s actual_hash
	)
'

test_expect_success 'show-ref --tags --hash lists only SHAs for tags' '
	(
	cd repo &&
	grit show-ref --tags --hash >actual &&
	lines=$(wc -l <actual) &&
	test "$lines" -gt 0 &&
	while IFS= read -r line; do
		len=$(printf "%s" "$line" | wc -c) &&
		test "$len" = 40
	done <actual
	)
'

# -- --head -----------------------------------------------------------------

test_expect_success 'show-ref --head includes HEAD' '
	(
	cd repo &&
	grit show-ref --head >actual &&
	grep HEAD actual
	)
'

test_expect_success 'show-ref --head HEAD line matches rev-parse HEAD' '
	(
	cd repo &&
	grit show-ref --head >actual &&
	head_line=$(grep "HEAD$" actual | head -1) &&
	head_sha=$(echo "$head_line" | awk "{print \$1}") &&
	expected=$(grit rev-parse HEAD) &&
	test "$head_sha" = "$expected"
	)
'

# -- --dereference / -d ------------------------------------------------------

test_expect_success 'show-ref --dereference shows peeled annotated tags' '
	(
	cd repo &&
	grit show-ref --dereference refs/tags/ann-v1.0 >actual &&
	test $(wc -l <actual) = 2 &&
	grep "ann-v1.0$" actual &&
	grep "ann-v1.0\^{}$" actual
	)
'

test_expect_success 'show-ref -d peeled value matches tag target' '
	(
	cd repo &&
	grit show-ref -d refs/tags/ann-v1.0 >actual &&
	peeled=$(grep "\^{}$" actual | awk "{print \$1}") &&
	expected=$(grit rev-parse v1.0) &&
	test "$peeled" = "$expected"
	)
'

test_expect_success 'show-ref --dereference on lightweight tag shows one line' '
	(
	cd repo &&
	grit show-ref --dereference refs/tags/v1.0 >actual &&
	test $(wc -l <actual) = 1
	)
'

# -- --abbrev ---------------------------------------------------------------

test_expect_success 'show-ref --abbrev shortens SHAs' '
	(
	cd repo &&
	grit show-ref --abbrev refs/heads/master >actual &&
	sha=$(awk "{print \$1}" actual) &&
	len=$(echo "$sha" | tr -d "\n" | wc -c) &&
	test "$len" -lt 40
	)
'

# -- --quiet ----------------------------------------------------------------

test_expect_success 'show-ref --quiet --verify succeeds silently for existing ref' '
	(
	cd repo &&
	grit show-ref --quiet --verify refs/heads/master >actual &&
	test $(wc -c <actual) = 0
	)
'

test_expect_success 'show-ref --quiet --verify fails silently for missing ref' '
	(
	cd repo &&
	test_must_fail grit show-ref --quiet --verify refs/heads/nonexistent 2>err
	)
'

test_done
