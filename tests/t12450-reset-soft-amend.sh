#!/bin/sh
test_description='grit reset: --soft, --mixed, --hard, --quiet, pathspec reset'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup - create repo with three commits' '
	grit init repo &&
	(cd repo &&
	 git config user.email "t@t.com" &&
	 git config user.name "T" &&
	 echo a >file1.txt &&
	 echo b >file2.txt &&
	 echo c >file3.txt &&
	 mkdir dir &&
	 echo d >dir/nested.txt &&
	 grit add . &&
	 grit commit -m "first" &&
	 grit rev-parse HEAD >../first_hash &&
	 echo a2 >file1.txt &&
	 echo b2 >file2.txt &&
	 grit add . &&
	 grit commit -m "second" &&
	 grit rev-parse HEAD >../second_hash &&
	 echo a3 >file1.txt &&
	 grit add . &&
	 grit commit -m "third" &&
	 grit rev-parse HEAD >../third_hash)
'

# === SOFT RESET TESTS ===

test_expect_success 'reset --soft moves HEAD but keeps index and worktree' '
	(cd repo &&
	 grit reset --soft "$(cat ../second_hash)" &&
	 grit rev-parse HEAD >../head_actual) &&
	test_cmp second_hash head_actual
'

test_expect_success 'after soft reset, worktree still has third content' '
	(cd repo && cat file1.txt >../actual) &&
	echo "a3" >expect &&
	test_cmp expect actual
'

test_expect_success 'after soft reset, changes are staged' '
	(cd repo &&
	 grit status --porcelain | grep -v "^##" >../actual) &&
	grep "^M  file1.txt" actual
'

test_expect_success 'restore to third commit via hard reset' '
	(cd repo && grit reset --hard "$(cat ../third_hash)")
'

# === MIXED RESET TESTS ===

test_expect_success 'reset --mixed resets index but keeps worktree' '
	(cd repo &&
	 grit reset --mixed "$(cat ../second_hash)" &&
	 cat file1.txt >../wt_actual &&
	 grit status --porcelain | grep -v "^##" >../status_actual) &&
	echo "a3" >wt_expect &&
	test_cmp wt_expect wt_actual &&
	grep " M file1.txt" status_actual
'

test_expect_success 'restore to third after mixed' '
	(cd repo &&
	 grit add . &&
	 grit commit -m "re-third" &&
	 grit rev-parse HEAD >../third_hash)
'

test_expect_success 'reset with no mode defaults to --mixed' '
	(cd repo &&
	 grit reset "$(cat ../second_hash)" &&
	 cat file1.txt >../wt_actual &&
	 grit status --porcelain | grep -v "^##" >../status_actual) &&
	echo "a3" >wt_expect &&
	test_cmp wt_expect wt_actual &&
	grep " M file1.txt" status_actual
'

test_expect_success 'restore to third after default reset' '
	(cd repo &&
	 grit add . &&
	 grit commit -m "re-third-2" &&
	 grit rev-parse HEAD >../third_hash)
'

# === HARD RESET TESTS ===

test_expect_success 'reset --hard resets HEAD, index, and worktree' '
	(cd repo &&
	 grit reset --hard "$(cat ../second_hash)" &&
	 cat file1.txt >../wt_actual &&
	 grit status --porcelain | grep -v "^##" | grep "file1" >../status_actual || true) &&
	echo "a2" >wt_expect &&
	test_cmp wt_expect wt_actual &&
	test_must_be_empty status_actual
'

test_expect_success 'restore to third after hard' '
	(cd repo &&
	 echo a3 >file1.txt &&
	 grit add . &&
	 grit commit -m "re-third-3" &&
	 grit rev-parse HEAD >../third_hash)
'

test_expect_success 'reset --hard to first commit' '
	(cd repo &&
	 grit reset --hard "$(cat ../first_hash)" &&
	 cat file1.txt >../actual) &&
	echo "a" >expect &&
	test_cmp expect actual
'

test_expect_success 'restore to third after hard to first' '
	(cd repo &&
	 echo a3 >file1.txt &&
	 echo b2 >file2.txt &&
	 grit add . &&
	 grit commit -m "re-third-4" &&
	 grit rev-parse HEAD >../third_hash)
'

# === QUIET FLAG ===

test_expect_success 'reset --quiet suppresses output' '
	(cd repo &&
	 grit reset --quiet --hard "$(cat ../second_hash)" >../actual 2>&1) &&
	test_must_be_empty actual
'

test_expect_success 'restore to third after quiet' '
	(cd repo &&
	 echo a3 >file1.txt &&
	 grit add . &&
	 grit commit -m "re-third-5" &&
	 grit rev-parse HEAD >../third_hash)
'

# === UNSTAGE WITH RESET HEAD ===

test_expect_success 'reset HEAD unstages all changes' '
	(cd repo &&
	 echo modified >file1.txt &&
	 echo modified2 >file2.txt &&
	 grit add file1.txt file2.txt &&
	 grit reset HEAD &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	cat >expect <<-\EOF &&
	 M file1.txt
	 M file2.txt
	EOF
	sort expect >expect_sorted &&
	test_cmp expect_sorted actual
'

test_expect_success 'reset hard after unstage test' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'reset single file unstages it' '
	(cd repo &&
	 echo changed >file1.txt &&
	 grit add file1.txt &&
	 grit reset HEAD file1.txt &&
	 grit status --porcelain | grep -v "^##" | grep "file1" >../actual) &&
	echo " M file1.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset hard after single unstage' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'reset multiple files unstages them' '
	(cd repo &&
	 echo x >file1.txt &&
	 echo y >file2.txt &&
	 grit add file1.txt file2.txt &&
	 grit reset HEAD file1.txt file2.txt &&
	 grit status --porcelain | grep -v "^##" | sort >../actual) &&
	cat >expect <<-\EOF &&
	 M file1.txt
	 M file2.txt
	EOF
	sort expect >expect_sorted &&
	test_cmp expect_sorted actual
'

test_expect_success 'reset hard after multi unstage' '
	(cd repo && grit reset --hard HEAD)
'

test_expect_success 'reset unstages newly added file' '
	(cd repo &&
	 echo new >newfile.txt &&
	 grit add newfile.txt &&
	 grit reset HEAD newfile.txt &&
	 grit status --porcelain | grep -v "^##" | grep "newfile" >../actual) &&
	grep "??" actual
'

test_expect_success 'cleanup newfile' '
	(cd repo && rm -f newfile.txt)
'

test_expect_success 'reset file in subdirectory' '
	(cd repo &&
	 echo changed >dir/nested.txt &&
	 grit add dir/nested.txt &&
	 grit reset HEAD dir/nested.txt &&
	 grit status --porcelain | grep -v "^##" | grep "nested" >../actual) &&
	echo " M dir/nested.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset hard after subdir test' '
	(cd repo && grit reset --hard HEAD)
'

# === -C FLAG ===

test_expect_success 'reset with -C flag' '
	(echo modified >repo/file1.txt &&
	 grit -C repo add file1.txt &&
	 grit -C repo reset HEAD file1.txt &&
	 cd repo && grit status --porcelain | grep -v "^##" | grep "file1" >../actual) &&
	echo " M file1.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset hard after -C test' '
	(cd repo && grit reset --hard HEAD)
'

# === HARD RESET IDEMPOTENT ===

test_expect_success 'reset --hard HEAD on clean tree is idempotent' '
	(cd repo &&
	 grit reset --hard HEAD &&
	 grit status --porcelain | grep -v "^##" | grep -v "^??" >../actual || true) &&
	test_must_be_empty actual
'

# === SOFT RESET HEAD IS NO-OP ===

test_expect_success 'reset --soft HEAD is a no-op' '
	(cd repo &&
	 head_before=$(grit rev-parse HEAD) &&
	 grit reset --soft HEAD &&
	 head_after=$(grit rev-parse HEAD) &&
	 echo "$head_before" >../before &&
	 echo "$head_after" >../after) &&
	test_cmp before after
'

# === HARD RESET REMOVES STAGED NEW FILE FROM INDEX ===

test_expect_success 'reset --hard removes staged new file from index' '
	(cd repo &&
	 echo brand >brandnew.txt &&
	 grit add brandnew.txt &&
	 grit reset --hard HEAD &&
	 grit ls-files >../actual) &&
	! grep "brandnew" actual
'

test_expect_success 'cleanup brandnew' '
	(cd repo && rm -f brandnew.txt)
'

# === SOFT RESET ACCUMULATES ===

test_expect_success 'sequential soft resets accumulate changes' '
	(cd repo &&
	 grit reset --soft "$(cat ../second_hash)" &&
	 grit status --porcelain | grep -v "^##" | grep "^M" >../actual) &&
	grep "file1.txt" actual
'

test_expect_success 'final hard reset to clean state' '
	(cd repo &&
	 grit reset --hard HEAD &&
	 grit status --porcelain | grep -v "^##" | grep -v "^??" >../actual || true) &&
	test_must_be_empty actual
'

test_done
