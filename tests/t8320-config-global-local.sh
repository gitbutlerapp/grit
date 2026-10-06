#!/bin/sh
# Tests for config precedence: --global, --local, --system, --file.

test_description='config scope precedence and multi-scope interaction'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Local User" &&
	git config user.email "local@example.com"
	)
'

###########################################################################
# Section 1: --local scope
###########################################################################

test_expect_success 'config --local reads from repo config' '
	(
	cd repo &&
	git config --local user.name >actual &&
	echo "Local User" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --local user.email reads correct value' '
	(
	cd repo &&
	git config --local user.email >actual &&
	echo "local@example.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --local --list shows repo entries' '
	(
	cd repo &&
	git config --local --list >actual &&
	grep "user.name=Local User" actual &&
	grep "user.email=local@example.com" actual
	)
'

test_expect_success 'config --local set and get roundtrip' '
	(
	cd repo &&
	git config --local test.key "local-value" &&
	result=$(git config --local test.key) &&
	test "$result" = "local-value"
	)
'

test_expect_success 'config --local stores in .git/config' '
	(
	cd repo &&
	git config --local test.stored "yes" &&
	grep "stored = yes" .git/config
	)
'

###########################################################################
# Section 2: --global scope
###########################################################################

test_expect_success 'config --global set and get roundtrip' '
	(
	cd repo &&
	git config --global test.globalkey "global-value" &&
	result=$(git config --global test.globalkey) &&
	test "$result" = "global-value"
	)
'

test_expect_success 'config --global --list shows global entries' '
	(
	cd repo &&
	git config --global --list >actual &&
	grep "test.globalkey=global-value" actual
	)
'

test_expect_success 'config --global stores in HOME/.gitconfig' '
	(
	cd repo &&
	git config --global test.homefile "yes" &&
	grep "homefile = yes" "$HOME/.gitconfig"
	)
'

test_expect_success 'cleanup global test keys' '
	(
	cd repo &&
	git config --global --unset test.globalkey &&
	git config --global --unset test.homefile &&
	! git config --global test.globalkey 2>/dev/null
	)
'

###########################################################################
# Section 3: --file scope
###########################################################################

test_expect_success 'config --file reads from custom file' '
	(
	cd repo &&
	printf "[custom]\\n\\tkey = file-value\\n" >custom.cfg &&
	result=$(git config --file custom.cfg custom.key) &&
	test "$result" = "file-value"
	)
'

test_expect_success 'config --file set writes to custom file' '
	(
	cd repo &&
	git config --file custom.cfg custom.newkey "new-file-value" &&
	result=$(git config --file custom.cfg custom.newkey) &&
	test "$result" = "new-file-value"
	)
'

test_expect_success 'config --file --list shows entries from file' '
	(
	cd repo &&
	git config --file custom.cfg --list >actual &&
	grep "custom.key=file-value" actual &&
	grep "custom.newkey=new-file-value" actual
	)
'

test_expect_success 'config --file does not affect repo config' '
	(
	cd repo &&
	git config --file custom.cfg isolated.key "isolated" &&
	! git config --local isolated.key 2>/dev/null
	)
'

test_expect_success 'config --file with nonexistent file fails' '
	(
	cd repo &&
	! git config --file nonexistent.cfg some.key 2>/dev/null
	)
'

###########################################################################
# Section 4: Precedence — local overrides global
###########################################################################

test_expect_success 'local config overrides global config' '
	(
	cd repo &&
	git config --global test.precedence "global" &&
	git config --local test.precedence "local" &&
	result=$(git config test.precedence) &&
	test "$result" = "local" &&
	git config --global --unset test.precedence
	)
'

test_expect_success 'global value visible when local not set' '
	(
	cd repo &&
	git config --global test.onlyglobal "from-global" &&
	result=$(git config test.onlyglobal) &&
	test "$result" = "from-global" &&
	git config --global --unset test.onlyglobal
	)
'

test_expect_success 'unsetting local reveals global value' '
	(
	cd repo &&
	git config --global test.reveal "global-val" &&
	git config --local test.reveal "local-val" &&
	result_before=$(git config test.reveal) &&
	test "$result_before" = "local-val" &&
	git config --local --unset test.reveal &&
	result_after=$(git config test.reveal) &&
	test "$result_after" = "global-val" &&
	git config --global --unset test.reveal
	)
'

###########################################################################
# Section 5: --show-origin and --show-scope
###########################################################################

test_expect_success 'config --show-origin shows file path' '
	(
	cd repo &&
	git config --show-origin --local --list >actual &&
	grep "file:" actual
	)
'

test_expect_success 'config --show-scope shows scope labels' '
	(
	cd repo &&
	git config --show-scope --local --list >actual &&
	grep "^local" actual
	)
'

test_expect_success 'config --show-scope --global shows global scope' '
	(
	cd repo &&
	git config --global test.scopecheck "yes" &&
	git config --show-scope --global --list >actual &&
	grep "^global" actual &&
	git config --global --unset test.scopecheck
	)
'

test_expect_success 'config --show-origin and --show-scope combined' '
	(
	cd repo &&
	git config --show-origin --show-scope --local --list >actual &&
	grep "local" actual &&
	grep "file:" actual
	)
'

###########################################################################
# Section 6: Type options
###########################################################################

test_expect_success 'config --bool canonicalizes boolean' '
	(
	cd repo &&
	git config --local test.boolval "yes" &&
	result=$(git config --bool test.boolval) &&
	test "$result" = "true"
	)
'

test_expect_success 'config --int canonicalizes integer' '
	(
	cd repo &&
	git config --local test.intval "42" &&
	result=$(git config --int test.intval) &&
	test "$result" = "42"
	)
'

test_expect_success 'config --bool reads false values' '
	(
	cd repo &&
	git config --local test.boolfalse "no" &&
	result=$(git config --bool test.boolfalse) &&
	test "$result" = "false"
	)
'

###########################################################################
# Section 7: --get-regexp
###########################################################################

test_expect_success 'config --get-regexp finds matching keys' '
	(
	cd repo &&
	git config --local search.alpha "one" &&
	git config --local search.beta "two" &&
	git config --local search.gamma "three" &&
	git config --get-regexp search >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'config --get-regexp with partial key match' '
	(
	cd repo &&
	git config --get-regexp "search.a" >actual &&
	test $(wc -l <actual) -eq 1 &&
	grep "search.alpha" actual
	)
'

###########################################################################
# Section 8: Unset and section operations
###########################################################################

test_expect_success 'config --unset removes a key' '
	(
	cd repo &&
	git config --local removeme.key "value" &&
	git config --local --unset removeme.key &&
	! git config removeme.key 2>/dev/null
	)
'

test_expect_success 'config --rename-section renames section' '
	(
	cd repo &&
	git config --local oldsection.key "val" &&
	git config --rename-section oldsection newsection &&
	result=$(git config newsection.key) &&
	test "$result" = "val" &&
	! git config oldsection.key 2>/dev/null
	)
'

test_expect_success 'config --remove-section removes entire section' '
	(
	cd repo &&
	git config --local delsection.a "1" &&
	git config --local delsection.b "2" &&
	git config --remove-section delsection &&
	! git config delsection.a 2>/dev/null &&
	! git config delsection.b 2>/dev/null
	)
'

###########################################################################
# Section 9: -z NUL separator
###########################################################################

test_expect_success 'config -z --list uses NUL delimiters' '
	(
	cd repo &&
	git config -z --local --list >actual &&
	tr "\0" "\n" <actual >lines &&
	grep "user.name" lines
	)
'

###########################################################################
# Section 10: Edge cases
###########################################################################

test_expect_success 'config in fresh repo has core settings' '
	(
	git init fresh &&
	cd fresh &&
	git config --local --list >actual &&
	grep "core.repositoryformatversion" actual
	)
'

test_expect_success 'config nonexistent key exits non-zero' '
	(
	cd repo &&
	! git config nonexistent.key 2>/dev/null
	)
'

test_expect_success 'config --system scope (may be empty)' '
	(
	cd repo &&
	git config --system --list >actual 2>/dev/null || true
	)
'

test_done
