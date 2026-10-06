#!/bin/sh
# Tests for grit config --get-regexp and config get --regexp.

test_description='grit config --get-regexp and config get --regexp pattern matching'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repository with config entries' '
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

test_expect_success 'setup: populate config with multiple keys' '
	(
	cd repo &&
	grit config color.ui "auto" &&
	grit config color.diff "always" &&
	grit config color.status "never" &&
	grit config core.autocrlf "false" &&
	grit config core.bare "false" &&
	grit config core.filemode "true" &&
	grit config alias.co "checkout" &&
	grit config alias.br "branch" &&
	grit config alias.ci "commit" &&
	grit config alias.st "status"
	)
'

###########################################################################
# Section 2: Basic --get-regexp (legacy)
###########################################################################

test_expect_success 'config --get-regexp matches color keys' '
	(
	cd repo &&
	grit config --get-regexp "color" >actual &&
	grep "color.ui" actual &&
	grep "color.diff" actual &&
	grep "color.status" actual
	)
'

test_expect_success 'config --get-regexp does not match unrelated keys' '
	(
	cd repo &&
	grit config --get-regexp "color" >actual &&
	! grep "core\." actual &&
	! grep "alias\." actual
	)
'

test_expect_success 'config --get-regexp matches alias keys' '
	(
	cd repo &&
	grit config --get-regexp "alias" >actual &&
	grep "alias.co" actual &&
	grep "alias.br" actual &&
	grep "alias.ci" actual &&
	grep "alias.st" actual
	)
'

test_expect_success 'config --get-regexp output format key space value' '
	(
	cd repo &&
	grit config --get-regexp "color.ui" >actual &&
	echo "color.ui auto" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --get-regexp alias matches real git output' '
	(
	cd repo &&
	grit config --get-regexp "alias" | sort >actual &&
	"$REAL_GIT" config --get-regexp "alias" | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --get-regexp with no matches exits nonzero' '
	(
	cd repo &&
	test_must_fail grit config --get-regexp "nonexistent_key_xyz"
	)
'

test_expect_success 'config --get-regexp matches core keys' '
	(
	cd repo &&
	grit config --get-regexp "core" >actual &&
	grep "core.bare" actual &&
	grep "core.filemode" actual
	)
'

test_expect_success 'config --get-regexp color matches real git' '
	(
	cd repo &&
	grit config --get-regexp "color" | sort >actual &&
	"$REAL_GIT" config --get-regexp "color" | sort >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: config get --regexp (new subcommand)
###########################################################################

test_expect_success 'config get --regexp outputs values only' '
	(
	cd repo &&
	grit config get --regexp "color" >actual &&
	grep "auto" actual &&
	grep "always" actual &&
	grep "never" actual
	)
'

test_expect_success 'config get --regexp values do not contain key names' '
	(
	cd repo &&
	grit config get --regexp "color" >actual &&
	! grep "color.ui" actual
	)
'

test_expect_success 'config get --regexp with --show-names shows keys and values' '
	(
	cd repo &&
	grit config get --regexp "alias" --show-names >actual &&
	grep "alias.co" actual &&
	grep "checkout" actual &&
	grep "alias.br" actual &&
	grep "branch" actual
	)
'

test_expect_success 'config get --regexp no match exits nonzero' '
	(
	cd repo &&
	test_must_fail grit config get --regexp "zzz_nonexistent"
	)
'

test_expect_success 'config get --regexp --show-names matches --get-regexp output' '
	(
	cd repo &&
	grit config get --regexp "alias" --show-names | sort >actual &&
	grit config --get-regexp "alias" | sort >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 4: --get-regexp with --file
###########################################################################

test_expect_success 'config --file --get-regexp works on custom file' '
	(
	cd repo &&
	grit config --file custom.cfg sec.alpha "one" &&
	grit config --file custom.cfg sec.beta "two" &&
	grit config --file custom.cfg other.gamma "three" &&
	grit config --file custom.cfg --get-regexp "sec" >actual &&
	grep "sec.alpha" actual &&
	grep "sec.beta" actual &&
	! grep "other.gamma" actual
	)
'

test_expect_success 'config --file --get-regexp matches real git' '
	(
	cd repo &&
	grit config --file custom.cfg --get-regexp "sec" | sort >actual &&
	"$REAL_GIT" config --file custom.cfg --get-regexp "sec" | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file --get-regexp with no match fails' '
	(
	cd repo &&
	test_must_fail grit config --file custom.cfg --get-regexp "nope_xyz"
	)
'

test_expect_success 'config --file --get-regexp only matches in file scope' '
	(
	cd repo &&
	grit config --file custom.cfg --get-regexp "other" >actual &&
	grep "other.gamma three" actual &&
	test_line_count = 1 actual
	)
'

###########################################################################
# Section 5: Pattern matching behavior
###########################################################################

test_expect_success 'config --get-regexp dot in key name matches' '
	(
	cd repo &&
	grit config --get-regexp "color.ui" >actual &&
	grep "color.ui" actual
	)
'

test_expect_success 'config --get-regexp user.name matches single entry' '
	(
	cd repo &&
	grit config --get-regexp "user.name" >actual &&
	grep "Test User" actual
	)
'

test_expect_success 'config --get-regexp user matches name and email' '
	(
	cd repo &&
	grit config --get-regexp "user" >actual &&
	grep "user.name" actual &&
	grep "user.email" actual
	)
'

test_expect_success 'config --get-regexp returns all alias entries' '
	(
	cd repo &&
	grit config --get-regexp "alias" >actual &&
	test_line_count = 4 actual
	)
'

test_expect_success 'config --get-regexp alias count matches real git' '
	(
	cd repo &&
	grit config --get-regexp "alias" >grit_out &&
	"$REAL_GIT" config --get-regexp "alias" >git_out &&
	grit_count=$(wc -l <grit_out | tr -d " ") &&
	git_count=$(wc -l <git_out | tr -d " ") &&
	test "$grit_count" = "$git_count"
	)
'

###########################################################################
# Section 6: --get-regexp with --global
###########################################################################

test_expect_success 'config --global --get-regexp works' '
	(
	cd repo &&
	grit config --global test.glob.a "1" &&
	grit config --global test.glob.b "2" &&
	grit config --global --get-regexp "test.glob" >actual &&
	grep "test.glob.a" actual &&
	grep "test.glob.b" actual
	)
'

test_expect_success 'config --global --get-regexp matches real git' '
	(
	cd repo &&
	grit config --global --get-regexp "test.glob" | sort >actual &&
	"$REAL_GIT" config --global --get-regexp "test.glob" | sort >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 7: Multi-valued config via --file
###########################################################################

test_expect_success 'config --get-regexp with multi-valued key via --file' '
	(
	cd repo &&
	cat >multi.cfg <<-\EOF &&
	[remote "origin"]
		url = https://example.com/repo.git
		fetch = +refs/heads/*:refs/remotes/origin/*
		push = refs/heads/main
	EOF
	grit config --file multi.cfg --get-regexp "remote.origin" >actual &&
	grep "url" actual &&
	grep "fetch" actual
	)
'

test_expect_success 'config --file --get-regexp multi matches real git' '
	(
	cd repo &&
	cat >multi.cfg <<-\EOF &&
	[remote "origin"]
		url = https://example.com/repo.git
		fetch = +refs/heads/*:refs/remotes/origin/*
		push = refs/heads/main
	EOF
	grit config --file multi.cfg --get-regexp "remote.origin" | sort >actual &&
	"$REAL_GIT" config --file multi.cfg --get-regexp "remote.origin" | sort >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 8: Edge cases
###########################################################################

test_expect_success 'config --get-regexp on empty config file fails' '
	(
	cd repo &&
	>empty.cfg &&
	test_must_fail grit config --file empty.cfg --get-regexp "anything"
	)
'

test_expect_success 'config --get-regexp with subsection keys' '
	(
	cd repo &&
	grit config "branch.main.remote" "origin" &&
	grit config "branch.main.merge" "refs/heads/main" &&
	grit config --get-regexp "branch.main" >actual &&
	grep "branch.main.remote" actual &&
	grep "branch.main.merge" actual
	)
'

test_expect_success 'config --get-regexp subsection matches real git' '
	(
	cd repo &&
	grit config --get-regexp "branch.main" | sort >actual &&
	"$REAL_GIT" config --get-regexp "branch.main" | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --get-regexp value containing spaces' '
	(
	cd repo &&
	grit config test.spaced "hello world" &&
	grit config --get-regexp "test.spaced" >actual &&
	grep "hello world" actual
	)
'

test_expect_success 'config --get-regexp partial section match' '
	(
	cd repo &&
	grit config --get-regexp "col" >actual &&
	grep "color" actual
	)
'

###########################################################################
# Section 9: Interaction with set/unset
###########################################################################

test_expect_success 'config --get-regexp reflects newly set key' '
	(
	cd repo &&
	grit config newkey.added "yes" &&
	grit config --get-regexp "newkey.added" >actual &&
	grep "newkey.added yes" actual
	)
'

test_expect_success 'config --get-regexp does not show unset key' '
	(
	cd repo &&
	grit config ephemeral.key "temp" &&
	grit config --get-regexp "ephemeral" >before &&
	grep "ephemeral.key" before &&
	grit config --unset ephemeral.key &&
	test_must_fail grit config --get-regexp "ephemeral"
	)
'

test_expect_success 'config --get-regexp after overwrite shows new value' '
	(
	cd repo &&
	grit config overwrite.key "old" &&
	grit config overwrite.key "new" &&
	grit config --get-regexp "overwrite.key" >actual &&
	echo "overwrite.key new" >expect &&
	test_cmp expect actual
	)
'

test_done
