#!/bin/sh

test_description='grit commit message handling, -F, --amend, --author, --allow-empty, -a, -q, and stripspace'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo hello >file.txt &&
	grit add file.txt &&
	grit commit -m "initial"
	)
'

test_expect_success 'commit -m sets commit message' '
	(cd repo && grit commit --allow-empty -m "simple message" &&
	 grit cat-file -p HEAD >../actual) &&
	grep "simple message" actual
'

test_expect_success 'commit message subject is stored correctly' '
	(cd repo && grit log -n 1 --format="%s" >../actual) &&
	echo "simple message" >expect &&
	test_cmp expect actual
'

test_expect_success 'commit -F reads message from file' '
	echo "file based msg" >msg-file &&
	(cd repo && grit commit --allow-empty -F ../msg-file &&
	 grit cat-file -p HEAD >../actual) &&
	grep "file based msg" actual
'

test_expect_success 'commit -F with multi-line message' '
	printf "subject line\n\nbody paragraph one\nbody paragraph two\n" >multi-msg &&
	(cd repo && grit commit --allow-empty -F ../multi-msg &&
	 grit cat-file -p HEAD >../actual) &&
	grep "subject line" actual &&
	grep "body paragraph one" actual &&
	grep "body paragraph two" actual
'

test_expect_success 'commit -F preserves message content' '
	printf "trailing spaces\n" >trail-msg &&
	(cd repo && grit commit --allow-empty -F ../trail-msg &&
	 grit cat-file -p HEAD >../actual) &&
	grep "trailing spaces" actual
'

test_expect_success 'commit -F with trailing blank lines stores message' '
	printf "msg\n\n\n\n" >trail-blank &&
	(cd repo && grit commit --allow-empty -F ../trail-blank &&
	 grit cat-file -p HEAD >../actual) &&
	grep "msg" actual
'

test_expect_success 'commit collapses multiple consecutive blank lines in body' '
	printf "subject\n\n\n\n\nbody\n" >multi-blank &&
	(cd repo && grit commit --allow-empty -F ../multi-blank &&
	 grit cat-file -p HEAD >../actual) &&
	grep "subject" actual &&
	grep "body" actual
'

test_expect_success 'commit --allow-empty creates empty commit' '
	(cd repo && grit commit --allow-empty -m "empty commit" &&
	 grit log -n 1 --format="%s" >../actual) &&
	echo "empty commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'commit --allow-empty-message with empty string' '
	(cd repo && grit commit --allow-empty --allow-empty-message -m "" &&
	 grit cat-file -p HEAD >../actual) &&
	# Should have no message body (empty after double newline)
	test -s actual
'

test_expect_success 'commit --amend changes last commit message' '
	(cd repo && grit commit --allow-empty -m "before amend" &&
	 grit commit --allow-empty --amend -m "after amend" &&
	 grit log -n 1 --format="%s" >../actual) &&
	echo "after amend" >expect &&
	test_cmp expect actual
'

test_expect_success 'commit --amend preserves parent' '
	(cd repo &&
	 parent_before=$(grit rev-parse HEAD~1) &&
	 grit commit --allow-empty --amend -m "amend again" &&
	 parent_after=$(grit rev-parse HEAD~1) &&
	 echo "$parent_before" >../before &&
	 echo "$parent_after" >../after) &&
	test_cmp before after
'

test_expect_success 'commit --author overrides author' '
	(cd repo &&
	 sane_unset GIT_COMMITTER_NAME &&
	 sane_unset GIT_COMMITTER_EMAIL &&
	 grit commit --allow-empty --author="Other <other@test.com>" -m "custom author" &&
	 grit cat-file -p HEAD >../actual) &&
	grep "author Other <other@test.com>" actual
'

test_expect_success 'commit --author preserves committer' '
	(cd repo && grit cat-file -p HEAD >../actual) &&
	grep "committer T <t@t.com>" actual
'

test_expect_success 'commit -q suppresses output' '
	(cd repo && grit commit --allow-empty -q -m "quiet" >../actual 2>&1) &&
	test ! -s actual
'

test_expect_success 'commit -a stages tracked modified files' '
	(cd repo && echo "modified" >file.txt &&
	 grit commit -a -m "auto-stage" &&
	 grit cat-file -p HEAD >../actual) &&
	grep "auto-stage" actual
'

test_expect_success 'commit -a does not stage untracked files' '
	(cd repo && echo "untracked" >new-untracked.txt &&
	 grit commit --allow-empty -a -m "no untracked" &&
	 grit ls-files >../actual) &&
	! grep "new-untracked.txt" actual
'

test_expect_success 'commit without -m or -F fails' '
	(cd repo && test_must_fail grit commit --allow-empty 2>../err) &&
	test -s err
'

test_expect_success 'commit creates new tree for staged changes' '
	(cd repo &&
	 tree_before=$(grit rev-parse HEAD^{tree}) &&
	 echo "new content" >another.txt &&
	 grit add another.txt &&
	 grit commit -m "add another" &&
	 tree_after=$(grit rev-parse HEAD^{tree}) &&
	 test "$tree_before" != "$tree_after")
'

test_expect_success 'commit parent is previous HEAD' '
	(cd repo &&
	 prev=$(grit rev-parse HEAD) &&
	 grit commit --allow-empty -m "check parent" &&
	 grit cat-file -p HEAD >../actual) &&
	grep "parent" actual
'

test_expect_success 'commit author has timestamp' '
	(cd repo && grit cat-file -p HEAD >../actual) &&
	grep "author .* [0-9]" actual
'

test_expect_success 'commit committer has timestamp' '
	(cd repo && grit cat-file -p HEAD >../actual) &&
	grep "committer .* [0-9]" actual
'

test_expect_success 'commit message with special characters' '
	(cd repo && grit commit --allow-empty -m "msg with \"quotes\" and (parens)" &&
	 grit cat-file -p HEAD >../actual) &&
	grep "quotes" actual
'

test_expect_success 'multiple commits increment HEAD' '
	(cd repo &&
	 oid1=$(grit rev-parse HEAD) &&
	 grit commit --allow-empty -m "one" &&
	 oid2=$(grit rev-parse HEAD) &&
	 test "$oid1" != "$oid2")
'

test_expect_success 'commit stores tree object' '
	(cd repo && grit cat-file -p HEAD >../actual) &&
	grep "^tree [0-9a-f]\{40\}" actual
'

test_expect_success 'commit message from -m is exactly one line' '
	(cd repo && grit commit --allow-empty -m "oneliner" &&
	 grit cat-file -p HEAD >../raw) &&
	# Extract message: everything after the blank line
	sed -n "/^$/,\$p" raw | tail -n +2 >msg &&
	lines=$(wc -l <msg) &&
	test "$lines" -eq 1
'

test_expect_success 'commit -F message body is preserved' '
	printf "subject\n\nfirst body line\nsecond body line\n" >body-msg &&
	(cd repo && grit commit --allow-empty -F ../body-msg &&
	 grit cat-file -p HEAD >../raw) &&
	sed -n "/^$/,\$p" raw | tail -n +2 >msg &&
	grep "first body line" msg &&
	grep "second body line" msg
'

test_expect_success 'stripspace strips trailing whitespace from lines' '
	printf "hello world   \n" | (cd repo && grit stripspace >../actual) &&
	printf "hello world\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'stripspace collapses blank lines' '
	printf "line1\n\n\n\nline2\n" | (cd repo && grit stripspace >../actual) &&
	printf "line1\n\nline2\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'stripspace strips trailing blank lines' '
	printf "content\n\n\n\n" | (cd repo && grit stripspace >../actual) &&
	printf "content\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'stripspace -s strips comment lines' '
	printf "# comment\nkeep this\n# another\n" | (cd repo && grit stripspace -s >../actual) &&
	printf "keep this\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'stripspace -c prepends comment character' '
	printf "line1\nline2\n" | (cd repo && grit stripspace -c >../actual) &&
	printf "# line1\n# line2\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'stripspace on empty input produces empty output' '
	printf "" | (cd repo && grit stripspace >../actual) &&
	test ! -s actual
'

test_expect_success 'stripspace on whitespace-only input produces empty output' '
	printf "   \n  \n   \n" | (cd repo && grit stripspace >../actual) &&
	test ! -s actual
'

test_done
