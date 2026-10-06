#!/bin/sh
# Tests for grit branch -m (move/rename) and branch management.

test_description='grit branch -m (move/rename) and branch management'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with commits on main' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test" &&
	"$REAL_GIT" config user.email "t@t.com" &&
	echo a >a.txt &&
	"$REAL_GIT" add a.txt &&
	"$REAL_GIT" commit -m "first" &&
	echo b >b.txt &&
	"$REAL_GIT" add b.txt &&
	"$REAL_GIT" commit -m "second" &&
	"$REAL_GIT" checkout -b feature &&
	echo c >c.txt &&
	"$REAL_GIT" add c.txt &&
	"$REAL_GIT" commit -m "feature commit" &&
	"$REAL_GIT" checkout main
	)
'

###########################################################################
# Section 2: branch -m (rename) with two args
###########################################################################

test_expect_success 'branch -m renames a branch' '
	(
	cd repo &&
	git branch topic &&
	git branch -m topic renamed &&
	git branch >out &&
	grep "renamed" out &&
	! grep "  topic" out
	)
'

test_expect_success 'branch -m preserves commit history' '
	(
	cd repo &&
	git log --oneline renamed >out &&
	grep "second" out
	)
'

test_expect_success 'branch -m old ref no longer resolves' '
	(
	cd repo &&
	test_must_fail git rev-parse --verify topic 2>/dev/null
	)
'

test_expect_success 'branch -m new ref resolves' '
	(
	cd repo &&
	git rev-parse --verify renamed
	)
'

test_expect_success 'branch -m fails if target exists' '
	(
	cd repo &&
	git branch existing &&
	test_must_fail git branch -m renamed existing
	)
'

test_expect_success 'branch -M force renames even if target exists' '
	(
	cd repo &&
	git branch -M renamed existing &&
	git branch >out &&
	grep "existing" out
	)
'

test_expect_success 'branch -m with two args renames old to new' '
	(
	cd repo &&
	git branch src-branch &&
	git branch -m src-branch dst-branch &&
	git branch >out &&
	grep "dst-branch" out &&
	! grep "  src-branch" out
	)
'

test_expect_success 'branch -m new ref points to same commit' '
	(
	cd repo &&
	git rev-parse dst-branch >out &&
	test -s out
	)
'

###########################################################################
# Section 3: branch -m with slashes
###########################################################################

test_expect_success 'branch -m to namespaced name' '
	(
	cd repo &&
	git branch flat-src &&
	git branch -m flat-src topic/one &&
	git branch >out &&
	grep "topic/one" out
	)
'

test_expect_success 'branch -m between namespaced names' '
	(
	cd repo &&
	git branch -m topic/one topic/two &&
	git branch >out &&
	grep "topic/two" out &&
	! grep "topic/one" out
	)
'

test_expect_success 'branch -m from namespaced to flat' '
	(
	cd repo &&
	git branch -m topic/two flat-dst &&
	git branch >out &&
	grep "flat-dst" out
	)
'

test_expect_success 'branch -m to deeply nested name' '
	(
	cd repo &&
	git branch -m flat-dst ns/deep/nested/name &&
	git branch >out &&
	grep "ns/deep/nested/name" out
	)
'

test_expect_success 'branch -m from deeply nested back to flat' '
	(
	cd repo &&
	git branch -m ns/deep/nested/name simple &&
	git branch >out &&
	grep "simple" out
	)
'

###########################################################################
# Section 4: branch listing
###########################################################################

test_expect_success 'branch lists all branches' '
	(
	cd repo &&
	git branch >out &&
	grep "main" out &&
	grep "feature" out
	)
'

test_expect_success 'branch marks current branch with asterisk' '
	(
	cd repo &&
	git branch >out &&
	grep "^[*] main" out
	)
'

test_expect_success 'branch -v shows commit subject' '
	(
	cd repo &&
	git branch -v >out &&
	grep "second" out
	)
'

test_expect_success 'branch --list with pattern' '
	(
	cd repo &&
	git branch --list "feat*" >out &&
	grep "feature" out
	)
'

test_expect_success 'branch --list with non-matching pattern returns no matching lines' '
	(
	cd repo &&
	git branch --list "zzz*" >out 2>&1 &&
	! grep "zzz" out
	)
'

###########################################################################
# Section 5: branch creation and deletion
###########################################################################

test_expect_success 'branch creates new branch' '
	(
	cd repo &&
	git branch new-branch &&
	git branch >out &&
	grep "new-branch" out
	)
'

test_expect_success 'branch -d deletes merged branch' '
	(
	cd repo &&
	git branch -d new-branch &&
	git branch >out &&
	! grep "new-branch" out
	)
'

test_expect_success 'branch -d deletes a fully merged branch' '
	(
	cd repo &&
	git branch merged-test &&
	git branch -d merged-test &&
	git branch >out &&
	! grep "merged-test" out
	)
'

test_expect_success 'branch -D force deletes unmerged branch' '
	(
	cd repo &&
	git branch temp-unmerged &&
	git checkout temp-unmerged &&
	echo x >x.txt &&
	git add x.txt &&
	git commit -m "unmerged work" &&
	git checkout main &&
	git branch -D temp-unmerged &&
	git branch >out &&
	! grep "temp-unmerged" out
	)
'

test_expect_success 'branch -d refuses to delete current branch' '
	(
	cd repo &&
	test_must_fail git branch -d main
	)
'

###########################################################################
# Section 6: branch -m after rename
###########################################################################

test_expect_success 'branch -m then delete renamed branch' '
	(
	cd repo &&
	git branch to-rename &&
	git branch -m to-rename was-renamed &&
	git branch -d was-renamed &&
	git branch >out &&
	! grep "was-renamed" out
	)
'

test_expect_success 'branch -m twice in sequence' '
	(
	cd repo &&
	git branch chain-a &&
	git branch -m chain-a chain-b &&
	git branch -m chain-b chain-c &&
	git branch >out &&
	grep "chain-c" out &&
	! grep "chain-a" out &&
	! grep "chain-b" out
	)
'

test_expect_success 'branch -m preserves target commit through renames' '
	(
	cd repo &&
	git rev-parse chain-c >chain &&
	git rev-parse main >m &&
	test_cmp chain m
	)
'

###########################################################################
# Section 7: branch pointing
###########################################################################

test_expect_success 'branch at specific commit' '
	(
	cd repo &&
	first_sha=$(git rev-parse HEAD~1) &&
	git branch at-first "$first_sha" &&
	test "$(git rev-parse at-first)" = "$first_sha"
	)
'

test_expect_success 'branch from tag-like ref' '
	(
	cd repo &&
	"$REAL_GIT" tag v1.0 HEAD &&
	git branch from-tag v1.0 &&
	test "$(git rev-parse from-tag)" = "$(git rev-parse v1.0)"
	)
'

test_expect_success 'branch --contains lists branches containing commit' '
	(
	cd repo &&
	git branch --contains main >out &&
	grep "main" out
	)
'

test_expect_success 'branch -m does not affect other branches' '
	(
	cd repo &&
	git branch canary &&
	canary_sha=$(git rev-parse canary) &&
	git branch another &&
	git branch -m another another-renamed &&
	test "$(git rev-parse canary)" = "$canary_sha"
	)
'

test_done
