#!/bin/sh
# Tests for config --type=bool/int/path, --bool, --int, --path flags.

test_description='config type coercion and validation'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com"
	)
'

# ── --bool / --type=bool ─────────────────────────────────────────────────

test_expect_success 'config --bool: true values canonicalize to true' '
	(
	cd repo &&
	git config test.a yes &&
	git config --bool test.a >out &&
	echo "true" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --bool: on becomes true' '
	(
	cd repo &&
	git config test.b on &&
	git config --bool test.b >out &&
	echo "true" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --bool: false values canonicalize to false' '
	(
	cd repo &&
	git config test.c no &&
	git config --bool test.c >out &&
	echo "false" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --bool: off becomes false' '
	(
	cd repo &&
	git config test.d off &&
	git config --bool test.d >out &&
	echo "false" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --type=bool: true values' '
	(
	cd repo &&
	git config test.e true &&
	git config --type=bool test.e >out &&
	echo "true" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --type=bool: false values' '
	(
	cd repo &&
	git config test.f false &&
	git config --type=bool test.f >out &&
	echo "false" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --bool: 1 becomes true' '
	(
	cd repo &&
	git config test.g 1 &&
	git config --bool test.g >out &&
	echo "true" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --bool: 0 becomes false' '
	(
	cd repo &&
	git config test.h 0 &&
	git config --bool test.h >out &&
	echo "false" >expected &&
	test_cmp expected out
	)
'

# ── --int / --type=int ───────────────────────────────────────────────────

test_expect_success 'config --int: integer value passes through' '
	(
	cd repo &&
	git config test.num 42 &&
	git config --int test.num >out &&
	echo "42" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --type=int: integer value passes through' '
	(
	cd repo &&
	git config test.num2 100 &&
	git config --type=int test.num2 >out &&
	echo "100" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --int: zero' '
	(
	cd repo &&
	git config test.zero 0 &&
	git config --int test.zero >out &&
	echo "0" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --int: negative number' '
	(
	cd repo &&
	git config test.neg -5 &&
	git config --int test.neg >out &&
	echo "-5" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --int: k suffix (kilobytes)' '
	(
	cd repo &&
	git config test.kilo 8k &&
	git config --int test.kilo >out &&
	echo "8192" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --int: m suffix (megabytes)' '
	(
	cd repo &&
	git config test.mega 2m &&
	git config --int test.mega >out &&
	echo "2097152" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --int: g suffix (gigabytes)' '
	(
	cd repo &&
	git config test.giga 1g &&
	git config --int test.giga >out &&
	echo "1073741824" >expected &&
	test_cmp expected out
	)
'

# ── --path / --type=path ─────────────────────────────────────────────────

test_expect_success 'config --path: tilde expands to HOME' '
	(
	cd repo &&
	git config test.mypath "~/mydir" &&
	git config --path test.mypath >out &&
	expected="$HOME/mydir" &&
	echo "$expected" >exp &&
	test_cmp exp out
	)
'

test_expect_success 'config --type=path: tilde expands to HOME' '
	(
	cd repo &&
	git config test.mypath2 "~/another" &&
	git config --type=path test.mypath2 >out &&
	expected="$HOME/another" &&
	echo "$expected" >exp &&
	test_cmp exp out
	)
'

test_expect_success 'config --path: absolute path unchanged' '
	(
	cd repo &&
	git config test.abspath "/usr/local/bin" &&
	git config --path test.abspath >out &&
	echo "/usr/local/bin" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --path: relative path unchanged' '
	(
	cd repo &&
	git config test.relpath "foo/bar" &&
	git config --path test.relpath >out &&
	echo "foo/bar" >expected &&
	test_cmp expected out
	)
'

# ── Type flag with set operations ────────────────────────────────────────

test_expect_success 'config --bool: set boolean then read back' '
	(
	cd repo &&
	git config --bool test.setbool true &&
	git config test.setbool >out &&
	echo "true" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --int: set int then read back' '
	(
	cd repo &&
	git config --int test.setnum 99 &&
	git config test.setnum >out &&
	echo "99" >expected &&
	test_cmp expected out
	)
'

# ── Default config values ────────────────────────────────────────────────

test_expect_success 'config: core.bare is false by default' '
	(
	cd repo &&
	git config --bool core.bare >out &&
	echo "false" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config: core.repositoryformatversion as int' '
	(
	cd repo &&
	git config --int core.repositoryformatversion >out &&
	echo "0" >expected &&
	test_cmp expected out
	)
'

# ── Multiple values in same section ──────────────────────────────────────

test_expect_success 'config: multiple keys in same section' '
	(
	cd repo &&
	git config mysect.key1 "val1" &&
	git config mysect.key2 "val2" &&
	git config mysect.key1 >out1 &&
	git config mysect.key2 >out2 &&
	echo "val1" >exp1 &&
	echo "val2" >exp2 &&
	test_cmp exp1 out1 &&
	test_cmp exp2 out2
	)
'

test_expect_success 'config: overwrite existing key' '
	(
	cd repo &&
	git config mysect.key1 "original" &&
	git config mysect.key1 "updated" &&
	git config mysect.key1 >out &&
	echo "updated" >expected &&
	test_cmp expected out
	)
'

# ── Config list ──────────────────────────────────────────────────────────

test_expect_success 'config --list: shows all entries' '
	(
	cd repo &&
	git config --list >out &&
	grep "user.name=Test User" out &&
	grep "user.email=test@example.com" out
	)
'

test_expect_success 'config --list: includes custom keys' '
	(
	cd repo &&
	git config --list >out &&
	grep "mysect.key1" out
	)
'

# ── Unset and missing keys ──────────────────────────────────────────────

test_expect_success 'config: missing key returns error' '
	(
	cd repo &&
	test_must_fail git config nonexistent.key
	)
'

test_expect_success 'config --unset: removes a key' '
	(
	cd repo &&
	git config test.removeme "bye" &&
	git config --unset test.removeme &&
	test_must_fail git config test.removeme
	)
'

test_done
