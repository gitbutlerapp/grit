#!/bin/sh
# Tests for git config with subsection handling (dotted keys, case sensitivity, etc.)

test_description='config subsection handling'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- setup ---------------------------------------------------------------------

test_expect_success 'setup: init repo for config tests' '
	(
	git init config-repo &&
	cd config-repo &&
	git config user.email "t@t.com" &&
	git config user.name "T"
	)
'

# -- basic subsection ---------------------------------------------------------

test_expect_success 'set and get config with subsection' '
	(
	cd config-repo &&
	git config branch.main.remote origin &&
	git config branch.main.remote >out &&
	echo origin >expect &&
	test_cmp expect out
	)
'

test_expect_success 'set and get config with dotted subsection' '
	(
	cd config-repo &&
	git config remote.origin.url https://example.com/repo.git &&
	git config remote.origin.url >out &&
	echo "https://example.com/repo.git" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'set multiple keys in same subsection' '
	(
	cd config-repo &&
	git config remote.origin.url https://example.com/repo.git &&
	git config remote.origin.fetch "+refs/heads/*:refs/remotes/origin/*" &&
	git config remote.origin.url >out-url &&
	git config remote.origin.fetch >out-fetch &&
	echo "https://example.com/repo.git" >expect-url &&
	echo "+refs/heads/*:refs/remotes/origin/*" >expect-fetch &&
	test_cmp expect-url out-url &&
	test_cmp expect-fetch out-fetch
	)
'

test_expect_success 'overwrite existing key in subsection' '
	(
	cd config-repo &&
	git config remote.origin.url https://old.com/repo.git &&
	git config remote.origin.url https://new.com/repo.git &&
	git config remote.origin.url >out &&
	echo "https://new.com/repo.git" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'get nonexistent key returns error' '
	(
	cd config-repo &&
	test_expect_code 1 git config remote.origin.nonexistent
	)
'

# -- case sensitivity ----------------------------------------------------------

test_expect_success 'section names are case-insensitive' '
	(
	cd config-repo &&
	git config Core.Bare false &&
	git config core.bare >out &&
	echo false >expect &&
	test_cmp expect out
	)
'

test_expect_success 'variable names are case-insensitive' '
	(
	cd config-repo &&
	git config core.BARE false &&
	git config core.bare >out &&
	echo false >expect &&
	test_cmp expect out
	)
'

test_expect_success 'subsection names are case-sensitive' '
	(
	cd config-repo &&
	git config branch.Main.remote upstream &&
	git config branch.main.remote origin &&
	git config branch.Main.remote >out-upper &&
	git config branch.main.remote >out-lower &&
	echo upstream >expect-upper &&
	echo origin >expect-lower &&
	test_cmp expect-upper out-upper &&
	test_cmp expect-lower out-lower
	)
'

# -- list/get-regexp -----------------------------------------------------------

test_expect_success 'config --list shows all entries' '
	(
	cd config-repo &&
	git config --list >out &&
	grep "remote.origin.url" out &&
	grep "user.email" out
	)
'

test_expect_success 'config --list filtered with grep matches subsection' '
	(
	cd config-repo &&
	git config remote.origin.url https://example.com/repo.git &&
	git config remote.origin.fetch "+refs/heads/*:refs/remotes/origin/*" &&
	git config --list >out &&
	grep "remote.origin.url" out &&
	grep "remote.origin.fetch" out
	)
'

test_expect_success 'config --list shows no nonexistent section entries' '
	(
	cd config-repo &&
	git config --list >out &&
	! grep "nonexistent\.pattern" out
	)
'

# -- unset ---------------------------------------------------------------------

test_expect_success 'config --unset removes a key' '
	(
	cd config-repo &&
	git config test.key value &&
	git config test.key >out &&
	echo value >expect &&
	test_cmp expect out &&
	git config --unset test.key &&
	test_expect_code 1 git config test.key
	)
'

test_expect_success 'config --unset on nonexistent key returns error' '
	(
	cd config-repo &&
	test_expect_code 5 git config --unset nonexistent.key
	)
'

# -- bool/int types ------------------------------------------------------------

test_expect_success 'config --bool returns true/false' '
	(
	cd config-repo &&
	git config core.autocrlf true &&
	git config --bool core.autocrlf >out &&
	echo true >expect &&
	test_cmp expect out
	)
'

test_expect_success 'config --bool normalizes yes to true' '
	(
	cd config-repo &&
	git config test.boolval yes &&
	git config --bool test.boolval >out &&
	echo true >expect &&
	test_cmp expect out
	)
'

test_expect_success 'config --int reads integer values' '
	(
	cd config-repo &&
	git config test.intval 42 &&
	git config --int test.intval >out &&
	echo 42 >expect &&
	test_cmp expect out
	)
'

test_expect_success 'config --int handles k suffix' '
	(
	cd config-repo &&
	git config test.size 8k &&
	git config --int test.size >out &&
	echo 8192 >expect &&
	test_cmp expect out
	)
'

test_expect_success 'config --int handles m suffix' '
	(
	cd config-repo &&
	git config test.size 2m &&
	git config --int test.size >out &&
	echo 2097152 >expect &&
	test_cmp expect out
	)
'

# -- multiple values -----------------------------------------------------------

test_expect_success 'config overwrites value when set twice' '
	(
	cd config-repo &&
	git config test.multi first &&
	git config test.multi second &&
	git config test.multi >out &&
	echo second >expect &&
	test_cmp expect out
	)
'

test_expect_success 'config --list shows overwritten value' '
	(
	cd config-repo &&
	git config test.multi >out &&
	echo second >expect &&
	test_cmp expect out
	)
'

# -- global vs local -----------------------------------------------------------

test_expect_success 'config --local only reads local config' '
	(
	cd config-repo &&
	git config --local user.name "Local User" &&
	git config --local user.name >out &&
	echo "Local User" >expect &&
	test_cmp expect out
	)
'

# -- special characters --------------------------------------------------------

test_expect_success 'config value with spaces' '
	(
	cd config-repo &&
	git config test.spaced "hello world" &&
	git config test.spaced >out &&
	echo "hello world" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'config value with equals sign' '
	(
	cd config-repo &&
	git config test.eqval "key=value" &&
	git config test.eqval >out &&
	echo "key=value" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'config subsection with dots in name' '
	(
	cd config-repo &&
	git config "url.https://example.com/.insteadOf" "git://example.com/" &&
	git config "url.https://example.com/.insteadOf" >out &&
	echo "git://example.com/" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'config value with special characters preserved' '
	(
	cd config-repo &&
	git config test.special "a#b;c" &&
	git config test.special >out &&
	echo "a#b;c" >expect &&
	test_cmp expect out
	)
'

# -- empty / missing section ---------------------------------------------------

test_expect_success 'config with empty value' '
	(
	cd config-repo &&
	git config test.empty "" &&
	git config test.empty >out &&
	echo "" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'config --list in fresh repo shows minimal config' '
	(
	git init fresh-config &&
	cd fresh-config &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	git config --list >out &&
	grep "user.email" out
	)
'

test_done
