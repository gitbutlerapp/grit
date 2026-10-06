#!/bin/sh

test_description='grit init: initial and reinit config behavior'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

# ── basic init creates proper config ─────────────────────────────────────

test_expect_success 'init creates .git directory' '
	grit init repo &&
	test -d repo/.git
'

test_expect_success 'init creates config file' '
	test -f repo/.git/config
'

test_expect_success 'init sets core.repositoryformatversion to 0' '
	(cd repo && grit config get core.repositoryformatversion >../actual) &&
	echo "0" >expect &&
	test_cmp expect actual
'

test_expect_success 'init sets core.filemode' '
	(cd repo && grit config get core.filemode >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'init sets core.bare to false' '
	(cd repo && grit config get core.bare >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'init sets core.logallrefupdates to true' '
	(cd repo && grit config get core.logallrefupdates >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'init creates HEAD pointing to master or main' '
	(cd repo && cat .git/HEAD >../actual) &&
	grep "ref: refs/heads/" actual
'

test_expect_success 'init creates refs directory' '
	test -d repo/.git/refs
'

test_expect_success 'init creates objects directory' '
	test -d repo/.git/objects
'

# ── reinit on existing repo ──────────────────────────────────────────────

test_expect_success 'setup: add content to repo' '
	(cd repo &&
	 $REAL_GIT config user.email "t@t.com" &&
	 $REAL_GIT config user.name "T" &&
	 echo hello >file.txt &&
	 grit add file.txt &&
	 grit commit -m "initial")
'

test_expect_success 'reinit does not destroy existing content' '
	(cd repo && grit init) &&
	(cd repo && test -f file.txt)
'

test_expect_success 'reinit preserves commits' '
	(cd repo && grit log --oneline >../actual) &&
	grep "initial" actual
'

test_expect_success 'reinit resets config to defaults' '
	(cd repo && grit config get core.bare >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'reinit preserves user config entries' '
	(cd repo && grit config get user.email >../actual) &&
	grep "t@t.com" actual
'

test_expect_success 'reinit preserves user.name' '
	(cd repo && grit config get user.name >../actual) &&
	grep "T" actual
'

test_expect_success 'reinit config still has core section' '
	(cd repo && grit config get core.repositoryformatversion >../actual) &&
	echo "0" >expect &&
	test_cmp expect actual
'

# ── init --bare ──────────────────────────────────────────────────────────

test_expect_success 'init --bare creates bare repo' '
	grit init --bare bare-repo &&
	test -f bare-repo/config
'

test_expect_success 'bare repo has core.bare = true' '
	(cd bare-repo && grit config get core.bare >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'bare repo has no .git subdirectory' '
	test ! -d bare-repo/.git
'

test_expect_success 'bare repo has HEAD' '
	test -f bare-repo/HEAD
'

test_expect_success 'bare repo has objects directory' '
	test -d bare-repo/objects
'

test_expect_success 'bare repo has refs directory' '
	test -d bare-repo/refs
'

# ── init with directory argument ─────────────────────────────────────────

test_expect_success 'init creates specified directory' '
	grit init newdir &&
	test -d newdir/.git
'

test_expect_success 'init with nested directory path' '
	grit init parent/child &&
	test -d parent/child/.git
'

test_expect_success 'nested init has correct config' '
	(cd parent/child && grit config get core.bare >../../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

# ── init with --initial-branch ───────────────────────────────────────────

test_expect_success 'init -b sets custom initial branch' '
	grit init -b develop branch-repo &&
	(cd branch-repo && cat .git/HEAD >../actual) &&
	echo "ref: refs/heads/develop" >expect &&
	test_cmp expect actual
'

test_expect_success 'init --initial-branch sets custom initial branch' '
	grit init --initial-branch trunk trunk-repo &&
	(cd trunk-repo && cat .git/HEAD >../actual) &&
	echo "ref: refs/heads/trunk" >expect &&
	test_cmp expect actual
'

# ── init quiet mode ──────────────────────────────────────────────────────

test_expect_success 'init -q produces no output' '
	grit init -q quiet-repo >actual 2>&1 &&
	test_must_be_empty actual
'

test_expect_success 'init --quiet produces no output' '
	grit init --quiet quiet-repo2 >actual 2>&1 &&
	test_must_be_empty actual
'

# ── reinit with --bare on non-bare fails or converts ─────────────────────

test_expect_success 'reinit bare on existing bare keeps core.bare true' '
	(cd bare-repo &&
	 grit init --bare &&
	 grit config get core.bare >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

# ── config file contents after init ──────────────────────────────────────

test_expect_success 'config file contains [core] section' '
	grep "\\[core\\]" repo/.git/config
'

test_expect_success 'config file is valid ini format' '
	(cd repo && grit config list >../actual) &&
	test -s actual
'

test_expect_success 'init in current directory works' '
	mkdir cwd-init &&
	(cd cwd-init && grit init && test -d .git)
'

test_expect_success 'init in current directory sets core.bare false' '
	(cd cwd-init && grit config get core.bare >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_done
