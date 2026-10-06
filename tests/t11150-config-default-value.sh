#!/bin/sh
# Tests for grit config get --default (default value when key is missing).

test_description='grit config get --default returns fallback for missing keys'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repository' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "content" >file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial"
	)
'

###########################################################################
# Section 2: Basic --default behavior
###########################################################################

test_expect_success 'config get --default returns default for missing key' '
	(
	cd repo &&
	grit config get --default "fallback" missing.key >actual &&
	echo "fallback" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default returns real value when key exists' '
	(
	cd repo &&
	grit config get --default "fallback" user.name >actual &&
	echo "Test User" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default exits 0 for missing key' '
	(
	cd repo &&
	grit config get --default "val" nonexistent.key >actual &&
	echo "val" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default with empty string default' '
	(
	cd repo &&
	grit config get --default "" absent.key >actual &&
	echo "" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default with numeric default' '
	(
	cd repo &&
	grit config get --default "42" missing.number >actual &&
	echo "42" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default with boolean default' '
	(
	cd repo &&
	grit config get --default "true" missing.bool >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default with path default' '
	(
	cd repo &&
	grit config get --default "/usr/local/bin" missing.path >actual &&
	echo "/usr/local/bin" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: Existing values override default
###########################################################################

test_expect_success 'config get --default does not override existing email' '
	(
	cd repo &&
	grit config get --default "other@example.com" user.email >actual &&
	echo "test@example.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default with custom key set' '
	(
	cd repo &&
	grit config custom.key "real-value" &&
	grit config get --default "default-value" custom.key >actual &&
	echo "real-value" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default: empty existing value used over default' '
	(
	cd repo &&
	grit config empty.val "" &&
	grit config get --default "notempty" empty.val >actual &&
	echo "" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default overridden by core.bare' '
	(
	cd repo &&
	grit config get --default "true" core.bare >actual &&
	echo "false" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 4: Default with various key patterns
###########################################################################

test_expect_success 'config get --default with deep section key' '
	(
	cd repo &&
	grit config get --default "default-remote" branch.feature.remote >actual &&
	echo "default-remote" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default with subsection missing' '
	(
	cd repo &&
	grit config get --default "origin" remote.upstream.url >actual &&
	echo "origin" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default with two-part key' '
	(
	cd repo &&
	grit config get --default "value" section.key >actual &&
	echo "value" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 5: Default with --file
###########################################################################

test_expect_success 'config get --default with --file for missing key' '
	(
	cd repo &&
	grit config --file custom.cfg aa.bb "cc" &&
	grit config get --default "default-val" nonexist.key >actual &&
	echo "default-val" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default with --file for existing key in file' '
	(
	cd repo &&
	grit config --file getdef.cfg set.key "file-value" &&
	grit config get --default "default-val" set.key >actual_local &&
	echo "file-value" >not_expect &&
	# key is only in the custom file, not in repo config
	# so local get should use default
	grit config get --default "default-val" set.key >actual &&
	echo "default-val" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 6: Default value with special characters
###########################################################################

test_expect_success 'config get --default with spaces in default' '
	(
	cd repo &&
	grit config get --default "hello world" missing.spaced >actual &&
	echo "hello world" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default with URL as default' '
	(
	cd repo &&
	grit config get --default "https://example.com/repo.git" missing.url >actual &&
	echo "https://example.com/repo.git" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default with equals sign in default' '
	(
	cd repo &&
	grit config get --default "key=value" missing.equals >actual &&
	echo "key=value" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default with tilde in default' '
	(
	cd repo &&
	grit config get --default "~/repos" missing.tilde >actual &&
	echo "~/repos" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 7: Interaction with set and unset
###########################################################################

test_expect_success 'config get --default after setting then unsetting key' '
	(
	cd repo &&
	grit config transient.key "exists" &&
	grit config get --default "default" transient.key >actual_before &&
	echo "exists" >expect_before &&
	test_cmp expect_before actual_before &&
	grit config --unset transient.key &&
	grit config get --default "default" transient.key >actual_after &&
	echo "default" >expect_after &&
	test_cmp expect_after actual_after
	)
'

test_expect_success 'config get --default after overwrite returns new value' '
	(
	cd repo &&
	grit config ow.key "first" &&
	grit config ow.key "second" &&
	grit config get --default "default" ow.key >actual &&
	echo "second" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default reflects latest config set' '
	(
	cd repo &&
	grit config latest.key "latest" &&
	grit config get --default "old" latest.key >actual &&
	echo "latest" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 8: --default with --global
###########################################################################

test_expect_success 'config get --default with --global for missing key' '
	(
	cd repo &&
	grit config get --default "global-default" globmissing.key >actual &&
	echo "global-default" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default with --global for existing key' '
	(
	cd repo &&
	grit config --global test.gdef "global-value" &&
	grit config get --default "unused-default" test.gdef >actual &&
	echo "global-value" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 9: Without --default, missing key fails
###########################################################################

test_expect_success 'config get without --default fails for missing key' '
	(
	cd repo &&
	test_must_fail grit config get nosuch.key
	)
'

test_expect_success 'config --get without --default fails for missing key' '
	(
	cd repo &&
	test_must_fail grit config --get nosuch.key
	)
'

test_expect_success 'config get --default vs no --default contrast' '
	(
	cd repo &&
	test_must_fail grit config get absent.key &&
	grit config get --default "fallback" absent.key >actual &&
	echo "fallback" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 10: Multiple calls with different defaults
###########################################################################

test_expect_success 'config get --default with different defaults for same key' '
	(
	cd repo &&
	grit config get --default "alpha" missing.multi >actual1 &&
	grit config get --default "beta" missing.multi >actual2 &&
	echo "alpha" >expect1 &&
	echo "beta" >expect2 &&
	test_cmp expect1 actual1 &&
	test_cmp expect2 actual2
	)
'

test_expect_success 'config get --default with false as default' '
	(
	cd repo &&
	grit config get --default "false" missing.boolfalse >actual &&
	echo "false" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default with zero as default' '
	(
	cd repo &&
	grit config get --default "0" missing.zero >actual &&
	echo "0" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get --default with large number as default' '
	(
	cd repo &&
	grit config get --default "999999999" missing.bignum >actual &&
	echo "999999999" >expect &&
	test_cmp expect actual
	)
'

test_done
