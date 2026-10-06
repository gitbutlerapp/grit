#!/bin/sh
# Test grit config unset, unset --all, legacy --unset/--unset-all, and edge cases.

test_description='grit config unset and unset --all'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo
	)
'

test_expect_success 'set a config key and verify it exists' '
	(
	cd repo &&
	grit config set user.name "Alice" &&
	grit config get user.name >actual &&
	echo "Alice" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'unset a config key removes it' '
	(
	cd repo &&
	grit config set test.key "value1" &&
	grit config unset test.key &&
	! grit config get test.key 2>/dev/null
	)
'

test_expect_success 'unset nonexistent key fails' '
	(
	cd repo &&
	! grit config unset no.such.key 2>/dev/null
	)
'

test_expect_success 'unset one key does not affect others' '
	(
	cd repo &&
	grit config set keep.this "yes" &&
	grit config set remove.this "no" &&
	grit config unset remove.this &&
	grit config get keep.this >actual &&
	echo "yes" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'manually create multi-value key in config' '
	(
	cd repo &&
	cat >>.git/config <<-\EOF &&
	[multi]
		key = val1
		key = val2
	EOF
	grit config get --all multi.key >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'unset --all removes all occurrences of multi-value key' '
	(
	cd repo &&
	grit config unset --all multi.key &&
	! grit config get multi.key 2>/dev/null
	)
'

test_expect_success 'manually create triple multi-value and unset --all' '
	(
	cd repo &&
	cat >>.git/config <<-\EOF &&
	[triple]
		key = a
		key = b
		key = c
	EOF
	grit config get --all triple.key >actual &&
	test_line_count = 3 actual &&
	grit config unset --all triple.key &&
	! grit config get triple.key 2>/dev/null
	)
'

test_expect_success 'legacy --unset flag works' '
	(
	cd repo &&
	grit config set legacy.key "legval" &&
	grit config --unset legacy.key &&
	! grit config get legacy.key 2>/dev/null
	)
'

test_expect_success 'legacy --unset-all flag works on multi-value' '
	(
	cd repo &&
	cat >>.git/config <<-\EOF &&
	[legmulti]
		key = a
		key = b
	EOF
	grit config --unset-all legmulti.key &&
	! grit config get legmulti.key 2>/dev/null
	)
'

test_expect_success 'unset in global scope with --global' '
	(
	cd repo &&
	grit config --global set globaltest.key "gval" &&
	grit config --global get globaltest.key >actual &&
	echo "gval" >expected &&
	test_cmp expected actual &&
	grit config --global unset globaltest.key &&
	! grit config --global get globaltest.key 2>/dev/null
	)
'

test_expect_success 'unset local key leaves global key intact' '
	(
	cd repo &&
	grit config --global set scope.key "global" &&
	grit config --local set scope.key "local" &&
	grit config --local unset scope.key &&
	grit config get scope.key >actual &&
	echo "global" >expected &&
	test_cmp expected actual &&
	grit config --global unset scope.key
	)
'

test_expect_success 'config list shows key before unset, not after' '
	(
	cd repo &&
	grit config set visible.key "here" &&
	grit config list >listed &&
	grep "visible.key" listed &&
	grit config unset visible.key &&
	grit config list >listed2 &&
	! grep "visible.key" listed2
	)
'

test_expect_success 'unset section.subsection.key works' '
	(
	cd repo &&
	grit config set section.sub.key "nested" &&
	grit config get section.sub.key >actual &&
	echo "nested" >expected &&
	test_cmp expected actual &&
	grit config unset section.sub.key &&
	! grit config get section.sub.key 2>/dev/null
	)
'

test_expect_success 'unset preserves other keys in same section' '
	(
	cd repo &&
	grit config set mysect.a "1" &&
	grit config set mysect.b "2" &&
	grit config set mysect.c "3" &&
	grit config unset mysect.b &&
	grit config get mysect.a >actual_a &&
	echo "1" >expected_a &&
	test_cmp expected_a actual_a &&
	grit config get mysect.c >actual_c &&
	echo "3" >expected_c &&
	test_cmp expected_c actual_c &&
	! grit config get mysect.b 2>/dev/null
	)
'

test_expect_success 'config file is valid after unset' '
	(
	cd repo &&
	grit config set valid.check "yes" &&
	grit config unset valid.check &&
	grit config list >/dev/null
	)
'

test_expect_success 'unset value with special characters' '
	(
	cd repo &&
	grit config set special.key "hello world = foo" &&
	grit config get special.key >actual &&
	echo "hello world = foo" >expected &&
	test_cmp expected actual &&
	grit config unset special.key &&
	! grit config get special.key 2>/dev/null
	)
'

test_expect_success 'unset value that is empty string' '
	(
	cd repo &&
	grit config set empty.key "" &&
	grit config unset empty.key &&
	! grit config get empty.key 2>/dev/null
	)
'

test_expect_success 'set and unset boolean-like values' '
	(
	cd repo &&
	grit config set bool.key "true" &&
	grit config get bool.key >actual &&
	echo "true" >expected &&
	test_cmp expected actual &&
	grit config unset bool.key
	)
'

test_expect_success 'set and unset numeric values' '
	(
	cd repo &&
	grit config set num.key "42" &&
	grit config get num.key >actual &&
	echo "42" >expected &&
	test_cmp expected actual &&
	grit config unset num.key
	)
'

test_expect_success 'rapid set/unset cycle does not corrupt config' '
	(
	cd repo &&
	for i in 1 2 3 4 5; do
		grit config set cycle.key "val$i" &&
		grit config unset cycle.key || return 1
	done &&
	! grit config get cycle.key 2>/dev/null &&
	grit config list >/dev/null
	)
'

test_expect_success 'unset --all on single-value key works' '
	(
	cd repo &&
	grit config set single.key "onlyone" &&
	grit config unset --all single.key &&
	! grit config get single.key 2>/dev/null
	)
'

test_expect_success 'unset key with dots in subsection' '
	(
	cd repo &&
	grit config set "url.https://example.com.insteadOf" "ex:" &&
	grit config get "url.https://example.com.insteadOf" >actual &&
	echo "ex:" >expected &&
	test_cmp expected actual &&
	grit config unset "url.https://example.com.insteadOf" &&
	! grit config get "url.https://example.com.insteadOf" 2>/dev/null
	)
'

test_expect_success 'unset preserves comments in config file' '
	(
	cd repo &&
	grit config set comment.test "val" &&
	echo "# this is a comment" >>.git/config &&
	grit config unset comment.test &&
	grep "# this is a comment" .git/config
	)
'

test_expect_success 'config set after unset re-creates key' '
	(
	cd repo &&
	grit config set reuse.key "original" &&
	grit config unset reuse.key &&
	grit config set reuse.key "new" &&
	grit config get reuse.key >actual &&
	echo "new" >expected &&
	test_cmp expected actual &&
	grit config unset reuse.key
	)
'

test_expect_success 'unset all keys in a section one by one' '
	(
	cd repo &&
	grit config set cleanup.a "1" &&
	grit config set cleanup.b "2" &&
	grit config set cleanup.c "3" &&
	grit config unset cleanup.a &&
	grit config unset cleanup.b &&
	grit config unset cleanup.c &&
	grit config list >listed &&
	! grep "cleanup\." listed
	)
'

test_expect_success 'config get --default with unset key returns default' '
	(
	cd repo &&
	grit config get --default "fallback" nokey.here >actual &&
	echo "fallback" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'unset user.email and re-set it' '
	(
	cd repo &&
	grit config set user.email "old@example.com" &&
	grit config unset user.email &&
	! grit config get user.email 2>/dev/null &&
	grit config set user.email "new@example.com" &&
	grit config get user.email >actual &&
	echo "new@example.com" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'config list after multiple unset operations is clean' '
	(
	cd repo &&
	grit config set temp1.key "a" &&
	grit config set temp2.key "b" &&
	grit config set temp3.key "c" &&
	grit config unset temp1.key &&
	grit config unset temp2.key &&
	grit config unset temp3.key &&
	grit config list >final &&
	! grep "temp[123]\.key" final
	)
'

test_expect_success 'unset --all after manual multi-value leaves config valid' '
	(
	cd repo &&
	cat >>.git/config <<-\EOF &&
	[postunset]
		k = x
		k = y
	EOF
	grit config unset --all postunset.k &&
	grit config list >/dev/null
	)
'

test_expect_success 'remove-section removes entire section' '
	(
	cd repo &&
	grit config set removeme.a "1" &&
	grit config set removeme.b "2" &&
	grit config remove-section removeme &&
	grit config list >listed &&
	! grep "removeme\." listed
	)
'

test_expect_success 'unset with --file flag on external config' '
	(
	cd repo &&
	echo "[ext]" >../ext.cfg &&
	echo "	key = val" >>../ext.cfg &&
	grit config --file ../ext.cfg get ext.key >actual &&
	echo "val" >expected &&
	test_cmp expected actual &&
	grit config --file ../ext.cfg unset ext.key &&
	! grit config --file ../ext.cfg get ext.key 2>/dev/null
	)
'

test_expect_success 'legacy positional unset: grit config --unset key' '
	(
	cd repo &&
	grit config set posleg.key "pval" &&
	grit config --unset posleg.key &&
	! grit config get posleg.key 2>/dev/null
	)
'

test_done
