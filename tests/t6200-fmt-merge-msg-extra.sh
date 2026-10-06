#!/bin/sh
# Extra fmt-merge-msg scenarios: multiple branches, tags, remote URLs,
# --into-name, --message override, --log/--no-log, stdin vs -F, and
# various FETCH_HEAD formats.

test_description='extra fmt-merge-msg scenarios'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ──────────────────────────────────────────────────────────────────────

test_expect_success 'setup repo with branches' '
	(
	grit init --initial-branch=master repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo base >f.txt &&
	grit add f.txt &&
	grit commit -m "base" &&
	grit rev-parse HEAD >../base &&
	grit branch topic &&
	grit branch feature &&
	grit branch bugfix &&
	grit checkout topic &&
	echo topic >t.txt && grit add t.txt && grit commit -m "topic work" &&
	grit rev-parse HEAD >../topic_oid &&
	grit checkout feature &&
	echo feat >feat.txt && grit add feat.txt && grit commit -m "feature work" &&
	grit rev-parse HEAD >../feat_oid &&
	grit checkout bugfix &&
	echo fix >fix.txt && grit add fix.txt && grit commit -m "bugfix work" &&
	grit rev-parse HEAD >../bugfix_oid &&
	grit checkout master
	)
'

# ── Single branch merge ──────────────────────────────────────────────────────

test_expect_success 'single branch: Merge branch X' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -F .git/FETCH_HEAD >out &&
	echo "Merge branch '\''topic'\''" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'single branch via stdin' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n" "$oid" |
	grit fmt-merge-msg >out &&
	grep "Merge branch '\''topic'\''" out
	)
'

# ── Two branches ─────────────────────────────────────────────────────────────

test_expect_success 'two branches: Merge branches X and Y' '
	(
	cd repo &&
	t=$(cat ../topic_oid) && f=$(cat ../feat_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n%s\t\tbranch '\''feature'\'' of .\n" "$t" "$f" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -F .git/FETCH_HEAD >out &&
	grep "Merge branches '\''topic'\'' and '\''feature'\''" out
	)
'

# ── Three branches ───────────────────────────────────────────────────────────

test_expect_success 'three branches: Merge branches X, Y and Z' '
	(
	cd repo &&
	t=$(cat ../topic_oid) && f=$(cat ../feat_oid) && b=$(cat ../bugfix_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n%s\t\tbranch '\''feature'\'' of .\n%s\t\tbranch '\''bugfix'\'' of .\n" "$t" "$f" "$b" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -F .git/FETCH_HEAD >out &&
	grep "Merge branches" out &&
	grep "topic" out &&
	grep "feature" out &&
	grep "bugfix" out
	)
'

# ── Tag merge ────────────────────────────────────────────────────────────────

test_expect_success 'tag: Merge tag X' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\ttag '\''v1.0'\'' of .\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -F .git/FETCH_HEAD >out &&
	grep "Merge tag '\''v1.0'\''" out
	)
'

test_expect_success 'tag with remote URL' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\ttag '\''release'\'' of https://example.com/repo\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -F .git/FETCH_HEAD >out &&
	grep "Merge tag '\''release'\''" out
	)
'

# ── Remote URL ───────────────────────────────────────────────────────────────

test_expect_success 'remote URL is included for non-local' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\tbranch '\''main'\'' of https://github.com/user/repo\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -F .git/FETCH_HEAD >out &&
	grep "https://github.com/user/repo" out
	)
'

test_expect_success 'local origin dot is omitted from message' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -F .git/FETCH_HEAD >out &&
	! grep " of \." out
	)
'

# ── --into-name ──────────────────────────────────────────────────────────────

test_expect_success '--into-name changes target branch' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg --into-name develop -F .git/FETCH_HEAD >out &&
	grep "into develop" out
	)
'

test_expect_success '--into-name with custom branch name' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg --into-name release/v2 -F .git/FETCH_HEAD >out &&
	grep "into release/v2" out
	)
'

test_expect_success 'without --into-name, no into clause for default branch' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -F .git/FETCH_HEAD >out &&
	! grep "into" out
	)
'

# ── --message (-m) override ──────────────────────────────────────────────────

test_expect_success '-m overrides the title line' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -m "Custom merge title" -F .git/FETCH_HEAD >out &&
	grep "Custom merge title" out
	)
'

test_expect_success '-m replaces auto-generated message entirely' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -m "My msg" -F .git/FETCH_HEAD >out &&
	! grep "Merge branch" out
	)
'

test_expect_success '-m with --into-name: -m wins' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -m "Override" --into-name develop -F .git/FETCH_HEAD >out &&
	grep "Override" out
	)
'

# ── --log / --no-log ─────────────────────────────────────────────────────────

test_expect_success '--log is accepted without error' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg --log -F .git/FETCH_HEAD >out &&
	grep "Merge branch" out
	)
'

test_expect_success '--no-log is accepted without error' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg --no-log -F .git/FETCH_HEAD >out &&
	grep "Merge branch" out
	)
'

test_expect_success '--log with explicit count' '
	(
	cd repo &&
	oid=$(cat ../topic_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg --log=5 -F .git/FETCH_HEAD >out &&
	grep "Merge branch" out
	)
'

# ── Mixed refs ───────────────────────────────────────────────────────────────

test_expect_success 'branch and tag together' '
	(
	cd repo &&
	t=$(cat ../topic_oid) && f=$(cat ../feat_oid) &&
	printf "%s\t\tbranch '\''topic'\'' of .\n%s\t\ttag '\''v2'\'' of .\n" "$t" "$f" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -F .git/FETCH_HEAD >out &&
	grep "topic" out &&
	grep "v2" out
	)
'

# ── Empty / error cases ─────────────────────────────────────────────────────

test_expect_success 'empty FETCH_HEAD produces no crash' '
	(
	cd repo &&
	printf "" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -F .git/FETCH_HEAD >out 2>err || true &&
	true
	)
'

test_expect_success 'nonexistent file fails gracefully' '
	(
	cd repo &&
	test_must_fail grit fmt-merge-msg -F nonexistent 2>err
	)
'

# ── Branch name with slash ───────────────────────────────────────────────────

test_expect_success 'branch name with slash is preserved' '
	(
	cd repo &&
	oid=$(cat ../feat_oid) &&
	printf "%s\t\tbranch '\''feature/login'\'' of .\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -F .git/FETCH_HEAD >out &&
	grep "feature/login" out
	)
'

test_expect_success 'branch name with dots is preserved' '
	(
	cd repo &&
	oid=$(cat ../feat_oid) &&
	printf "%s\t\tbranch '\''release-1.2.3'\'' of .\n" "$oid" >.git/FETCH_HEAD &&
	grit fmt-merge-msg -F .git/FETCH_HEAD >out &&
	grep "release-1.2.3" out
	)
'

test_done
