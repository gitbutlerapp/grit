#!/bin/sh
# Test grit commit: --amend, --allow-empty, --allow-empty-message,
# -a/--all, --author, --date, --signoff, -F, and message handling.

test_description='grit commit amend, allow-empty, and flags'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "initial" >file.txt &&
	grit add file.txt &&
	grit commit -m "initial commit"
	)
'

###########################################################################
# Section 2: Basic commit
###########################################################################

test_expect_success 'commit with -m creates commit' '
	(
	cd repo &&
	echo "change1" >>file.txt &&
	grit add file.txt &&
	grit commit -m "second commit" &&
	grit log --oneline >out &&
	grep "second commit" out
	)
'

test_expect_success 'commit without staged changes fails' '
	(
	cd repo &&
	test_must_fail grit commit -m "nothing staged"
	)
'

test_expect_success 'commit message appears in log' '
	(
	cd repo &&
	echo "more" >>file.txt &&
	grit add file.txt &&
	grit commit -m "specific message here" &&
	grit log --oneline >out &&
	grep "specific message here" out
	)
'

###########################################################################
# Section 3: --amend
###########################################################################

test_expect_success '--amend changes last commit message' '
	(
	cd repo &&
	grit commit --amend -m "amended message" &&
	grit log -n 1 --oneline >out &&
	grep "amended message" out
	)
'

test_expect_success '--amend does not create new commit' '
	(
	cd repo &&
	grit log --oneline >before &&
	count_before=$(wc -l <before) &&
	grit commit --amend -m "amend again" &&
	grit log --oneline >after &&
	count_after=$(wc -l <after) &&
	test "$count_before" = "$count_after"
	)
'

test_expect_success '--amend with staged changes includes them' '
	(
	cd repo &&
	echo "amend-content" >new-file.txt &&
	grit add new-file.txt &&
	grit commit --amend -m "amend with new file" &&
	grit log -n 1 --oneline >out &&
	grep "amend with new file" out &&
	grit ls-files >files &&
	grep "new-file.txt" files
	)
'

test_expect_success '--amend changes the commit hash' '
	(
	cd repo &&
	grit rev-parse HEAD >before &&
	grit commit --amend -m "changed hash" &&
	grit rev-parse HEAD >after &&
	! test_cmp before after
	)
'

test_expect_success '--amend preserves parent commit' '
	(
	cd repo &&
	grit rev-parse HEAD~1 >parent_before &&
	grit commit --amend -m "still same parent" &&
	grit rev-parse HEAD~1 >parent_after &&
	test_cmp parent_before parent_after
	)
'

test_expect_success '--amend result verifiable via log' '
	(
	cd repo &&
	grit commit --amend -m "verified amend" &&
	grit log -n 1 --oneline >out &&
	grep "verified amend" out
	)
'

###########################################################################
# Section 4: --allow-empty
###########################################################################

test_expect_success '--allow-empty creates commit with no changes' '
	(
	cd repo &&
	grit log --oneline >before &&
	grit commit --allow-empty -m "empty commit" &&
	grit log --oneline >after &&
	count_before=$(wc -l <before) &&
	count_after=$(wc -l <after) &&
	test "$count_after" -gt "$count_before"
	)
'

test_expect_success '--allow-empty commit appears in log' '
	(
	cd repo &&
	grit log -n 1 --oneline >out &&
	grep "empty commit" out
	)
'

test_expect_success '--allow-empty tree matches parent tree' '
	(
	cd repo &&
	grit rev-parse HEAD^{tree} >tree_head &&
	grit rev-parse HEAD~1^{tree} >tree_parent &&
	test_cmp tree_parent tree_head
	)
'

test_expect_success 'multiple --allow-empty commits' '
	(
	cd repo &&
	grit commit --allow-empty -m "empty 1" &&
	grit commit --allow-empty -m "empty 2" &&
	grit commit --allow-empty -m "empty 3" &&
	grit log --oneline >out &&
	grep "empty 1" out &&
	grep "empty 2" out &&
	grep "empty 3" out
	)
'

###########################################################################
# Section 5: --allow-empty-message
###########################################################################

test_expect_success '--allow-empty-message with empty string' '
	(
	cd repo &&
	echo "emptymsg" >emptymsg.txt &&
	grit add emptymsg.txt &&
	grit commit --allow-empty-message -m ""
	)
'

test_expect_success 'commit without message flag fails' '
	(
	cd repo &&
	echo "nomsg" >nomsg.txt &&
	grit add nomsg.txt &&
	test_must_fail grit commit
	)
'

###########################################################################
# Section 6: -a / --all flag
###########################################################################

test_expect_success '-a commits modified tracked files' '
	(
	cd repo &&
	echo "tracked-change" >>file.txt &&
	grit commit -a -m "commit all modified" &&
	grit log -n 1 --oneline >out &&
	grep "commit all modified" out
	)
'

test_expect_success '-a does not add untracked files' '
	(
	cd repo &&
	echo "untracked" >untracked.txt &&
	echo "more" >>file.txt &&
	grit commit -a -m "only tracked" &&
	grit ls-files >out &&
	! grep "untracked.txt" out
	)
'

test_expect_success '--all commits modified tracked files' '
	(
	cd repo &&
	echo "another" >>file.txt &&
	grit commit --all -m "all flag" &&
	grit log -n 1 --oneline >out &&
	grep "all flag" out
	)
'

###########################################################################
# Section 7: --author override
###########################################################################

test_expect_success '--author overrides commit author' '
	(
	cd repo &&
	echo "author test" >>file.txt &&
	grit add file.txt &&
	grit commit --author="Other Person <other@test.com>" -m "custom author" &&
	grit log -n 1 >out &&
	grep "Other Person" out
	)
'

test_expect_success '--author includes email' '
	(
	cd repo &&
	grit log -n 1 >out &&
	grep "other@test.com" out
	)
'

###########################################################################
# Section 8: Commit with various content
###########################################################################

test_expect_success 'commit binary-like filename' '
	(
	cd repo &&
	echo "data" >"file with spaces.txt" &&
	grit add "file with spaces.txt" &&
	grit commit -m "add spaced file" &&
	grit ls-files >out &&
	grep "file with spaces.txt" out
	)
'

test_expect_success 'commit removes deleted file with -a' '
	(
	cd repo &&
	echo "temp" >willdelete.txt &&
	grit add willdelete.txt &&
	grit commit -m "add willdelete" &&
	rm willdelete.txt &&
	grit commit -a -m "remove willdelete" &&
	grit ls-files >out &&
	! grep "willdelete" out
	)
'

###########################################################################
# Section 9: -F / --file
###########################################################################

test_expect_success '-F reads message from file' '
	(
	cd repo &&
	echo "Message from file" >msg.txt &&
	echo "more data" >>file.txt &&
	grit add file.txt &&
	grit commit -F msg.txt &&
	grit log -n 1 --oneline >out &&
	grep "Message from file" out
	)
'

test_expect_success '--file reads message from file' '
	(
	cd repo &&
	echo "File message 2" >msg2.txt &&
	echo "even more" >>file.txt &&
	grit add file.txt &&
	grit commit --file msg2.txt &&
	grit log -n 1 --oneline >out &&
	grep "File message 2" out
	)
'

###########################################################################
# Section 10: Quiet mode and edge cases
###########################################################################

test_expect_success '--quiet suppresses output' '
	(
	cd repo &&
	echo "quiet" >>file.txt &&
	grit add file.txt &&
	grit commit -q -m "quiet commit" >out 2>&1 &&
	test_must_be_empty out
	)
'

test_expect_success 'commit preserves file modes' '
	(
	cd repo &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	grit add script.sh &&
	grit commit -m "add script" &&
	grit ls-tree HEAD -- script.sh >out &&
	grep "100755" out
	)
'

test_expect_success 'amend with --allow-empty' '
	(
	cd repo &&
	grit commit --allow-empty -m "will amend" &&
	grit commit --amend --allow-empty -m "amended empty" &&
	grit log -n 1 --oneline >out &&
	grep "amended empty" out
	)
'

test_expect_success 'multiple amends keep single commit' '
	(
	cd repo &&
	grit log --oneline >before &&
	grit commit --amend -m "amend 1" &&
	grit commit --amend -m "amend 2" &&
	grit commit --amend -m "amend 3" &&
	grit log --oneline >after &&
	count_before=$(wc -l <before) &&
	count_after=$(wc -l <after) &&
	test "$count_before" = "$count_after"
	)
'

test_expect_success 'commit with multiline -F message' '
	(
	cd repo &&
	printf "Line 1\n\nLine 3\n" >multi.txt &&
	echo "ml" >>file.txt &&
	grit add file.txt &&
	grit commit -F multi.txt &&
	grit log -n 1 >out &&
	grep "Line 1" out
	)
'

test_done
