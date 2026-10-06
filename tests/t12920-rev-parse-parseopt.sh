#!/bin/sh

test_description='grit rev-parse: query modes, --verify, --short, --git-dir, --show-toplevel'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo && cd repo &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo hello >file.txt && grit add file.txt && grit commit -m "initial" &&
	echo second >file2.txt && grit add file2.txt && grit commit -m "second" &&
	echo third >file3.txt && grit add file3.txt && grit commit -m "third" &&
	mkdir -p sub/deep
	)
'

test_expect_success 'rev-parse HEAD returns 40-char hash' '
	(cd repo && grit rev-parse HEAD >../actual) &&
	grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'rev-parse HEAD matches git rev-parse HEAD' '
	(cd repo && grit rev-parse HEAD >../grit_out) &&
	(cd repo && git rev-parse HEAD >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'rev-parse main returns same as HEAD' '
	(cd repo && grit rev-parse main >../main_hash) &&
	(cd repo && grit rev-parse HEAD >../head_hash) &&
	test_cmp head_hash main_hash
'

test_expect_success 'rev-parse HEAD~1 returns parent' '
	(cd repo && grit rev-parse HEAD~1 >../actual) &&
	grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'rev-parse HEAD~1 differs from HEAD' '
	(cd repo && grit rev-parse HEAD >../head) &&
	(cd repo && grit rev-parse HEAD~1 >../parent) &&
	! test_cmp head parent
'

test_expect_success 'rev-parse HEAD^ same as HEAD~1' '
	(cd repo && grit rev-parse "HEAD^" >../caret) &&
	(cd repo && grit rev-parse HEAD~1 >../tilde) &&
	test_cmp tilde caret
'

test_expect_success 'rev-parse HEAD~2 returns grandparent' '
	(cd repo && grit rev-parse HEAD~2 >../actual) &&
	grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'rev-parse HEAD~2 is parent of HEAD~1' '
	(cd repo && grit rev-parse HEAD~2 >../grandparent) &&
	(cd repo && grit rev-parse HEAD~1 >../parent) &&
	! test_cmp parent grandparent
'

test_expect_success 'rev-parse HEAD^{commit} resolves to same hash' '
	(cd repo && grit rev-parse "HEAD^{commit}" >../actual) &&
	(cd repo && grit rev-parse HEAD >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --verify HEAD succeeds' '
	(cd repo && grit rev-parse --verify HEAD >../actual) &&
	grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'rev-parse --verify HEAD matches rev-parse HEAD' '
	(cd repo && grit rev-parse --verify HEAD >../verified) &&
	(cd repo && grit rev-parse HEAD >../plain) &&
	test_cmp plain verified
'

test_expect_success 'rev-parse --verify nonexistent fails' '
	(cd repo && ! grit rev-parse --verify nonexistent 2>/dev/null)
'

test_expect_success 'rev-parse --short HEAD returns abbreviated hash' '
	(cd repo && grit rev-parse --short HEAD >../actual) &&
	grep "^[0-9a-f]\{7\}$" actual
'

test_expect_success 'rev-parse --short HEAD is prefix of full hash' '
	(cd repo && grit rev-parse --short HEAD >../short) &&
	(cd repo && grit rev-parse HEAD >../full) &&
	short=$(cat short) &&
	grep "^$short" full
'

test_expect_success 'rev-parse --git-dir from repo root' '
	(cd repo && grit rev-parse --git-dir >../actual) &&
	echo ".git" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --show-toplevel from repo root' '
	(cd repo && grit rev-parse --show-toplevel >../actual) &&
	(cd repo && pwd >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --show-prefix from repo root is empty or newline' '
	(cd repo && grit rev-parse --show-prefix >../actual) &&
	echo >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --show-prefix from subdirectory' '
	(cd repo/sub && grit rev-parse --show-prefix >../../actual) &&
	echo "sub/" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --show-prefix from deep subdirectory' '
	(cd repo/sub/deep && grit rev-parse --show-prefix >../../../actual) &&
	echo "sub/deep/" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --show-toplevel from subdirectory' '
	(cd repo/sub && grit rev-parse --show-toplevel >../../actual) &&
	(cd repo && pwd >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --is-bare-repository returns false' '
	(cd repo && grit rev-parse --is-bare-repository >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-work-tree returns true' '
	(cd repo && grit rev-parse --is-inside-work-tree >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-git-dir returns false from worktree' '
	(cd repo && grit rev-parse --is-inside-git-dir >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-work-tree from subdirectory' '
	(cd repo/sub && grit rev-parse --is-inside-work-tree >../../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'setup bare repository' '
	grit init --bare bare.git
'

test_expect_success 'rev-parse --is-bare-repository in bare repo' '
	(cd bare.git && grit rev-parse --is-bare-repository >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-work-tree in bare repo returns false' '
	(cd bare.git && grit rev-parse --is-inside-work-tree >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-parse multiple refs resolves each' '
	(cd repo && grit rev-parse HEAD HEAD~1 >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'rev-parse multiple refs are different' '
	(cd repo && grit rev-parse HEAD HEAD~1 >../actual) &&
	head -1 actual >first &&
	tail -1 actual >second &&
	! test_cmp first second
'

test_expect_success 'rev-parse tag resolves to tagged commit' '
	(cd repo && git tag v1.0 HEAD~1) &&
	(cd repo && grit rev-parse v1.0 >../actual) &&
	(cd repo && grit rev-parse HEAD~1 >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-parse --short returns unique prefix' '
	(cd repo && grit rev-parse --short HEAD >../short_hash) &&
	short=$(cat short_hash) &&
	len=${#short} &&
	test "$len" -ge 4 &&
	test "$len" -le 40
'

test_expect_success 'rev-parse --git-dir from subdirectory' '
	(cd repo/sub && grit rev-parse --git-dir >../../actual) &&
	test -s actual
'

test_done
