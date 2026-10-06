#!/bin/sh
# Test grit commit --date, --author, -m, -F/--file, -a/--all,
# --amend, --allow-empty, --allow-empty-message, -q/--quiet,
# and various commit options.

test_description='grit commit --date and --author options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: init repo' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "initial" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "initial commit"
	)
'

# --- commit --author ---

test_expect_success 'commit --author overrides author name' '
	(
	cd repo &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "author test" >author.txt &&
	grit add author.txt &&
	test_tick &&
	grit commit --author "Custom Author <custom@example.com>" -m "custom author" &&
	grit log -n 1 --format="%an" >actual &&
	echo "Custom Author" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit --author sets author email' '
	(
	cd repo &&
	grit log -n 1 --format="%ae" >actual &&
	echo "custom@example.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit --author does not affect committer' '
	(
	cd repo &&
	grit log -n 1 --format="%cn" >actual &&
	echo "Test User" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit --author committer email unchanged' '
	(
	cd repo &&
	grit log -n 1 --format="%ce" >actual &&
	echo "test@example.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit --author with different identity' '
	(
	cd repo &&
	echo "author2" >author2.txt &&
	grit add author2.txt &&
	test_tick &&
	grit commit --author "Another Dev <dev@other.org>" -m "another author" &&
	grit log -n 1 --format="%an" >actual &&
	echo "Another Dev" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit --author email verified' '
	(
	cd repo &&
	grit log -n 1 --format="%ae" >actual &&
	echo "dev@other.org" >expect &&
	test_cmp expect actual
	)
'

# --- commit --date ---

test_expect_success 'commit --date overrides author date (ISO)' '
	(
	cd repo &&
	echo "dated" >dated.txt &&
	grit add dated.txt &&
	grit commit --date "2020-01-15T10:30:00+00:00" -m "dated commit" &&
	sha=$(grit rev-parse HEAD) &&
	grit cat-file -p "$sha" >raw &&
	grep "author.*2020-01-15" raw ||
	grep "author.*1579084200" raw
	)
'

test_expect_success 'commit --date with unix timestamp' '
	(
	cd repo &&
	echo "unix date" >unix-dated.txt &&
	grit add unix-dated.txt &&
	grit commit --date "1600000000 +0000" -m "unix dated" &&
	sha=$(grit rev-parse HEAD) &&
	grit cat-file -p "$sha" >raw &&
	grep "author.*1600000000" raw
	)
'

test_expect_success 'commit --date with timezone offset' '
	(
	cd repo &&
	echo "tz" >tz.txt &&
	grit add tz.txt &&
	grit commit --date "1600000000 +0530" -m "tz commit" &&
	sha=$(grit rev-parse HEAD) &&
	grit cat-file -p "$sha" >raw &&
	grep "author.*+0530" raw
	)
'

test_expect_success 'commit --date does not affect committer date' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD) &&
	grit cat-file -p "$sha" >raw &&
	# committer line should NOT have 1600000000
	author_line=$(grep "^author" raw) &&
	committer_line=$(grep "^committer" raw) &&
	echo "$author_line" | grep "1600000000" &&
	! echo "$committer_line" | grep "1600000000"
	)
'

# --- commit --author and --date combined ---

test_expect_success 'commit --author --date both set' '
	(
	cd repo &&
	echo "both" >both.txt &&
	grit add both.txt &&
	grit commit --author "Dual Override <dual@test.com>" --date "1500000000 +0000" -m "dual override" &&
	sha=$(grit rev-parse HEAD) &&
	grit cat-file -p "$sha" >raw &&
	grep "author Dual Override <dual@test.com>" raw
	)
'

# --- commit -F / --file ---

test_expect_success 'commit -F reads message from file' '
	(
	cd repo &&
	echo "File commit message" >commit-msg.txt &&
	echo "file-content" >f-content.txt &&
	grit add f-content.txt &&
	test_tick &&
	grit commit -F commit-msg.txt &&
	grit log -n 1 --format="%s" >actual &&
	echo "File commit message" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit --file reads message from file' '
	(
	cd repo &&
	echo "Long form file message" >commit-msg2.txt &&
	echo "more content" >f-content2.txt &&
	grit add f-content2.txt &&
	test_tick &&
	grit commit --file commit-msg2.txt &&
	grit log -n 1 --format="%s" >actual &&
	echo "Long form file message" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit -F with multi-line message' '
	(
	cd repo &&
	printf "Subject line\n\nBody paragraph.\n" >multi-msg.txt &&
	echo "multi" >multi-f.txt &&
	grit add multi-f.txt &&
	test_tick &&
	grit commit -F multi-msg.txt &&
	grit log -n 1 --format="%s" >actual &&
	echo "Subject line" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit -F body is preserved' '
	(
	cd repo &&
	grit log -n 1 --format="%b" >body &&
	grep "Body paragraph" body
	)
'

# --- commit -a / --all ---

test_expect_success 'commit -a stages modified tracked files' '
	(
	cd repo &&
	echo "modified" >file.txt &&
	test_tick &&
	grit commit -a -m "auto stage" &&
	grit log -n 1 --format="%s" >actual &&
	echo "auto stage" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit --all stages modifications' '
	(
	cd repo &&
	echo "modified again" >file.txt &&
	test_tick &&
	grit commit --all -m "all stage" &&
	grit log -n 1 --format="%s" >actual &&
	echo "all stage" >expect &&
	test_cmp expect actual
	)
'

# --- commit --amend ---

test_expect_success 'commit --amend changes last commit message' '
	(
	cd repo &&
	test_tick &&
	grit commit --amend -m "amended message" &&
	grit log -n 1 --format="%s" >actual &&
	echo "amended message" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit --amend does not add extra commit' '
	(
	cd repo &&
	grit log --oneline >before &&
	count_before=$(wc -l <before | tr -d " ") &&
	test_tick &&
	grit commit --amend -m "amended again" &&
	grit log --oneline >after &&
	count_after=$(wc -l <after | tr -d " ") &&
	test "$count_before" = "$count_after"
	)
'

test_expect_success 'commit --amend --author changes author' '
	(
	cd repo &&
	test_tick &&
	grit commit --amend --author "Amend Author <amend@test.com>" -m "amend author" &&
	grit log -n 1 --format="%an" >actual &&
	echo "Amend Author" >expect &&
	test_cmp expect actual
	)
'

# --- commit --allow-empty ---

test_expect_success 'commit --allow-empty creates empty commit' '
	(
	cd repo &&
	test_tick &&
	grit commit --allow-empty -m "empty commit" &&
	grit log -n 1 --format="%s" >actual &&
	echo "empty commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit without --allow-empty fails with no changes' '
	(
	cd repo &&
	test_must_fail grit commit -m "should fail"
	)
'

# --- commit --allow-empty-message ---

test_expect_success 'commit --allow-empty-message allows blank msg' '
	(
	cd repo &&
	echo "empty msg" >empty-msg.txt &&
	grit add empty-msg.txt &&
	test_tick &&
	grit commit --allow-empty-message -m "" &&
	grit log -n 1 --format="%s" >actual &&
	echo "" >expect &&
	test_cmp expect actual
	)
'

# --- commit -q / --quiet ---

test_expect_success 'commit -q suppresses output' '
	(
	cd repo &&
	echo "quiet" >quiet.txt &&
	grit add quiet.txt &&
	test_tick &&
	grit commit -q -m "quiet commit" >actual 2>&1 &&
	test_line_count = 0 actual
	)
'

test_expect_success 'commit --quiet suppresses output' '
	(
	cd repo &&
	echo "quiet2" >quiet2.txt &&
	grit add quiet2.txt &&
	test_tick &&
	grit commit --quiet -m "quiet commit 2" >actual 2>&1 &&
	test_line_count = 0 actual
	)
'

# --- commit message with special chars ---

test_expect_success 'commit with colon in message' '
	(
	cd repo &&
	echo "colon" >colon.txt &&
	grit add colon.txt &&
	test_tick &&
	grit commit -m "feat: add new feature" &&
	grit log -n 1 --format="%s" >actual &&
	echo "feat: add new feature" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit with parentheses in message' '
	(
	cd repo &&
	echo "parens" >parens.txt &&
	grit add parens.txt &&
	test_tick &&
	grit commit -m "fix(core): resolve issue" &&
	grit log -n 1 --format="%s" >actual &&
	echo "fix(core): resolve issue" >expect &&
	test_cmp expect actual
	)
'

# --- commit -m with long message ---

test_expect_success 'commit with long message' '
	(
	cd repo &&
	echo "long" >long.txt &&
	grit add long.txt &&
	test_tick &&
	grit commit -m "This is a somewhat longer commit message that describes the change in detail" &&
	grit log -n 1 --format="%s" >actual &&
	grep "longer commit message" actual
	)
'

test_expect_success 'commit -F with absolute path' '
	(
	cd repo &&
	echo "Absolute path message" >"$(pwd)/abs-msg.txt" &&
	echo "abs" >abs.txt &&
	grit add abs.txt &&
	test_tick &&
	grit commit -F "$(pwd)/abs-msg.txt" &&
	grit log -n 1 --format="%s" >actual &&
	echo "Absolute path message" >expect &&
	test_cmp expect actual
	)
'

# --- verify commit count ---

test_expect_success 'repo has expected number of commits' '
	(
	cd repo &&
	grit log --oneline >all &&
	count=$(wc -l <all | tr -d " ") &&
	test "$count" -gt 15
	)
'

test_done
