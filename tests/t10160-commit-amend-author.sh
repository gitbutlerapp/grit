#!/bin/sh
# Test grit commit --amend, --author, --date, --allow-empty,
# --allow-empty-message, -a, -F, and combinations.

test_description='grit commit --amend and --author'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "first" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "initial commit"
	)
'

test_expect_success 'amend changes commit message' '
	(
	cd repo &&
	grit commit --amend -m "amended message" &&
	grit log --oneline >actual &&
	grep "amended message" actual
	)
'

test_expect_success 'amend preserves the file content' '
	(
	cd repo &&
	grit show HEAD:file.txt >actual &&
	echo "first" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'amend keeps single commit (no new parent)' '
	(
	cd repo &&
	grit log --oneline >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'amend with staged changes includes them' '
	(
	cd repo &&
	echo "second line" >>file.txt &&
	grit add file.txt &&
	grit commit --amend -m "amended with changes" &&
	grit show HEAD:file.txt >actual &&
	grep "second line" actual
	)
'

test_expect_success 'amend still single commit' '
	(
	cd repo &&
	grit log --oneline >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'commit --author overrides author in cat-file' '
	(
	cd repo &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "new" >new.txt &&
	grit add new.txt &&
	test_tick &&
	grit commit -m "custom author" --author "Alice Smith <alice@example.com>" &&
	grit cat-file -p HEAD >actual &&
	grep "^author Alice Smith <alice@example.com>" actual
	)
'

test_expect_success 'committer is still default user with --author' '
	(
	cd repo &&
	grit cat-file -p HEAD >actual &&
	grep "^committer Test User" actual
	)
'

test_expect_success 'amend --author changes author on existing commit' '
	(
	cd repo &&
	grit commit --amend --author "Bob Jones <bob@example.com>" -m "bob authored" &&
	grit cat-file -p HEAD >actual &&
	grep "^author Bob Jones" actual
	)
'

test_expect_success 'commit --date overrides author date' '
	(
	cd repo &&
	echo "dated" >dated.txt &&
	grit add dated.txt &&
	grit commit -m "dated commit" --date "1234567890 +0000" &&
	grit cat-file -p HEAD >actual &&
	grep "1234567890" actual
	)
'

test_expect_success 'commit --allow-empty creates empty commit' '
	(
	cd repo &&
	test_tick &&
	grit commit --allow-empty -m "empty commit" &&
	grit log --oneline >actual &&
	grep "empty commit" actual
	)
'

test_expect_success 'allow-empty commit has same tree as parent' '
	(
	cd repo &&
	tree_head=$(grit rev-parse HEAD^{tree}) &&
	tree_parent=$(grit rev-parse HEAD~1^{tree}) &&
	test "$tree_head" = "$tree_parent"
	)
'

test_expect_success 'commit --allow-empty-message with empty message' '
	(
	cd repo &&
	echo "content" >empty_msg.txt &&
	grit add empty_msg.txt &&
	test_tick &&
	grit commit --allow-empty-message -m ""
	)
'

test_expect_success 'commit -a stages and commits tracked file changes' '
	(
	cd repo &&
	echo "modified" >file.txt &&
	test_tick &&
	grit commit -a -m "auto staged" &&
	grit show HEAD:file.txt >actual &&
	echo "modified" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'commit -a does not add untracked files' '
	(
	cd repo &&
	echo "untracked" >untracked.txt &&
	test_tick &&
	grit commit -a --allow-empty -m "no untracked" &&
	! grit ls-files --cached | grep "untracked.txt"
	)
'

test_expect_success 'commit -F reads message from file' '
	(
	cd repo &&
	echo "message from file" >msg.txt &&
	echo "extra" >extra.txt &&
	grit add extra.txt &&
	test_tick &&
	grit commit -F msg.txt &&
	grit log --oneline -n 1 >actual &&
	grep "message from file" actual
	)
'

test_expect_success 'commit with multi-line message via -F' '
	(
	cd repo &&
	printf "line1\n\nline3\n" >multi_msg.txt &&
	echo "multiline" >ml.txt &&
	grit add ml.txt &&
	test_tick &&
	grit commit -F multi_msg.txt &&
	grit cat-file -p HEAD >actual &&
	grep "line1" actual &&
	grep "line3" actual
	)
'

test_expect_success 'amend preserves parent commit' '
	(
	cd repo &&
	parent=$(grit rev-parse HEAD~1) &&
	grit commit --amend -m "amend keeps parent" &&
	new_parent=$(grit rev-parse HEAD~1) &&
	test "$parent" = "$new_parent"
	)
'

test_expect_success 'amend changes commit OID' '
	(
	cd repo &&
	old_oid=$(grit rev-parse HEAD) &&
	grit commit --amend -m "new oid message" &&
	new_oid=$(grit rev-parse HEAD) &&
	test "$old_oid" != "$new_oid"
	)
'

test_expect_success 'multiple amends in succession' '
	(
	cd repo &&
	grit commit --amend -m "amend1" &&
	grit log --oneline -n 1 >a1 &&
	grep "amend1" a1 &&
	grit commit --amend -m "amend2" &&
	grit log --oneline -n 1 >a2 &&
	grep "amend2" a2 &&
	grit commit --amend -m "amend3" &&
	grit log --oneline -n 1 >a3 &&
	grep "amend3" a3
	)
'

test_expect_success 'amend with --author override' '
	(
	cd repo &&
	grit commit --amend \
		--author "Charlie <charlie@example.com>" \
		-m "charlie authored" &&
	grit cat-file -p HEAD >actual &&
	grep "^author Charlie" actual
	)
'

test_expect_success 'commit --quiet suppresses output' '
	(
	cd repo &&
	echo "quiet" >quiet.txt &&
	grit add quiet.txt &&
	test_tick &&
	grit commit --quiet -m "quiet commit" >out 2>&1 &&
	test_must_be_empty out
	)
'

test_expect_success 'amend on root commit (single commit repo)' '
	(
	grit init single &&
	cd single &&
	grit config user.email "s@s.com" &&
	grit config user.name "S" &&
	echo root >root.txt &&
	grit add root.txt &&
	test_tick &&
	grit commit -m "root" &&
	grit commit --amend -m "amended root" &&
	grit log --oneline >actual &&
	test_line_count = 1 actual &&
	grep "amended root" actual &&
	cd ..
	)
'

test_expect_success 'author format: Name <email> in cat-file' '
	(
	cd repo &&
	echo "fmt" >fmt.txt &&
	grit add fmt.txt &&
	test_tick &&
	grit commit -m "fmt test" --author "Dina Test <dina@test.org>" &&
	grit cat-file -p HEAD >actual &&
	grep "^author Dina Test <dina@test.org>" actual
	)
'

test_expect_success 'commit count is reasonable' '
	(
	cd repo &&
	grit log --oneline >actual &&
	count=$(wc -l <actual | tr -d " ") &&
	test "$count" -ge 5
	)
'

test_expect_success 'HEAD always moves forward on new commit' '
	(
	cd repo &&
	old=$(grit rev-parse HEAD) &&
	echo "forward" >fwd.txt &&
	grit add fwd.txt &&
	test_tick &&
	grit commit -m "move forward" &&
	new=$(grit rev-parse HEAD) &&
	test "$old" != "$new" &&
	parent=$(grit rev-parse HEAD~1) &&
	test "$parent" = "$old"
	)
'

test_expect_success 'amend with no changes preserves tree' '
	(
	cd repo &&
	tree_before=$(grit rev-parse HEAD^{tree}) &&
	grit commit --amend -m "move forward" &&
	tree_after=$(grit rev-parse HEAD^{tree}) &&
	test "$tree_before" = "$tree_after"
	)
'

test_expect_success 'commit --author with unicode name' '
	(
	cd repo &&
	echo "unicode" >uni.txt &&
	grit add uni.txt &&
	test_tick &&
	grit commit -m "unicode author" --author "Ünïcödé Nàme <uni@test.com>" &&
	grit cat-file -p HEAD >actual &&
	grep "Nàme" actual
	)
'

test_expect_success 'amend does not create merge commit' '
	(
	cd repo &&
	grit cat-file -p HEAD >actual &&
	parents=$(grep "^parent" actual | wc -l | tr -d " ") &&
	test "$parents" -le 1
	)
'

test_expect_success 'commit with very long message' '
	(
	cd repo &&
	long_msg=$(printf "a]%.0s" $(seq 1 500)) &&
	echo "longmsg" >long.txt &&
	grit add long.txt &&
	test_tick &&
	grit commit -m "$long_msg" &&
	grit cat-file -p HEAD >actual &&
	test $(wc -c <actual) -gt 500
	)
'

test_done
