#!/bin/sh
# Tests for grit config with --type=bool and --type=int conversions.

test_description='grit config --type=bool and --type=int type coercion'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo' '
	(
	"$REAL_GIT" init repo &&
	cd repo
	)
'

###########################################################################
# Section 2: --type=bool reading
###########################################################################

test_expect_success 'config --type=bool returns true for "yes"' '
	(
	cd repo &&
	git config core.foo yes &&
	test "$(git config --type=bool core.foo)" = "true"
	)
'

test_expect_success 'config --type=bool returns true for "on"' '
	(
	cd repo &&
	git config core.foo on &&
	test "$(git config --type=bool core.foo)" = "true"
	)
'

test_expect_success 'config --type=bool returns true for "1"' '
	(
	cd repo &&
	git config core.foo 1 &&
	test "$(git config --type=bool core.foo)" = "true"
	)
'

test_expect_success 'config --type=bool returns true for "true"' '
	(
	cd repo &&
	git config core.foo true &&
	test "$(git config --type=bool core.foo)" = "true"
	)
'

test_expect_success 'config --type=bool returns false for "no"' '
	(
	cd repo &&
	git config core.foo no &&
	test "$(git config --type=bool core.foo)" = "false"
	)
'

test_expect_success 'config --type=bool returns false for "off"' '
	(
	cd repo &&
	git config core.foo off &&
	test "$(git config --type=bool core.foo)" = "false"
	)
'

test_expect_success 'config --type=bool returns false for "0"' '
	(
	cd repo &&
	git config core.foo 0 &&
	test "$(git config --type=bool core.foo)" = "false"
	)
'

test_expect_success 'config --type=bool returns false for "false"' '
	(
	cd repo &&
	git config core.foo false &&
	test "$(git config --type=bool core.foo)" = "false"
	)
'

test_expect_success 'config --type=bool fails for non-boolean value' '
	(
	cd repo &&
	git config core.foo "hello" &&
	test_must_fail git config --type=bool core.foo
	)
'

test_expect_success 'config --type=bool returns true for empty value (implicit true)' '
	(
	cd repo &&
	printf "[core]\n\tfoo\n" >.git/config &&
	test "$(git config --type=bool core.foo)" = "true"
	)
'

###########################################################################
# Section 3: --type=int reading
###########################################################################

test_expect_success 'config --type=int returns plain integer' '
	(
	cd repo &&
	git config core.bar 42 &&
	test "$(git config --type=int core.bar)" = "42"
	)
'

test_expect_success 'config --type=int expands k suffix' '
	(
	cd repo &&
	git config core.bar 2k &&
	test "$(git config --type=int core.bar)" = "2048"
	)
'

test_expect_success 'config --type=int expands K suffix' '
	(
	cd repo &&
	git config core.bar 3K &&
	test "$(git config --type=int core.bar)" = "3072"
	)
'

test_expect_success 'config --type=int expands m suffix' '
	(
	cd repo &&
	git config core.bar 1m &&
	test "$(git config --type=int core.bar)" = "1048576"
	)
'

test_expect_success 'config --type=int expands M suffix' '
	(
	cd repo &&
	git config core.bar 2M &&
	test "$(git config --type=int core.bar)" = "2097152"
	)
'

test_expect_success 'config --type=int expands g suffix' '
	(
	cd repo &&
	git config core.bar 1g &&
	test "$(git config --type=int core.bar)" = "1073741824"
	)
'

test_expect_success 'config --type=int expands G suffix' '
	(
	cd repo &&
	git config core.bar 1G &&
	test "$(git config --type=int core.bar)" = "1073741824"
	)
'

test_expect_success 'config --type=int returns 0' '
	(
	cd repo &&
	git config core.bar 0 &&
	test "$(git config --type=int core.bar)" = "0"
	)
'

test_expect_success 'config --type=int handles negative values' '
	(
	cd repo &&
	git config core.bar -1 &&
	test "$(git config --type=int core.bar)" = "-1"
	)
'

test_expect_success 'config --type=int fails for non-integer value' '
	(
	cd repo &&
	git config core.bar "not-a-number" &&
	test_must_fail git config --type=int core.bar
	)
'

###########################################################################
# Section 4: --bool flag (legacy alias)
###########################################################################

test_expect_success 'config --bool returns true for yes' '
	(
	cd repo &&
	git config core.legacy yes &&
	test "$(git config --bool core.legacy)" = "true"
	)
'

test_expect_success 'config --bool returns false for no' '
	(
	cd repo &&
	git config core.legacy no &&
	test "$(git config --bool core.legacy)" = "false"
	)
'

###########################################################################
# Section 5: --int flag (legacy alias)
###########################################################################

test_expect_success 'config --int returns expanded integer' '
	(
	cd repo &&
	git config core.legacyint 4k &&
	test "$(git config --int core.legacyint)" = "4096"
	)
'

test_expect_success 'config --int returns plain integer' '
	(
	cd repo &&
	git config core.legacyint 100 &&
	test "$(git config --int core.legacyint)" = "100"
	)
'

###########################################################################
# Section 6: Additional --type=bool cases
###########################################################################

test_expect_success 'config --type=bool with TRUE (case-insensitive)' '
	(
	cd repo &&
	git config core.bcase TRUE &&
	test "$(git config --type=bool core.bcase)" = "true"
	)
'

test_expect_success 'config --type=bool with False (case-insensitive)' '
	(
	cd repo &&
	git config core.bcase False &&
	test "$(git config --type=bool core.bcase)" = "false"
	)
'

test_expect_success 'config --type=bool with Yes (case-insensitive)' '
	(
	cd repo &&
	git config core.bcase Yes &&
	test "$(git config --type=bool core.bcase)" = "true"
	)
'

test_expect_success 'config --type=bool with No (case-insensitive)' '
	(
	cd repo &&
	git config core.bcase No &&
	test "$(git config --type=bool core.bcase)" = "false"
	)
'

###########################################################################
# Section 7: bool and int with different config scopes
###########################################################################

test_expect_success 'config --type=bool reads from local config' '
	(
	cd repo &&
	git config --local core.localbool on &&
	test "$(git config --type=bool core.localbool)" = "true"
	)
'

test_expect_success 'config --type=int reads from local config' '
	(
	cd repo &&
	git config --local core.localint 16k &&
	test "$(git config --type=int core.localint)" = "16384"
	)
'

test_expect_success 'config --type=bool with large int is true' '
	(
	cd repo &&
	git config core.notbool 42 &&
	git config --type=bool core.notbool >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 8: More int and bool edge cases
###########################################################################

test_expect_success 'config --type=int with 0k returns 0' '
	(
	cd repo &&
	git config core.zero 0k &&
	test "$(git config --type=int core.zero)" = "0"
	)
'

test_expect_success 'config --type=int handles 10m' '
	(
	cd repo &&
	git config core.bigm 10m &&
	test "$(git config --type=int core.bigm)" = "10485760"
	)
'

test_expect_success 'config --type=bool ON is true (case-insensitive)' '
	(
	cd repo &&
	git config core.onoff ON &&
	test "$(git config --type=bool core.onoff)" = "true"
	)
'

test_expect_success 'config --type=bool OFF is false (case-insensitive)' '
	(
	cd repo &&
	git config core.onoff OFF &&
	test "$(git config --type=bool core.onoff)" = "false"
	)
'

test_expect_success 'config --bool and --type=bool agree' '
	(
	cd repo &&
	git config core.agree yes &&
	test "$(git config --bool core.agree)" = "$(git config --type=bool core.agree)"
	)
'

test_expect_success 'config --int and --type=int agree' '
	(
	cd repo &&
	git config core.agreeint 5k &&
	test "$(git config --int core.agreeint)" = "$(git config --type=int core.agreeint)"
	)
'

test_done
