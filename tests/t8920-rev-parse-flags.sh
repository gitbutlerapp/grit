#!/bin/sh
# Tests for rev-parse flag handling and plumbing options.

test_description='rev-parse flags and plumbing options'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- setup -----------------------------------------------------------------

test_expect_success 'setup: create repo with commits and tags' '
	(
	git init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "base" >file.txt &&
	git add file.txt &&
	test_tick &&
	git commit -m "initial" &&
	echo "second" >>file.txt &&
	git add file.txt &&
	test_tick &&
	git commit -m "second" &&
	git tag v1.0 &&
	sha_second=$(git rev-parse HEAD) &&
	echo "third" >>file.txt &&
	git add file.txt &&
	test_tick &&
	git commit -m "third" &&
	git tag -a v2.0 -m "version 2" &&
	git branch feature "$sha_second"
	)
'

# -- --verify ---------------------------------------------------------------

test_expect_success 'rev-parse --verify HEAD resolves to SHA' '
	(
	cd repo &&
	grit rev-parse --verify HEAD >actual &&
	test $(wc -c <actual) -ge 40
	)
'

test_expect_success 'rev-parse --verify HEAD matches expected commit' '
	(
	cd repo &&
	grit rev-parse HEAD >expect &&
	grit rev-parse --verify HEAD >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse --verify with invalid ref fails' '
	(
	cd repo &&
	test_must_fail grit rev-parse --verify refs/heads/nonexistent 2>err
	)
'

test_expect_success 'rev-parse --verify with tag name' '
	(
	cd repo &&
	grit rev-parse v1.0 >a &&
	grit rev-parse --verify v1.0 >b &&
	test_cmp a b
	)
'

test_expect_success 'rev-parse --verify with annotated tag' '
	(
	cd repo &&
	grit rev-parse v2.0 >a &&
	grit rev-parse --verify v2.0 >b &&
	test_cmp a b
	)
'

# -- --git-dir and --show-toplevel -------------------------------------------

test_expect_success 'rev-parse --git-dir from repo root' '
	(
	cd repo &&
	grit rev-parse --git-dir >actual &&
	echo ".git" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse --git-dir from subdirectory' '
	(
	cd repo &&
	mkdir -p sub/deep &&
	cd sub/deep &&
	grit rev-parse --git-dir >actual &&
	# Should be a path ending in .git
	grep "\.git" actual
	)
'

test_expect_success 'rev-parse --show-toplevel from subdirectory' '
	(
	cd repo &&
	mkdir -p sub/deep &&
	expected=$(grit rev-parse --show-toplevel) &&
	cd sub/deep &&
	grit rev-parse --show-toplevel >actual &&
	echo "$expected" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse --show-toplevel from root' '
	(
	cd repo &&
	grit rev-parse --show-toplevel >actual &&
	test -s actual
	)
'

# -- --is-inside-work-tree and related --------------------------------------

test_expect_success 'rev-parse --is-inside-work-tree in worktree' '
	(
	cd repo &&
	grit rev-parse --is-inside-work-tree >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse --is-bare-repository in non-bare' '
	(
	cd repo &&
	grit rev-parse --is-bare-repository >actual &&
	echo "false" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse --is-bare-repository in bare repo' '
	(
	git init --bare bare.git &&
	cd bare.git &&
	grit rev-parse --is-bare-repository >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

# -- --short ----------------------------------------------------------------

test_expect_success 'rev-parse --short HEAD produces abbreviated hash' '
	(
	cd repo &&
	grit rev-parse --short HEAD >actual &&
	full=$(grit rev-parse HEAD) &&
	abbrev=$(cat actual) &&
	case "$full" in
	"$abbrev"*) true ;;
	*) false ;;
	esac
	)
'

test_expect_success 'rev-parse --short=10 HEAD produces 10-char hash' '
	(
	cd repo &&
	grit rev-parse --short=10 HEAD >actual &&
	len=$(tr -d "\n" <actual | wc -c) &&
	test "$len" -ge 10
	)
'

# -- multiple args ----------------------------------------------------------

test_expect_success 'rev-parse resolves HEAD to 40-char hex' '
	(
	cd repo &&
	grit rev-parse HEAD >actual &&
	test $(tr -d "\n" <actual | wc -c) = 40
	)
'

test_expect_success 'rev-parse resolves multiple refs at once' '
	(
	cd repo &&
	grit rev-parse HEAD HEAD^ HEAD~2 >actual &&
	test $(wc -l <actual) = 3
	)
'

test_expect_success 'rev-parse HEAD and HEAD^ differ' '
	(
	cd repo &&
	grit rev-parse HEAD >a &&
	grit rev-parse HEAD^ >b &&
	! test_cmp a b
	)
'

# -- --show-prefix ----------------------------------------------------------

test_expect_success 'rev-parse --show-prefix from subdirectory' '
	(
	cd repo &&
	mkdir -p sub/deep &&
	cd sub/deep &&
	grit rev-parse --show-prefix >actual &&
	echo "sub/deep/" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse --show-prefix from root is empty' '
	(
	cd repo &&
	grit rev-parse --show-prefix >actual &&
	test ! -s actual || test "$(cat actual)" = ""
	)
'

# -- ref^{commit} and ref^{tree} dereferencing ------------------------------

test_expect_success 'rev-parse v2.0^{commit} peels annotated tag' '
	(
	cd repo &&
	grit rev-parse v2.0^{commit} >actual &&
	grit rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-parse HEAD^{tree} resolves to tree' '
	(
	cd repo &&
	grit rev-parse HEAD^{tree} >actual &&
	test $(tr -d "\n" <actual | wc -c) = 40
	)
'

test_expect_success 'rev-parse HEAD^{tree} differs from HEAD' '
	(
	cd repo &&
	grit rev-parse HEAD^{tree} >tree &&
	grit rev-parse HEAD >commit &&
	! test_cmp tree commit
	)
'

# -- caret and tilde ancestry -----------------------------------------------

test_expect_success 'rev-parse HEAD^ same as HEAD~1' '
	(
	cd repo &&
	grit rev-parse HEAD^ >a &&
	grit rev-parse HEAD~1 >b &&
	test_cmp a b
	)
'

test_expect_success 'rev-parse HEAD~2 goes back two commits' '
	(
	cd repo &&
	grit rev-parse HEAD~2 >actual &&
	# Should differ from HEAD and HEAD~1
	grit rev-parse HEAD >head &&
	grit rev-parse HEAD~1 >parent &&
	! test_cmp actual head &&
	! test_cmp actual parent
	)
'

test_expect_success 'rev-parse HEAD^^ same as HEAD~2' '
	(
	cd repo &&
	grit rev-parse HEAD^^ >a &&
	grit rev-parse HEAD~2 >b &&
	test_cmp a b
	)
'

# -- full ref resolution ----------------------------------------------------

test_expect_success 'rev-parse refs/heads/feature resolves' '
	(
	cd repo &&
	grit rev-parse refs/heads/feature >actual &&
	test $(tr -d "\n" <actual | wc -c) = 40
	)
'

test_expect_success 'rev-parse refs/tags/v1.0 resolves' '
	(
	cd repo &&
	grit rev-parse refs/tags/v1.0 >actual &&
	test $(tr -d "\n" <actual | wc -c) = 40
	)
'

test_expect_success 'rev-parse refs/heads/master resolves to HEAD' '
	(
	cd repo &&
	grit rev-parse refs/heads/master >a &&
	grit rev-parse HEAD >b &&
	test_cmp a b
	)
'

test_expect_success 'rev-parse invalid ref fails' '
	(
	cd repo &&
	test_must_fail grit rev-parse --verify refs/heads/does-not-exist
	)
'

test_expect_success 'rev-parse annotated tag object vs commit differ' '
	(
	cd repo &&
	grit rev-parse v2.0 >tag_obj &&
	grit rev-parse v2.0^{commit} >commit_obj &&
	! test_cmp tag_obj commit_obj
	)
'

test_done
