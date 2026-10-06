#!/bin/sh
# Tests for config set and config unset subcommands.

test_description='config set and unset operations'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup -----------------------------------------------------------------

test_expect_success 'setup: create repo' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	echo "base" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "initial"
	)
'

# -- config set basic -------------------------------------------------------

test_expect_success 'config set creates a new key' '
	(
	cd repo &&
	grit config set user.email "bob@example.com" &&
	grit config get user.email >actual &&
	echo "bob@example.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set creates a new name key' '
	(
	cd repo &&
	grit config set user.name "Bob" &&
	grit config get user.name >actual &&
	echo "Bob" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set overwrites existing key' '
	(
	cd repo &&
	grit config set user.email "charlie@example.com" &&
	grit config get user.email >actual &&
	echo "charlie@example.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set with dotted section key' '
	(
	cd repo &&
	grit config set core.autocrlf "false" &&
	grit config get core.autocrlf >actual &&
	echo "false" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set boolean value true' '
	(
	cd repo &&
	grit config set core.ignorecase "true" &&
	grit config get core.ignorecase >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set numeric value' '
	(
	cd repo &&
	grit config set core.compression "9" &&
	grit config get core.compression >actual &&
	echo "9" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set with spaces in value' '
	(
	cd repo &&
	grit config set alias.lg "log --oneline --graph" &&
	grit config get alias.lg >actual &&
	echo "log --oneline --graph" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set url value' '
	(
	cd repo &&
	grit config set remote.origin.url "https://example.com/repo.git" &&
	grit config get remote.origin.url >actual &&
	echo "https://example.com/repo.git" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set refspec value' '
	(
	cd repo &&
	grit config set remote.origin.fetch "+refs/heads/*:refs/remotes/origin/*" &&
	grit config get remote.origin.fetch >actual &&
	echo "+refs/heads/*:refs/remotes/origin/*" >expect &&
	test_cmp expect actual
	)
'

# -- config unset ------------------------------------------------------------

test_expect_success 'config unset removes an existing key' '
	(
	cd repo &&
	grit config set test.toremove "value" &&
	grit config get test.toremove >actual &&
	echo "value" >expect &&
	test_cmp expect actual &&
	grit config unset test.toremove &&
	test_must_fail grit config get test.toremove
	)
'

test_expect_success 'config unset of nonexistent key fails' '
	(
	cd repo &&
	test_must_fail grit config unset nonexistent.key
	)
'

test_expect_success 'config unset removes only the specified key' '
	(
	cd repo &&
	grit config set section.keep "keepme" &&
	grit config set section.remove "removeme" &&
	grit config unset section.remove &&
	grit config get section.keep >actual &&
	echo "keepme" >expect &&
	test_cmp expect actual &&
	test_must_fail grit config get section.remove
	)
'

# -- config set then overwrite then unset ------------------------------------

test_expect_success 'config set, overwrite, then unset lifecycle' '
	(
	cd repo &&
	grit config set lifecycle.key "first" &&
	grit config get lifecycle.key >actual &&
	echo "first" >expect &&
	test_cmp expect actual &&
	grit config set lifecycle.key "second" &&
	grit config get lifecycle.key >actual2 &&
	echo "second" >expect2 &&
	test_cmp expect2 actual2 &&
	grit config unset lifecycle.key &&
	test_must_fail grit config get lifecycle.key
	)
'

# -- legacy set syntax (positional) ------------------------------------------

test_expect_success 'legacy config set with positional args' '
	(
	cd repo &&
	grit config legacy.key "legacyval" &&
	grit config get legacy.key >actual &&
	echo "legacyval" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'legacy config --get reads the value' '
	(
	cd repo &&
	grit config set readtest.key "readval" &&
	grit config --get readtest.key >actual &&
	echo "readval" >expect &&
	test_cmp expect actual
	)
'

# -- config set in different sections ----------------------------------------

test_expect_success 'config set color.ui' '
	(
	cd repo &&
	grit config set color.ui "auto" &&
	grit config get color.ui >actual &&
	echo "auto" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set merge.tool' '
	(
	cd repo &&
	grit config set merge.tool "vimdiff" &&
	grit config get merge.tool >actual &&
	echo "vimdiff" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set diff.renames' '
	(
	cd repo &&
	grit config set diff.renames "true" &&
	grit config get diff.renames >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

# -- config unset then re-set -----------------------------------------------

test_expect_success 'config unset then set again works' '
	(
	cd repo &&
	grit config set bounce.key "original" &&
	grit config unset bounce.key &&
	test_must_fail grit config get bounce.key &&
	grit config set bounce.key "restored" &&
	grit config get bounce.key >actual &&
	echo "restored" >expect &&
	test_cmp expect actual
	)
'

# -- legacy --unset ----------------------------------------------------------

test_expect_success 'legacy --unset removes key' '
	(
	cd repo &&
	grit config set legacyunset.key "val" &&
	grit config --unset legacyunset.key &&
	test_must_fail grit config get legacyunset.key
	)
'

# -- config list after modifications ----------------------------------------

test_expect_success 'config list shows all entries' '
	(
	cd repo &&
	grit config list >actual &&
	grep "user.email" actual &&
	grep "user.name" actual &&
	grep "core.autocrlf" actual
	)
'

test_expect_success 'config list includes recently set keys' '
	(
	cd repo &&
	grit config set custom.testkey "testvalue" &&
	grit config list >actual &&
	grep "custom.testkey" actual
	)
'

# -- compare with real git ---------------------------------------------------

test_expect_success 'config set matches real git set' '
	(
	cd repo &&
	grit config set compare.key "comparevalue" &&
	grit config get compare.key >actual &&
	$REAL_GIT config --get compare.key >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set overwrite matches real git' '
	(
	cd repo &&
	grit config set compare.key "newvalue" &&
	grit config get compare.key >actual &&
	$REAL_GIT config --get compare.key >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config unset then real git confirms removal' '
	(
	cd repo &&
	grit config set toverify.key "exists" &&
	grit config unset toverify.key &&
	test_must_fail $REAL_GIT config --get toverify.key
	)
'

test_expect_success 'real git set then grit reads it' '
	(
	cd repo &&
	$REAL_GIT config crosscheck.key "fromgit" &&
	grit config get crosscheck.key >actual &&
	echo "fromgit" >expect &&
	test_cmp expect actual
	)
'

test_done
