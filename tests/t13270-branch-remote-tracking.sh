#!/bin/sh

test_description='grit branch: remote tracking and listing'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup origin repo' '
	grit init origin &&
	(cd origin &&
	 $REAL_GIT config user.email "t@t.com" &&
	 $REAL_GIT config user.name "T" &&
	 echo hello >file.txt &&
	 grit add file.txt &&
	 grit commit -m "initial" &&
	 grit branch feature &&
	 grit branch release)
'

test_expect_success 'setup local repo with remote' '
	grit init repo &&
	(cd repo &&
	 $REAL_GIT config user.email "t@t.com" &&
	 $REAL_GIT config user.name "T" &&
	 $REAL_GIT remote add origin ../origin &&
	 mkdir -p .git/objects/info &&
	 echo "$(cd ../origin && pwd)/.git/objects" >.git/objects/info/alternates &&
	 grit update-ref refs/remotes/origin/main "$($REAL_GIT -C ../origin rev-parse main)" &&
	 grit update-ref refs/remotes/origin/feature "$($REAL_GIT -C ../origin rev-parse feature)" &&
	 grit update-ref refs/remotes/origin/release "$($REAL_GIT -C ../origin rev-parse release)" &&
	 grit branch --track main origin/main)
'

# ── listing remote branches ──────────────────────────────────────────────

test_expect_success 'branch -r lists remote-tracking branches' '
	(cd repo && grit branch -r >../actual) &&
	grep "origin/main" actual
'

test_expect_success 'branch -r shows feature' '
	(cd repo && grit branch -r >../actual) &&
	grep "origin/feature" actual
'

test_expect_success 'branch -r shows release' '
	(cd repo && grit branch -r >../actual) &&
	grep "origin/release" actual
'

test_expect_success 'branch -r does not show local branches' '
	(cd repo &&
	 $REAL_GIT checkout main &&
	 grit branch -r >../actual) &&
	! grep "^  main" actual
'

# ── listing all branches ────────────────────────────────────────────────

test_expect_success 'branch -a lists local and remote branches' '
	(cd repo && grit branch -a >../actual) &&
	grep "main" actual &&
	grep "remotes/origin/main" actual
'

test_expect_success 'branch -a shows current branch with asterisk' '
	(cd repo && grit branch -a >../actual) &&
	grep "^\\* main" actual
'

# ── verbose listing ──────────────────────────────────────────────────────

test_expect_success 'branch -v shows commit hash' '
	(cd repo && grit branch -v >../actual) &&
	grep "[0-9a-f]" actual
'

test_expect_success 'branch -v shows commit subject' '
	(cd repo && grit branch -v >../actual) &&
	grep "initial" actual
'

test_expect_success 'branch -r -v shows remote branches with commit info' '
	(cd repo && grit branch -r -v >../actual) &&
	grep "origin/feature" actual &&
	grep "initial" actual
'

# ── tracking setup via git, verified by grit ─────────────────────────────

test_expect_success 'tracking config set by git is readable by grit' '
	(cd repo && grit config get branch.main.remote >../actual) &&
	echo "origin" >expect &&
	test_cmp expect actual
'

test_expect_success 'tracking merge ref is readable by grit' '
	(cd repo && grit config get branch.main.merge >../actual) &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'create tracked branch via git and verify config' '
	(cd repo &&
	 $REAL_GIT branch --track feat origin/feature &&
	 grit config get branch.feat.remote >../actual) &&
	echo "origin" >expect &&
	test_cmp expect actual
'

test_expect_success 'tracked branch merge ref is correct' '
	(cd repo && grit config get branch.feat.merge >../actual) &&
	echo "refs/heads/feature" >expect &&
	test_cmp expect actual
'

# ── --show-current ───────────────────────────────────────────────────────

test_expect_success 'branch --show-current shows current branch' '
	(cd repo && grit branch --show-current >../actual) &&
	echo "main" >expect &&
	test_cmp expect actual
'

test_expect_success 'branch --show-current after checkout' '
	(cd repo &&
	 $REAL_GIT checkout feat &&
	 grit branch --show-current >../actual) &&
	echo "feat" >expect &&
	test_cmp expect actual
'

test_expect_success 'switch back to main' '
	(cd repo && $REAL_GIT checkout main)
'

# ── --contains ───────────────────────────────────────────────────────────

test_expect_success 'branch --contains HEAD lists current branch' '
	(cd repo && grit branch --contains HEAD >../actual) &&
	grep "main" actual
'

test_expect_success 'branch --contains shows all branches with that commit' '
	(cd repo && grit branch --contains HEAD >../actual) &&
	grep "feat" actual
'

# ── --merged ─────────────────────────────────────────────────────────────

test_expect_success 'branch --merged HEAD lists merged branches' '
	(cd repo && grit branch --merged HEAD >../actual) &&
	grep "main" actual
'

test_expect_success 'branch --merged includes identical branches' '
	(cd repo && grit branch --merged HEAD >../actual) &&
	grep "feat" actual
'

# ── branch creation and deletion ─────────────────────────────────────────

test_expect_success 'create new branch' '
	(cd repo && grit branch newbranch &&
	 grit branch >../actual) &&
	grep "newbranch" actual
'

test_expect_success 'create branch at specific start point' '
	(cd repo && grit branch frombranch HEAD &&
	 grit branch >../actual) &&
	grep "frombranch" actual
'

test_expect_success 'delete branch with -d' '
	(cd repo && grit branch -d newbranch &&
	 grit branch >../actual) &&
	! grep "newbranch" actual
'

test_expect_success 'force delete with -D' '
	(cd repo && grit branch -D frombranch &&
	 grit branch >../actual) &&
	! grep "frombranch" actual
'

test_expect_success 'delete nonexistent branch fails' '
	(cd repo && test_must_fail grit branch -d nonexistent)
'

# ── branch rename ────────────────────────────────────────────────────────

test_expect_success 'rename branch with -m' '
	(cd repo &&
	 grit branch rename-me &&
	 grit branch -m rename-me renamed &&
	 grit branch >../actual) &&
	grep "renamed" actual &&
	! grep "rename-me" actual
'

test_expect_success 'rename to existing name fails without -M' '
	(cd repo && test_must_fail grit branch -m renamed main)
'

# ── branch copy ──────────────────────────────────────────────────────────

test_expect_success 'copy current branch with -c' '
	(cd repo &&
	 grit branch -c copy-of-main &&
	 grit branch >../actual) &&
	grep "copy-of-main" actual
'

# ── quiet mode ───────────────────────────────────────────────────────────

test_expect_success 'branch -q suppresses output on create' '
	(cd repo && grit branch -q silent-branch >../actual 2>&1) &&
	test_must_be_empty actual
'

test_expect_success 'branch -q -d suppresses output on delete' '
	(cd repo && grit branch -q -d silent-branch >../actual 2>&1) &&
	test_must_be_empty actual
'

# ── listing with no remote ───────────────────────────────────────────────

test_expect_success 'branch -r in repo with no remotes shows nothing' '
	grit init no-remote &&
	(cd no-remote && grit branch -r >../actual 2>&1) &&
	test_must_be_empty actual
'

test_done
