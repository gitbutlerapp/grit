#!/bin/sh
# Tests for config --get-regexp: pattern matching, multi-value, section filtering.

test_description='config --get-regexp pattern matching'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup -----------------------------------------------------------------

test_expect_success 'setup: repo with various config entries' '
	(
	$REAL_GIT init repo &&
	cd repo &&
	$REAL_GIT config user.email "alice@example.com" &&
	$REAL_GIT config user.name "Alice" &&
	$REAL_GIT config core.autocrlf false &&
	$REAL_GIT config core.ignorecase true &&
	$REAL_GIT config color.ui auto &&
	$REAL_GIT config color.diff always &&
	$REAL_GIT config color.status auto &&
	$REAL_GIT config merge.tool vimdiff &&
	$REAL_GIT config merge.conflictstyle diff3 &&
	$REAL_GIT config alias.co checkout &&
	$REAL_GIT config alias.br branch &&
	$REAL_GIT config alias.ci commit &&
	$REAL_GIT config alias.st status &&
	$REAL_GIT config remote.origin.url "https://example.com/repo.git" &&
	$REAL_GIT config remote.origin.fetch "+refs/heads/*:refs/remotes/origin/*" &&
	$REAL_GIT config remote.upstream.url "https://example.com/upstream.git" &&
	$REAL_GIT config remote.upstream.fetch "+refs/heads/*:refs/remotes/upstream/*" &&
	echo "base" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "initial"
	)
'

# -- basic regexp matching ---------------------------------------------------

test_expect_success 'config --get-regexp matches user section' '
	(
	cd repo &&
	grit config --get-regexp "user" >actual &&
	grep "user.email alice@example.com" actual &&
	grep "user.name Alice" actual
	)
'

test_expect_success 'config --get-regexp matches core section' '
	(
	cd repo &&
	grit config --get-regexp "core" >actual &&
	grep "core.autocrlf" actual &&
	grep "core.ignorecase" actual
	)
'

test_expect_success 'config --get-regexp matches color section' '
	(
	cd repo &&
	grit config --get-regexp "color" >actual &&
	grep "color.ui auto" actual &&
	grep "color.diff always" actual &&
	grep "color.status auto" actual
	)
'

test_expect_success 'config --get-regexp matches alias section' '
	(
	cd repo &&
	grit config --get-regexp "alias" >actual &&
	grep "alias.co checkout" actual &&
	grep "alias.br branch" actual &&
	grep "alias.ci commit" actual &&
	grep "alias.st status" actual
	)
'

test_expect_success 'config --get-regexp matches url keys' '
	(
	cd repo &&
	grit config --get-regexp "url" >actual &&
	grep "remote.origin.url" actual &&
	grep "remote.upstream.url" actual
	)
'

test_expect_success 'config --get-regexp matches remote.origin keys' '
	(
	cd repo &&
	grit config --get-regexp "remote.origin" >actual &&
	grep "remote.origin.url" actual &&
	grep "remote.origin.fetch" actual
	)
'

test_expect_success 'config --get-regexp matches remote.upstream keys' '
	(
	cd repo &&
	grit config --get-regexp "remote.upstream" >actual &&
	grep "remote.upstream.url" actual &&
	grep "remote.upstream.fetch" actual
	)
'

test_expect_success 'config --get-regexp returns only matching keys' '
	(
	cd repo &&
	grit config --get-regexp "merge" >actual &&
	grep "merge.tool vimdiff" actual &&
	grep "merge.conflictstyle diff3" actual &&
	! grep "color" actual &&
	! grep "alias" actual
	)
'

test_expect_success 'config --get-regexp with no matches exits non-zero' '
	(
	cd repo &&
	test_must_fail grit config --get-regexp "nonexistentxyz123"
	)
'

# -- output format -----------------------------------------------------------

test_expect_success 'config --get-regexp output has key-space-value format' '
	(
	cd repo &&
	grit config --get-regexp "user.email" >actual &&
	grep "user.email alice@example.com" actual
	)
'

test_expect_success 'config --get-regexp for broad pattern lists many entries' '
	(
	cd repo &&
	grit config --get-regexp "." >actual &&
	test_line_count -ge 10 actual
	)
'

test_expect_success 'config --get-regexp single key match returns correct value' '
	(
	cd repo &&
	grit config --get-regexp "merge.tool" >actual &&
	grep "merge.tool vimdiff" actual
	)
'

# -- multi-value config -------------------------------------------------------

test_expect_success 'setup: add multi-value config' '
	(
	cd repo &&
	$REAL_GIT config --add remote.origin.push "+refs/heads/main:refs/heads/main" &&
	$REAL_GIT config --add remote.origin.push "+refs/heads/develop:refs/heads/develop"
	)
'

test_expect_success 'config --get-regexp shows multi-value entries' '
	(
	cd repo &&
	grit config --get-regexp "remote.origin.push" >actual &&
	test_line_count -ge 2 actual
	)
'

# -- fetch refspec ----------------------------------------------------------

test_expect_success 'config --get-regexp matches fetch refspec keys' '
	(
	cd repo &&
	grit config --get-regexp "fetch" >actual &&
	grep "remote.origin.fetch" actual &&
	grep "remote.upstream.fetch" actual
	)
'

# -- partial key matches ------------------------------------------------------

test_expect_success 'config --get-regexp with partial key autocrlf' '
	(
	cd repo &&
	grit config --get-regexp "autocrlf" >actual &&
	grep "core.autocrlf false" actual
	)
'

test_expect_success 'config --get-regexp with partial key ignorecase' '
	(
	cd repo &&
	grit config --get-regexp "ignorecase" >actual &&
	grep "core.ignorecase true" actual
	)
'

test_expect_success 'config --get-regexp with partial key conflictstyle' '
	(
	cd repo &&
	grit config --get-regexp "conflictstyle" >actual &&
	grep "merge.conflictstyle diff3" actual
	)
'

# -- subcommand form: config get --regexp ------------------------------------

test_expect_success 'config get --regexp --show-names matches user section' '
	(
	cd repo &&
	grit config get --regexp --show-names "user" >actual &&
	grep "user.email alice@example.com" actual &&
	grep "user.name Alice" actual
	)
'

test_expect_success 'config get --regexp without --show-names gives values only' '
	(
	cd repo &&
	grit config get --regexp "alias" >actual &&
	grep "checkout" actual &&
	grep "branch" actual &&
	grep "commit" actual &&
	grep "status" actual
	)
'

test_expect_success 'config get --regexp --show-names matches color section' '
	(
	cd repo &&
	grit config get --regexp --show-names "color" >actual &&
	grep "color.ui auto" actual &&
	grep "color.diff always" actual
	)
'

# -- compare with real git ---------------------------------------------------

test_expect_success 'config --get-regexp matches real git for user section' '
	(
	cd repo &&
	grit config --get-regexp "user" | sort >actual &&
	$REAL_GIT config --get-regexp "user" | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --get-regexp matches real git for alias section' '
	(
	cd repo &&
	grit config --get-regexp "alias" | sort >actual &&
	$REAL_GIT config --get-regexp "alias" | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --get-regexp matches real git for merge section' '
	(
	cd repo &&
	grit config --get-regexp "merge" | sort >actual &&
	$REAL_GIT config --get-regexp "merge" | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --get-regexp matches real git for color section' '
	(
	cd repo &&
	grit config --get-regexp "color" | sort >actual &&
	$REAL_GIT config --get-regexp "color" | sort >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --get-regexp matches real git for remote.origin' '
	(
	cd repo &&
	grit config --get-regexp "remote.origin" | sort >actual &&
	$REAL_GIT config --get-regexp "remote.origin" | sort >expect &&
	test_cmp expect actual
	)
'

test_done
