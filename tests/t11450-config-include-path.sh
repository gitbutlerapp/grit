#!/bin/sh
# Tests for grit config include.path and includeIf directives.

test_description='grit config include.path and includeIf'

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
# Section 2: Basic include.path
###########################################################################

test_expect_success 'include.path includes another config file' '
	(
	cd repo &&
	cat >.git/other.config <<-EOF &&
	[user]
		name = Included User
	EOF
	git config include.path other.config &&
	test "$(git config user.name)" = "Included User"
	)
'

test_expect_success 'include.path with absolute path' '
	(
	cd repo &&
	cat >"$TRASH_DIRECTORY/abs.config" <<-EOF &&
	[user]
		email = abs@example.com
	EOF
	git config include.path "$TRASH_DIRECTORY/abs.config" &&
	test "$(git config user.email)" = "abs@example.com"
	)
'

test_expect_success 'include.path relative to config file' '
	(
	cd repo &&
	mkdir -p .git/config.d &&
	cat >.git/config.d/extra.config <<-EOF &&
	[core]
		includetest = works
	EOF
	git config include.path config.d/extra.config &&
	test "$(git config core.includetest)" = "works"
	)
'

test_expect_success 'include.path values from included file are readable' '
	(
	cd repo &&
	cat >.git/inc1.config <<-EOF &&
	[test]
		value1 = hello
		value2 = world
	EOF
	git config include.path inc1.config &&
	test "$(git config test.value1)" = "hello" &&
	test "$(git config test.value2)" = "world"
	)
'

test_expect_success 'include.path with tilde expands to HOME' '
	(
	cd repo &&
	cat >"$HOME/home.config" <<-EOF &&
	[home]
		key = fromhome
	EOF
	git config include.path "~/home.config" &&
	test "$(git config home.key)" = "fromhome"
	)
'

test_expect_success 'local value set after include also appears in get-all' '
	(
	cd repo &&
	cat >.git/early.config <<-EOF &&
	[over]
		ride = included
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = early.config
	EOF
	git config over.ride local &&
	git config --get-all over.ride >out &&
	grep "local" out &&
	grep "included" out
	)
'

test_expect_success 'included values override earlier local values' '
	(
	cd repo &&
	cat >.git/late.config <<-EOF &&
	[over]
		ride2 = frominclude
	EOF
	cat >.git/config <<-EOF &&
	[over]
		ride2 = original
	[include]
		path = late.config
	EOF
	test "$(git config over.ride2)" = "frominclude"
	)
'

test_expect_success 'chained includes (A includes B)' '
	(
	cd repo &&
	cat >.git/chain-b.config <<-EOF &&
	[chain]
		depth = two
	EOF
	cat >.git/chain-a.config <<-EOF &&
	[include]
		path = chain-b.config
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = chain-a.config
	EOF
	test "$(git config chain.depth)" = "two"
	)
'

test_expect_success 'missing include.path file is silently ignored' '
	(
	cd repo &&
	cat >.git/config <<-EOF &&
	[include]
		path = nonexistent.config
	[fallback]
		key = present
	EOF
	test "$(git config fallback.key)" = "present"
	)
'

test_expect_success 'multiple include.path directives' '
	(
	cd repo &&
	cat >.git/inc-a.config <<-EOF &&
	[multi]
		a = alpha
	EOF
	cat >.git/inc-b.config <<-EOF &&
	[multi]
		b = beta
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = inc-a.config
		path = inc-b.config
	EOF
	test "$(git config multi.a)" = "alpha" &&
	test "$(git config multi.b)" = "beta"
	)
'

###########################################################################
# Section 3: More include.path patterns
###########################################################################

test_expect_success 'include.path with dot-relative path ./file' '
	(
	cd repo &&
	cat >.git/dotrel.config <<-EOF &&
	[dotrel]
		key = found
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = ./dotrel.config
	EOF
	test "$(git config dotrel.key)" = "found"
	)
'

test_expect_success 'include.path absolute path to .git subfile' '
	(
	cd repo &&
	cat >.git/absrel.config <<-EOF &&
	[absrel]
		key = found
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = $TRASH_DIRECTORY/repo/.git/absrel.config
	EOF
	test "$(git config absrel.key)" = "found"
	)
'

test_expect_success 'include of file that includes another file (3 deep)' '
	(
	cd repo &&
	cat >.git/deep-c.config <<-EOF &&
	[deep]
		level = three
	EOF
	cat >.git/deep-b.config <<-EOF &&
	[include]
		path = deep-c.config
	EOF
	cat >.git/deep-a.config <<-EOF &&
	[include]
		path = deep-b.config
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = deep-a.config
	EOF
	test "$(git config deep.level)" = "three"
	)
'

test_expect_success 'include.path overridden by later include.path' '
	(
	cd repo &&
	cat >.git/first.config <<-EOF &&
	[prio]
		key = first
	EOF
	cat >.git/second.config <<-EOF &&
	[prio]
		key = second
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = first.config
	[include]
		path = second.config
	EOF
	test "$(git config prio.key)" = "second"
	)
'

###########################################################################
# Section 4: Include interactions with --list and --get-regexp
###########################################################################

test_expect_success 'config --list shows values from includes' '
	(
	cd repo &&
	cat >.git/list-inc.config <<-EOF &&
	[listinc]
		alpha = one
		beta = two
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = list-inc.config
	EOF
	git config --list >out &&
	grep "listinc.alpha=one" out &&
	grep "listinc.beta=two" out
	)
'

test_expect_success 'config --get-regexp finds included values' '
	(
	cd repo &&
	cat >.git/regexp-inc.config <<-EOF &&
	[regexp]
		foo = bar
		fob = baz
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = regexp-inc.config
	EOF
	git config --get-regexp "regexp.fo" >out &&
	test_line_count = 2 out
	)
'

test_expect_success 'included config contributes to --get-all' '
	(
	cd repo &&
	cat >.git/getall-inc.config <<-EOF &&
	[getall]
		val = from-include
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = getall-inc.config
	[getall]
		val = from-local
	EOF
	git config --get-all getall.val >out &&
	test_line_count = 2 out
	)
'

test_expect_success 'include.path with boolean values in included file' '
	(
	cd repo &&
	cat >.git/bool-inc.config <<-EOF &&
	[boolinc]
		flag = true
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = bool-inc.config
	EOF
	test "$(git config --type=bool boolinc.flag)" = "true"
	)
'

###########################################################################
# Section 5: include in global config
###########################################################################

test_expect_success 'global config include.path works' '
	(
	cd repo &&
	cat >"$HOME/global-inc.config" <<-EOF &&
	[globalinc]
		key = fromglobal
	EOF
	cat >"$HOME/.gitconfig" <<-EOF &&
	[include]
		path = global-inc.config
	EOF
	test "$(git config globalinc.key)" = "fromglobal"
	)
'

test_expect_success 'local config overrides global included config' '
	(
	cd repo &&
	cat >"$HOME/global-over.config" <<-EOF &&
	[over]
		global = fromglobal
	EOF
	cat >"$HOME/.gitconfig" <<-EOF &&
	[include]
		path = global-over.config
	EOF
	git config over.global fromlocal &&
	test "$(git config over.global)" = "fromlocal"
	)
'

###########################################################################
# Section 6: Edge cases
###########################################################################

test_expect_success 'include.path with empty file is okay' '
	(
	cd repo &&
	>"$TRASH_DIRECTORY/empty.config" &&
	cat >.git/config <<-EOF &&
	[include]
		path = $TRASH_DIRECTORY/empty.config
	[empty]
		test = works
	EOF
	test "$(git config empty.test)" = "works"
	)
'

test_expect_success 'include.path file with only comments' '
	(
	cd repo &&
	cat >"$TRASH_DIRECTORY/comments.config" <<-EOF &&
	# this is a comment
	; this too
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = $TRASH_DIRECTORY/comments.config
	[comment]
		test = works
	EOF
	test "$(git config comment.test)" = "works"
	)
'

test_expect_success 'include.path with multiple sections in included file' '
	(
	cd repo &&
	cat >.git/multi-section.config <<-EOF &&
	[sec1]
		key = val1
	[sec2]
		key = val2
	[sec3]
		key = val3
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = multi-section.config
	EOF
	test "$(git config sec1.key)" = "val1" &&
	test "$(git config sec2.key)" = "val2" &&
	test "$(git config sec3.key)" = "val3"
	)
'

test_expect_success 'include.path with subsections in included file' '
	(
	cd repo &&
	cat >.git/subsec.config <<-\EOF &&
	[remote "origin"]
		url = https://example.com/repo.git
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = subsec.config
	EOF
	test "$(git config remote.origin.url)" = "https://example.com/repo.git"
	)
'

test_expect_success 'config --list shows included values' '
	(
	cd repo &&
	cat >.git/listed.config <<-EOF &&
	[listed]
		key = visible
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = listed.config
	EOF
	git config --list >output &&
	grep "listed.key=visible" output
	)
'

test_expect_success 'config --get-all with included multi-values' '
	(
	cd repo &&
	cat >.git/multi-val.config <<-EOF &&
	[multi]
		val = one
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = multi-val.config
	[multi]
		val = two
	EOF
	git config --get-all multi.val >output &&
	test_line_count = 2 output
	)
'

test_expect_success 'include with different key types preserved' '
	(
	cd repo &&
	cat >.git/types.config <<-EOF &&
	[types]
		boolval = true
		intval = 42
		strval = hello
	EOF
	cat >.git/config <<-EOF &&
	[include]
		path = types.config
	EOF
	test "$(git config --type=bool types.boolval)" = "true" &&
	test "$(git config --type=int types.intval)" = "42" &&
	test "$(git config types.strval)" = "hello"
	)
'

test_done
