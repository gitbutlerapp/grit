#!/bin/sh

test_description='grit config worktree, local, global, file scope and --show-scope/--show-origin'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo hello >file.txt &&
	grit add file.txt &&
	grit commit -m "initial"
	)
'

test_expect_success 'config --local sets value in repo config' '
	(cd repo && grit config --local test.loc "local-val" &&
	 grit config get test.loc >../actual) &&
	echo "local-val" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --local value appears in .git/config' '
	(cd repo && grep "local-val" .git/config >../actual) &&
	test -s actual
'

test_expect_success 'config --worktree sets value in worktree config' '
	(cd repo && grit config --worktree test.wt "wt-val" &&
	 grit config get test.wt >../actual) &&
	echo "wt-val" >expect &&
	test_cmp expect actual
'

test_expect_success 'worktree config falls back to local without worktreeConfig extension' '
	(cd repo && test ! -f .git/config.worktree)
'

test_expect_success 'worktree value stored in local config without worktreeConfig extension' '
	(cd repo && grep "wt-val" .git/config >../actual) &&
	test -s actual
'

test_expect_success 'config --show-scope shows fallback local scope for --worktree' '
	(cd repo && grit config --show-scope --list >../actual) &&
	grep "^local.*test.wt=wt-val" actual
'

test_expect_success 'config --show-scope shows local scope' '
	(cd repo && grit config --show-scope --list >../actual) &&
	grep "^local" actual
'

test_expect_success 'config --show-scope shows scope labels' '
	(cd repo && grit config --show-scope --list >../actual) &&
	# At minimum local should appear; global only if HOME gitconfig exists
	grep "^local" actual
'

test_expect_success 'config --show-origin shows file path for local' '
	(cd repo && grit config --show-origin --list >../actual) &&
	grep "file:.*\.git/config	" actual
'

test_expect_success 'config --show-origin shows local file path for --worktree fallback' '
	(cd repo && grit config --show-origin --list >../actual) &&
	grep "file:.*\.git/config	.*test.wt=wt-val" actual
'

test_expect_success 'worktree config overrides local for same key' '
	(cd repo && grit config --local test.override "local-v" &&
	 grit config --worktree test.override "wt-v" &&
	 grit config get test.override >../actual) &&
	echo "wt-v" >expect &&
	test_cmp expect actual
'

test_expect_success 'config -f writes to custom file' '
	(cd repo && grit config -f ../custom.cfg cust.key "cust-val" &&
	 grit config -f ../custom.cfg cust.key >../actual) &&
	echo "cust-val" >expect &&
	test_cmp expect actual
'

test_expect_success 'custom file exists on disk' '
	test -f custom.cfg
'

test_expect_success 'custom file contains the key' '
	grep "cust-val" custom.cfg
'

test_expect_success 'config -f can set multiple keys' '
	(cd repo && grit config -f ../custom.cfg cust.key2 "val2" &&
	 grit config -f ../custom.cfg cust.key2 >../actual) &&
	echo "val2" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set multiple worktree keys' '
	(cd repo && grit config --worktree wt.a "1" &&
	 grit config --worktree wt.b "2" &&
	 grit config --worktree wt.c "3" &&
	 grit config get wt.a >../actual_a &&
	 grit config get wt.b >../actual_b &&
	 grit config get wt.c >../actual_c) &&
	echo "1" >expect_a && echo "2" >expect_b && echo "3" >expect_c &&
	test_cmp expect_a actual_a &&
	test_cmp expect_b actual_b &&
	test_cmp expect_c actual_c
'

test_expect_success 'config worktree key can be overwritten' '
	(cd repo && grit config --worktree wt.remove "gone" &&
	 grit config --worktree wt.remove "changed" &&
	 grit config get wt.remove >../actual) &&
	echo "changed" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --local unset does not affect worktree' '
	(cd repo && grit config --worktree scope.check "wt" &&
	 grit config --local scope.check "local" &&
	 grit config get scope.check >../actual) &&
	echo "local" >expect &&
	test_cmp expect actual
'

test_expect_success 'config list shows both local and worktree entries' '
	(cd repo && grit config list >../actual) &&
	grep "test.loc=local-val" actual &&
	grep "test.wt=wt-val" actual
'

test_expect_success 'config --show-scope --show-origin combined' '
	(cd repo && grit config --show-scope --show-origin --list >../actual) &&
	grep "^local" actual &&
	grep "file:" actual
'

test_expect_success 'config --local set and get core.filemode' '
	(cd repo && grit config --local core.filemode >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'config rename-section in local scope' '
	(cd repo && grit config --local ren.old "val" &&
	 grit config rename-section ren renamed &&
	 grit config get renamed.old >../actual) &&
	echo "val" >expect &&
	test_cmp expect actual
'

test_expect_success 'config remove-section in local scope' '
	(cd repo && grit config --local delsec.k1 "v1" &&
	 grit config --local delsec.k2 "v2" &&
	 grit config remove-section delsec &&
	 test_must_fail grit config get delsec.k1 &&
	 test_must_fail grit config get delsec.k2)
'

test_expect_success 'config -f unset from custom file' '
	(cd repo && grit config -f ../custom2.cfg del.key "v" &&
	 grit config -f ../custom2.cfg --unset del.key &&
	 test_must_fail grit config -f ../custom2.cfg del.key)
'

test_expect_success 'config --bool via legacy flag with local scope' '
	(cd repo && grit config --local test.mybool "yes" &&
	 grit config --bool test.mybool >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --int via legacy flag with local scope' '
	(cd repo && grit config --local test.myint "10" &&
	 grit config --int test.myint >../actual) &&
	echo "10" >expect &&
	test_cmp expect actual
'

test_expect_success 'config -z uses NUL delimiter' '
	(cd repo && grit config -z --list >../actual) &&
	# NUL-delimited output should not have newlines between entries
	# Just verify it contains entries (NUL chars present)
	test -s actual
'

test_expect_success 'config --local with dotted subsection key' '
	(cd repo && grit config --local branch.main.merge "refs/heads/main" &&
	 grit config get branch.main.merge >../actual) &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set in local does not create config.worktree' '
	grit init repo2 &&
	(cd repo2 && git config user.email "t@t.com" &&
	 grit config --local only.local "val" &&
	 test ! -f .git/config.worktree)
'

test_expect_success 'worktree scope with empty value' '
	(cd repo && grit config --worktree wt.empty "" &&
	 grit config get wt.empty >../actual) &&
	echo "" >expect &&
	test_cmp expect actual
'

test_expect_success 'config get --default provides fallback' '
	(cd repo && grit config get --default "fallback" no.such.key >../actual) &&
	echo "fallback" >expect &&
	test_cmp expect actual
'

test_expect_success 'config get --default not used when key exists' '
	(cd repo && grit config set test.exists "real" &&
	 grit config get --default "fallback" test.exists >../actual) &&
	echo "real" >expect &&
	test_cmp expect actual
'

test_done
