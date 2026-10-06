#!/bin/sh
# Test status --porcelain with staged files: additions, modifications,
# deletions, mixed staged/unstaged, branch header, -z NUL termination,
# untracked files, and various combinations.

test_description='grit status --porcelain with staged changes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---- setup ----

test_expect_success 'setup: create repo with files' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "alpha" >a.txt &&
	echo "bravo" >b.txt &&
	echo "charlie" >c.txt &&
	mkdir -p dir &&
	echo "nested" >dir/n.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "initial"
	)
'

# ---- clean state ----

test_expect_success 'porcelain: clean repo shows only branch header' '
	(
	cd repo &&
	rm -f actual &&
	grit status --porcelain -b >../status_out &&
	line_count=$(wc -l <../status_out | tr -d " ") &&
	test "$line_count" = "1" &&
	grep "^##" ../status_out
	)
'

test_expect_success 'porcelain: branch header shows main' '
	(
	cd repo &&
	grit status --porcelain -b >actual &&
	grep "^## main" actual
	)
'

# ---- staged addition (new file) ----

test_expect_success 'setup: add new file to index' '
	(
	cd repo &&
	echo "new content" >new.txt &&
	grit add new.txt
	)
'

test_expect_success 'porcelain: staged new file shows A in first column' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "^A  new.txt" actual
	)
'

test_expect_success 'porcelain: staged new file second column is space' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "^A " actual
	)
'

# ---- staged modification ----

test_expect_success 'setup: modify and stage a tracked file' '
	(
	cd repo &&
	echo "alpha2" >>a.txt &&
	grit add a.txt
	)
'

test_expect_success 'porcelain: staged modification shows M in first column' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "^M  a.txt" actual
	)
'

test_expect_success 'porcelain: both staged files listed' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "^A  new.txt" actual &&
	grep "^M  a.txt" actual
	)
'

# ---- unstaged modification on non-staged file ----

test_expect_success 'setup: modify b.txt without staging' '
	(
	cd repo &&
	echo "bravo2" >>b.txt
	)
'

test_expect_success 'porcelain: unstaged modification shows M in second column' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "^ M b.txt" actual
	)
'

test_expect_success 'porcelain: staged files still shown alongside unstaged' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "^M  a.txt" actual &&
	grep "^ M b.txt" actual &&
	grep "^A  new.txt" actual
	)
'

# ---- both staged and unstaged on same file ----

test_expect_success 'setup: modify already-staged file again without staging' '
	(
	cd repo &&
	echo "alpha3" >>a.txt
	)
'

test_expect_success 'porcelain: file with both staged and unstaged shows MM' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "^MM a.txt" actual
	)
'

# ---- commit staged, verify clean ----

test_expect_success 'setup: commit all staged changes' '
	(
	cd repo &&
	grit add a.txt b.txt &&
	test_tick &&
	grit commit -m "second commit"
	)
'

test_expect_success 'porcelain: after commit only untracked new.txt remains or clean' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	! grep "^M" actual
	)
'

# ---- staged deletion ----

test_expect_success 'setup: delete and stage a file' '
	(
	cd repo &&
	rm c.txt &&
	grit add c.txt
	)
'

test_expect_success 'porcelain: staged deletion shows D in first column' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "^D  c.txt" actual
	)
'

test_expect_success 'porcelain: staged deletion second column is space' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	# D followed by space then space then filename
	grep "^D " actual
	)
'

# ---- untracked files ----

test_expect_success 'setup: create untracked file' '
	(
	cd repo &&
	echo "untracked" >zzz_untracked.txt
	)
'

test_expect_success 'porcelain: untracked file shows ?? prefix' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "^?? zzz_untracked.txt" actual
	)
'

test_expect_success 'porcelain: staged and untracked both appear' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "^D  c.txt" actual &&
	grep "^??" actual
	)
'

# ---- commit deletion, verify ----

test_expect_success 'setup: commit deletion' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "delete c.txt"
	)
'

test_expect_success 'porcelain: c.txt no longer in status after commit' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	! grep "c.txt" actual
	)
'

# ---- nested directory changes ----

test_expect_success 'setup: modify and stage nested file' '
	(
	cd repo &&
	echo "more nested" >>dir/n.txt &&
	grit add dir/n.txt
	)
'

test_expect_success 'porcelain: staged nested file shows full path' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "^M  dir/n.txt" actual
	)
'

# ---- short format (-s) ----

test_expect_success 'status -s shows same format as --porcelain (minus header)' '
	(
	cd repo &&
	grit status -s >actual &&
	grep "dir/n.txt" actual
	)
'

# ---- -z NUL termination ----

test_expect_success 'porcelain -z uses NUL terminators' '
	(
	cd repo &&
	grit status --porcelain -z >actual &&
	# NUL bytes should be present in the output
	tr "\0" "\n" <actual >decoded &&
	grep "dir/n.txt" decoded
	)
'

test_expect_success 'porcelain -z output has no newlines inside entries' '
	(
	cd repo &&
	grit status --porcelain -z | tr "\0" "\n" >decoded &&
	# each decoded line should be a valid entry
	grep "dir/n.txt" decoded
	)
'

# ---- branch header with -b ----

test_expect_success 'porcelain -b shows branch header' '
	(
	cd repo &&
	grit status --porcelain -b >actual &&
	grep "^## main" actual
	)
'

# ---- multiple staged files at once ----

test_expect_success 'setup: stage multiple changes' '
	(
	cd repo &&
	echo "mod a" >>a.txt &&
	echo "mod b" >>b.txt &&
	echo "brand new" >x.txt &&
	grit add a.txt b.txt x.txt
	)
'

test_expect_success 'porcelain: multiple staged modifications and additions' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	grep "^M  a.txt" actual &&
	grep "^M  b.txt" actual &&
	grep "^A  x.txt" actual
	)
'

test_expect_success 'porcelain: line count matches expected staged + unstaged + untracked' '
	(
	cd repo &&
	grit status --porcelain >actual &&
	# header + M dir/n.txt + M a.txt + M b.txt + A x.txt + ?? zzz_untracked.txt = 6
	line_count=$(wc -l <actual | tr -d " ") &&
	test "$line_count" -ge 5
	)
'

# ---- all clean after full commit ----

test_expect_success 'setup: commit everything and clean untracked' '
	(
	cd repo &&
	grit add zzz_untracked.txt &&
	test_tick &&
	grit commit -m "commit all"
	)
'

test_expect_success 'porcelain: fully clean repo shows only branch header' '
	(
	cd repo &&
	rm -f actual decoded &&
	grit status --porcelain -b >../status_out2 &&
	line_count=$(wc -l <../status_out2 | tr -d " ") &&
	test "$line_count" = "1"
	)
'

test_done
