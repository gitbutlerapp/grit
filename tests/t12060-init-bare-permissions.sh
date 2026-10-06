#!/bin/sh
test_description='grit init --bare and directory permissions'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

# ── basic init ────────────────────────────────────────────────────────────────

test_expect_success 'grit init creates a working repo' '
	grit init normal-repo &&
	test -d normal-repo/.git &&
	test -d normal-repo/.git/objects &&
	test -d normal-repo/.git/refs
'

test_expect_success 'grit init creates HEAD pointing to main' '
	cat normal-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'grit init creates config with bare=false' '
	(cd normal-repo && grit config get core.bare >../actual) &&
	echo false >expect &&
	test_cmp expect actual
'

test_expect_success 'grit init creates description file' '
	test -f normal-repo/.git/description
'

test_expect_success 'grit init creates hooks directory' '
	test -d normal-repo/.git/hooks
'

test_expect_success 'grit init creates info directory' '
	test -d normal-repo/.git/info
'

# ── bare init ─────────────────────────────────────────────────────────────────

test_expect_success 'grit init --bare creates a bare repo' '
	grit init --bare bare-repo &&
	test -d bare-repo &&
	test -f bare-repo/HEAD &&
	test -d bare-repo/objects &&
	test -d bare-repo/refs
'

test_expect_success 'bare repo has no .git subdirectory' '
	! test -d bare-repo/.git
'

test_expect_success 'bare repo HEAD points to main' '
	cat bare-repo/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'bare repo config has bare=true' '
	grit config --file bare-repo/config --get core.bare >actual &&
	echo true >expect &&
	test_cmp expect actual
'

test_expect_success 'bare repo config has repositoryformatversion=0' '
	grit config --file bare-repo/config --get core.repositoryformatversion >actual &&
	echo 0 >expect &&
	test_cmp expect actual
'

test_expect_success 'bare repo has no working tree' '
	! test -f bare-repo/index
'

# ── permissions ───────────────────────────────────────────────────────────────

test_expect_success 'normal repo .git/objects is a directory' '
	test -d normal-repo/.git/objects
'

test_expect_success 'normal repo .git/objects is user-accessible' '
	test -r normal-repo/.git/objects &&
	test -x normal-repo/.git/objects
'

test_expect_success 'bare repo objects is a directory' '
	test -d bare-repo/objects
'

test_expect_success 'bare repo objects is user-accessible' '
	test -r bare-repo/objects &&
	test -x bare-repo/objects
'

test_expect_success 'normal repo refs directory exists and is accessible' '
	test -d normal-repo/.git/refs &&
	test -r normal-repo/.git/refs
'

test_expect_success 'bare repo refs directory exists and is accessible' '
	test -d bare-repo/refs &&
	test -r bare-repo/refs
'

test_expect_success 'refs/heads and refs/tags exist in normal repo' '
	test -d normal-repo/.git/refs/heads &&
	test -d normal-repo/.git/refs/tags
'

test_expect_success 'refs/heads and refs/tags exist in bare repo' '
	test -d bare-repo/refs/heads &&
	test -d bare-repo/refs/tags
'

# ── reinit ────────────────────────────────────────────────────────────────────

test_expect_success 'grit init on existing repo reinitializes' '
	grit init normal-repo 2>err &&
	test -d normal-repo/.git
'

test_expect_success 'reinit does not destroy .git structure' '
	grit init normal-repo &&
	test -d normal-repo/.git/objects &&
	test -d normal-repo/.git/refs &&
	test -f normal-repo/.git/HEAD
'

# ── -b / --initial-branch ────────────────────────────────────────────────────

test_expect_success 'grit init -b sets initial branch name' '
	grit init -b main custom-branch-repo &&
	cat custom-branch-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'grit init --initial-branch sets initial branch name' '
	grit init --initial-branch develop init-branch-repo &&
	cat init-branch-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/develop" >expect &&
	test_cmp expect actual
'

test_expect_success 'grit init --bare -b works' '
	grit init --bare -b trunk bare-trunk &&
	cat bare-trunk/HEAD >actual &&
	echo "ref: refs/heads/trunk" >expect &&
	test_cmp expect actual
'

# ── quiet mode ────────────────────────────────────────────────────────────────

test_expect_success 'grit init -q suppresses output' '
	grit init -q quiet-repo >actual 2>&1 &&
	test_must_be_empty actual
'

# ── init with explicit directory ──────────────────────────────────────────────

test_expect_success 'grit init with nested path creates directories' '
	grit init nested/deep/repo &&
	test -d nested/deep/repo/.git
'

test_expect_success 'grit init --bare with nested path creates directories' '
	grit init --bare nested/deep/bare &&
	test -f nested/deep/bare/HEAD
'

# ── objects subdirectories ────────────────────────────────────────────────────

test_expect_success 'objects/pack directory exists in normal repo' '
	test -d normal-repo/.git/objects/pack
'

test_expect_success 'objects/info directory exists in normal repo' '
	test -d normal-repo/.git/objects/info
'

test_expect_success 'objects/pack directory exists in bare repo' '
	test -d bare-repo/objects/pack
'

test_expect_success 'objects/info directory exists in bare repo' '
	test -d bare-repo/objects/info
'

# ── commits work in both types ───────────────────────────────────────────────

test_expect_success 'can commit in normal repo' '
	(cd normal-repo &&
	 $REAL_GIT config user.email "t@t.com" &&
	 $REAL_GIT config user.name "T" &&
	 echo content >f.txt &&
	 grit add f.txt &&
	 grit commit -m "test commit") &&
	(cd normal-repo && grit log --oneline >../actual) &&
	grep "test commit" actual
'

test_expect_success 'bare repo has filemode set' '
	grit config --file bare-repo/config --get core.filemode >actual &&
	echo true >expect &&
	test_cmp expect actual
'

test_expect_success 'normal repo has logallrefupdates=true' '
	(cd normal-repo && grit config get core.logallrefupdates >../actual) &&
	echo true >expect &&
	test_cmp expect actual
'

test_done
