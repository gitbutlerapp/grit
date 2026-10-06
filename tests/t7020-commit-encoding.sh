#!/bin/sh
# Tests for commit encoding: i18n.commitEncoding config, commit-tree --encoding,
# and handling of commits with encoding headers.

test_description='commit encoding (i18n.commitEncoding, commit-tree --encoding)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "initial" >file.txt &&
	git add file.txt &&
	git commit -m "initial commit"
	)
'

# ── i18n.commitEncoding config ──────────────────────────────────────────────

test_expect_success 'set i18n.commitEncoding to UTF-8' '
	(
	cd repo &&
	git config i18n.commitEncoding UTF-8 &&
	val=$(git config --get i18n.commitEncoding) &&
	test "$val" = "UTF-8"
	)
'

test_expect_success 'set i18n.commitEncoding to ISO-8859-1' '
	(
	cd repo &&
	git config i18n.commitEncoding ISO-8859-1 &&
	val=$(git config --get i18n.commitEncoding) &&
	test "$val" = "ISO-8859-1"
	)
'

test_expect_success 'i18n.commitEncoding is case-insensitive in config key' '
	(
	cd repo &&
	git config i18n.commitEncoding EUC-JP &&
	val=$(git config --get I18N.COMMITENCODING) &&
	test "$val" = "EUC-JP"
	)
'

test_expect_success 'unset i18n.commitEncoding' '
	(
	cd repo &&
	git config --unset i18n.commitEncoding &&
	! git config --get i18n.commitEncoding
	)
'

# ── commit-tree --encoding writes encoding header ──────────────────────────

test_expect_success 'commit-tree --encoding ISO-8859-1 adds encoding header' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	oid=$(echo "latin1 message" | \
		GIT_AUTHOR_NAME="Author" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="Committer" GIT_COMMITTER_EMAIL="c@e.com" \
		git commit-tree "$tree" -p "$parent" --encoding ISO-8859-1) &&
	git cat-file commit "$oid" >raw &&
	grep "^encoding ISO-8859-1$" raw
	)
'

test_expect_success 'commit-tree --encoding UTF-8 adds encoding header' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	oid=$(echo "utf8 message" | \
		GIT_AUTHOR_NAME="Author" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="Committer" GIT_COMMITTER_EMAIL="c@e.com" \
		git commit-tree "$tree" -p "$parent" --encoding UTF-8) &&
	git cat-file commit "$oid" >raw &&
	grep "^encoding UTF-8$" raw
	)
'

test_expect_success 'commit-tree --encoding EUC-JP adds encoding header' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	oid=$(echo "euc-jp message" | \
		GIT_AUTHOR_NAME="Author" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="Committer" GIT_COMMITTER_EMAIL="c@e.com" \
		git commit-tree "$tree" -p "$parent" --encoding EUC-JP) &&
	git cat-file commit "$oid" >raw &&
	grep "^encoding EUC-JP$" raw
	)
'

test_expect_success 'commit-tree without --encoding has no encoding header' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	oid=$(echo "no encoding" | \
		GIT_AUTHOR_NAME="Author" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="Committer" GIT_COMMITTER_EMAIL="c@e.com" \
		git commit-tree "$tree" -p "$parent") &&
	git cat-file commit "$oid" >raw &&
	! grep "^encoding" raw
	)
'

# ── commit-tree preserves message with encoding header ──────────────────────

test_expect_success 'commit message is preserved with encoding header' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	oid=$(echo "preserved message content" | \
		GIT_AUTHOR_NAME="Author" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="Committer" GIT_COMMITTER_EMAIL="c@e.com" \
		git commit-tree "$tree" -p "$parent" --encoding ISO-8859-1) &&
	git cat-file commit "$oid" >raw &&
	grep "preserved message content" raw
	)
'

test_expect_success 'cat-file -t on encoded commit returns commit' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	oid=$(echo "type check" | \
		GIT_AUTHOR_NAME="A" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="C" GIT_COMMITTER_EMAIL="c@e.com" \
		git commit-tree "$tree" -p "$parent" --encoding ISO-8859-1) &&
	type=$(git cat-file -t "$oid") &&
	test "$type" = "commit"
	)
'

test_expect_success 'cat-file -s on encoded commit returns nonzero size' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	oid=$(echo "size check" | \
		GIT_AUTHOR_NAME="A" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="C" GIT_COMMITTER_EMAIL="c@e.com" \
		git commit-tree "$tree" -p "$parent" --encoding ISO-8859-1) &&
	size=$(git cat-file -s "$oid") &&
	test "$size" -gt 0
	)
'

# ── log reads encoded commits ──────────────────────────────────────────────

test_expect_success 'log shows subject of encoded commit' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	oid=$(echo "log subject test" | \
		GIT_AUTHOR_NAME="A" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="C" GIT_COMMITTER_EMAIL="c@e.com" \
		git commit-tree "$tree" -p "$parent" --encoding ISO-8859-1) &&
	git update-ref HEAD "$oid" &&
	git log -n1 --format="%s" >actual &&
	echo "log subject test" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log shows author of encoded commit' '
	(
	cd repo &&
	git log -n1 --format="%an <%ae>" >actual &&
	echo "A <a@e.com>" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --oneline works on encoded commit' '
	(
	cd repo &&
	git log --oneline -n1 >actual &&
	grep "log subject test" actual
	)
'

# ── Multiple encoding values ────────────────────────────────────────────────

test_expect_success 'encoding header field is exact string' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	oid=$(echo "exact encoding" | \
		GIT_AUTHOR_NAME="A" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="C" GIT_COMMITTER_EMAIL="c@e.com" \
		git commit-tree "$tree" -p "$parent" --encoding Shift_JIS) &&
	git cat-file commit "$oid" >raw &&
	grep "^encoding Shift_JIS$" raw
	)
'

# ── Commit with -m from porcelain ───────────────────────────────────────────

test_expect_success 'porcelain commit does not add encoding header by default' '
	(
	cd repo &&
	git config --unset i18n.commitEncoding 2>/dev/null || true &&
	echo "porcelain" >porcelain.txt &&
	git add porcelain.txt &&
	git commit -m "porcelain commit" &&
	git cat-file commit HEAD >raw &&
	! grep "^encoding" raw
	)
'

test_expect_success 'porcelain commit with UTF-8 message content' '
	(
	cd repo &&
	echo "utf8 content" >utf8file.txt &&
	git add utf8file.txt &&
	git commit -m "message with ASCII only" &&
	git log -n1 --format="%s" >actual &&
	echo "message with ASCII only" >expect &&
	test_cmp expect actual
	)
'

# ── commit-tree with -m flag ────────────────────────────────────────────────

test_expect_success 'commit-tree -m with --encoding works' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(GIT_AUTHOR_NAME="A" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="C" GIT_COMMITTER_EMAIL="c@e.com" \
		git commit-tree "$tree" -m "dash-m message" --encoding KOI8-R) &&
	git cat-file commit "$oid" >raw &&
	grep "^encoding KOI8-R$" raw &&
	grep "dash-m message" raw
	)
'

# ── Encoding header position in raw commit ──────────────────────────────────

test_expect_success 'encoding header appears after committer line' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(echo "position test" | \
		GIT_AUTHOR_NAME="A" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="C" GIT_COMMITTER_EMAIL="c@e.com" \
		git commit-tree "$tree" --encoding ISO-8859-15) &&
	git cat-file commit "$oid" >raw &&
	committer_line=$(grep -n "^committer" raw | head -1 | cut -d: -f1) &&
	encoding_line=$(grep -n "^encoding" raw | head -1 | cut -d: -f1) &&
	test "$encoding_line" -gt "$committer_line"
	)
'

test_expect_success 'encoding header appears before blank line (message separator)' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(echo "before blank" | \
		GIT_AUTHOR_NAME="A" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="C" GIT_COMMITTER_EMAIL="c@e.com" \
		git commit-tree "$tree" --encoding ISO-8859-1) &&
	git cat-file commit "$oid" >raw &&
	encoding_line=$(grep -n "^encoding" raw | head -1 | cut -d: -f1) &&
	blank_line=$(grep -n "^$" raw | head -1 | cut -d: -f1) &&
	test "$encoding_line" -lt "$blank_line"
	)
'

# ── Multiline commit message with encoding ──────────────────────────────────

test_expect_success 'multiline message preserved with encoding header' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	oid=$(printf "line one\n\nline three\n" | \
		GIT_AUTHOR_NAME="A" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="C" GIT_COMMITTER_EMAIL="c@e.com" \
		git commit-tree "$tree" --encoding ISO-8859-1) &&
	git cat-file commit "$oid" >raw &&
	grep "line one" raw &&
	grep "line three" raw
	)
'

test_expect_success 'rev-parse works on encoded commit' '
	(
	cd repo &&
	oid=$(git rev-parse HEAD) &&
	test -n "$oid" &&
	test $(echo "$oid" | wc -c) -gt 40
	)
'

test_expect_success 'log --format=%H on encoded commit returns full hash' '
	(
	cd repo &&
	hash=$(git log -n1 --format="%H") &&
	test $(echo "$hash" | wc -c) -eq 41
	)
'

test_expect_success 'log --format=%T on encoded commit returns tree hash' '
	(
	cd repo &&
	tree_from_log=$(git log -n1 --format="%T") &&
	tree_from_parse=$(git rev-parse HEAD^{tree}) &&
	test "$tree_from_log" = "$tree_from_parse"
	)
'

test_expect_success 'show on encoded commit displays message' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	parent=$(git rev-parse HEAD) &&
	oid=$(echo "show test msg" | \
		GIT_AUTHOR_NAME="A" GIT_AUTHOR_EMAIL="a@e.com" \
		GIT_COMMITTER_NAME="C" GIT_COMMITTER_EMAIL="c@e.com" \
		git commit-tree "$tree" -p "$parent" --encoding ISO-8859-1) &&
	git update-ref HEAD "$oid" &&
	git show -q HEAD >actual &&
	grep "show test msg" actual
	)
'

test_done
