#!/bin/sh
# Tests for ls-remote with refs, patterns, bare repos, and edge cases.

test_description='ls-remote extra scenarios'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

A=1111111111111111111111111111111111111111
B=2222222222222222222222222222222222222222
C=3333333333333333333333333333333333333333

test_expect_success 'setup: create local remote with several refs' '
	grit init remote &&
	cd remote &&
	grit update-ref refs/heads/main "$A" &&
	grit update-ref refs/heads/develop "$B" &&
	grit update-ref refs/heads/topic "$C" &&
	grit update-ref refs/tags/v1.0 "$A" &&
	grit update-ref refs/tags/v2.0 "$B" &&
	grit symbolic-ref HEAD refs/heads/main &&
	cd ..
'

# ── Basic listing ──────────────────────────────────────────────────────────

test_expect_success 'ls-remote lists HEAD' '
	grit ls-remote remote >actual &&
	grep HEAD actual
'

test_expect_success 'ls-remote lists branches' '
	grit ls-remote remote >actual &&
	grep refs/heads/main actual &&
	grep refs/heads/develop actual &&
	grep refs/heads/topic actual
'

test_expect_success 'ls-remote lists tags' '
	grit ls-remote remote >actual &&
	grep refs/tags/v1.0 actual &&
	grep refs/tags/v2.0 actual
'

test_expect_success 'ls-remote output has correct OIDs' '
	grit ls-remote remote >actual &&
	grep "$A" actual &&
	grep "$B" actual &&
	grep "$C" actual
'

# ── Filtering ─────────────────────────────────────────────────────────────

test_expect_success '--heads shows only branches' '
	grit ls-remote --heads remote >actual &&
	grep refs/heads/ actual &&
	! grep refs/tags/ actual
'

test_expect_success '--tags shows only tags' '
	grit ls-remote --tags remote >actual &&
	grep refs/tags/ actual &&
	! grep refs/heads/ actual
'

test_expect_success '--heads lists all branches' '
	grit ls-remote --heads remote >actual &&
	grep refs/heads/main actual &&
	grep refs/heads/develop actual &&
	grep refs/heads/topic actual
'

test_expect_success '--tags lists all tags' '
	grit ls-remote --tags remote >actual &&
	grep refs/tags/v1.0 actual &&
	grep refs/tags/v2.0 actual
'

# ── --refs excludes HEAD ──────────────────────────────────────────────────

test_expect_success '--refs excludes HEAD' '
	grit ls-remote --refs remote >actual &&
	! grep "HEAD" actual &&
	grep refs/ actual
'

test_expect_success '--refs --heads excludes HEAD and tags' '
	grit ls-remote --refs --heads remote >actual &&
	! grep HEAD actual &&
	! grep refs/tags/ actual &&
	grep refs/heads/ actual
'

test_expect_success '--refs --tags excludes HEAD and branches' '
	grit ls-remote --refs --tags remote >actual &&
	! grep HEAD actual &&
	! grep refs/heads/ actual &&
	grep refs/tags/ actual
'

# ── --symref ──────────────────────────────────────────────────────────────

test_expect_success '--symref shows symbolic ref line for HEAD' '
	grit ls-remote --symref remote >actual &&
	grep "^ref: refs/heads/main" actual
'

test_expect_success '--symref also shows normal refs' '
	grit ls-remote --symref remote >actual &&
	grep refs/heads/main actual &&
	grep refs/tags/ actual
'

# ── Pattern matching ──────────────────────────────────────────────────────

test_expect_success 'single pattern filters to matching ref' '
	printf "%s\trefs/heads/main\n" "$A" >expect &&
	grit ls-remote remote main >actual &&
	test_cmp expect actual
'

test_expect_success 'pattern matches tag too' '
	grit ls-remote remote v1.0 >actual &&
	grep refs/tags/v1.0 actual
'

test_expect_success 'no matching pattern returns empty' '
	grit ls-remote remote nonexistent >actual || true &&
	test_must_be_empty actual
'

test_expect_success 'pattern is case-sensitive' '
	grit ls-remote remote MAIN >actual || true &&
	! grep refs/heads/main actual
'

# ── -q (quiet) mode ──────────────────────────────────────────────────────

test_expect_success '-q produces no output' '
	grit ls-remote -q remote >actual &&
	test_must_be_empty actual
'

test_expect_success '-q exits 0 for valid repo' '
	grit ls-remote -q remote
'

# ── Bare repositories ─────────────────────────────────────────────────────

test_expect_success 'setup bare repository' '
	grit init --bare bare.git &&
	GIT_DIR=bare.git grit update-ref refs/heads/main "$A" &&
	GIT_DIR=bare.git grit update-ref refs/heads/topic "$B" &&
	GIT_DIR=bare.git grit update-ref refs/tags/release "$C" &&
	GIT_DIR=bare.git grit symbolic-ref HEAD refs/heads/main
'

test_expect_success 'ls-remote on bare repo lists refs' '
	grit ls-remote bare.git >actual &&
	grep refs/heads/main actual &&
	grep refs/heads/topic actual &&
	grep refs/tags/release actual
'

test_expect_success 'ls-remote --heads on bare repo' '
	grit ls-remote --heads bare.git >actual &&
	grep refs/heads/ actual &&
	! grep refs/tags/ actual
'

test_expect_success 'ls-remote --tags on bare repo' '
	grit ls-remote --tags bare.git >actual &&
	grep refs/tags/ actual &&
	! grep refs/heads/ actual
'

test_expect_success 'ls-remote --symref on bare repo shows symbolic ref' '
	grit ls-remote --symref bare.git >actual &&
	grep "^ref:" actual
'

# ── Packed refs ───────────────────────────────────────────────────────────

test_expect_success 'setup repo with packed-refs' '
	grit init packed-repo &&
	cd packed-repo &&
	grit update-ref refs/heads/main "$A" &&
	grit symbolic-ref HEAD refs/heads/main &&
	printf "%s refs/heads/packed\n" "$C" >.git/packed-refs &&
	cd ..
'

test_expect_success 'ls-remote reads packed-refs' '
	grit ls-remote packed-repo >actual &&
	grep refs/heads/packed actual &&
	grep "$C" actual
'

test_expect_success 'ls-remote shows both loose and packed refs' '
	grit ls-remote packed-repo >actual &&
	grep refs/heads/main actual &&
	grep refs/heads/packed actual
'

# ── Sorting ───────────────────────────────────────────────────────────────

test_expect_success 'branch refs are sorted alphabetically' '
	grit ls-remote --heads remote >actual &&
	cut -f2 actual >names &&
	sort names >sorted &&
	test_cmp sorted names
'

# ── Edge cases ────────────────────────────────────────────────────────────

test_expect_success 'ls-remote nonexistent path fails' '
	test_must_fail grit ls-remote /no/such/path 2>err &&
	test -s err
'

test_expect_success 'ls-remote on empty repo (no refs)' '
	grit init empty-repo &&
	grit ls-remote empty-repo >actual 2>&1 || true &&
	! grep refs/heads/ actual
'

test_expect_success 'ls-remote --help shows usage' '
	grit ls-remote --help >out 2>&1 &&
	grep -i "usage" out
'

test_done
