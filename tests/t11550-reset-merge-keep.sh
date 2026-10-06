#!/bin/sh
# Tests for grit reset: --soft, --mixed (default), --hard, path reset,
# quiet mode, and interactions with various repository states.

test_description='grit reset: soft, mixed, hard, path reset, quiet'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: init repo with multiple commits' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "v1" >file.txt &&
	echo "other" >other.txt &&
	grit add . &&
	grit commit -m "first" &&
	grit rev-parse HEAD >../first_oid &&
	echo "v2" >file.txt &&
	grit add file.txt &&
	grit commit -m "second" &&
	grit rev-parse HEAD >../second_oid &&
	echo "v3" >file.txt &&
	grit add file.txt &&
	grit commit -m "third" &&
	grit rev-parse HEAD >../third_oid
	)
'

# ---- soft reset ----
test_expect_success 'reset --soft moves HEAD but keeps index and worktree' '
	(
	cd repo &&
	second=$(cat ../second_oid) &&
	grit reset --soft "$second" &&
	test "$(grit rev-parse HEAD)" = "$second" &&
	test "$(cat file.txt)" = "v3"
	)
'

test_expect_success 'after soft reset, changes are staged' '
	(
	cd repo &&
	grit diff --cached --name-only >staged &&
	grep "file.txt" staged
	)
'

test_expect_success 'soft reset back to third' '
	(
	cd repo &&
	third=$(cat ../third_oid) &&
	grit reset --soft "$third" &&
	test "$(grit rev-parse HEAD)" = "$third"
	)
'

# ---- mixed reset (default) ----
test_expect_success 'reset --mixed moves HEAD and resets index but keeps worktree' '
	(
	cd repo &&
	second=$(cat ../second_oid) &&
	grit reset --mixed "$second" &&
	test "$(grit rev-parse HEAD)" = "$second" &&
	test "$(cat file.txt)" = "v3"
	)
'

test_expect_success 'after mixed reset, changes are unstaged' '
	(
	cd repo &&
	grit diff --cached --name-only >staged &&
	! grep "file.txt" staged &&
	grit diff --name-only >unstaged &&
	grep "file.txt" unstaged
	)
'

test_expect_success 'default reset (no flag) is same as --mixed' '
	(
	cd repo &&
	grit add file.txt &&
	grit commit -m "restore third" &&
	second=$(cat ../second_oid) &&
	grit reset "$second" &&
	test "$(grit rev-parse HEAD)" = "$second" &&
	grit diff --cached --name-only >staged &&
	! grep "file.txt" staged
	)
'

# ---- hard reset ----
test_expect_success 'reset --hard moves HEAD, resets index and worktree' '
	(
	cd repo &&
	first=$(cat ../first_oid) &&
	grit reset --hard "$first" &&
	test "$(grit rev-parse HEAD)" = "$first" &&
	test "$(cat file.txt)" = "v1"
	)
'

test_expect_success 'after hard reset, working tree matches commit' '
	(
	cd repo &&
	grit diff --name-only >unstaged &&
	test ! -s unstaged &&
	grit diff --cached --name-only >staged &&
	test ! -s staged
	)
'

test_expect_success 'hard reset discards uncommitted changes' '
	(
	cd repo &&
	echo "dirty" >file.txt &&
	grit add file.txt &&
	echo "dirtier" >file.txt &&
	first=$(cat ../first_oid) &&
	grit reset --hard "$first" &&
	test "$(cat file.txt)" = "v1"
	)
'

# ---- reset to HEAD ----
test_expect_success 'reset HEAD unstages all changes' '
	(
	cd repo &&
	echo "staged" >file.txt &&
	grit add file.txt &&
	grit reset HEAD &&
	grit diff --cached --name-only >staged &&
	! grep "file.txt" staged &&
	test "$(cat file.txt)" = "staged"
	)
'

test_expect_success 'reset --hard HEAD discards all changes' '
	(
	cd repo &&
	echo "dirty" >file.txt &&
	grit add file.txt &&
	echo "dirtier" >file.txt &&
	grit reset --hard HEAD &&
	test "$(cat file.txt)" = "v1"
	)
'

# ---- path reset ----
test_expect_success 'reset -- path unstages specific file' '
	(
	cd repo &&
	echo "mod1" >file.txt &&
	echo "mod2" >other.txt &&
	grit add file.txt other.txt &&
	grit reset -- file.txt &&
	grit diff --cached --name-only >staged &&
	! grep "file.txt" staged &&
	grep "other.txt" staged
	)
'

test_expect_success 'path reset keeps worktree intact' '
	(
	cd repo &&
	test "$(cat file.txt)" = "mod1"
	)
'

test_expect_success 'reset multiple paths at once' '
	(
	cd repo &&
	grit add file.txt &&
	grit reset -- file.txt other.txt &&
	grit diff --cached --name-only >staged &&
	test ! -s staged
	)
'

# ---- quiet mode ----
test_expect_success 'reset --quiet suppresses output' '
	(
	cd repo &&
	echo "q" >file.txt &&
	grit add file.txt &&
	grit reset --quiet HEAD >output 2>&1 &&
	test ! -s output
	)
'

test_expect_success 'reset -q is synonym for --quiet' '
	(
	cd repo &&
	grit add file.txt &&
	grit reset -q HEAD >output 2>&1 &&
	test ! -s output
	)
'

# ---- soft reset preserves staged new file ----
test_expect_success 'soft reset preserves newly staged file' '
	(
	cd repo &&
	grit reset --hard HEAD &&
	echo "newfile" >newfile.txt &&
	grit add newfile.txt &&
	grit commit -m "with newfile" &&
	grit rev-parse HEAD >../with_new_oid &&
	first=$(cat ../first_oid) &&
	grit reset --soft "$first" &&
	grit ls-files --error-unmatch newfile.txt &&
	grit diff --cached --name-only >staged &&
	grep "newfile.txt" staged
	)
'

# ---- hard reset removes new files from index ----
test_expect_success 'hard reset removes tracked-but-not-in-target files from index' '
	(
	cd repo &&
	first=$(cat ../first_oid) &&
	grit reset --hard "$first" &&
	! grit ls-files --error-unmatch newfile.txt 2>/dev/null
	)
'

# ---- reset to create divergent history ----
test_expect_success 'reset allows creating divergent commits' '
	(
	cd repo &&
	first=$(cat ../first_oid) &&
	grit reset --hard "$first" &&
	echo "diverge" >diverge.txt &&
	grit add diverge.txt &&
	grit commit -m "divergent" &&
	grit log --oneline >log &&
	grep "divergent" log &&
	grep "first" log
	)
'

# ---- mixed reset then re-add and commit ----
test_expect_success 'mixed reset then re-add produces clean diff' '
	(
	cd repo &&
	echo "extra" >extra.txt &&
	grit add extra.txt &&
	grit reset HEAD -- extra.txt &&
	grit diff --cached --name-only >staged &&
	! grep "extra.txt" staged &&
	grit add extra.txt &&
	grit diff --cached --name-only >staged2 &&
	grep "extra.txt" staged2
	)
'

# ---- reset with directory path ----
test_expect_success 'reset -- path unstages directory files individually' '
	(
	cd repo &&
	mkdir -p dir &&
	echo "a" >dir/a.txt &&
	echo "b" >dir/b.txt &&
	grit add dir/a.txt dir/b.txt &&
	grit reset -- dir/a.txt dir/b.txt &&
	grit diff --cached --name-only >staged &&
	! grep "dir/a.txt" staged &&
	! grep "dir/b.txt" staged
	)
'

# ---- hard reset restores deleted file ----
test_expect_success 'hard reset restores file deleted from worktree' '
	(
	cd repo &&
	rm -f diverge.txt &&
	! test -f diverge.txt &&
	grit reset --hard HEAD &&
	test -f diverge.txt
	)
'

# ---- soft reset then amend-like workflow ----
test_expect_success 'soft reset enables amend-like workflow' '
	(
	cd repo &&
	echo "pre-amend" >amend.txt &&
	grit add amend.txt &&
	grit commit -m "to amend" &&
	grit rev-parse HEAD >../pre_amend_oid &&
	pre=$(cat ../pre_amend_oid) &&
	parent_count=$(grit rev-list HEAD | wc -l) &&
	grit reset --soft "$pre"^ 2>/dev/null || grit reset --soft "$(grit log --oneline | sed -n 2p | cut -d\" \" -f1)" &&
	echo "amended" >amend.txt &&
	grit add amend.txt &&
	grit commit -m "amended commit" &&
	test "$(cat amend.txt)" = "amended" &&
	grit log --oneline | grep "amended commit"
	)
'

# ---- reset on root commit ----
test_expect_success 'setup fresh repo for root reset tests' '
	(
	grit init fresh &&
	cd fresh &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "root" >root.txt &&
	grit add root.txt &&
	grit commit -m "root commit" &&
	grit rev-parse HEAD >../root_oid
	)
'

test_expect_success 'mixed reset HEAD on single commit keeps file' '
	(
	cd fresh &&
	echo "mod" >root.txt &&
	grit add root.txt &&
	grit reset HEAD &&
	grit diff --cached --name-only >staged &&
	test ! -s staged &&
	test "$(cat root.txt)" = "mod" &&
	cd ..
	)
'

# ---- reset preserves untracked files ----
test_expect_success 'hard reset does not remove untracked files' '
	(
	cd repo &&
	echo "untracked" >untracked.txt &&
	grit reset --hard HEAD &&
	test -f untracked.txt
	)
'

# ---- reset --soft to same commit is no-op ----
test_expect_success 'soft reset to HEAD is a no-op' '
	(
	cd repo &&
	head_before=$(grit rev-parse HEAD) &&
	grit reset --soft HEAD &&
	head_after=$(grit rev-parse HEAD) &&
	test "$head_before" = "$head_after"
	)
'

# ---- reset --hard to same commit cleans state ----
test_expect_success 'reset --mixed preserves executable bit in worktree' '
	(
	cd repo &&
	echo "exec" >exec.sh &&
	chmod +x exec.sh &&
	grit add exec.sh &&
	grit commit -m "exec" &&
	echo "modified" >exec.sh &&
	grit add exec.sh &&
	grit reset HEAD &&
	test -x exec.sh
	)
'

test_expect_success 'hard reset to HEAD cleans staged and worktree changes' '
	(
	cd repo &&
	echo "dirty" >diverge.txt &&
	grit add diverge.txt &&
	echo "dirtier" >diverge.txt &&
	grit reset --hard HEAD &&
	grit diff --name-only >unstaged &&
	test ! -s unstaged &&
	grit diff --cached --name-only >staged &&
	test ! -s staged
	)
'

test_done
