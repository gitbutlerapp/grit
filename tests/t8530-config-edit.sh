#!/bin/sh
# Tests for 'grit config' — --replace-all, --bool, --int, --type, subcommands.

test_description='config --replace-all, --bool, --int, type coercion, and subcommands'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup: create repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com"
	)
'

# ── Basic set and get ────────────────────────────────────────────────────────

test_expect_success 'config set and get a value' '
	(
	cd repo &&
	git config core.editor vim &&
	git config core.editor >actual &&
	echo "vim" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set overwrites previous value' '
	(
	cd repo &&
	git config core.editor nano &&
	git config core.editor >actual &&
	echo "nano" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get on nonexistent key fails' '
	(
	cd repo &&
	test_must_fail git config no.such.key 2>err
	)
'

# ── Bool type ────────────────────────────────────────────────────────────────

test_expect_success 'config --bool true normalizes to true' '
	(
	cd repo &&
	git config test.flag yes &&
	git config --bool test.flag >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --bool false normalizes to false' '
	(
	cd repo &&
	git config test.flag no &&
	git config --bool test.flag >actual &&
	echo "false" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --bool on normalizes to true' '
	(
	cd repo &&
	git config test.flag on &&
	git config --bool test.flag >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --bool off normalizes to false' '
	(
	cd repo &&
	git config test.flag off &&
	git config --bool test.flag >actual &&
	echo "false" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --bool 1 normalizes to true' '
	(
	cd repo &&
	git config test.num 1 &&
	git config --bool test.num >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --bool 0 normalizes to false' '
	(
	cd repo &&
	git config test.num 0 &&
	git config --bool test.num >actual &&
	echo "false" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --bool rejects non-boolean' '
	(
	cd repo &&
	git config test.bad "not-a-bool" &&
	test_must_fail git config --bool test.bad 2>err
	)
'

# ── Int type ─────────────────────────────────────────────────────────────────

test_expect_success 'config --int returns numeric value' '
	(
	cd repo &&
	git config test.count 42 &&
	git config --int test.count >actual &&
	echo "42" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --int handles k suffix' '
	(
	cd repo &&
	git config test.size 8k &&
	git config --int test.size >actual &&
	echo "8192" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --int handles m suffix' '
	(
	cd repo &&
	git config test.size 2m &&
	git config --int test.size >actual &&
	echo "2097152" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --int handles g suffix' '
	(
	cd repo &&
	git config test.size 1g &&
	git config --int test.size >actual &&
	echo "1073741824" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --int rejects non-numeric value' '
	(
	cd repo &&
	git config test.bad "abc" &&
	test_must_fail git config --int test.bad 2>err
	)
'

# ── --type bool / --type int ─────────────────────────────────────────────────

test_expect_success 'config --type=bool works like --bool' '
	(
	cd repo &&
	git config test.tbool yes &&
	git config --type=bool test.tbool >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --type=int works like --int' '
	(
	cd repo &&
	git config test.tint 4k &&
	git config --type=int test.tint >actual &&
	echo "4096" >expect &&
	test_cmp expect actual
	)
'

# ── Multivar and --replace-all ───────────────────────────────────────────────

test_expect_success 'config can add multiple values for same key' '
	(
	cd repo &&
	git config --unset-all test.multi 2>/dev/null || true &&
	git config test.multi "alpha" &&
	printf "[test]\n\tmulti = beta\n" >>.git/config &&
	git config --get-all test.multi >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'config --get-all lists all values' '
	(
	cd repo &&
	git config --get-all test.multi >actual &&
	grep "alpha" actual &&
	grep "beta" actual
	)
'

test_expect_success 'config --replace-all adds replacement value' '
	(
	cd repo &&
	git config --replace-all test.multi "gamma" &&
	git config --get-all test.multi >actual &&
	grep "gamma" actual
	)
'

# ── Unset ────────────────────────────────────────────────────────────────────

test_expect_success 'config --unset removes a key' '
	(
	cd repo &&
	git config test.remove me &&
	git config test.remove >out &&
	grep "me" out &&
	git config --unset test.remove &&
	test_must_fail git config test.remove 2>err
	)
'

test_expect_success 'config --unset-all removes all occurrences' '
	(
	cd repo &&
	git config test.dup "one" &&
	printf "[test]\n\tdup = two\n" >>.git/config &&
	git config --unset-all test.dup &&
	test_must_fail git config test.dup 2>err
	)
'

# ── Section operations ───────────────────────────────────────────────────────

test_expect_success 'config --remove-section removes entire section' '
	(
	cd repo &&
	git config removeme.key1 val1 &&
	git config removeme.key2 val2 &&
	git config --remove-section removeme &&
	test_must_fail git config removeme.key1 2>err &&
	test_must_fail git config removeme.key2 2>err
	)
'

test_expect_success 'config --rename-section renames a section' '
	(
	cd repo &&
	git config oldsec.key val &&
	git config --rename-section oldsec newsec &&
	git config newsec.key >actual &&
	echo "val" >expect &&
	test_cmp expect actual &&
	test_must_fail git config oldsec.key 2>err
	)
'

# ── --list ───────────────────────────────────────────────────────────────────

test_expect_success 'config --list shows all entries' '
	(
	cd repo &&
	git config --list >out &&
	grep "user.name=Test User" out &&
	grep "user.email=test@example.com" out
	)
'

test_expect_success 'config list subcommand works' '
	(
	cd repo &&
	git config list >out &&
	grep "user.name" out
	)
'

# ── --get-regexp ─────────────────────────────────────────────────────────────

test_expect_success 'config --get-regexp matches pattern' '
	(
	cd repo &&
	git config --get-regexp "user" >out &&
	grep "user.name" out &&
	grep "user.email" out
	)
'

test_expect_success 'config --get-regexp with no match fails' '
	(
	cd repo &&
	test_must_fail git config --get-regexp "zzz-no-match" 2>err
	)
'

# ── Scope flags: --local, --global ──────────────────────────────────────────

test_expect_success 'config --local reads from repo config' '
	(
	cd repo &&
	git config --local user.name >actual &&
	echo "Test User" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --global sets in global config' '
	(
	cd repo &&
	git config --global global.test "hello" &&
	git config --global global.test >actual &&
	echo "hello" >expect &&
	test_cmp expect actual
	)
'

# ── -z NUL delimiter ────────────────────────────────────────────────────────

test_expect_success 'config -z --list uses NUL delimiters' '
	(
	cd repo &&
	git config -z --list >out &&
	tr "\0" "\n" <out >readable &&
	grep "user.name" readable
	)
'

# ── --show-origin ────────────────────────────────────────────────────────────

test_expect_success 'config --show-origin includes value' '
	(
	cd repo &&
	git config --show-origin user.name >out &&
	grep "Test User" out
	)
'

test_done
