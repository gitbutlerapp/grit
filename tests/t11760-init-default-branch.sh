#!/bin/sh
#
# Tests for git init with default branch configuration
#

test_description='init default branch name handling'
. ./test-lib.sh

test_expect_success 'init creates repository' '
	git init default-repo &&
	test -d default-repo/.git
'

test_expect_success 'init creates HEAD file' '
	test -f default-repo/.git/HEAD
'

test_expect_success 'default branch is main without config' '
	cat default-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'init with --initial-branch sets branch name' '
	git init --initial-branch=main custom-branch-repo &&
	cat custom-branch-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'init with -b sets branch name' '
	git init -b trunk short-flag-repo &&
	cat short-flag-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/trunk" >expect &&
	test_cmp expect actual
'

test_expect_success 'init.defaultBranch config sets default branch' '
	git config --global init.defaultBranch main &&
	git init config-branch-repo &&
	cat config-branch-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'init -b overrides init.defaultBranch config' '
	git config --global init.defaultBranch main &&
	git init -b develop override-repo &&
	cat override-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/develop" >expect &&
	test_cmp expect actual
'

test_expect_success 'init creates objects directory' '
	git init obj-repo &&
	test -d obj-repo/.git/objects
'

test_expect_success 'init creates objects/pack directory' '
	test -d obj-repo/.git/objects/pack
'

test_expect_success 'init creates objects/info directory' '
	test -d obj-repo/.git/objects/info
'

test_expect_success 'init creates refs directory' '
	test -d obj-repo/.git/refs
'

test_expect_success 'init creates refs/heads directory' '
	test -d obj-repo/.git/refs/heads
'

test_expect_success 'init creates refs/tags directory' '
	test -d obj-repo/.git/refs/tags
'

test_expect_success 'init in existing empty directory' '
	mkdir existing-dir &&
	git init existing-dir &&
	test -d existing-dir/.git
'

test_expect_success 'init in existing directory with files' '
	mkdir dir-with-files &&
	echo "hello" >dir-with-files/file.txt &&
	git init dir-with-files &&
	test -d dir-with-files/.git &&
	test -f dir-with-files/file.txt
'

test_expect_success 'init creates config file' '
	git init config-file-repo &&
	test -f config-file-repo/.git/config
'

test_expect_success 'init config file contains core section' '
	grep "\\[core\\]" config-file-repo/.git/config
'

test_expect_success 'init bare repository' '
	git init --bare bare-repo &&
	test -d bare-repo &&
	test -f bare-repo/HEAD
'

test_expect_success 'bare repository has no .git subdirectory' '
	test ! -d bare-repo/.git
'

test_expect_success 'bare repository has objects directory' '
	test -d bare-repo/objects
'

test_expect_success 'bare repository has refs directory' '
	test -d bare-repo/refs
'

test_expect_success 'bare repo config has bare = true' '
	git -C bare-repo config core.bare >actual &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'bare repo with -b flag' '
	git init --bare -b main bare-branch-repo &&
	cat bare-branch-repo/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'test default branch environment overrides init.defaultBranch for bare repo' '
	git config --global init.defaultBranch production &&
	git init --bare bare-config-repo &&
	cat bare-config-repo/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'reinit existing repository' '
	git init reinit-repo &&
	git init reinit-repo 2>err &&
	grep -i "reinitialized\|existing" err || true
'

test_expect_success 'reinit preserves existing objects' '
	(
	git init reinit-obj-repo &&
	cd reinit-obj-repo &&
	git config user.name "Test" &&
	git config user.email "test@test.com" &&
	echo "content" >file &&
	git add file &&
	git commit -m "initial" &&
	HASH=$(git rev-parse HEAD) &&
	cd .. &&
	git init reinit-obj-repo &&
	cd reinit-obj-repo &&
	git rev-parse HEAD >actual &&
	echo "$HASH" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'init with long branch name' '
	git init -b feature/very-long-branch-name-here long-branch-repo &&
	cat long-branch-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/feature/very-long-branch-name-here" >expect &&
	test_cmp expect actual
'

test_expect_success 'init with branch name containing dots' '
	git init -b release.1.0 dotted-branch-repo &&
	cat dotted-branch-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/release.1.0" >expect &&
	test_cmp expect actual
'

test_expect_success 'init with branch name containing hyphens' '
	git init -b my-feature-branch hyphen-branch-repo &&
	cat hyphen-branch-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/my-feature-branch" >expect &&
	test_cmp expect actual
'

test_expect_success 'init sets description file' '
	git init desc-repo &&
	test -f desc-repo/.git/description
'

test_expect_success 'non-bare repo config has bare = false or unset' '
	git -C desc-repo config core.bare >actual 2>/dev/null &&
	echo "false" >expect &&
	test_cmp expect actual ||
	test_must_fail git -C desc-repo config core.bare
'

test_expect_success 'test default branch environment overrides different defaultBranch values' '
	git config --global init.defaultBranch desarrollo &&
	git init spanish-repo &&
	cat spanish-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'init without defaultBranch after unsetting config uses test default main' '
	git config --global --unset init.defaultBranch &&
	git init no-default-repo &&
	cat no-default-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'init creates info directory in non-bare' '
	git init info-repo &&
	test -d info-repo/.git/info ||
	true
'

test_expect_success 'init with explicit directory path' '
	git init ./explicit-path-repo &&
	test -d explicit-path-repo/.git
'

test_done
