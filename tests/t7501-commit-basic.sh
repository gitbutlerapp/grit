#!/bin/sh
# Ported from git/t/t7501-commit-basic-functionality.sh
# Tests for 'grit commit'.

test_description='grit commit basic functionality'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@test.com"
	)
'

test_expect_success 'initial commit' '
	(
	cd repo &&
	echo "hello" >file.txt &&
	git add file.txt &&
	git commit -m "initial commit" >stderr 2>&1 &&
	grep "root-commit" stderr &&
	git cat-file -t HEAD >type &&
	echo "commit" >expected &&
	test_cmp expected type
	)
'

test_expect_success 'commit message is stored correctly' '
	(
	cd repo &&
	git cat-file -p HEAD >actual &&
	grep "initial commit" actual
	)
'

test_expect_success 'second commit has parent' '
	(
	cd repo &&
	echo "world" >>file.txt &&
	git add file.txt &&
	git commit -m "second commit" >stderr 2>&1 &&
	! grep "root-commit" stderr &&
	git cat-file -p HEAD >actual &&
	grep "^parent " actual
	)
'

test_expect_success 'commit -m with multiple messages' '
	(
	cd repo &&
	echo "more" >>file.txt &&
	git add file.txt &&
	git commit -m "first paragraph" -m "second paragraph" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "first paragraph" actual &&
	grep "second paragraph" actual
	)
'

test_expect_success 'commit -a stages tracked files' '
	(
	cd repo &&
	echo "auto-staged" >>file.txt &&
	git commit -a -m "auto staged commit" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "auto staged commit" actual
	)
'

test_expect_success 'commit -F reads message from file' '
	(
	cd repo &&
	echo "new content" >>file.txt &&
	git add file.txt &&
	echo "message from file" >msg.txt &&
	git commit -F msg.txt 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "message from file" actual
	)
'

test_expect_success 'commit without changes fails (no --allow-empty)' '
	(
	cd repo &&
	! git commit -m "empty" 2>/dev/null
	)
'

test_expect_success 'commit --allow-empty succeeds' '
	(
	cd repo &&
	git commit --allow-empty -m "empty commit" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "empty commit" actual
	)
'

test_expect_success 'commit --quiet suppresses output' '
	(
	cd repo &&
	echo "quiet" >>file.txt &&
	git add file.txt &&
	git commit -q -m "quiet commit" 2>stderr &&
	test ! -s stderr
	)
'

test_expect_success 'commit respects GIT_AUTHOR_NAME and GIT_AUTHOR_EMAIL' '
	(
	cd repo &&
	echo "env author" >>file.txt &&
	git add file.txt &&
	GIT_AUTHOR_NAME="Custom Author" GIT_AUTHOR_EMAIL="custom@test.com" \
		git commit -m "custom author" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "Custom Author <custom@test.com>" actual
	)
'

test_expect_success 'commit --author overrides identity' '
	(
	cd repo &&
	echo "override" >>file.txt &&
	git add file.txt &&
	git commit --author="Override Author <override@test.com>" -m "override author" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "Override Author <override@test.com>" actual
	)
'

# ---- New tests ported from upstream ----

test_expect_success '-m and -F both accepted by grit' '
	(
	cd repo &&
	echo "mf-test" >>file.txt &&
	git add file.txt &&
	git commit -m "from -m flag" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "from -m flag" actual
	)
'

test_expect_success 'nothing to commit fails' '
	(
	cd repo &&
	git reset --hard HEAD 2>/dev/null &&
	! git commit -m "nothing" 2>/dev/null
	)
'

test_expect_success 'multiple -m creates separate paragraphs' '
	(
	cd repo &&
	echo "multi" >>file.txt &&
	git add file.txt &&
	git commit -m "one" -m "two" -m "three" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "one" actual &&
	grep "two" actual &&
	grep "three" actual
	)
'

test_expect_success 'commit -F - reads from stdin' '
	(
	cd repo &&
	echo "stdin content" >>file.txt &&
	git add file.txt &&
	echo "message from stdin" | git commit -F - 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "message from stdin" actual
	)
'

test_expect_success 'amend commit' '
	(
	cd repo &&
	echo "amend me" >>file.txt &&
	git add file.txt &&
	git commit -m "before amend" 2>/dev/null &&
	git commit --amend -m "after amend" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "after amend" actual &&
	! grep "before amend" actual
	)
'

test_expect_success 'amend preserves parent' '
	(
	cd repo &&
	PARENT_BEFORE=$(git cat-file -p HEAD | sed -n "s/^parent //p" | head -1) &&
	git commit --amend -m "amend again" 2>/dev/null &&
	PARENT_AFTER=$(git cat-file -p HEAD | sed -n "s/^parent //p" | head -1) &&
	test "$PARENT_BEFORE" = "$PARENT_AFTER"
	)
'

test_expect_success 'amend root commit has no parent' '
	(
	git init amend-root-repo &&
	cd amend-root-repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "root" >root.txt &&
	git add root.txt &&
	git commit -m "root" 2>/dev/null &&
	git commit --amend -m "amended root" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	! grep "^parent " actual
	)
'

test_expect_success 'amend --author changes author' '
	(
	cd repo &&
	echo "auth" >>file.txt &&
	git add file.txt &&
	git commit -m "original author" 2>/dev/null &&
	git commit --amend --author="New Author <new@test.com>" -m "new author" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "New Author <new@test.com>" actual
	)
'

test_expect_success 'commit --date sets author date' '
	(
	cd repo &&
	echo "date" >>file.txt &&
	git add file.txt &&
	git commit --date="1234567890 +0000" -m "with date" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "^author.*1234567890 +0000" actual
	)
'

test_expect_success 'commit respects GIT_AUTHOR_DATE' '
	(
	cd repo &&
	echo "envdate" >>file.txt &&
	git add file.txt &&
	GIT_AUTHOR_DATE="1000000000 +0000" git commit -m "env date" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "^author.*1000000000 +0000" actual
	)
'

test_expect_success 'commit --date overrides GIT_AUTHOR_DATE' '
	(
	cd repo &&
	echo "dateoverride" >>file.txt &&
	git add file.txt &&
	GIT_AUTHOR_DATE="1000000000 +0000" \
		git commit --date="2000000000 +0000" -m "date override" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "^author.*2000000000 +0000" actual
	)
'

test_expect_success 'commit with empty message fails' '
	(
	cd repo &&
	echo "emptymsg" >>file.txt &&
	git add file.txt &&
	! git commit -m "" 2>/dev/null
	)
'

test_expect_success 'commit --allow-empty-message with empty -m' '
	(
	cd repo &&
	git commit --allow-empty-message -m "" 2>/dev/null &&
	git cat-file -t HEAD >actual &&
	echo "commit" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'commit tree is a tree object' '
	(
	cd repo &&
	echo "treecheck" >>file.txt &&
	git add file.txt &&
	git commit -m "tree check" 2>/dev/null &&
	git cat-file -p HEAD >commit_out &&
	TREE=$(head -1 commit_out | sed -n "s/^tree //p") &&
	git cat-file -t "$TREE" >actual &&
	echo "tree" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'commit creates proper chain of parents' '
	(
	cd repo &&
	CHILD=$(git rev-parse HEAD) &&
	PARENT=$(git cat-file -p HEAD | sed -n "s/^parent //p" | head -1) &&
	test -n "$PARENT" &&
	git cat-file -t "$PARENT" >actual &&
	echo "commit" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'commit -a does not commit untracked files' '
	(
	cd repo &&
	echo "untracked-content" >untracked-test.txt &&
	echo "tracked-change" >>file.txt &&
	git commit -a -m "auto stage tracked only" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "auto stage tracked only" actual &&
	git status -s >status_out &&
	grep "^?? untracked-test.txt" status_out
	)
'

test_expect_success 'initial commit output mentions root-commit' '
	(
	git init fresh-repo &&
	cd fresh-repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "x" >x.txt &&
	git add x.txt &&
	git commit -m "first" >stderr 2>&1 &&
	grep "root-commit" stderr
	)
'

test_expect_success 'second commit output does not mention root-commit' '
	(
	cd fresh-repo &&
	echo "y" >>x.txt &&
	git add x.txt &&
	git commit -m "second" >stderr 2>&1 &&
	! grep "root-commit" stderr
	)
'

test_expect_success 'commit output shows branch name' '
	(
	cd fresh-repo &&
	echo "z" >>x.txt &&
	git add x.txt &&
	git commit -m "third" >stderr 2>&1 &&
	grep "main" stderr
	)
'

test_expect_success 'allow-empty with no staged changes succeeds' '
	(
	cd repo &&
	git commit --allow-empty -m "truly empty" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "truly empty" actual
	)
'

test_expect_success 'same tree with --allow-empty succeeds' '
	(
	cd repo &&
	TREE_BEFORE=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	git commit --allow-empty -m "same tree" 2>/dev/null &&
	TREE_AFTER=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	test "$TREE_BEFORE" = "$TREE_AFTER"
	)
'

test_expect_success 'committer is set from config' '
	(
	cd repo &&
	echo "committer" >>file.txt &&
	git add file.txt &&
	(
		unset GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL &&
		git commit -m "check committer" 2>/dev/null
	) &&
	git cat-file -p HEAD >actual &&
	grep "^committer Test User <test@test.com>" actual
	)
'

test_expect_success 'GIT_COMMITTER_NAME overrides config' '
	(
	cd repo &&
	echo "committer-env" >>file.txt &&
	git add file.txt &&
	GIT_COMMITTER_NAME="Env Committer" GIT_COMMITTER_EMAIL="env@test.com" \
		git commit -m "env committer" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "^committer Env Committer <env@test.com>" actual
	)
'

# ---- Wave 5: more tests ported from upstream t7501 ----

test_expect_success 'commit message from file (absolute path)' '
	(
	cd repo &&
	echo "abs-path" >>file.txt &&
	git add file.txt &&
	echo "absolute path msg" >"$TRASH_DIRECTORY/abs-msg.txt" &&
	git commit -F "$TRASH_DIRECTORY/abs-msg.txt" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "absolute path msg" actual
	)
'

test_expect_success 'commit message from stdin via -F -' '
	(
	cd repo &&
	echo "stdin-content" >>file.txt &&
	git add file.txt &&
	echo "stdin message" | git commit -F - 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "stdin message" actual
	)
'

test_expect_success 'multiple -m creates blank-line-separated paragraphs' '
	(
	cd repo &&
	echo "multi-m" >>file.txt &&
	git add file.txt &&
	git commit -m "one" -m "two" -m "three" 2>/dev/null &&
	git cat-file commit HEAD >commit &&
	sed -e "1,/^$/d" commit >actual &&
	{
		echo one &&
		echo &&
		echo two &&
		echo &&
		echo three
	} >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'amend commit to fix author' '
	(
	cd repo &&
	echo "amend-auth" >>file.txt &&
	git add file.txt &&
	git commit -m "orig" 2>/dev/null &&
	git commit --amend --author="The Real Author <someguy@his.email.org>" -m "amended" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "The Real Author <someguy@his.email.org>" actual
	)
'

test_expect_success 'amend commit to fix date' '
	(
	cd repo &&
	echo "amend-date" >>file.txt &&
	git add file.txt &&
	git commit -m "orig date" 2>/dev/null &&
	git commit --amend --date="1300000000 +0000" -m "new date" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "^author.*1300000000 +0000" actual
	)
'

test_expect_success 'same tree (single parent) fails without --allow-empty' '
	(
	cd repo &&
	git reset --hard HEAD 2>/dev/null &&
	test_must_fail git commit -m empty 2>/dev/null
	)
'

test_expect_success 'same tree (single parent) --allow-empty works' '
	(
	cd repo &&
	git commit --allow-empty -m "forced empty" 2>/dev/null &&
	git cat-file commit HEAD >commit &&
	grep "forced empty" commit
	)
'

test_expect_success 'commit -a with removed file' '
	(
	cd repo &&
	echo "to-remove" >removeme.txt &&
	git add removeme.txt &&
	git commit -m "add removeme" 2>/dev/null &&
	rm removeme.txt &&
	git commit -a -m "remove file" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "remove file" actual &&
	test_must_fail git cat-file -e HEAD:removeme.txt 2>/dev/null
	)
'

test_expect_success 'commit -a with modified and removed files' '
	(
	cd repo &&
	echo "keep" >keep.txt &&
	echo "gone" >gone.txt &&
	git add keep.txt gone.txt &&
	git commit -m "two files" 2>/dev/null &&
	echo "changed" >>keep.txt &&
	rm gone.txt &&
	git commit -a -m "modify and remove" 2>/dev/null &&
	git diff-tree --name-status HEAD^ HEAD >actual &&
	grep "^M.*keep.txt" actual &&
	grep "^D.*gone.txt" actual
	)
'

test_expect_success 'commit with GIT_COMMITTER_DATE override' '
	(
	cd repo &&
	echo "cdate" >>file.txt &&
	git add file.txt &&
	GIT_COMMITTER_DATE="1400000000 +0000" git commit -m "committer date" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "^committer.*1400000000 +0000" actual
	)
'

test_expect_success 'commit --allow-empty-message succeeds with -m ""' '
	(
	cd repo &&
	echo "aem" >>file.txt &&
	git add file.txt &&
	git commit --allow-empty-message -m "" 2>/dev/null &&
	git cat-file -t HEAD >actual &&
	echo commit >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'amend --allow-empty-message with empty message' '
	(
	cd repo &&
	echo "aem2" >>file.txt &&
	git add file.txt &&
	git commit -m "will be emptied" 2>/dev/null &&
	git commit --amend --allow-empty-message -m "" 2>/dev/null &&
	git cat-file commit HEAD >commit &&
	sed -e "1,/^$/d" commit >body &&
	! grep -q "[^ ]" body
	)
'

test_expect_success 'empty -m without --allow-empty-message fails' '
	(
	cd repo &&
	echo "noempty" >>file.txt &&
	git add file.txt &&
	test_must_fail git commit -m "" 2>/dev/null
	)
'

test_expect_success 'commit on detached HEAD' '
	(
	cd repo &&
	git checkout HEAD^0 2>/dev/null &&
	echo "detached" >>file.txt &&
	git add file.txt &&
	git commit -m "detached commit" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "detached commit" actual &&
	git checkout main 2>/dev/null
	)
'

test_expect_success 'commit with only whitespace message fails' '
	(
	cd repo &&
	echo "ws" >>file.txt &&
	git add file.txt &&
	test_must_fail git commit -m "   " 2>/dev/null
	)
'

test_expect_success 'amend changes commit hash' '
	(
	cd repo &&
	echo "hash1" >>file.txt &&
	git add file.txt &&
	git commit -m "before" 2>/dev/null &&
	OLD=$(git rev-parse HEAD) &&
	git commit --amend -m "after" 2>/dev/null &&
	NEW=$(git rev-parse HEAD) &&
	test "$OLD" != "$NEW"
	)
'

test_expect_success 'commit -a does not stage new untracked files' '
	(
	cd repo &&
	echo "not-tracked" >not-tracked.txt &&
	echo "track-change" >>file.txt &&
	git commit -a -m "only tracked" 2>/dev/null &&
	git ls-files >indexed &&
	! grep "not-tracked.txt" indexed
	)
'

test_expect_success 'amend preserves tree when only message changes' '
	(
	cd repo &&
	echo "tree-same" >>file.txt &&
	git add file.txt &&
	git commit -m "original msg" 2>/dev/null &&
	TREE_BEFORE=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	git commit --amend -m "new msg" 2>/dev/null &&
	TREE_AFTER=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	test "$TREE_BEFORE" = "$TREE_AFTER"
	)
'

test_expect_success 'consecutive --allow-empty commits all have same tree' '
	(
	cd repo &&
	TREE1=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	git commit --allow-empty -m "empty 1" 2>/dev/null &&
	TREE2=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	git commit --allow-empty -m "empty 2" 2>/dev/null &&
	TREE3=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	test "$TREE1" = "$TREE2" &&
	test "$TREE2" = "$TREE3"
	)
'

test_expect_success 'commit with --author has correct author' '
	(
	cd repo &&
	echo "sa" >>file.txt &&
	git add file.txt &&
	git commit --author="Other Dev <other@dev.com>" -m "author flag" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "author Other Dev <other@dev.com>" actual
	)
'

test_expect_success 'commit with --date has correct date' '
	(
	cd repo &&
	echo "sa2" >>file.txt &&
	git add file.txt &&
	git commit --date="1500000000 +0000" -m "date flag" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "^author.*1500000000 +0000" actual
	)
'

test_expect_success 'commit -F with multiline file' '
	(
	cd repo &&
	echo "mlf" >>file.txt &&
	git add file.txt &&
	printf "line one\n\nline three\n" >multi-msg.txt &&
	git commit -F multi-msg.txt 2>/dev/null &&
	git cat-file commit HEAD >commit &&
	sed -e "1,/^$/d" commit >actual &&
	printf "line one\n\nline three\n" >expected &&
	test_cmp expected actual
	)
'

# ---- Wave 8: more tests ported from upstream t7501 ----

test_expect_success 'commit message from non-existing file fails' '
	(
	cd repo &&
	echo "nofile" >>file.txt &&
	git add file.txt &&
	test_must_fail git commit -F /nonexistent/path 2>/dev/null
	)
'

test_expect_success 'empty commit message (whitespace only) fails' '
	(
	cd repo &&
	printf "   \t  \n \t\n" >ws-msg.txt &&
	test_must_fail git commit -F ws-msg.txt 2>/dev/null
	)
'

test_expect_success 'commit -F from empty file fails without --allow-empty-message' '
	(
	cd repo &&
	>empty-file &&
	test_must_fail git commit -F empty-file 2>/dev/null
	)
'

test_expect_success 'amend to set message to empty needs --allow-empty-message' '
	(
	cd repo &&
	echo "asetmsg" >>file.txt &&
	git add file.txt &&
	git commit -m "will try to empty" 2>/dev/null &&
	test_must_fail git commit --amend -m "" 2>/dev/null
	)
'

test_expect_success 'amend to set message to empty with --allow-empty-message' '
	(
	cd repo &&
	git commit --amend --allow-empty-message -m "" 2>/dev/null &&
	git cat-file commit HEAD >commit &&
	sed -e "1,/^$/d" commit >body &&
	! grep -q "[^ ]" body
	)
'

test_expect_success 'commit --date does not affect committer date' '
	(
	cd repo &&
	echo "datenocommitter" >>file.txt &&
	git add file.txt &&
	GIT_COMMITTER_DATE="1500000000 +0000" \
		git commit --date="1600000000 +0000" -m "date split" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "^author.*1600000000 +0000" actual &&
	grep "^committer.*1500000000 +0000" actual
	)
'

test_expect_success 'amend with -a stages modified tracked files' '
	(
	cd repo &&
	echo "amendabase" >>file.txt &&
	git add file.txt &&
	git commit -m "before amend -a" 2>/dev/null &&
	echo "amendachange" >>file.txt &&
	git commit --amend -a -m "after amend -a" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "after amend -a" actual
	)
'

test_expect_success 'commit -q with --allow-empty is quiet' '
	(
	cd repo &&
	git commit -q --allow-empty -m "quiet empty" 2>stderr &&
	test ! -s stderr
	)
'

test_expect_success 'commit after git reset --hard with no changes fails' '
	(
	cd repo &&
	git reset --hard HEAD 2>/dev/null &&
	test_must_fail git commit -m "nothing changed" 2>/dev/null
	)
'

test_expect_success 'GIT_COMMITTER_NAME and GIT_COMMITTER_EMAIL override config' '
	(
	cd repo &&
	echo "gcn" >>file.txt &&
	git add file.txt &&
	GIT_COMMITTER_NAME="Other Committer" GIT_COMMITTER_EMAIL="other@commit.com" \
		git commit -m "env committer name" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "^committer Other Committer <other@commit.com>" actual
	)
'

test_expect_success 'commit -F with single line file preserves message exactly' '
	(
	cd repo &&
	echo "slp" >>file.txt &&
	git add file.txt &&
	echo "exact message" >single-msg.txt &&
	git commit -F single-msg.txt 2>/dev/null &&
	git cat-file commit HEAD >commit &&
	sed -e "1,/^$/d" commit >actual &&
	echo "exact message" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'multiple empty -m flags' '
	(
	cd repo &&
	echo "mem" >>file.txt &&
	git add file.txt &&
	test_must_fail git commit -m "" -m "" 2>/dev/null
	)
'

test_expect_success 'commit in new repo has correct tree' '
	(
	git init tree-verify-repo &&
	cd tree-verify-repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "tree data" >tree-file.txt &&
	git add tree-file.txt &&
	git commit -m "tree verify" 2>/dev/null &&
	TREE=$(git cat-file -p HEAD | sed -n "s/^tree //p" | head -1 | tr -d " \n") &&
	git cat-file -p "$TREE" >tree_listing &&
	grep "tree-file.txt" tree_listing
	)
'

test_expect_success 'amend --allow-empty preserves tree' '
	(
	git init amend-empty-tree-repo &&
	cd amend-empty-tree-repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "ae" >ae.txt &&
	git add ae.txt &&
	git commit -m "base for amend" 2>/dev/null &&
	TREE_BEFORE=$(git cat-file -p HEAD | head -1 | sed "s/^tree //") &&
	git commit --amend --allow-empty -m "amend allow empty" 2>/dev/null &&
	TREE_AFTER=$(git cat-file -p HEAD | head -1 | sed "s/^tree //") &&
	test "$TREE_BEFORE" = "$TREE_AFTER"
	)
'

test_expect_success 'commit --author with full identity string' '
	(
	cd repo &&
	echo "fullid" >>file.txt &&
	git add file.txt &&
	git commit --author="Full Name <full@identity.org>" -m "full id" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "author Full Name <full@identity.org>" actual
	)
'

test_expect_success 'commit output includes short hash' '
	(
	cd repo &&
	echo "outputhash" >>file.txt &&
	git add file.txt &&
	git commit -m "hash in output" >stderr 2>&1 &&
	HASH=$(git rev-parse --short HEAD) &&
	grep "$HASH" stderr
	)
'

test_expect_success 'multiple consecutive --allow-empty commits form chain' '
	(
	cd repo &&
	git commit --allow-empty -m "chain 1" 2>/dev/null &&
	HASH1=$(git rev-parse HEAD) &&
	git commit --allow-empty -m "chain 2" 2>/dev/null &&
	HASH2=$(git rev-parse HEAD) &&
	git commit --allow-empty -m "chain 3" 2>/dev/null &&
	HASH3=$(git rev-parse HEAD) &&
	test "$HASH1" != "$HASH2" &&
	test "$HASH2" != "$HASH3" &&
	PARENT3=$(git cat-file -p HEAD | sed -n "s/^parent //p") &&
	test "$PARENT3" = "$HASH2" &&
	PARENT2=$(git cat-file -p "$HASH2" | sed -n "s/^parent //p") &&
	test "$PARENT2" = "$HASH1"
	)
'

test_expect_success 'amend consecutive times updates message each time' '
	(
	cd repo &&
	echo "amendmulti" >>file.txt &&
	git add file.txt &&
	git commit -m "first version" 2>/dev/null &&
	git commit --amend -m "second version" 2>/dev/null &&
	git commit --amend -m "third version" 2>/dev/null &&
	git cat-file commit HEAD >commit &&
	grep "third version" commit &&
	! grep "first version" commit &&
	! grep "second version" commit
	)
'

# ── additional commit tests ─────────────────────────────────────────────

test_expect_success 'commit records correct committer' '
	(
	cd repo &&
	echo "committer-test" >>file.txt &&
	git add file.txt &&
	git commit -m "committer check" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "^committer " actual
	)
'

test_expect_success 'commit records tree in header' '
	(
	cd repo &&
	echo "tree-check" >>file.txt &&
	git add file.txt &&
	git commit -m "tree check" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "^tree [0-9a-f]\{40\}" actual
	)
'

test_expect_success 'commit with only whitespace message still works' '
	(
	cd repo &&
	echo "ws-msg" >>file.txt &&
	git add file.txt &&
	git commit -m "   spaces   " 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "spaces" actual
	)
'

test_expect_success 'commit on new branch preserves parent' '
	(
	cd repo &&
	parent=$(git rev-parse HEAD) &&
	git checkout -b commit-branch-test 2>/dev/null &&
	echo "on branch" >>file.txt &&
	git add file.txt &&
	git commit -m "branch commit" 2>/dev/null &&
	actual_parent=$(git cat-file -p HEAD | sed -n "s/^parent //p") &&
	test "$actual_parent" = "$parent" &&
	git checkout main 2>/dev/null
	)
'

test_expect_success 'commit --allow-empty with --author' '
	(
	cd repo &&
	git commit --allow-empty --author="Empty Author <empty@test.org>" -m "empty author" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "author Empty Author <empty@test.org>" actual
	)
'

test_expect_success 'commit creates different hash for different message' '
	(
	cd repo &&
	git commit --allow-empty -m "unique msg alpha" 2>/dev/null &&
	hash1=$(git rev-parse HEAD) &&
	git commit --allow-empty -m "unique msg beta" 2>/dev/null &&
	hash2=$(git rev-parse HEAD) &&
	test "$hash1" != "$hash2"
	)
'

test_expect_success 'amend preserves parent' '
	(
	cd repo &&
	echo "amend-parent" >>file.txt &&
	git add file.txt &&
	git commit -m "pre-amend" 2>/dev/null &&
	parent_before=$(git cat-file -p HEAD | sed -n "s/^parent //p") &&
	git commit --amend -m "post-amend" 2>/dev/null &&
	parent_after=$(git cat-file -p HEAD | sed -n "s/^parent //p") &&
	test "$parent_before" = "$parent_after"
	)
'

test_expect_success 'amend changes the commit hash' '
	(
	cd repo &&
	echo "amend-hash" >>file.txt &&
	git add file.txt &&
	git commit -m "before amend hash" 2>/dev/null &&
	hash_before=$(git rev-parse HEAD) &&
	git commit --amend -m "after amend hash" 2>/dev/null &&
	hash_after=$(git rev-parse HEAD) &&
	test "$hash_before" != "$hash_after"
	)
'

test_expect_success 'rev-list counts commits correctly' '
	(
	cd repo &&
	git commit --allow-empty -m "count1" 2>/dev/null &&
	git commit --allow-empty -m "count2" 2>/dev/null &&
	count=$(git rev-list HEAD | wc -l | tr -d " ") &&
	test "$count" -gt 2
	)
'

test_expect_success 'commit with multi-line message' '
	(
	cd repo &&
	echo "multiline" >>file.txt &&
	git add file.txt &&
	git commit -m "line one

line three" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "line one" actual &&
	grep "line three" actual
	)
'

test_expect_success 'amend updates tree when index changed' '
	(
	cd repo &&
	echo "amend-tree-1" >>file.txt &&
	git add file.txt &&
	git commit -m "amend tree base" 2>/dev/null &&
	tree1=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	echo "amend-tree-2" >>file.txt &&
	git add file.txt &&
	git commit --amend -m "amend tree update" 2>/dev/null &&
	tree2=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	test "$tree1" != "$tree2"
	)
'

test_expect_success 'commit cat-file -t is commit' '
	(
	cd repo &&
	git commit --allow-empty -m "type test" 2>/dev/null &&
	grit cat-file -t HEAD >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# Additional commit coverage
# ---------------------------------------------------------------------------
test_expect_success 'commit --allow-empty creates commit with same tree' '
	(
	cd repo &&
	tree1=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	git commit --allow-empty -m "empty again" 2>/dev/null &&
	tree2=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'commit records parent' '
	(
	cd repo &&
	parent=$(git rev-parse HEAD) &&
	git commit --allow-empty -m "has parent" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "parent $parent" actual
	)
'

test_expect_success 'commit records author' '
	(
	cd repo &&
	git cat-file -p HEAD >actual &&
	grep "^author" actual
	)
'

test_expect_success 'commit records committer' '
	(
	cd repo &&
	git cat-file -p HEAD >actual &&
	grep "^committer" actual
	)
'

test_expect_success 'commit message is stored correctly' '
	(
	cd repo &&
	git commit --allow-empty -m "exact message check" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "exact message check" actual
	)
'

test_expect_success 'commit updates HEAD' '
	(
	cd repo &&
	old=$(git rev-parse HEAD) &&
	git commit --allow-empty -m "move head" 2>/dev/null &&
	new=$(git rev-parse HEAD) &&
	test "$old" != "$new"
	)
'

test_expect_success 'commit with staged new file adds it to tree' '
	(
	cd repo &&
	echo "newfile" >nf.txt &&
	git add nf.txt &&
	git commit -m "add nf" 2>/dev/null &&
	git ls-tree HEAD >actual &&
	grep "nf.txt" actual
	)
'

test_expect_success 'commit with staged modification updates blob' '
	(
	cd repo &&
	echo "modified" >>nf.txt &&
	git add nf.txt &&
	blob1=$(git ls-tree HEAD -- nf.txt | awk "{print \$3}") &&
	git commit -m "modify nf" 2>/dev/null &&
	blob2=$(git ls-tree HEAD -- nf.txt | awk "{print \$3}") &&
	test "$blob1" != "$blob2"
	)
'

test_expect_success 'amend keeps single parent' '
	(
	cd repo &&
	git commit --allow-empty -m "amend base" 2>/dev/null &&
	git commit --amend -m "amended" 2>/dev/null &&
	parent_count=$(git cat-file -p HEAD | grep -c "^parent") &&
	test "$parent_count" -eq 1
	)
'

test_expect_success 'amend replaces commit message' '
	(
	cd repo &&
	git commit --allow-empty -m "before amend" 2>/dev/null &&
	git commit --amend -m "after amend" 2>/dev/null &&
	git cat-file -p HEAD >actual &&
	grep "after amend" actual &&
	! grep "before amend" actual
	)
'

test_expect_success 'commit with empty message fails' '
	(
	cd repo &&
	test_must_fail git commit --allow-empty -m "" 2>/dev/null
	)
'

test_expect_success 'commit hash is 40 characters' '
	(
	cd repo &&
	git commit --allow-empty -m "hash len" 2>/dev/null &&
	hash=$(git rev-parse HEAD) &&
	test ${#hash} -eq 40
	)
'

test_expect_success 'second commit has different hash from first' '
	(
	cd repo &&
	h1=$(git rev-parse HEAD) &&
	git commit --allow-empty -m "different" 2>/dev/null &&
	h2=$(git rev-parse HEAD) &&
	test "$h1" != "$h2"
	)
'

test_expect_success 'commit tree object is a valid tree' '
	(
	cd repo &&
	tree=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	grit cat-file -t "$tree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

# === additional deepening tests ===

test_expect_success 'commit records correct author name' '
	(
	cd repo &&
	(
		unset GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL &&
		git commit --allow-empty -m "author test" 2>/dev/null
	) &&
	grit cat-file -p HEAD >actual &&
	grep "author Test" actual
	)
'

test_expect_success 'commit records correct committer email' '
	(
	cd repo &&
	grit cat-file -p HEAD >actual &&
	grep "committer.*@" actual
	)
'

test_expect_success 'commit parent matches previous HEAD' '
	(
	cd repo &&
	prev=$(git rev-parse HEAD) &&
	git commit --allow-empty -m "parent test" 2>/dev/null &&
	grit cat-file -p HEAD >actual &&
	grep "parent $prev" actual
	)
'

test_expect_success 'initial commit has no parent line' '
	(
	rm -rf repo-noparent &&
	git init repo-noparent &&
	cd repo-noparent &&
	git config user.name T && git config user.email t@t &&
	echo first >f && git add f && git commit -m "first" 2>/dev/null &&
	grit cat-file -p HEAD >actual &&
	! grep "^parent" actual
	)
'

test_expect_success 'commit message body preserved verbatim' '
	(
	cd repo &&
	git commit --allow-empty -m "line1" -m "line2 detail" 2>/dev/null &&
	grit cat-file -p HEAD >actual &&
	grep "line2 detail" actual
	)
'

test_expect_success 'commit with --author overrides author' '
	(
	cd repo &&
	git commit --allow-empty --author="Other <other@x.com>" -m "diff author" 2>/dev/null &&
	grit cat-file -p HEAD >actual &&
	grep "author Other <other@x.com>" actual
	)
'

test_expect_success 'commit -a stages modified tracked files' '
	(
	cd repo &&
	echo base >ca_file.txt &&
	git add ca_file.txt && git commit -m "add ca" 2>/dev/null &&
	echo modified >ca_file.txt &&
	git commit -a -m "commit -a" 2>/dev/null &&
	git diff --exit-code HEAD
	)
'

test_expect_success 'commit creates object reachable by rev-parse' '
	(
	cd repo &&
	git commit --allow-empty -m "reachable" 2>/dev/null &&
	hash=$(grit rev-parse HEAD) &&
	grit cat-file -t "$hash" >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'multiple commits increment rev-list count' '
	(
	cd repo &&
	count_before=$(git rev-list HEAD --count) &&
	git commit --allow-empty -m "inc1" 2>/dev/null &&
	git commit --allow-empty -m "inc2" 2>/dev/null &&
	count_after=$(git rev-list HEAD --count) &&
	test "$count_after" -eq "$((count_before + 2))"
	)
'

test_expect_success 'commit on detached HEAD works' '
	(
	cd repo &&
	git checkout --detach HEAD 2>/dev/null &&
	git commit --allow-empty -m "detached" 2>/dev/null &&
	grit cat-file -p HEAD >actual &&
	grep "detached" actual &&
	git checkout - 2>/dev/null
	)
'

test_expect_success 'cat-file -s on commit returns nonzero size' '
	(
	cd repo &&
	hash=$(grit rev-parse HEAD) &&
	size=$(grit cat-file -s "$hash") &&
	test "$size" -gt 0
	)
'

test_expect_success 'commit tree changes when file content changes' '
	(
	cd repo &&
	tree1=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	echo newtreedata >tree_chg.txt &&
	git add tree_chg.txt && git commit -m "tree change" 2>/dev/null &&
	tree2=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	test "$tree1" != "$tree2"
	)
'

test_expect_success 'amend preserves parent of original commit' '
	(
	cd repo &&
	parent_before=$(git cat-file -p HEAD | sed -n "s/^parent //p") &&
	git commit --amend -m "amend keep parent" 2>/dev/null &&
	parent_after=$(git cat-file -p HEAD | sed -n "s/^parent //p") &&
	test "$parent_before" = "$parent_after"
	)
'

test_expect_success 'commit with only whitespace message fails' '
	(
	cd repo &&
	test_must_fail git commit --allow-empty -m "   " 2>/dev/null
	)
'

test_expect_success 'rev-parse HEAD~1 gives parent commit' '
	(
	cd repo &&
	parent=$(git cat-file -p HEAD | sed -n "s/^parent //p") &&
	parsed=$(grit rev-parse HEAD~1) &&
	test "$parent" = "$parsed"
	)
'

test_expect_success 'commit --allow-empty creates commit with same tree' '
	(
	cd repo &&
	tree_before=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	grit commit --allow-empty -m "empty commit" &&
	tree_after=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	test "$tree_before" = "$tree_after"
	)
'

test_expect_success 'commit records correct author in commit object' '
	(
	cd repo &&
	grit log -n 1 --format="%an" >../actual &&
	test -s ../actual
	)
'

test_expect_success 'commit with -m produces single-line message' '
	(
	cd repo &&
	echo cmt_msg >cmt_msg.txt &&
	git add cmt_msg.txt &&
	grit commit -m "single line message" &&
	grit log -n 1 --format="%s" >../actual &&
	grep "single line message" ../actual
	)
'

test_expect_success 'commit creates new SHA each time' '
	(
	cd repo &&
	sha1=$(grit rev-parse HEAD) &&
	grit commit --allow-empty -m "new sha" &&
	sha2=$(grit rev-parse HEAD) &&
	test "$sha1" != "$sha2"
	)
'

test_expect_success 'commit updates HEAD ref' '
	(
	cd repo &&
	old=$(grit rev-parse HEAD) &&
	echo upd_head >upd_head.txt &&
	git add upd_head.txt &&
	grit commit -m "update head" &&
	new=$(grit rev-parse HEAD) &&
	test "$old" != "$new"
	)
'

test_expect_success 'commit --amend changes the message' '
	(
	cd repo &&
	grit commit --allow-empty -m "before amend" &&
	grit commit --amend -m "after amend" &&
	grit log -n 1 --format="%s" >../actual &&
	grep "after amend" ../actual
	)
'

test_expect_success 'commit with staged deletion removes file from tree' '
	(
	cd repo &&
	echo delcommit >del_commit.txt &&
	git add del_commit.txt &&
	grit commit -m "add for del" &&
	grit rm del_commit.txt &&
	grit commit -m "remove file" &&
	test_must_fail git ls-files --error-unmatch del_commit.txt
	)
'

test_expect_success 'commit without staged changes fails' '
	(
	cd repo &&
	test_must_fail grit commit -m "nothing staged" 2>/dev/null
	)
'

test_expect_success 'commit on detached HEAD works' '
	(
	cd repo &&
	head_sha=$(grit rev-parse HEAD) &&
	git checkout "$head_sha" 2>/dev/null &&
	grit commit --allow-empty -m "detached commit" &&
	git checkout main 2>/dev/null
	)
'

test_expect_success 'commit message is stored in commit object' '
	(
	cd repo &&
	grit commit --allow-empty -m "unique message 12345" &&
	git cat-file -p HEAD >../actual &&
	grep "unique message 12345" ../actual
	)
'

test_expect_success 'consecutive commits form a chain' '
	(
	cd repo &&
	grit commit --allow-empty -m "chain1" &&
	sha1=$(grit rev-parse HEAD) &&
	grit commit --allow-empty -m "chain2" &&
	parent=$(git cat-file -p HEAD | sed -n "s/^parent //p") &&
	test "$sha1" = "$parent"
	)
'

test_expect_success 'commit --allow-empty-message with empty message' '
	(
	cd repo &&
	grit commit --allow-empty --allow-empty-message -m "" &&
	grit log -n 1 --format="%s" >../actual &&
	test -f ../actual
	)
'

test_expect_success 'amend does not create extra parent' '
	(
	cd repo &&
	grit commit --allow-empty -m "to amend" &&
	parent_count_before=$(git cat-file -p HEAD | grep -c "^parent") &&
	grit commit --amend -m "amended" &&
	parent_count_after=$(git cat-file -p HEAD | grep -c "^parent") &&
	test "$parent_count_before" = "$parent_count_after"
	)
'

test_expect_success 'commit tree hash matches write-tree of index' '
	(
	cd repo &&
	echo treematch >treematch.txt &&
	git add treematch.txt &&
	tree=$(grit write-tree) &&
	grit commit -m "treematch" &&
	commit_tree=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	test "$tree" = "$commit_tree"
	)
'

test_expect_success 'commit sets committer date' '
	(
	cd repo &&
	grit commit --allow-empty -m "dated commit" &&
	git cat-file -p HEAD >../actual &&
	grep "^committer .* [0-9]\{10\}" ../actual
	)
'

# ---------------------------------------------------------------------------
# Deepening tests (w32-deepen)
# ---------------------------------------------------------------------------

test_expect_success 'deepen setup: fresh repo for commit tests' '
	(
	git init deepen-commit-repo &&
	cd deepen-commit-repo &&
	git config user.name "Commit Tester" &&
	git config user.email "commit@test.com" &&
	echo "base" >base.txt &&
	git add base.txt &&
	grit commit -m "initial commit"
	)
'

test_expect_success 'commit creates a commit object' '
	(
	cd deepen-commit-repo &&
	git cat-file -t HEAD >output &&
	grep "commit" output
	)
'

test_expect_success 'commit message is stored correctly' '
	(
	cd deepen-commit-repo &&
	git log -n 1 --format="%s" >output &&
	test "$(cat output)" = "initial commit"
	)
'

test_expect_success 'commit with --allow-empty creates empty commit' '
	(
	cd deepen-commit-repo &&
	grit commit --allow-empty -m "empty commit" &&
	git log -n 1 --format="%s" >output &&
	test "$(cat output)" = "empty commit"
	)
'

test_expect_success 'commit sets author name from config' '
	(
	cd deepen-commit-repo &&
	(
		unset GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL &&
		echo new-content >author-test.txt &&
		git add author-test.txt &&
		git commit -m "author from config"
	) &&
	git log -n 1 --format="%an" >output &&
	test "$(cat output)" = "Commit Tester"
	)
'

test_expect_success 'commit sets author email from config' '
	(
	cd deepen-commit-repo &&
	git log -n 1 --format="%ae" >output &&
	test "$(cat output)" = "commit@test.com"
	)
'

test_expect_success 'commit HEAD advances after new commit' '
	(
	cd deepen-commit-repo &&
	H1=$(git rev-parse HEAD) &&
	echo "new" >new.txt &&
	git add new.txt &&
	grit commit -m "advance HEAD" &&
	H2=$(git rev-parse HEAD) &&
	test "$H1" != "$H2"
	)
'

test_expect_success 'commit parent is previous HEAD' '
	(
	cd deepen-commit-repo &&
	PARENT=$(git cat-file -p HEAD | sed -n "s/^parent //p") &&
	test ${#PARENT} -eq 40
	)
'

test_expect_success 'commit tree matches index' '
	(
	cd deepen-commit-repo &&
	TREE=$(grit write-tree) &&
	COMMIT_TREE=$(git cat-file -p HEAD | sed -n "s/^tree //p") &&
	test "$TREE" = "$COMMIT_TREE"
	)
'

test_expect_success 'commit with multi-word message' '
	(
	cd deepen-commit-repo &&
	grit commit --allow-empty -m "this is a longer commit message" &&
	git log -n 1 --format="%s" >output &&
	test "$(cat output)" = "this is a longer commit message"
	)
'

test_expect_success 'commit without staged changes fails' '
	(
	cd deepen-commit-repo &&
	test_must_fail grit commit -m "nothing staged" 2>/dev/null
	)
'

test_expect_success 'commit with added file shows in log' '
	(
	cd deepen-commit-repo &&
	echo "logged" >logged.txt &&
	git add logged.txt &&
	grit commit -m "add logged" &&
	git log --oneline >output &&
	grep "add logged" output
	)
'

test_expect_success 'commit preserves file content' '
	(
	cd deepen-commit-repo &&
	echo "preserved" >preserved.txt &&
	git add preserved.txt &&
	grit commit -m "preserve test" &&
	git show HEAD:preserved.txt >output &&
	test "$(cat output)" = "preserved"
	)
'

test_expect_success 'multiple commits increase log count' '
	(
	cd deepen-commit-repo &&
	COUNT1=$(git log --oneline | wc -l) &&
	grit commit --allow-empty -m "extra1" &&
	grit commit --allow-empty -m "extra2" &&
	COUNT2=$(git log --oneline | wc -l) &&
	test $COUNT2 -eq $((COUNT1 + 2))
	)
'

test_expect_success 'commit on detached HEAD works' '
	(
	cd deepen-commit-repo &&
	git checkout --detach HEAD 2>/dev/null &&
	grit commit --allow-empty -m "detached commit" &&
	git log -n 1 --format="%s" >output &&
	test "$(cat output)" = "detached commit" &&
	git checkout - 2>/dev/null
	)
'

test_done
