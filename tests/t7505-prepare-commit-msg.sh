#!/bin/sh
# Tests for 'grit commit' message preparation: -m, -F, --allow-empty,
# --allow-empty-message, --amend, --signoff, --author, --date.

test_description='grit commit message preparation'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup base repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "A U Thor" &&
	git config user.email "author@example.com" &&
	echo initial >file &&
	git add file &&
	test_tick &&
	git commit -m "initial commit"
	)
'

# ── -m flag ──────────────────────────────────────────────────────────────────

test_expect_success 'commit -m with simple message' '
	(
	cd repo &&
	echo change1 >file &&
	git add file &&
	test_tick &&
	git commit -m "simple message" &&
	git log -n1 --format="%s" >actual &&
	echo "simple message" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit -m with empty string fails' '
	(
	cd repo &&
	echo change2 >file &&
	git add file &&
	test_must_fail git commit -m ""
	)
'

test_expect_success 'commit -m preserves leading whitespace in message' '
	(
	cd repo &&
	echo change2b >file &&
	git add file &&
	test_tick &&
	git commit -m "  spaced message  " &&
	git log -n1 --format="%s" >actual &&
	echo "  spaced message" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit -m with multi-word message' '
	(
	cd repo &&
	echo change3 >file &&
	git add file &&
	test_tick &&
	git commit -m "this is a longer commit message" &&
	git log -n1 --format="%s" >actual &&
	echo "this is a longer commit message" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit -m with UTF-8 message' '
	(
	cd repo &&
	echo change4 >file &&
	git add file &&
	test_tick &&
	git commit -m "café résumé" &&
	git log -n1 --format="%s" >actual &&
	echo "café résumé" >expect &&
	test_cmp expect actual
	)
'

# ── -F flag ──────────────────────────────────────────────────────────────────

test_expect_success 'commit -F reads message from file' '
	(
	cd repo &&
	echo change5 >file &&
	git add file &&
	echo "message from file" >msg.txt &&
	test_tick &&
	git commit -F msg.txt &&
	git log -n1 --format="%s" >actual &&
	echo "message from file" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit -F with multi-line message file' '
	(
	cd repo &&
	echo change6 >file &&
	git add file &&
	cat >msg.txt <<-\EOF &&
	Subject line from file

	This is the body of the commit message.
	It has multiple lines.
	EOF
	test_tick &&
	git commit -F msg.txt &&
	git log -n1 --format="%s" >actual &&
	echo "Subject line from file" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit -F with empty file fails' '
	(
	cd repo &&
	echo change7 >file &&
	git add file &&
	>empty.txt &&
	test_must_fail git commit -F empty.txt
	)
'

test_expect_success 'commit -F with nonexistent file fails' '
	(
	cd repo &&
	echo change7b >file &&
	git add file &&
	test_must_fail git commit -F no-such-file.txt
	)
'

test_expect_success 'commit -F reads stdin with -F -' '
	(
	cd repo &&
	echo change8 >file &&
	git add file &&
	test_tick &&
	echo "message from stdin" | git commit -F - &&
	git log -n1 --format="%s" >actual &&
	echo "message from stdin" >expect &&
	test_cmp expect actual
	)
'

# ── --allow-empty ────────────────────────────────────────────────────────────

test_expect_success 'commit --allow-empty with no changes' '
	(
	cd repo &&
	test_tick &&
	git commit --allow-empty -m "empty commit" &&
	git log -n1 --format="%s" >actual &&
	echo "empty commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit without --allow-empty and no changes fails' '
	(
	cd repo &&
	test_must_fail git commit -m "should fail"
	)
'

test_expect_success 'commit --allow-empty multiple times' '
	(
	cd repo &&
	test_tick &&
	git commit --allow-empty -m "empty 1" &&
	test_tick &&
	git commit --allow-empty -m "empty 2" &&
	git log -n1 --format="%s" >actual &&
	echo "empty 2" >expect &&
	test_cmp expect actual
	)
'

# ── --allow-empty-message ────────────────────────────────────────────────────

test_expect_success 'commit --allow-empty-message accepts blank message' '
	(
	cd repo &&
	echo change9 >file &&
	git add file &&
	test_tick &&
	git commit --allow-empty-message -m "" &&
	git log -n1 --format="%s" >actual &&
	test -f actual
	)
'

test_expect_success 'commit --allow-empty --allow-empty-message with blank' '
	(
	cd repo &&
	test_tick &&
	git commit --allow-empty --allow-empty-message -m "" &&
	git log -n1 --format="%s" >actual &&
	test -f actual
	)
'

# ── --amend ──────────────────────────────────────────────────────────────────

test_expect_success 'commit --amend changes last message' '
	(
	cd repo &&
	echo amend1 >file &&
	git add file &&
	test_tick &&
	git commit -m "original message" &&
	test_tick &&
	git commit --amend -m "amended message" &&
	git log -n1 --format="%s" >actual &&
	echo "amended message" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit --amend preserves author when message changes' '
	(
	cd repo &&
	git log -n1 --format="%an <%ae>" >actual &&
	echo "A U Thor <author@example.com>" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit --amend does not create new commit hash' '
	(
	cd repo &&
	echo amend2 >file &&
	git add file &&
	test_tick &&
	git commit -m "before amend" &&
	git rev-parse HEAD >hash_before &&
	test_tick &&
	git commit --amend -m "after amend" &&
	git rev-parse HEAD >hash_after &&
	! test_cmp hash_before hash_after
	)
'

# ── --signoff ────────────────────────────────────────────────────────────────

test_expect_success 'commit --signoff adds Signed-off-by trailer' '
	(
	cd repo &&
	echo signoff1 >file &&
	git add file &&
	test_tick &&
	git commit --signoff -m "signed commit" &&
	git log -n1 --format="%B" >actual &&
	grep "Signed-off-by: C O Mitter <committer@example.com>" actual
	)
'

test_expect_success 'commit -s flag is accepted' '
	(
	cd repo &&
	echo signoff2 >file &&
	git add file &&
	test_tick &&
	git commit -s -m "short signed" &&
	git log -n1 --format="%s" >actual &&
	echo "short signed" >expect &&
	test_cmp expect actual
	)
'

# ── --author ─────────────────────────────────────────────────────────────────

test_expect_success 'commit --author overrides author' '
	(
	cd repo &&
	echo auth1 >file &&
	git add file &&
	test_tick &&
	git commit --author="Other Person <other@example.com>" -m "other author" &&
	git log -n1 --format="%an <%ae>" >actual &&
	echo "Other Person <other@example.com>" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit --author does not affect committer' '
	(
	cd repo &&
	git log -n1 --format="%cn <%ce>" >actual &&
	echo "C O Mitter <committer@example.com>" >expect &&
	test_cmp expect actual
	)
'

# ── --date ───────────────────────────────────────────────────────────────────

test_expect_success 'commit --date overrides author date' '
	(
	cd repo &&
	echo date1 >file &&
	git add file &&
	git commit --date="2005-04-07T22:13:13" -m "custom date" &&
	git log -n1 --format="%an" >actual &&
	echo "A U Thor" >expect &&
	test_cmp expect actual
	)
'

# ── -a (--all) ───────────────────────────────────────────────────────────────

test_expect_success 'commit -a stages tracked modified files' '
	(
	cd repo &&
	echo all1 >file &&
	test_tick &&
	git commit -a -m "commit all" &&
	git log -n1 --format="%s" >actual &&
	echo "commit all" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit -a does not stage untracked files' '
	(
	cd repo &&
	echo untracked >newfile &&
	test_must_fail git commit -a -m "should not include newfile"
	)
'

# ── quiet mode ───────────────────────────────────────────────────────────────

test_expect_success 'commit -q suppresses output' '
	(
	cd repo &&
	echo quiet1 >file &&
	git add file &&
	test_tick &&
	git commit -q -m "quiet commit" >output 2>&1 &&
	test_must_be_empty output
	)
'

test_expect_success 'commit without -q shows summary' '
	(
	cd repo &&
	echo loud1 >file &&
	git add file &&
	test_tick &&
	git commit -m "loud commit" >output 2>&1 &&
	test -s output
	)
'

# ── combined flags ───────────────────────────────────────────────────────────

test_expect_success 'commit --allow-empty --signoff -m accepts flags' '
	(
	cd repo &&
	test_tick &&
	git commit --allow-empty --signoff -m "empty signed" &&
	git log -n1 --format="%s" >actual_subj &&
	echo "empty signed" >expect &&
	test_cmp expect actual_subj
	)
'

test_expect_success 'commit -F with --signoff accepts flags' '
	(
	cd repo &&
	echo combined1 >file &&
	git add file &&
	echo "file message for signoff" >msg.txt &&
	test_tick &&
	git commit -F msg.txt --signoff &&
	git log -n1 --format="%s" >actual &&
	echo "file message for signoff" >expect &&
	test_cmp expect actual
	)
'

test_done
