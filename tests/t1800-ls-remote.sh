#!/bin/sh
# Tests for grit ls-remote (local path transport only).

test_description='ls-remote with local repository path'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# Two stable fake OIDs used throughout these tests.
A=1111111111111111111111111111111111111111
B=2222222222222222222222222222222222222222

test_expect_success 'setup: create a local remote repository' '
	grit init remote &&
	cd remote &&
	grit update-ref refs/heads/main "$A" &&
	grit update-ref refs/heads/topic "$B" &&
	grit update-ref refs/tags/v1.0 "$A" &&
	grit symbolic-ref HEAD refs/heads/main &&
	cd ..
'

test_expect_success 'ls-remote lists HEAD then refs in sorted order' '
	printf "%s\tHEAD\n" "$A" >expect &&
	printf "%s\trefs/heads/main\n" "$A" >>expect &&
	printf "%s\trefs/heads/topic\n" "$B" >>expect &&
	printf "%s\trefs/tags/v1.0\n" "$A" >>expect &&
	grit ls-remote remote >actual &&
	test_cmp expect actual
'

test_expect_success 'ls-remote --heads shows only branches' '
	printf "%s\trefs/heads/main\n" "$A" >expect &&
	printf "%s\trefs/heads/topic\n" "$B" >>expect &&
	grit ls-remote --heads remote >actual &&
	test_cmp expect actual
'

test_expect_success 'ls-remote --tags shows only tags' '
	printf "%s\trefs/tags/v1.0\n" "$A" >expect &&
	grit ls-remote --tags remote >actual &&
	test_cmp expect actual
'

test_expect_success 'ls-remote --refs excludes HEAD' '
	printf "%s\trefs/heads/main\n" "$A" >expect &&
	printf "%s\trefs/heads/topic\n" "$B" >>expect &&
	printf "%s\trefs/tags/v1.0\n" "$A" >>expect &&
	grit ls-remote --refs remote >actual &&
	test_cmp expect actual
'

test_expect_success 'ls-remote --symref shows symbolic ref line before HEAD' '
	printf "ref: refs/heads/main\tHEAD\n" >expect &&
	printf "%s\tHEAD\n" "$A" >>expect &&
	printf "%s\trefs/heads/main\n" "$A" >>expect &&
	printf "%s\trefs/heads/topic\n" "$B" >>expect &&
	printf "%s\trefs/tags/v1.0\n" "$A" >>expect &&
	grit ls-remote --symref remote >actual &&
	test_cmp expect actual
'

test_expect_success 'ls-remote -q produces no output and exits 0' '
	grit ls-remote -q remote >actual &&
	! test -s actual
'

test_expect_success 'ls-remote with pattern filters to matching refs' '
	printf "%s\trefs/heads/main\n" "$A" >expect &&
	grit ls-remote remote main >actual &&
	test_cmp expect actual
'

test_expect_success 'ls-remote reads packed-refs' '
	C=3333333333333333333333333333333333333333 &&
	grit init packed-remote &&
	GIT_DIR=packed-remote/.git grit update-ref refs/heads/main "$A" &&
	GIT_DIR=packed-remote/.git grit symbolic-ref HEAD refs/heads/main &&
	printf "%s refs/heads/packed-branch\n" "$C" \
		>packed-remote/.git/packed-refs &&
	printf "%s\tHEAD\n" "$A" >expect &&
	printf "%s\trefs/heads/main\n" "$A" >>expect &&
	printf "%s\trefs/heads/packed-branch\n" "$C" >>expect &&
	grit ls-remote packed-remote >actual &&
	test_cmp expect actual
'

test_expect_success 'ls-remote with multiple patterns' '
	printf "%s\trefs/heads/main\n" "$A" >expect &&
	printf "%s\trefs/tags/v1.0\n" "$A" >>expect &&
	grit ls-remote remote main v1.0 >actual &&
	test_cmp expect actual
'

test_expect_success 'ls-remote with no matching pattern returns empty' '
	: >expect &&
	grit ls-remote remote nonexistent >actual || true &&
	test_cmp expect actual
'

test_expect_success 'ls-remote --heads and --tags separately cover all non-HEAD refs' '
	grit ls-remote --heads remote >heads &&
	grit ls-remote --tags remote >tags &&
	test -s heads &&
	test -s tags &&
	! grep refs/tags/ heads &&
	! grep refs/heads/ tags
'

test_expect_success 'ls-remote --refs --heads excludes HEAD' '
	grit ls-remote --refs --heads remote >actual &&
	! grep HEAD actual &&
	grep refs/heads/ actual
'

test_expect_success 'ls-remote --refs --tags excludes HEAD' '
	grit ls-remote --refs --tags remote >actual &&
	! grep HEAD actual &&
	grep refs/tags/ actual
'

test_expect_success 'ls-remote on bare repository' '
	grit init --bare bare-remote &&
	GIT_DIR=bare-remote grit update-ref refs/heads/main "$A" &&
	GIT_DIR=bare-remote grit symbolic-ref HEAD refs/heads/main &&
	printf "%s\tHEAD\n" "$A" >expect &&
	printf "%s\trefs/heads/main\n" "$A" >>expect &&
	grit ls-remote bare-remote >actual &&
	test_cmp expect actual
'

test_expect_success 'ls-remote with many refs sorts correctly' '
	grit init many-remote &&
	cd many-remote &&
	grit update-ref refs/heads/alpha "$A" &&
	grit update-ref refs/heads/beta "$B" &&
	grit update-ref refs/heads/gamma "$A" &&
	grit symbolic-ref HEAD refs/heads/alpha &&
	cd .. &&
	grit ls-remote many-remote >actual &&
	grep refs/heads/alpha actual &&
	grep refs/heads/beta actual &&
	grep refs/heads/gamma actual
'

test_expect_success 'ls-remote refs are sorted alphabetically' '
	grit ls-remote --heads many-remote >actual &&
	cut -f2 actual >names &&
	sort names >sorted &&
	test_cmp sorted names
'

test_expect_success 'ls-remote -q returns exit 0 with no output' '
	grit ls-remote -q many-remote >actual &&
	! test -s actual
'

test_expect_success 'ls-remote --symref shows ref: line for HEAD' '
	grit ls-remote --symref many-remote >actual &&
	grep "^ref:" actual
'

test_expect_success 'ls-remote on empty repo returns empty or fails' '
	grit init empty-remote &&
	grit ls-remote empty-remote >actual 2>&1 || true &&
	! grep refs/ actual
'

test_expect_success 'ls-remote pattern matching: MAIN does not match main' '
	grit ls-remote remote MAIN >actual 2>&1 || true &&
	! grep refs/heads/main actual
'

test_expect_success 'ls-remote with wildcard-like pattern for topic' '
	grit ls-remote remote topic >actual &&
	printf "%s\trefs/heads/topic\n" "$B" >expect &&
	test_cmp expect actual
'

test_expect_success 'ls-remote nonexistent path fails' '
	test_must_fail grit ls-remote /nonexistent/path 2>err &&
	test -s err
'

test_expect_success 'ls-remote --help shows usage' '
	grit ls-remote --help >out 2>&1 &&
	grep -i "usage" out
'

test_done
