#!/bin/sh
# Tests for grit commit: -m, -F, --amend, --allow-empty, --author, -a, --signoff

test_description='grit commit: message, file, amend, author, signoff'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with history' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo a >a.txt &&
	"$REAL_GIT" add a.txt &&
	"$REAL_GIT" commit -m "add a" &&
	echo b >b.txt &&
	"$REAL_GIT" add b.txt &&
	"$REAL_GIT" commit -m "add b" &&
	echo c >c.txt &&
	"$REAL_GIT" add c.txt &&
	"$REAL_GIT" commit -m "add c"
	)
'

###########################################################################
# Section 2: commit -m
###########################################################################

test_expect_success 'commit -m creates commit with message' '
	(
	cd repo &&
	echo d >d.txt &&
	git add d.txt &&
	git commit -m "add d" &&
	git log --oneline -n 1 >out &&
	grep "add d" out
	)
'

test_expect_success 'commit -m with multi-word message' '
	(
	cd repo &&
	echo e >e.txt &&
	git add e.txt &&
	git commit -m "this is a longer commit message" &&
	git log --oneline -n 1 >out &&
	grep "this is a longer" out
	)
'

test_expect_success 'commit -m records correct tree' '
	(
	cd repo &&
	git cat-file -p HEAD >out &&
	grep "^tree " out
	)
'

test_expect_success 'commit -m records parent' '
	(
	cd repo &&
	git rev-parse HEAD~1 >parent &&
	test -s parent
	)
'

test_expect_success 'commit creates proper commit object' '
	(
	cd repo &&
	git cat-file -t HEAD >out &&
	test "$(cat out)" = "commit"
	)
'

test_expect_success 'commit -m with special characters' '
	(
	cd repo &&
	echo f >f.txt &&
	git add f.txt &&
	git commit -m "fix: handle edge-case (issue #42)" &&
	git log --oneline -n 1 >out &&
	grep "fix:" out
	)
'

###########################################################################
# Section 3: commit -F (file)
###########################################################################

test_expect_success 'commit -F reads message from file' '
	(
	cd repo &&
	echo g >g.txt &&
	git add g.txt &&
	echo "Message from file" >msg.txt &&
	git commit -F msg.txt &&
	git log --oneline -n 1 >out &&
	grep "Message from file" out
	)
'

test_expect_success 'commit -F with multiline file' '
	(
	cd repo &&
	echo h >h.txt &&
	git add h.txt &&
	printf "Subject line\n\nBody paragraph one.\nBody paragraph two.\n" >multiline-msg.txt &&
	git commit -F multiline-msg.txt &&
	git log --format=%s -n 1 >out &&
	grep "Subject line" out
	)
'

test_expect_success 'commit -F with - reads from stdin' '
	(
	cd repo &&
	echo i >i.txt &&
	git add i.txt &&
	echo "From stdin message" | git commit -F - &&
	git log --oneline -n 1 >out &&
	grep "From stdin" out
	)
'

###########################################################################
# Section 4: commit --amend
###########################################################################

test_expect_success 'commit --amend changes last commit message' '
	(
	cd repo &&
	git commit --allow-empty -m "before amend" &&
	git commit --amend -m "after amend" &&
	git log --oneline -n 1 >out &&
	grep "after amend" out
	)
'

test_expect_success 'commit --amend changes the SHA' '
	(
	cd repo &&
	sha_before=$(git rev-parse HEAD) &&
	git commit --amend -m "amended again" &&
	sha_after=$(git rev-parse HEAD) &&
	test "$sha_before" != "$sha_after"
	)
'

test_expect_success 'commit --amend with staged changes' '
	(
	cd repo &&
	echo j >j.txt &&
	git add j.txt &&
	git commit -m "with j" &&
	echo "more" >>j.txt &&
	git add j.txt &&
	git commit --amend -m "with j amended" &&
	git log --oneline -n 1 >out &&
	grep "with j amended" out
	)
'

test_expect_success 'commit --amend does not add extra commit' '
	(
	cd repo &&
	count_before=$(git rev-list HEAD | wc -l) &&
	git commit --amend -m "same count" &&
	count_after=$(git rev-list HEAD | wc -l) &&
	test "$count_before" = "$count_after"
	)
'

###########################################################################
# Section 5: commit --allow-empty
###########################################################################

test_expect_success 'commit --allow-empty creates empty commit' '
	(
	cd repo &&
	git commit --allow-empty -m "empty commit" &&
	git log --oneline -n 1 >out &&
	grep "empty commit" out
	)
'

test_expect_success 'commit --allow-empty preserves same tree' '
	(
	cd repo &&
	parent_sha=$(git rev-parse HEAD~1) &&
	tree_parent=$(git cat-file -p "$parent_sha" | grep "^tree " | cut -d" " -f2) &&
	tree_head=$(git cat-file -p HEAD | grep "^tree " | cut -d" " -f2) &&
	test "$tree_parent" = "$tree_head"
	)
'

test_expect_success 'commit fails with no staged changes and no --allow-empty' '
	(
	cd repo &&
	test_must_fail git commit -m "nothing to commit" 2>/dev/null
	)
'

test_expect_success 'commit --allow-empty-message with empty message' '
	(
	cd repo &&
	echo k >k.txt &&
	git add k.txt &&
	git commit --allow-empty-message -m "" &&
	git cat-file -t HEAD >out &&
	test "$(cat out)" = "commit"
	)
'

###########################################################################
# Section 6: commit -a (auto-stage)
###########################################################################

test_expect_success 'commit -a auto-stages tracked modified files' '
	(
	cd repo &&
	echo "modified" >>a.txt &&
	git commit -a -m "auto staged" &&
	git log --oneline -n 1 >out &&
	grep "auto staged" out
	)
'

test_expect_success 'commit -a does not stage untracked files' '
	(
	cd repo &&
	echo "untracked" >untracked-file.txt &&
	git commit -a -m "no untracked" --allow-empty &&
	git status >out 2>&1 &&
	grep "untracked-file.txt" out
	)
'

test_expect_success 'commit -a with multiple modified files' '
	(
	cd repo &&
	echo "mod-a" >>a.txt &&
	echo "mod-b" >>b.txt &&
	git commit -a -m "multiple mods" &&
	git log --oneline -n 1 >out &&
	grep "multiple mods" out
	)
'

###########################################################################
# Section 7: commit --author
###########################################################################

test_expect_success 'commit --author sets author name' '
	(
	cd repo &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo auth >auth.txt &&
	git add auth.txt &&
	git commit --author="Other Person <other@example.com>" -m "by other" &&
	git log --format=%an -n 1 >out &&
	test "$(cat out)" = "Other Person"
	)
'

test_expect_success 'commit --author sets author email' '
	(
	cd repo &&
	git log --format=%ae -n 1 >out &&
	test "$(cat out)" = "other@example.com"
	)
'

test_expect_success 'commit --author does not change committer' '
	(
	cd repo &&
	git log --format=%cn -n 1 >out &&
	test "$(cat out)" = "Test User"
	)
'

test_expect_success 'commit --author committer email unchanged' '
	(
	cd repo &&
	git log --format=%ce -n 1 >out &&
	test "$(cat out)" = "test@example.com"
	)
'

###########################################################################
# Section 8: commit --signoff
###########################################################################

test_expect_success 'commit --date overrides author date' '
	(
	cd repo &&
	echo sign >sign.txt &&
	git add sign.txt &&
	git commit --date="2020-01-01T00:00:00+0000" -m "dated commit" &&
	git cat-file -p HEAD >out &&
	grep "author.*2020\|1577836800" out
	)
'

test_expect_success 'commit -q is quiet' '
	(
	cd repo &&
	echo quiet >quiet.txt &&
	git add quiet.txt &&
	git commit -q -m "quiet commit" >out 2>&1 &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 9: commit log verification
###########################################################################

test_expect_success 'log shows all commits' '
	(
	cd repo &&
	git log --oneline >out &&
	test "$(wc -l <out)" -gt 5
	)
'

test_expect_success 'rev-list HEAD counts commits' '
	(
	cd repo &&
	git rev-list HEAD >out &&
	test "$(wc -l <out)" -gt 5
	)
'

test_expect_success 'log --reverse shows oldest first' '
	(
	cd repo &&
	git log --oneline --reverse >out &&
	head -1 out >first &&
	grep "add a" first
	)
'

test_expect_success 'log -n limits output' '
	(
	cd repo &&
	git log --oneline -n 3 >out &&
	test "$(wc -l <out)" -eq 3
	)
'

test_expect_success 'log --format=%H shows full SHA' '
	(
	cd repo &&
	git log --format=%H -n 1 >out &&
	test "$(wc -c <out)" -gt 39
	)
'

test_expect_success 'log --format=%s shows subject' '
	(
	cd repo &&
	git log --format=%s -n 1 >out &&
	test -s out
	)
'

test_done
