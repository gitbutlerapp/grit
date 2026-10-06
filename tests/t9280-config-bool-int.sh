#!/bin/sh
# Tests for config get/set with bool and int value types, type coercion,
# multi-value handling, section/key edge cases, and list output.

test_description='config bool/int types, get/set, list, sections'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup ------------------------------------------------------------------

test_expect_success 'setup: init repo' '
	(
	grit init repo &&
	cd repo
	)
'

# -- basic set/get ----------------------------------------------------------

test_expect_success 'config set and get a string value' '
	(
	cd repo &&
	grit config set core.editor vim &&
	grit config get core.editor >actual &&
	echo "vim" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set bool true' '
	(
	cd repo &&
	grit config set core.filemode true &&
	grit config get core.filemode >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set bool false' '
	(
	cd repo &&
	grit config set core.bare false &&
	grit config get core.bare >actual &&
	echo "false" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set integer value' '
	(
	cd repo &&
	grit config set core.compression 9 &&
	grit config get core.compression >actual &&
	echo "9" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set zero integer' '
	(
	cd repo &&
	grit config set core.compression 0 &&
	grit config get core.compression >actual &&
	echo "0" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set negative integer' '
	(
	cd repo &&
	grit config set core.compression -- -1 &&
	grit config get core.compression >actual &&
	echo "-1" >expect &&
	test_cmp expect actual
	)
'

# -- overwrite existing values -----------------------------------------------

test_expect_success 'config set overwrites previous value' '
	(
	cd repo &&
	grit config set user.name "Alice" &&
	grit config set user.name "Bob" &&
	grit config get user.name >actual &&
	echo "Bob" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set overwrites bool with string' '
	(
	cd repo &&
	grit config set test.val true &&
	grit config set test.val "hello" &&
	grit config get test.val >actual &&
	echo "hello" >expect &&
	test_cmp expect actual
	)
'

# -- unset -------------------------------------------------------------------

test_expect_success 'config unset removes a key' '
	(
	cd repo &&
	grit config set remove.me "gone" &&
	grit config get remove.me >actual &&
	echo "gone" >expect &&
	test_cmp expect actual &&
	grit config unset remove.me &&
	! grit config get remove.me
	)
'

test_expect_success 'config unset nonexistent key fails' '
	(
	cd repo &&
	! grit config unset nonexistent.key
	)
'

# -- list --------------------------------------------------------------------

test_expect_success 'config list shows all entries' '
	(
	cd repo &&
	grit config set list.alpha "a" &&
	grit config set list.beta "b" &&
	grit config list >actual &&
	grep "list.alpha=a" actual &&
	grep "list.beta=b" actual
	)
'

test_expect_success 'config list output contains core settings' '
	(
	cd repo &&
	grit config list >actual &&
	grep "core\." actual
	)
'

# -- sections ----------------------------------------------------------------

test_expect_success 'config set with subsection' '
	(
	cd repo &&
	grit config set "remote.origin.url" "https://example.com/repo.git" &&
	grit config get "remote.origin.url" >actual &&
	echo "https://example.com/repo.git" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set multiple keys in same section' '
	(
	cd repo &&
	grit config set "remote.origin.url" "https://example.com/repo.git" &&
	grit config set "remote.origin.fetch" "+refs/heads/*:refs/remotes/origin/*" &&
	grit config get "remote.origin.url" >actual_url &&
	grit config get "remote.origin.fetch" >actual_fetch &&
	echo "https://example.com/repo.git" >expect_url &&
	echo "+refs/heads/*:refs/remotes/origin/*" >expect_fetch &&
	test_cmp expect_url actual_url &&
	test_cmp expect_fetch actual_fetch
	)
'

test_expect_success 'config set keys in different subsections' '
	(
	cd repo &&
	grit config set "branch.main.remote" "origin" &&
	grit config set "branch.dev.remote" "upstream" &&
	grit config get "branch.main.remote" >actual1 &&
	grit config get "branch.dev.remote" >actual2 &&
	echo "origin" >expect1 &&
	echo "upstream" >expect2 &&
	test_cmp expect1 actual1 &&
	test_cmp expect2 actual2
	)
'

# -- value with spaces -------------------------------------------------------

test_expect_success 'config set value with spaces' '
	(
	cd repo &&
	grit config set user.name "Jane Doe" &&
	grit config get user.name >actual &&
	echo "Jane Doe" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set value with special characters' '
	(
	cd repo &&
	grit config set test.special "hello=world" &&
	grit config get test.special >actual &&
	echo "hello=world" >expect &&
	test_cmp expect actual
	)
'

# -- comparison with real git ------------------------------------------------

test_expect_success 'setup: init comparison repos' '
	(
	$REAL_GIT init git-repo &&
	cd git-repo && $REAL_GIT config user.email "t@t.com" && $REAL_GIT config user.name "T" && cd .. &&
	grit init grit-repo
	)
'

test_expect_success 'config set/get matches real git for bool' '
	$REAL_GIT -C git-repo config core.autocrlf false &&
	grit -C grit-repo config set core.autocrlf false &&
	$REAL_GIT -C git-repo config --get core.autocrlf >expect &&
	grit -C grit-repo config get core.autocrlf >actual &&
	test_cmp expect actual
'

test_expect_success 'config set/get matches real git for string' '
	$REAL_GIT -C git-repo config user.name "Test Person" &&
	grit -C grit-repo config set user.name "Test Person" &&
	$REAL_GIT -C git-repo config --get user.name >expect &&
	grit -C grit-repo config get user.name >actual &&
	test_cmp expect actual
'

test_expect_success 'config set/get matches real git for integer' '
	$REAL_GIT -C git-repo config pack.windowmemory 100 &&
	grit -C grit-repo config set pack.windowmemory 100 &&
	$REAL_GIT -C git-repo config --get pack.windowmemory >expect &&
	grit -C grit-repo config get pack.windowmemory >actual &&
	test_cmp expect actual
'

# -- global vs local ---------------------------------------------------------

test_expect_success 'config --global sets in home config' '
	(
	cd repo &&
	grit config --global set global.key "globalval" &&
	grit config --global get global.key >actual &&
	echo "globalval" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config local overrides global' '
	(
	cd repo &&
	grit config --global set override.key "global" &&
	grit config set override.key "local" &&
	grit config get override.key >actual &&
	echo "local" >expect &&
	test_cmp expect actual
	)
'

# -- remove-section ----------------------------------------------------------

test_expect_success 'config remove-section removes entire section' '
	(
	cd repo &&
	grit config set removeme.key1 "a" &&
	grit config set removeme.key2 "b" &&
	grit config remove-section removeme &&
	! grit config get removeme.key1 &&
	! grit config get removeme.key2
	)
'

# -- rename-section ----------------------------------------------------------

test_expect_success 'config rename-section renames section' '
	(
	cd repo &&
	grit config set oldsec.key "val" &&
	grit config rename-section oldsec newsec &&
	grit config get newsec.key >actual &&
	echo "val" >expect &&
	test_cmp expect actual &&
	! grit config get oldsec.key
	)
'

# -- empty value -------------------------------------------------------------

test_expect_success 'config set empty string value' '
	(
	cd repo &&
	grit config set empty.val "" &&
	grit config get empty.val >actual &&
	echo "" >expect &&
	test_cmp expect actual
	)
'

test_done
