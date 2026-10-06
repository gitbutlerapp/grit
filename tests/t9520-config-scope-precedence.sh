#!/bin/sh
# Tests for grit config scope precedence: --local, --global, --system,
# --file, show-origin, show-scope, get, set, unset, list, sections.

test_description='grit config scope precedence and options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo
	)
'

###########################################################################
# Section 2: Basic set and get
###########################################################################

test_expect_success 'config set and get a value' '
	(
	cd repo &&
	grit config set user.name "Test User" &&
	grit config get user.name >actual &&
	echo "Test User" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set and get user.email' '
	(
	cd repo &&
	grit config set user.email "test@example.com" &&
	grit config get user.email >actual &&
	echo "test@example.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config overwrite existing key' '
	(
	cd repo &&
	grit config set user.name "New Name" &&
	grit config get user.name >actual &&
	echo "New Name" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get nonexistent key fails' '
	(
	cd repo &&
	test_must_fail grit config get nonexistent.key
	)
'

test_expect_success 'config set custom section' '
	(
	cd repo &&
	grit config set custom.mykey "myvalue" &&
	grit config get custom.mykey >actual &&
	echo "myvalue" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: Unset
###########################################################################

test_expect_success 'config unset removes key' '
	(
	cd repo &&
	grit config set temp.key "temporary" &&
	grit config get temp.key >actual &&
	echo "temporary" >expect &&
	test_cmp expect actual &&
	grit config unset temp.key &&
	test_must_fail grit config get temp.key
	)
'

test_expect_success 'config unset nonexistent key fails' '
	(
	cd repo &&
	test_must_fail grit config unset does.not.exist
	)
'

###########################################################################
# Section 4: List
###########################################################################

test_expect_success 'config list shows all entries' '
	(
	cd repo &&
	grit config list >actual &&
	grep "user.name=New Name" actual &&
	grep "user.email=test@example.com" actual &&
	grep "custom.mykey=myvalue" actual
	)
'

test_expect_success 'config -l (legacy) lists entries' '
	(
	cd repo &&
	grit config -l >actual &&
	grep "user.name" actual
	)
'

###########################################################################
# Section 5: --local scope
###########################################################################

test_expect_success 'config --local set writes to repo config' '
	(
	cd repo &&
	grit config --local set core.localonly "yes" &&
	grit config get core.localonly >actual &&
	echo "yes" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'local config is in .git/config' '
	(
	cd repo &&
	grep "localonly" .git/config
	)
'

###########################################################################
# Section 6: --global scope
###########################################################################

test_expect_success 'config --global set writes to user config' '
	(
	cd repo &&
	grit config --global set user.globalname "Global User" &&
	grit config --global get user.globalname >actual &&
	echo "Global User" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'global config is in HOME/.gitconfig' '
	test -f "$HOME/.gitconfig" &&
	grep "globalname" "$HOME/.gitconfig"
'

###########################################################################
# Section 7: Scope precedence (local overrides global)
###########################################################################

test_expect_success 'local config overrides global config' '
	(
	cd repo &&
	grit config --global set precedence.key "global-value" &&
	grit config --local set precedence.key "local-value" &&
	grit config get precedence.key >actual &&
	echo "local-value" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'after unsetting local, global shows through' '
	(
	cd repo &&
	grit config --local unset precedence.key &&
	grit config get precedence.key >actual &&
	echo "global-value" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 8: --file
###########################################################################

test_expect_success 'config --file writes to specified file' '
	(
	cd repo &&
	grit config --file custom.cfg set custom.filekey "fileval" &&
	test -f custom.cfg &&
	grep "fileval" custom.cfg
	)
'

test_expect_success 'config --file reads from specified file' '
	(
	cd repo &&
	grit config --file custom.cfg get custom.filekey >actual &&
	echo "fileval" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file list shows entries from file' '
	(
	cd repo &&
	grit config --file custom.cfg set another.key "val2" &&
	grit config --file custom.cfg list >actual &&
	grep "custom.filekey=fileval" actual &&
	grep "another.key=val2" actual
	)
'

###########################################################################
# Section 9: --show-origin
###########################################################################

test_expect_success 'config list --show-origin includes file paths' '
	(
	cd repo &&
	grit config --show-origin list >actual &&
	grep "file:" actual
	)
'

test_expect_success 'config --show-origin with get includes value' '
	(
	cd repo &&
	grit config --show-origin get user.name >actual &&
	grep "New Name" actual
	)
'

###########################################################################
# Section 10: --show-scope
###########################################################################

test_expect_success 'config list --show-scope shows scope labels' '
	(
	cd repo &&
	grit config --show-scope list >actual &&
	grep "local" actual
	)
'

test_expect_success 'config --show-scope with get includes value' '
	(
	cd repo &&
	grit config --show-scope get user.name >actual &&
	grep "New Name" actual
	)
'

###########################################################################
# Section 11: --bool type
###########################################################################

test_expect_success 'config set and get --bool true' '
	(
	cd repo &&
	grit config set core.mybool "true" &&
	grit config --bool get core.mybool >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --bool normalizes yes to true' '
	(
	cd repo &&
	grit config set core.mybool2 "yes" &&
	grit config --bool get core.mybool2 >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --bool rejects non-boolean' '
	(
	cd repo &&
	grit config set core.mybool3 "notabool" &&
	test_must_fail grit config --bool get core.mybool3
	)
'

###########################################################################
# Section 12: --int type
###########################################################################

test_expect_success 'config --int normalizes integer' '
	(
	cd repo &&
	grit config set core.myint "42" &&
	grit config --int get core.myint >actual &&
	echo "42" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --int handles k suffix' '
	(
	cd repo &&
	grit config set core.mysize "8k" &&
	grit config --int get core.mysize >actual &&
	echo "8192" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 13: Sections
###########################################################################

test_expect_success 'config rename-section works' '
	(
	cd repo &&
	grit config set oldsection.key "value" &&
	grit config rename-section oldsection newsection &&
	grit config get newsection.key >actual &&
	echo "value" >expect &&
	test_cmp expect actual &&
	test_must_fail grit config get oldsection.key
	)
'

test_expect_success 'config remove-section works' '
	(
	cd repo &&
	grit config set removeme.a "1" &&
	grit config set removeme.b "2" &&
	grit config remove-section removeme &&
	test_must_fail grit config get removeme.a &&
	test_must_fail grit config get removeme.b
	)
'

###########################################################################
# Section 14: -z NUL output
###########################################################################

test_expect_success 'config list -z uses NUL delimiters' '
	(
	cd repo &&
	grit config -z list >actual &&
	tr "\0" "\n" <actual >decoded &&
	grep "user.name" decoded
	)
'

###########################################################################
# Section 15: Multiple values in different scopes
###########################################################################

test_expect_success 'config shows combined list from all scopes' '
	(
	cd repo &&
	grit config --global set global.only "gval" &&
	grit config --local set local.only "lval" &&
	grit config list >actual &&
	grep "global.only=gval" actual &&
	grep "local.only=lval" actual
	)
'

test_expect_success 'config --local list only shows local entries' '
	(
	cd repo &&
	grit config --local list >actual &&
	grep "local.only=lval" actual &&
	! grep "global.only=gval" actual
	)
'

test_expect_success 'config --global list only shows global entries' '
	(
	cd repo &&
	grit config --global list >actual &&
	grep "global.only=gval" actual &&
	! grep "local.only=lval" actual
	)
'

###########################################################################
# Section 16: Edge cases
###########################################################################

test_expect_success 'config with empty value' '
	(
	cd repo &&
	grit config set empty.key "" &&
	grit config get empty.key >actual &&
	echo "" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config with spaces in value' '
	(
	cd repo &&
	grit config set spaced.key "hello world  test" &&
	grit config get spaced.key >actual &&
	echo "hello world  test" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config with dots in subsection' '
	(
	cd repo &&
	grit config set "remote.origin.url" "https://example.com/repo.git" &&
	grit config get "remote.origin.url" >actual &&
	echo "https://example.com/repo.git" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 17: Legacy mode
###########################################################################

test_expect_success 'legacy config --get works' '
	(
	cd repo &&
	grit config --get user.name >actual &&
	echo "New Name" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'legacy config key value sets' '
	(
	cd repo &&
	grit config legacy.set "legval" &&
	grit config get legacy.set >actual &&
	echo "legval" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'legacy config --unset removes' '
	(
	cd repo &&
	grit config set legacy.rm "bye" &&
	grit config --unset legacy.rm &&
	test_must_fail grit config get legacy.rm
	)
'

test_done
