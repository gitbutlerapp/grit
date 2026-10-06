#!/bin/sh

test_description='grit config get --all: retrieving multi-valued config keys'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup' '
	(
	grit init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo hello >file.txt &&
	grit add file.txt &&
	grit commit -m "initial"
	)
'

# ── basic get --all ──────────────────────────────────────────────────────

test_expect_success 'get --all returns single value for single-valued key' '
	(cd repo && grit config get --all user.name >../actual) &&
	echo "T" >expect &&
	test_cmp expect actual
'

test_expect_success 'get --all returns all values for multi-valued key' '
	(cd repo &&
	 $REAL_GIT config --add remote.origin.fetch "+refs/heads/*:refs/remotes/origin/*" &&
	 $REAL_GIT config --add remote.origin.fetch "+refs/tags/*:refs/tags/*" &&
	 grit config get --all remote.origin.fetch >../actual) &&
	cat >expect <<-\EOF &&
	+refs/heads/*:refs/remotes/origin/*
	+refs/tags/*:refs/tags/*
	EOF
	test_cmp expect actual
'

test_expect_success 'get --all with three values' '
	(cd repo &&
	 $REAL_GIT config --add test.multi "alpha" &&
	 $REAL_GIT config --add test.multi "beta" &&
	 $REAL_GIT config --add test.multi "gamma" &&
	 grit config get --all test.multi >../actual) &&
	cat >expect <<-\EOF &&
	alpha
	beta
	gamma
	EOF
	test_cmp expect actual
'

test_expect_success 'get without --all returns last value for multi-valued key' '
	(cd repo && grit config get test.multi >../actual) &&
	echo "gamma" >expect &&
	test_cmp expect actual
'

test_expect_success 'get --all for nonexistent key fails' '
	(cd repo && test_must_fail grit config get --all no.such.key)
'

test_expect_success 'legacy --get-all returns all values' '
	(cd repo && grit config --get-all test.multi >../actual) &&
	cat >expect <<-\EOF &&
	alpha
	beta
	gamma
	EOF
	test_cmp expect actual
'

# ── get --all with different value types ─────────────────────────────────

test_expect_success 'get --all with integer values' '
	(cd repo &&
	 $REAL_GIT config --add count.items "1" &&
	 $REAL_GIT config --add count.items "2" &&
	 $REAL_GIT config --add count.items "3" &&
	 grit config get --all count.items >../actual) &&
	cat >expect <<-\EOF &&
	1
	2
	3
	EOF
	test_cmp expect actual
'

test_expect_success 'get --all with boolean values' '
	(cd repo &&
	 $REAL_GIT config --add flags.opt "true" &&
	 $REAL_GIT config --add flags.opt "false" &&
	 grit config get --all flags.opt >../actual) &&
	cat >expect <<-\EOF &&
	true
	false
	EOF
	test_cmp expect actual
'

test_expect_success 'get --all with empty string values' '
	(cd repo &&
	 $REAL_GIT config --add empty.val "" &&
	 $REAL_GIT config --add empty.val "notempty" &&
	 grit config get --all empty.val >../actual) &&
	cat >expect <<-\EOF &&

	notempty
	EOF
	test_cmp expect actual
'

test_expect_success 'get --all with values containing spaces' '
	(cd repo &&
	 $REAL_GIT config --add spaced.val "hello world" &&
	 $REAL_GIT config --add spaced.val "foo bar baz" &&
	 grit config get --all spaced.val >../actual) &&
	cat >expect <<-\EOF &&
	hello world
	foo bar baz
	EOF
	test_cmp expect actual
'

test_expect_success 'get --all with values containing equals sign' '
	(cd repo &&
	 $REAL_GIT config --add equals.val "key=value" &&
	 $REAL_GIT config --add equals.val "a=b=c" &&
	 grit config get --all equals.val >../actual) &&
	cat >expect <<-\EOF &&
	key=value
	a=b=c
	EOF
	test_cmp expect actual
'

# ── get --all after modifications ────────────────────────────────────────

test_expect_success 'get --all after unsetting one value' '
	(cd repo &&
	 $REAL_GIT config --unset "test.multi" "beta" &&
	 grit config get --all test.multi >../actual) &&
	cat >expect <<-\EOF &&
	alpha
	gamma
	EOF
	test_cmp expect actual
'

test_expect_success 'get --all after adding value back' '
	(cd repo &&
	 $REAL_GIT config --add test.multi "delta" &&
	 grit config get --all test.multi >../actual) &&
	cat >expect <<-\EOF &&
	alpha
	gamma
	delta
	EOF
	test_cmp expect actual
'

test_expect_success 'get --all after grit config set replaces last value' '
	(cd repo &&
	 grit config set test.multi "replaced" &&
	 grit config get test.multi >../actual) &&
	echo "replaced" >expect &&
	test_cmp expect actual
'

test_expect_success 'get --all after set overwrites single-valued key' '
	(cd repo &&
	 grit config set single.key "original" &&
	 grit config set single.key "updated" &&
	 grit config get --all single.key >../actual) &&
	echo "updated" >expect &&
	test_cmp expect actual
'

# ── get --all with special characters ────────────────────────────────────

test_expect_success 'get --all with values containing hash' '
	(cd repo &&
	 $REAL_GIT config --add special.hash "value#1" &&
	 $REAL_GIT config --add special.hash "value#2" &&
	 grit config get --all special.hash >../actual) &&
	cat >expect <<-\EOF &&
	value#1
	value#2
	EOF
	test_cmp expect actual
'

test_expect_success 'get --all with values containing semicolons' '
	(cd repo &&
	 $REAL_GIT config --add special.semi "a;b" &&
	 $REAL_GIT config --add special.semi "c;d" &&
	 grit config get --all special.semi >../actual) &&
	cat >expect <<-\EOF &&
	a;b
	c;d
	EOF
	test_cmp expect actual
'

test_expect_success 'get --all with values containing backslash' '
	(cd repo &&
	 $REAL_GIT config --add special.bs "path\\to\\file" &&
	 $REAL_GIT config --add special.bs "another\\path" &&
	 grit config get --all special.bs >../actual) &&
	cat >expect <<-\EOF &&
	path\to\file
	another\path
	EOF
	test_cmp expect actual
'

# ── get --all with sections ──────────────────────────────────────────────

test_expect_success 'get --all distinguishes different sections' '
	(cd repo &&
	 $REAL_GIT config --add sec1.key "val1" &&
	 $REAL_GIT config --add sec2.key "val2" &&
	 grit config get --all sec1.key >../actual) &&
	echo "val1" >expect &&
	test_cmp expect actual
'

test_expect_success 'get --all distinguishes different subsections' '
	(cd repo &&
	 $REAL_GIT config --add "branch.main.remote" "origin" &&
	 $REAL_GIT config --add "branch.dev.remote" "upstream" &&
	 grit config get --all branch.main.remote >../actual) &&
	echo "origin" >expect &&
	test_cmp expect actual
'

test_expect_success 'get --all with case-insensitive section name' '
	(cd repo &&
	 $REAL_GIT config --add MixedCase.key "fromMixed" &&
	 grit config get --all mixedcase.key >../actual) &&
	echo "fromMixed" >expect &&
	test_cmp expect actual
'

test_expect_success 'get --all with case-insensitive variable name' '
	(cd repo &&
	 $REAL_GIT config --add casetest.MixedVar "thevalue" &&
	 grit config get --all casetest.mixedvar >../actual) &&
	echo "thevalue" >expect &&
	test_cmp expect actual
'

# ── get --all duplicate and edge cases ───────────────────────────────────

test_expect_success 'get --all shows duplicate values' '
	(cd repo &&
	 $REAL_GIT config --add dup.key "same" &&
	 $REAL_GIT config --add dup.key "same" &&
	 grit config get --all dup.key >../actual) &&
	cat >expect <<-\EOF &&
	same
	same
	EOF
	test_cmp expect actual
'

test_expect_success 'get --all after unset --all then re-add' '
	(cd repo &&
	 grit config unset --all dup.key &&
	 $REAL_GIT config --add dup.key "fresh" &&
	 grit config get --all dup.key >../actual) &&
	echo "fresh" >expect &&
	test_cmp expect actual
'

test_expect_success 'get --all with url-like values' '
	(cd repo &&
	 $REAL_GIT config --add remote.origin.url "https://github.com/user/repo.git" &&
	 grit config get --all remote.origin.url >../actual) &&
	echo "https://github.com/user/repo.git" >expect &&
	test_cmp expect actual
'

test_expect_success 'get --all with very long values' '
	long_val=$(printf "a%.0s" $(seq 1 200)) &&
	(cd repo &&
	 $REAL_GIT config --add longval.key "$long_val" &&
	 grit config get --all longval.key >../actual) &&
	echo "$long_val" >expect &&
	test_cmp expect actual
'

test_expect_success 'get --all five values for same key' '
	(cd repo &&
	 $REAL_GIT config --add fivevals.k "a" &&
	 $REAL_GIT config --add fivevals.k "b" &&
	 $REAL_GIT config --add fivevals.k "c" &&
	 $REAL_GIT config --add fivevals.k "d" &&
	 $REAL_GIT config --add fivevals.k "e" &&
	 grit config get --all fivevals.k >../actual) &&
	cat >expect <<-\EOF &&
	a
	b
	c
	d
	e
	EOF
	test_cmp expect actual
'

test_expect_success 'legacy --get-all also returns multi values' '
	(cd repo && grit config --get-all fivevals.k >../actual) &&
	cat >expect <<-\EOF &&
	a
	b
	c
	d
	e
	EOF
	test_cmp expect actual
'

test_expect_success 'get --all user.email returns configured email' '
	(cd repo && grit config get --all user.email >../actual) &&
	echo "t@t.com" >expect &&
	test_cmp expect actual
'

test_expect_success 'get --all core.bare returns false' '
	(cd repo && grit config get --all core.bare >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'get --all with default for missing key' '
	(cd repo && grit config get --all --default "fallback" missing.key >../actual) &&
	echo "fallback" >expect &&
	test_cmp expect actual
'

test_done
