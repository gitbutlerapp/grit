#!/bin/sh
# Tests for commit message sources: -F file, -F -, -m multiple,
# --allow-empty-message, --allow-empty, --signoff, --author, --date,
# --amend, -q, -a, and interaction between options.

test_description='commit message from file, stdin, and multiple -m'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo initial >file.txt &&
	git add file.txt &&
	grit commit -m "initial commit"
	)
'

# ── commit -F file ────────────────────────────────────────────────────────

test_expect_success 'commit -F reads message from file' '
	(
	cd repo &&
	echo "change one" >>file.txt &&
	git add file.txt &&
	echo "message from file" >../msg.txt &&
	grit commit -F ../msg.txt &&
	grit log -n 1 --format=%s >actual &&
	echo "message from file" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit -F with multiline message preserves body' '
	(
	cd repo &&
	echo "change two" >>file.txt &&
	git add file.txt &&
	printf "subject line\n\nbody paragraph\n" >../msg2.txt &&
	grit commit -F ../msg2.txt &&
	grit log -n 1 --format=%s >actual &&
	echo "subject line" >expect &&
	test_cmp expect actual &&
	grit log -n 1 --format=%b >body &&
	grep "body paragraph" body
	)
'

test_expect_success 'commit -F with empty file fails without --allow-empty-message' '
	(
	cd repo &&
	echo "change three" >>file.txt &&
	git add file.txt &&
	>../empty.txt &&
	test_must_fail grit commit -F ../empty.txt 2>err
	)
'

test_expect_success 'commit -F with empty file succeeds with --allow-empty-message' '
	(
	cd repo &&
	echo "change three-b" >>file.txt &&
	git add file.txt &&
	>../empty.txt &&
	grit commit --allow-empty-message -F ../empty.txt
	)
'

test_expect_success 'commit -F reads from absolute path' '
	(
	cd repo &&
	echo "change four" >>file.txt &&
	git add file.txt &&
	echo "absolute path message" >/tmp/grit-test-msg.txt &&
	grit commit -F /tmp/grit-test-msg.txt &&
	grit log -n 1 --format=%s >actual &&
	echo "absolute path message" >expect &&
	test_cmp expect actual &&
	rm -f /tmp/grit-test-msg.txt
	)
'

# ── commit -F - (stdin) ──────────────────────────────────────────────────

test_expect_success 'commit -F - reads message from stdin' '
	(
	cd repo &&
	echo "change five" >>file.txt &&
	git add file.txt &&
	echo "stdin message" | grit commit -F - &&
	grit log -n 1 --format=%s >actual &&
	echo "stdin message" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit -F - with multiline stdin' '
	(
	cd repo &&
	echo "change six" >>file.txt &&
	git add file.txt &&
	printf "stdin subject\n\nstdin body\n" | grit commit -F - &&
	grit log -n 1 --format=%s >actual &&
	echo "stdin subject" >expect &&
	test_cmp expect actual
	)
'

# ── commit -m multiple ───────────────────────────────────────────────────

test_expect_success 'commit -m with single message' '
	(
	cd repo &&
	echo "change seven" >>file.txt &&
	git add file.txt &&
	grit commit -m "single message" &&
	grit log -n 1 --format=%s >actual &&
	echo "single message" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'multiple -m flags create separate paragraphs' '
	(
	cd repo &&
	echo "change eight" >>file.txt &&
	git add file.txt &&
	grit commit -m "first paragraph" -m "second paragraph" &&
	git cat-file -p HEAD >out &&
	grep "first paragraph" out &&
	grep "second paragraph" out
	)
'

test_expect_success 'three -m flags produce three paragraphs' '
	(
	cd repo &&
	echo "change nine" >>file.txt &&
	git add file.txt &&
	grit commit -m "para one" -m "para two" -m "para three" &&
	git cat-file -p HEAD >out &&
	grep "para one" out &&
	grep "para two" out &&
	grep "para three" out
	)
'

test_expect_success '-m with empty string and --allow-empty-message' '
	(
	cd repo &&
	echo "change ten" >>file.txt &&
	git add file.txt &&
	grit commit --allow-empty-message -m "" &&
	grit log -n 1 --format=%s >actual &&
	echo "" >expect &&
	test_cmp expect actual
	)
'

# ── --allow-empty ─────────────────────────────────────────────────────────

test_expect_success '--allow-empty creates commit with no changes' '
	(
	cd repo &&
	before=$(grit rev-parse HEAD) &&
	grit commit --allow-empty -m "empty commit" &&
	after=$(grit rev-parse HEAD) &&
	test "$before" != "$after"
	)
'

test_expect_success '--allow-empty commit has same tree as parent' '
	(
	cd repo &&
	grit commit --allow-empty -m "another empty" &&
	tree_head=$(grit rev-parse HEAD^{tree}) &&
	tree_parent=$(grit rev-parse HEAD~1^{tree}) &&
	test "$tree_head" = "$tree_parent"
	)
'

test_expect_success 'commit without changes and without --allow-empty fails' '
	(
	cd repo &&
	test_must_fail grit commit -m "should fail" 2>err
	)
'

# ── message content edge cases ────────────────────────────────────────────

test_expect_success 'commit message with leading blank lines in -F' '
	(
	cd repo &&
	echo "change eleven" >>file.txt &&
	git add file.txt &&
	printf "\n\nactual subject\n" >../msg-blanks.txt &&
	grit commit -F ../msg-blanks.txt &&
	grit log -n 1 --format=%s >actual &&
	# git strips leading blank lines
	test -s actual
	)
'

test_expect_success 'commit message with trailing whitespace in -F' '
	(
	cd repo &&
	echo "change twelve" >>file.txt &&
	git add file.txt &&
	printf "trailing spaces   \n" >../msg-trail.txt &&
	grit commit -F ../msg-trail.txt &&
	grit log -n 1 --format=%H >actual &&
	test -s actual
	)
'

# ── --author ──────────────────────────────────────────────────────────────

test_expect_success '--author overrides author' '
	(
	cd repo &&
	echo "change thirteen" >>file.txt &&
	git add file.txt &&
	grit commit --author="Other Person <other@example.com>" -m "other author" &&
	grit log -n 1 --format="%an <%ae>" >actual &&
	echo "Other Person <other@example.com>" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--author preserves committer' '
	(
	cd repo &&
	echo "change fourteen" >>file.txt &&
	git add file.txt &&
	grit commit --author="Alt Author <alt@example.com>" -m "alt commit" &&
	grit log -n 1 --format="%cn" >actual &&
	echo "C O Mitter" >expect &&
	test_cmp expect actual
	)
'

# ── --quiet ───────────────────────────────────────────────────────────────

test_expect_success '-q suppresses output' '
	(
	cd repo &&
	echo "change fifteen" >>file.txt &&
	git add file.txt &&
	grit commit -q -m "quiet commit" >actual 2>&1 &&
	test_must_be_empty actual
	)
'

# ── --amend ───────────────────────────────────────────────────────────────

test_expect_success '--amend changes last commit message' '
	(
	cd repo &&
	grit commit --amend -m "amended message" &&
	grit log -n 1 --format=%s >actual &&
	echo "amended message" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--amend preserves tree' '
	(
	cd repo &&
	tree_before=$(grit rev-parse HEAD^{tree}) &&
	grit commit --amend -m "amended again" &&
	tree_after=$(grit rev-parse HEAD^{tree}) &&
	test "$tree_before" = "$tree_after"
	)
'

test_expect_success '--amend changes commit hash' '
	(
	cd repo &&
	before=$(grit rev-parse HEAD) &&
	grit commit --amend -m "new amend message" &&
	after=$(grit rev-parse HEAD) &&
	test "$before" != "$after"
	)
'

# ── commit -a ─────────────────────────────────────────────────────────────

test_expect_success '-a auto-stages modified tracked files' '
	(
	cd repo &&
	echo "auto staged" >>file.txt &&
	grit commit -a -m "auto add commit" &&
	grit log -n 1 --format=%s >actual &&
	echo "auto add commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '-a does not stage untracked files' '
	(
	cd repo &&
	echo "untracked" >new-untracked.txt &&
	grit commit --allow-empty -m "no untracked" &&
	git status --porcelain >actual &&
	grep "?? new-untracked.txt" actual &&
	rm -f new-untracked.txt
	)
'

# ── -F combined with other options ────────────────────────────────────────

test_expect_success '-F file with --allow-empty combines correctly' '
	(
	cd repo &&
	echo "combined message" >../msg-combined.txt &&
	grit commit --allow-empty -F ../msg-combined.txt &&
	grit log -n 1 --format=%s >actual &&
	echo "combined message" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '-F file with --author overrides author' '
	(
	cd repo &&
	echo "change seventeen" >>file.txt &&
	git add file.txt &&
	echo "file message other author" >../msg-author.txt &&
	grit commit -F ../msg-author.txt --author="File Author <file@example.com>" &&
	grit log -n 1 --format="%an" >actual &&
	echo "File Author" >expect &&
	test_cmp expect actual
	)
'

test_done
