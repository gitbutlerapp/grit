#!/bin/sh

test_description='grit log display: format placeholders, oneline, decorate, graph'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo && cd repo &&
	git config user.email "alice@example.com" && git config user.name "Alice" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo first >file.txt && grit add file.txt && grit commit -m "initial commit" &&
	echo second >file2.txt && grit add file2.txt && grit commit -m "second commit" &&
	parent=$(grit rev-parse HEAD) &&
	echo third >file3.txt && grit add file3.txt && grit commit -m "third commit" &&
	git branch topic "$parent" &&
	git checkout topic &&
	echo topic1 >topic.txt && grit add topic.txt && grit commit -m "topic one" &&
	echo topic2 >topic2.txt && grit add topic2.txt && grit commit -m "topic two" &&
	git checkout main
	)
'

test_expect_success 'log --oneline shows abbreviated hash and subject' '
	(cd repo && grit log -n1 --oneline >../actual) &&
	(cd repo && grit log -n1 --format="%h" >../hash) &&
	hash=$(cat hash) &&
	echo "$hash (HEAD -> main) third commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --oneline --no-decorate omits refs' '
	(cd repo && grit log -n1 --oneline --no-decorate >../actual) &&
	(cd repo && grit log -n1 --format="%h" >../hash) &&
	hash=$(cat hash) &&
	echo "$hash third commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --format=%H shows full hash' '
	(cd repo && grit log -n1 --format="%H" >../actual) &&
	(cd repo && grit rev-parse HEAD >../expect) &&
	test_cmp expect actual
'

test_expect_success 'log --format=%h shows abbreviated hash' '
	(cd repo && grit log -n1 --format="%h" >../actual) &&
	(cd repo && grit rev-parse --short HEAD >../expect) &&
	test_cmp expect actual
'

test_expect_success 'log --format=%T shows tree hash' '
	(cd repo && grit log -n1 --format="%T" >../actual) &&
	(cd repo && git cat-file -p HEAD >../commit_raw) &&
	sed -n "s/^tree //p" commit_raw >expect &&
	test_cmp expect actual
'

test_expect_success 'log --format=%t shows abbreviated tree hash' '
	(cd repo && grit log -n1 --format="%t" >../actual) &&
	(cd repo && grit log -n1 --format="%T" >../full_tree) &&
	cut -c1-7 full_tree >expect &&
	test_cmp expect actual
'

test_expect_success 'log --format=%P shows parent hash' '
	(cd repo && grit log -n1 --format="%P" >../actual) &&
	(cd repo && grit rev-parse HEAD~1 >../expect) &&
	test_cmp expect actual
'

test_expect_success 'log --format=%p shows abbreviated parent hash' '
	(cd repo && grit log -n1 --format="%p" >../actual) &&
	(cd repo && grit rev-parse --short HEAD~1 >../expect) &&
	test_cmp expect actual
'

test_expect_success 'log --format=%an shows author name' '
	(cd repo && grit log -n1 --format="%an" >../actual) &&
	echo "Alice" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --format=%ae shows author email' '
	(cd repo && grit log -n1 --format="%ae" >../actual) &&
	echo "alice@example.com" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --format=%cn shows committer name' '
	(cd repo && grit log -n1 --format="%cn" >../actual) &&
	echo "Alice" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --format=%ce shows committer email' '
	(cd repo && grit log -n1 --format="%ce" >../actual) &&
	echo "alice@example.com" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --format=%s shows subject line' '
	(cd repo && grit log -n1 --format="%s" >../actual) &&
	echo "third commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --format=%b shows empty line for no-body commit' '
	(cd repo && grit log -n1 --format="%b" >../actual) &&
	echo >expect &&
	test_cmp expect actual
'

test_expect_success 'log -n limits output count' '
	(cd repo && grit log -n2 --format="%s" >../actual) &&
	cat >expect <<-\EOF &&
	third commit
	second commit
	EOF
	test_cmp expect actual
'

test_expect_success 'log --max-count limits output count' '
	(cd repo && grit log --max-count=1 --format="%s" >../actual) &&
	echo "third commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --skip skips commits' '
	(cd repo && grit log --skip=1 -n1 --format="%s" >../actual) &&
	echo "second commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --skip=2 skips two commits' '
	(cd repo && grit log --skip=2 -n1 --format="%s" >../actual) &&
	echo "initial commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --reverse shows oldest first' '
	(cd repo && grit log --reverse --format="%s" >../actual) &&
	head -1 actual >first_line &&
	echo "initial commit" >expect &&
	test_cmp expect first_line
'

test_expect_success 'log --reverse combined with -n2' '
	(cd repo && grit log --reverse -n2 --format="%s" >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'log --first-parent on linear history shows all' '
	(cd repo && grit log --first-parent --format="%s" >../actual) &&
	(cd repo && grit log --format="%s" >../expect) &&
	test_cmp expect actual
'

test_expect_success 'log multiple format placeholders on one line' '
	(cd repo && grit log -n1 --format="%H %an %s" >../actual) &&
	(cd repo && grit rev-parse HEAD >../hash) &&
	hash=$(cat hash) &&
	echo "$hash Alice third commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'log format with separator characters' '
	(cd repo && grit log -n1 --format="%an|%ae|%s" >../actual) &&
	echo "Alice|alice@example.com|third commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --graph produces output' '
	(cd repo && grit log --graph --oneline >../actual) &&
	test -s actual
'

test_expect_success 'log on specific branch' '
	(cd repo && grit log topic --format="%s" -n1 >../actual) &&
	echo "topic two" >expect &&
	test_cmp expect actual
'

test_expect_success 'log with format %n outputs newline' '
	(cd repo && grit log -n1 --format="%s%n%an" >../actual) &&
	cat >expect <<-\EOF &&
	third commit
	Alice
	EOF
	test_cmp expect actual
'

test_expect_success 'log --oneline lists correct count' '
	(cd repo && grit log --oneline --no-decorate >../actual) &&
	test_line_count = 3 actual
'

test_expect_success 'log topic branch --oneline lists correct count' '
	(cd repo && grit log topic --oneline --no-decorate >../actual) &&
	test_line_count = 4 actual
'

test_expect_success 'log --skip beyond history produces empty output' '
	(cd repo && grit log --skip=100 --format="%s" >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'log -n1 limits to single commit' '
	(cd repo && grit log -n1 --format="%s" >../actual) &&
	test_line_count = 1 actual
'

test_expect_success 'log format with literal percent using %%' '
	(cd repo && grit log -n1 --format="100%%" >../actual) &&
	echo "100%" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --reverse first line is root commit' '
	(cd repo && grit log --reverse --format="%s" >../actual) &&
	head -1 actual >first_line &&
	echo "initial commit" >expect &&
	test_cmp expect first_line
'

test_expect_success 'log --format=%H count matches rev-list count' '
	(cd repo && grit log --format="%H" >../log_hashes) &&
	(cd repo && grit rev-list HEAD >../revlist_hashes) &&
	test_line_count = 3 log_hashes &&
	test_line_count = 3 revlist_hashes
'

test_done
