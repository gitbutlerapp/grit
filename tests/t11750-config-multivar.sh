#!/bin/sh
#
# Tests for git config with multi-valued variables and advanced operations
#

test_description='config multi-valued variables and advanced config operations'
. ./test-lib.sh

# Init a repo right in trash directory so all tests can use it
test_expect_success 'setup: init repo in trash' '
	git init repo
'

R="$TRASH_DIRECTORY/repo"

test_expect_success 'manually create multi-valued key in config' '
	cat >>"$R/.git/config" <<-\EOF &&
	[remote "origin"]
		fetch = +refs/heads/*:refs/remotes/origin/*
		fetch = +refs/tags/*:refs/tags/*
	EOF
	git -C "$R" config --get-all remote.origin.fetch >actual &&
	test_line_count = 2 actual
'

test_expect_success 'config --get-all lists all values' '
	git -C "$R" config --get-all remote.origin.fetch >actual &&
	grep "refs/heads" actual &&
	grep "refs/tags" actual
'

test_expect_success 'config --get returns last value for multi-valued' '
	git -C "$R" config remote.origin.fetch >actual &&
	grep "refs/tags" actual
'

test_expect_success 'config get --all returns all values' '
	git -C "$R" config get --all remote.origin.fetch >actual &&
	test_line_count = 2 actual
'

test_expect_success 'manually add third fetch refspec' '
	cat >>"$R/.git/config" <<-\EOF &&
	[remote "origin"]
		fetch = +refs/notes/*:refs/notes/*
	EOF
	git -C "$R" config --get-all remote.origin.fetch >actual &&
	test_line_count = 3 actual
'

test_expect_success 'config --get-all on single-valued key returns one line' '
	git -C "$R" config user.name "Test User" &&
	git -C "$R" config --get-all user.name >actual &&
	test_line_count = 1 actual
'

test_expect_success 'config --replace-all replaces values within a section' '
	git -C "$R" config --replace-all remote.origin.fetch "+refs/heads/main:refs/remotes/origin/main" &&
	git -C "$R" config --get-all remote.origin.fetch >actual &&
	grep "refs/heads/main" actual
'

test_expect_success 'config --unset-all removes all values' '
	cat >>"$R/.git/config" <<-\EOF &&
	[multi]
		key = val1
		key = val2
		key = val3
	EOF
	git -C "$R" config --unset-all multi.key &&
	test_must_fail git -C "$R" config multi.key
'

test_expect_success 'config --get-all on nonexistent key fails' '
	test_must_fail git -C "$R" config --get-all nonexistent.key
'

test_expect_success 'config set overwrites single-valued key' '
	git -C "$R" config single.key "first" &&
	git -C "$R" config single.key "second" &&
	git -C "$R" config single.key >actual &&
	echo "second" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set --all replaces matching values in section' '
	cat >>"$R/.git/config" <<-\EOF &&
	[setall]
		key = alpha
		key = beta
		key = gamma
	EOF
	git -C "$R" config set --all setall.key "replaced" &&
	git -C "$R" config --get-all setall.key >actual &&
	grep "replaced" actual
'

test_expect_success 'config unset --all removes all occurrences' '
	cat >>"$R/.git/config" <<-\EOF &&
	[unsetall]
		val = one
		val = two
	EOF
	git -C "$R" config unset --all unsetall.val &&
	test_must_fail git -C "$R" config unsetall.val
'

test_expect_success 'config --list shows multi-valued keys' '
	cat >>"$R/.git/config" <<-\EOF &&
	[listmulti]
		item = first
		item = second
	EOF
	git -C "$R" config --list >actual &&
	grep "listmulti.item=first" actual &&
	grep "listmulti.item=second" actual
'

test_expect_success 'config list shows all entries' '
	git -C "$R" config list >actual &&
	grep "listmulti.item" actual
'

test_expect_success 'config --get with multi-valued returns last' '
	git -C "$R" config --get listmulti.item >actual &&
	echo "second" >expect &&
	test_cmp expect actual
'

test_expect_success 'multi-valued in subsections are independent' '
	cat >>"$R/.git/config" <<-\EOF &&
	[branch "main"]
		merge = refs/heads/main
	[branch "dev"]
		merge = refs/heads/dev
	EOF
	git -C "$R" config branch.main.merge >actual_main &&
	echo "refs/heads/main" >expect_main &&
	test_cmp expect_main actual_main &&
	git -C "$R" config branch.dev.merge >actual_dev &&
	echo "refs/heads/dev" >expect_dev &&
	test_cmp expect_dev actual_dev
'

test_expect_success 'config --replace-all on single-valued is idempotent' '
	git -C "$R" config replace.single "one" &&
	git -C "$R" config --replace-all replace.single "two" &&
	git -C "$R" config replace.single >actual &&
	echo "two" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --unset on single-valued key' '
	git -C "$R" config removeme.key "val" &&
	git -C "$R" config --unset removeme.key &&
	test_must_fail git -C "$R" config removeme.key
'

test_expect_success 'config unset subcommand' '
	git -C "$R" config removeme2.key "val" &&
	git -C "$R" config unset removeme2.key &&
	test_must_fail git -C "$R" config removeme2.key
'

test_expect_success 'config with spaces in value' '
	git -C "$R" config space.key "hello world foo" &&
	git -C "$R" config space.key >actual &&
	echo "hello world foo" >expect &&
	test_cmp expect actual
'

test_expect_success 'config with equals in value' '
	git -C "$R" config equals.key "a=b=c" &&
	git -C "$R" config equals.key >actual &&
	echo "a=b=c" >expect &&
	test_cmp expect actual
'

test_expect_success 'config with URL value' '
	git -C "$R" config url.key "https://example.com/repo.git" &&
	git -C "$R" config url.key >actual &&
	echo "https://example.com/repo.git" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global multi-valued via file edit' '
	cat >>$HOME/.gitconfig <<-\EOF &&
	[globalm]
		key = gval1
		key = gval2
	EOF
	git config --global --get-all globalm.key >actual &&
	test_line_count = 2 actual
'

test_expect_success 'config --global --unset-all removes global multi-values' '
	git config --global --unset-all globalm.key &&
	test_must_fail git config --global globalm.key
'

test_expect_success 'config preserves other keys when editing' '
	git -C "$R" config preserve.alpha "1" &&
	git -C "$R" config preserve.beta "2" &&
	git -C "$R" config preserve.alpha "updated" &&
	git -C "$R" config preserve.beta >actual &&
	echo "2" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --remove-section removes entire section' '
	git -C "$R" config rmsec.key1 "a" &&
	git -C "$R" config rmsec.key2 "b" &&
	git -C "$R" config --remove-section rmsec &&
	test_must_fail git -C "$R" config rmsec.key1 &&
	test_must_fail git -C "$R" config rmsec.key2
'

test_expect_success 'config remove-section subcommand' '
	git -C "$R" config rmsec2.key1 "a" &&
	git -C "$R" config rmsec2.key2 "b" &&
	git -C "$R" config remove-section rmsec2 &&
	test_must_fail git -C "$R" config rmsec2.key1
'

test_expect_success 'config --rename-section renames section' '
	git -C "$R" config oldsec.key "value" &&
	git -C "$R" config --rename-section oldsec newsec &&
	git -C "$R" config newsec.key >actual &&
	echo "value" >expect &&
	test_cmp expect actual &&
	test_must_fail git -C "$R" config oldsec.key
'

test_expect_success 'config rename-section subcommand' '
	git -C "$R" config ren1.key "val" &&
	git -C "$R" config rename-section ren1 ren2 &&
	git -C "$R" config ren2.key >actual &&
	echo "val" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --list --local only shows local config' '
	git -C "$R" config --list --local >actual &&
	! grep "globalm" actual
'

test_expect_success 'config get --default provides fallback' '
	git -C "$R" config get --default "fallback" nonexistent.key >actual &&
	echo "fallback" >expect &&
	test_cmp expect actual
'

test_expect_success 'config get --default not used when key exists' '
	git -C "$R" config existing.key "real" &&
	git -C "$R" config get --default "fallback" existing.key >actual &&
	echo "real" >expect &&
	test_cmp expect actual
'

test_expect_success 'config with numeric value' '
	git -C "$R" config num.key "42" &&
	git -C "$R" config num.key >actual &&
	echo "42" >expect &&
	test_cmp expect actual
'

test_expect_success 'config with boolean true' '
	git -C "$R" config bool.key "true" &&
	git -C "$R" config bool.key >actual &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'config with boolean false' '
	git -C "$R" config bool.key "false" &&
	git -C "$R" config bool.key >actual &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_done
