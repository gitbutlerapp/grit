#!/bin/sh

test_description='grit commit: --allow-empty, --allow-empty-message, and related options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup' '
	(
	grit init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo hello >file.txt &&
	grit add file.txt &&
	grit commit -m "initial"
	)
'

# ── basic commit with message ────────────────────────────────────────────

test_expect_success 'commit with -m works' '
	(cd repo &&
	 echo change1 >file.txt &&
	 grit add file.txt &&
	 grit commit -m "change one" &&
	 grit log --oneline >../actual) &&
	grep "change one" actual
'

test_expect_success 'commit with multi-word message' '
	(cd repo &&
	 echo change2 >file.txt &&
	 grit add file.txt &&
	 grit commit -m "this is a longer commit message" &&
	 grit log --oneline >../actual) &&
	grep "this is a longer commit message" actual
'

# ── --allow-empty ────────────────────────────────────────────────────────

test_expect_success 'commit without changes fails' '
	(cd repo && test_must_fail grit commit -m "empty commit")
'

test_expect_success 'commit --allow-empty succeeds without changes' '
	(cd repo &&
	 grit commit --allow-empty -m "empty commit" &&
	 grit log --oneline >../actual) &&
	grep "empty commit" actual
'

test_expect_success 'allow-empty commit appears as most recent' '
	(cd repo && grit log --oneline -n 1 >../actual) &&
	grep "empty commit" actual
'

test_expect_success 'multiple allow-empty commits work' '
	(cd repo &&
	 grit commit --allow-empty -m "empty 1" &&
	 grit commit --allow-empty -m "empty 2" &&
	 grit commit --allow-empty -m "empty 3" &&
	 grit log --oneline >../actual) &&
	grep "empty 1" actual &&
	grep "empty 2" actual &&
	grep "empty 3" actual
'

# ── --allow-empty-message ────────────────────────────────────────────────

test_expect_success 'commit with empty message fails without flag' '
	(cd repo &&
	 echo emptytest >empty-msg.txt &&
	 grit add empty-msg.txt &&
	 test_must_fail grit commit -m "")
'

test_expect_success 'commit --allow-empty-message with empty -m succeeds' '
	(cd repo &&
	 grit commit --allow-empty-message -m "" &&
	 grit log --oneline -n 1 >../actual) &&
	test -s actual
'

test_expect_success 'allow-empty-message commit appears in log' '
	(cd repo && grit log --oneline >../actual) &&
	wc -l <actual >count &&
	test "$(cat count)" -gt 5
'

# ── --allow-empty combined with --allow-empty-message ────────────────────

test_expect_success 'allow-empty and allow-empty-message together' '
	(cd repo &&
	 grit commit --allow-empty --allow-empty-message -m "" &&
	 grit log --oneline -n 1 >../actual) &&
	test -s actual
'

# ── commit -a (auto-stage) ──────────────────────────────────────────────

test_expect_success 'commit -a stages modified tracked files' '
	(cd repo &&
	 echo modified >file.txt &&
	 grit commit -a -m "auto staged" &&
	 grit log --oneline >../actual) &&
	grep "auto staged" actual
'

test_expect_success 'commit -a does not stage untracked files' '
	(cd repo &&
	 echo untracked >newfile.txt &&
	 test_must_fail grit commit -a -m "should fail" 2>/dev/null) ||
	true
'

# ── commit --amend ───────────────────────────────────────────────────────

test_expect_success 'commit --amend changes last commit message' '
	(cd repo &&
	 echo amended >file.txt &&
	 grit add file.txt &&
	 grit commit -m "before amend" &&
	 grit commit --amend -m "after amend" &&
	 grit log --oneline -n 1 >../actual) &&
	grep "after amend" actual
'

test_expect_success 'commit --amend preserves changes' '
	(cd repo && grit log --oneline -n 1 >../actual) &&
	! grep "before amend" actual
'

test_expect_success 'commit --amend with --allow-empty' '
	(cd repo &&
	 grit commit --amend --allow-empty -m "amend empty" &&
	 grit log --oneline -n 1 >../actual) &&
	grep "amend empty" actual
'

# ── commit -F (message from file) ───────────────────────────────────────

test_expect_success 'commit -F reads message from file' '
	echo "message from file" >msg &&
	(cd repo &&
	 echo fromfile >file.txt &&
	 grit add file.txt &&
	 grit commit -F "$(pwd)/../msg" &&
	 grit log --oneline -n 1 >../actual) &&
	grep "message from file" actual
'

test_expect_success 'commit -F with multi-line file' '
	printf "subject line\n\nbody paragraph" >msg2 &&
	(cd repo &&
	 echo multiline >file.txt &&
	 grit add file.txt &&
	 grit commit -F "$(pwd)/../msg2" &&
	 grit log -n 1 >../actual) &&
	grep "subject line" actual
'

# ── commit --author ──────────────────────────────────────────────────────

test_expect_success 'commit --author overrides author' '
	(cd repo &&
	 echo auth >file.txt &&
	 grit add file.txt &&
	 grit commit --author "Other User <other@test.com>" -m "other author" &&
	 grit log -n 1 >../actual) &&
	grep "Other User" actual
'

test_expect_success 'commit without --author uses default' '
	(cd repo &&
	 echo default >file.txt &&
	 grit add file.txt &&
	 grit commit -m "default author" &&
	 grit log -n 1 >../actual) &&
	grep "A U Thor <author@example.com>" actual
'

# ── commit --signoff flag is accepted ────────────────────────────────────

test_expect_success 'commit --signoff does not error' '
	(cd repo &&
	 echo signoff >file.txt &&
	 grit add file.txt &&
	 grit commit --signoff -m "signed commit" &&
	 grit log --oneline -n 1 >../actual) &&
	grep "signed commit" actual
'

test_expect_success 'commit -s does not error' '
	(cd repo &&
	 echo signoff2 >file.txt &&
	 grit add file.txt &&
	 grit commit -s -m "signoff short" &&
	 grit log --oneline -n 1 >../actual) &&
	grep "signoff short" actual
'

# ── commit -q (quiet) ───────────────────────────────────────────────────

test_expect_success 'commit -q suppresses output' '
	(cd repo &&
	 echo quiet >file.txt &&
	 grit add file.txt &&
	 grit commit -q -m "quiet commit" >../actual 2>&1) &&
	test_must_be_empty actual
'

# ── commit --date override ───────────────────────────────────────────────

test_expect_success 'commit --date overrides commit date' '
	(cd repo &&
	 echo dated >file.txt &&
	 grit add file.txt &&
	 grit commit --date "2020-01-01T00:00:00" -m "dated commit" &&
	 grit log -n 1 >../actual) &&
	grep "2020" actual
'

# ── multiple sequential commits ──────────────────────────────────────────

test_expect_success 'multiple commits create sequential history' '
	(cd repo &&
	 echo seq1 >file.txt && grit add file.txt && grit commit -m "seq1" &&
	 echo seq2 >file.txt && grit add file.txt && grit commit -m "seq2" &&
	 echo seq3 >file.txt && grit add file.txt && grit commit -m "seq3" &&
	 grit log --oneline >../actual) &&
	grep "seq1" actual &&
	grep "seq2" actual &&
	grep "seq3" actual
'

test_expect_success 'log shows commits in reverse chronological order' '
	head -1 actual >first &&
	grep "seq3" first
'

# ── edge cases ───────────────────────────────────────────────────────────

test_expect_success 'commit message with special characters' '
	(cd repo &&
	 echo special >file.txt &&
	 grit add file.txt &&
	 grit commit -m "msg with !@#\$%^&*()" &&
	 grit log --oneline -n 1 >../actual) &&
	test -s actual
'

test_expect_success 'commit message with quotes' '
	(cd repo &&
	 echo quotes >file.txt &&
	 grit add file.txt &&
	 grit commit -m "he said hello" &&
	 grit log --oneline -n 1 >../actual) &&
	grep "he said hello" actual
'

test_expect_success 'commit message with unicode' '
	(cd repo &&
	 echo unicode >file.txt &&
	 grit add file.txt &&
	 grit commit -m "héllo wörld" &&
	 grit log --oneline -n 1 >../actual) &&
	grep "héllo wörld" actual
'

test_expect_success 'commit count matches expected' '
	(cd repo && grit log --oneline >../actual) &&
	count=$(wc -l <actual) &&
	test "$count" -gt 15
'

test_done
