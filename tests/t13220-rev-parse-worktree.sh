#!/bin/sh

test_description='grit rev-parse worktree and repository info options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo && cd repo &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo hello >file.txt && grit add file.txt && grit commit -m "initial" &&
	echo second >file2.txt && grit add file2.txt && grit commit -m "second" &&
	mkdir -p sub/deep
	)
'

test_expect_success 'rev-parse --show-toplevel from root' '
	(cd repo && grit rev-parse --show-toplevel >../actual) &&
	test -s actual
'

test_expect_success 'rev-parse --show-toplevel from subdirectory' '
	(cd repo/sub && grit rev-parse --show-toplevel >../../actual) &&
	(cd repo && grit rev-parse --show-toplevel >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --show-toplevel from deep subdirectory' '
	(cd repo/sub/deep && grit rev-parse --show-toplevel >../../../actual) &&
	(cd repo && grit rev-parse --show-toplevel >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --git-dir from root' '
	(cd repo && grit rev-parse --git-dir >../actual) &&
	echo ".git" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --git-dir from subdirectory is relative' '
	(cd repo/sub && grit rev-parse --git-dir >../../actual) &&
	(cd repo && pwd >../repo_path) &&
	printf "%s/.git\n" "$(cat repo_path)" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --git-dir from deep subdir' '
	(cd repo/sub/deep && grit rev-parse --git-dir >../../../actual) &&
	(cd repo && pwd >../repo_path) &&
	printf "%s/.git\n" "$(cat repo_path)" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-work-tree from root' '
	(cd repo && grit rev-parse --is-inside-work-tree >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-work-tree from subdirectory' '
	(cd repo/sub && grit rev-parse --is-inside-work-tree >../../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-work-tree from deep subdir' '
	(cd repo/sub/deep && grit rev-parse --is-inside-work-tree >../../../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-git-dir from root' '
	(cd repo && grit rev-parse --is-inside-git-dir >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-git-dir from subdirectory' '
	(cd repo/sub && grit rev-parse --is-inside-git-dir >../../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --is-bare-repository returns false' '
	(cd repo && grit rev-parse --is-bare-repository >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --show-prefix from root is empty' '
	(cd repo && grit rev-parse --show-prefix >../actual) &&
	echo "" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --show-prefix from sub' '
	(cd repo/sub && grit rev-parse --show-prefix >../../actual) &&
	echo "sub/" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --show-prefix from deep subdir' '
	(cd repo/sub/deep && grit rev-parse --show-prefix >../../../actual) &&
	echo "sub/deep/" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse HEAD returns full hash' '
	(cd repo && grit rev-parse HEAD >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'rev-parse --short HEAD returns short hash' '
	(cd repo && grit rev-parse --short HEAD >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 7
'

test_expect_success 'rev-parse --short HEAD is prefix of full' '
	(cd repo && grit rev-parse HEAD >../full) &&
	(cd repo && grit rev-parse --short HEAD >../short) &&
	short=$(cat short) &&
	full=$(cat full) &&
	case "$full" in
	"$short"*) true ;;
	*) false ;;
	esac
'

test_expect_success 'rev-parse main matches HEAD on main' '
	(cd repo && grit rev-parse HEAD >../head_hash) &&
	(cd repo && grit rev-parse main >../main_hash) &&
	test_cmp head_hash main_hash
'

test_expect_success 'rev-parse HEAD^ returns parent' '
	(cd repo && grit rev-parse "HEAD^" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40 &&
	(cd repo && grit rev-parse HEAD >../head_hash) &&
	head_hash=$(cat head_hash) &&
	test "$hash" != "$head_hash"
'

test_expect_success 'rev-parse HEAD~1 same as HEAD^' '
	(cd repo && grit rev-parse "HEAD^" >../caret) &&
	(cd repo && grit rev-parse "HEAD~1" >../tilde) &&
	test_cmp caret tilde
'

test_expect_success 'rev-parse HEAD^{tree} returns tree hash' '
	(cd repo && grit rev-parse "HEAD^{tree}" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'rev-parse HEAD^{tree} differs from HEAD' '
	(cd repo && grit rev-parse HEAD >../head_hash) &&
	(cd repo && grit rev-parse "HEAD^{tree}" >../tree_hash) &&
	! test_cmp head_hash tree_hash
'

test_expect_success 'rev-parse HEAD^{commit} same as HEAD' '
	(cd repo && grit rev-parse HEAD >../head_hash) &&
	(cd repo && grit rev-parse "HEAD^{commit}" >../commit_hash) &&
	test_cmp head_hash commit_hash
'

test_expect_success 'rev-parse --verify HEAD succeeds' '
	(cd repo && grit rev-parse --verify HEAD >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'rev-parse --verify with valid ref' '
	(cd repo && grit rev-parse --verify main >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'rev-parse --verify with invalid ref fails' '
	(cd repo && ! grit rev-parse --verify nonexistent 2>../err) &&
	test -s err
'

test_expect_success 'rev-parse HEAD from subdirectory same as root' '
	(cd repo && grit rev-parse HEAD >../head_root) &&
	(cd repo/sub && grit rev-parse HEAD >../../head_sub) &&
	test_cmp head_root head_sub
'

test_expect_success 'rev-parse --is-inside-work-tree consistent from anywhere' '
	(cd repo && grit rev-parse --is-inside-work-tree >../wt1) &&
	(cd repo/sub && grit rev-parse --is-inside-work-tree >../../wt2) &&
	(cd repo/sub/deep && grit rev-parse --is-inside-work-tree >../../../wt3) &&
	test_cmp wt1 wt2 &&
	test_cmp wt2 wt3
'

test_expect_success 'setup: create branch and switch' '
	(cd repo && git checkout -b mybranch &&
	 echo branch >branch.txt && grit add branch.txt && grit commit -m "branch commit")
'

test_expect_success 'rev-parse mybranch returns full hash' '
	(cd repo && grit rev-parse mybranch >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'rev-parse mybranch differs from main' '
	(cd repo && grit rev-parse mybranch >../mybranch_hash) &&
	(cd repo && grit rev-parse main >../main_hash) &&
	! test_cmp mybranch_hash main_hash
'

test_expect_success 'rev-parse HEAD matches mybranch on mybranch' '
	(cd repo && grit rev-parse HEAD >../head_hash) &&
	(cd repo && grit rev-parse mybranch >../mybranch_hash) &&
	test_cmp head_hash mybranch_hash
'

test_expect_success 'rev-parse --show-toplevel is absolute path' '
	(cd repo && grit rev-parse --show-toplevel >../actual) &&
	toplevel=$(cat actual) &&
	case "$toplevel" in
	/*) true ;;
	*) false ;;
	esac
'

test_expect_success 'rev-parse consistent across runs' '
	(cd repo && grit rev-parse HEAD >../run1) &&
	(cd repo && grit rev-parse HEAD >../run2) &&
	test_cmp run1 run2
'

test_done
