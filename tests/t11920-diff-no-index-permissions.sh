#!/bin/sh
test_description='diff behavior with permission/mode changes'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
		git config user.email "t@t.com" &&
		git config user.name "T" &&
		echo "hello" >file.txt &&
		grit add file.txt &&
		grit commit -m "initial"
	)
'

test_expect_success 'diff detects mode change from 644 to 755' '
	(cd repo &&
		chmod 755 file.txt &&
		grit diff >../actual
	) &&
	grep "old mode 100644" actual &&
	grep "new mode 100755" actual
'

test_expect_success 'diff --exit-code reports difference for mode change' '
	(cd repo && test_must_fail grit diff --exit-code)
'

test_expect_success 'diff --stat shows mode-only change' '
	(cd repo && grit diff --stat >../actual) &&
	grep "file.txt" actual
'

test_expect_success 'diff --name-only shows file with mode change' '
	(cd repo && grit diff --name-only >../actual) &&
	echo "file.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --name-status shows M for mode change' '
	(cd repo && grit diff --name-status >../actual) &&
	printf "M\tfile.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff --quiet suppresses output for mode change' '
	(cd repo &&
		test_must_fail grit diff --quiet >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'adding mode change to index' '
	(cd repo && grit add file.txt)
'

test_expect_success 'diff --cached shows mode change in index' '
	(cd repo && grit diff --cached >../actual) &&
	grep "old mode 100644" actual &&
	grep "new mode 100755" actual
'

test_expect_success 'diff --cached --name-only for mode change' '
	(cd repo && grit diff --cached --name-only >../actual) &&
	echo "file.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'commit mode change and verify clean diff' '
	(cd repo &&
		grit commit -m "make executable" &&
		grit diff >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'diff shows no change when mode is preserved' '
	(cd repo && grit diff --exit-code)
'

test_expect_success 'change mode back to 644' '
	(cd repo &&
		chmod 644 file.txt &&
		grit diff --name-only >../actual
	) &&
	echo "file.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff shows mode revert 755 to 644' '
	(cd repo && grit diff >../actual) &&
	grep "old mode 100755" actual &&
	grep "new mode 100644" actual
'

test_expect_success 'stage mode revert and diff --cached shows it' '
	(cd repo &&
		grit add file.txt &&
		grit diff --cached >../actual
	) &&
	grep "old mode 100755" actual &&
	grep "new mode 100644" actual
'

test_expect_success 'commit mode revert' '
	(cd repo && grit commit -m "revert to 644")
'

test_expect_success 'staged content change shows in diff --cached' '
	(cd repo &&
		echo "modified" >file.txt &&
		grit add file.txt &&
		grit diff --cached >../actual
	) &&
	grep "+modified" actual &&
	grep "\-hello" actual
'

test_expect_success 'diff --cached --stat for content change' '
	(cd repo && grit diff --cached --stat >../actual) &&
	grep "file.txt" actual &&
	grep "1 file changed" actual
'

test_expect_success 'diff --cached --numstat for content change' '
	(cd repo && grit diff --cached --numstat >../actual) &&
	grep "file.txt" actual
'

test_expect_success 'commit content change and add executable file' '
	(cd repo &&
		grit commit -m "modify content" &&
		echo "script" >run.sh &&
		chmod 755 run.sh &&
		grit add run.sh &&
		grit commit -m "add script"
	)
'

test_expect_success 'new executable file shows 100755 in diff --cached' '
	(cd repo &&
		echo "another" >tool.sh &&
		chmod 755 tool.sh &&
		grit add tool.sh &&
		grit diff --cached >../actual
	) &&
	grep "new file mode 100755" actual
'

test_expect_success 'diff --cached --name-status for new executable' '
	(cd repo && grit diff --cached --name-status >../actual) &&
	grep "A" actual &&
	grep "tool.sh" actual
'

test_expect_success 'commit new executable' '
	(cd repo && grit commit -m "add tool")
'

test_expect_success 'multiple files with different mode changes' '
	(cd repo &&
		chmod 755 file.txt &&
		chmod 644 run.sh &&
		grit diff --name-only >../actual
	) &&
	grep "file.txt" actual &&
	grep "run.sh" actual
'

test_expect_success 'diff shows both mode changes' '
	(cd repo && grit diff >../actual) &&
	grep -c "old mode" actual >count &&
	echo "2" >expect_count &&
	test_cmp expect_count count
'

test_expect_success 'diff --stat for multiple mode changes' '
	(cd repo && grit diff --stat >../actual) &&
	grep "2 files changed" actual
'

test_expect_success 'stage partial mode changes' '
	(cd repo &&
		grit add file.txt &&
		grit diff --name-only >../actual_unstaged &&
		grit diff --cached --name-only >../actual_staged
	) &&
	echo "run.sh" >expect_unstaged &&
	echo "file.txt" >expect_staged &&
	test_cmp expect_unstaged actual_unstaged &&
	test_cmp expect_staged actual_staged
'

test_expect_success 'diff --exit-code with only unstaged mode change' '
	(cd repo && test_must_fail grit diff --exit-code)
'

test_expect_success 'clean up mode changes' '
	(cd repo &&
		grit add run.sh &&
		grit commit -m "swap modes"
	)
'

test_expect_success 'new file with 644 mode shows in diff --cached' '
	(cd repo &&
		echo "data" >plain.txt &&
		grit add plain.txt &&
		grit diff --cached >../actual
	) &&
	grep "new file mode 100644" actual
'

test_expect_success 'commit plain file' '
	(cd repo && grit commit -m "add plain")
'

test_expect_success 'staged multiline content change in diff --cached' '
	(cd repo &&
		printf "line1\nline2\nline3\n" >plain.txt &&
		grit add plain.txt &&
		grit commit -m "multi-line" &&
		printf "line1\nCHANGED\nline3\n" >plain.txt &&
		grit add plain.txt &&
		grit diff --cached >../actual
	) &&
	grep "+CHANGED" actual &&
	grep "\-line2" actual
'

test_expect_success 'diff --cached -U1 limits context' '
	(cd repo && grit diff --cached -U1 >../actual) &&
	grep "+CHANGED" actual &&
	grep "\-line2" actual
'

test_expect_success 'diff --cached -U0 shows zero context' '
	(cd repo && grit diff --cached -U0 >../actual) &&
	grep "+CHANGED" actual &&
	grep "\-line2" actual
'

test_expect_success 'staged mode + content change in diff --cached' '
	(cd repo &&
		chmod 755 plain.txt &&
		grit add plain.txt &&
		grit diff --cached >../actual
	) &&
	grep "old mode 100644" actual &&
	grep "new mode 100755" actual &&
	grep "+CHANGED" actual
'

test_expect_success 'commit and verify clean state' '
	(cd repo &&
		grit commit -m "mode and content change" &&
		grit diff --cached >../actual
	) &&
	test_must_be_empty actual
'

test_done
