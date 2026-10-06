#!/bin/sh

test_description='diff --cached: mode changes (644->755, 755->644), combined with content changes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with non-executable file' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo content >file.txt &&
     grit add file.txt &&
     grit commit -m "initial"
    )
'

test_expect_success 'chmod +x and stage: diff --cached shows mode change' '
    (cd repo && chmod +x file.txt && grit add file.txt) &&
    (cd repo && grit diff --cached >../actual) &&
    grep "old mode 100644" actual &&
    grep "new mode 100755" actual
'

test_expect_success 'diff --cached mode change has no content hunk' '
    ! grep "^@@" actual
'

test_expect_success 'diff --cached --name-status shows M for mode change' '
    (cd repo && grit diff --cached --name-status >../actual) &&
    grep "^M" actual &&
    grep "file.txt" actual
'

test_expect_success 'diff --cached --stat shows 0 insertions/deletions for mode-only' '
    (cd repo && grit diff --cached --stat >../actual) &&
    grep "file.txt" actual &&
    grep "1 file changed" actual
'

test_expect_success 'diff --cached --numstat for mode change' '
    (cd repo && grit diff --cached --numstat >../actual) &&
    grep "file.txt" actual
'

test_expect_success 'diff --cached --name-only lists file' '
    (cd repo && grit diff --cached --name-only >../actual) &&
    echo "file.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit mode change to 755' '
    (cd repo && grit commit -m "make executable")
'

test_expect_success 'diff-tree shows mode change 644->755' '
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    grep "100644" actual &&
    grep "100755" actual &&
    grep "file.txt" actual
'

test_expect_success 'chmod -x and stage: diff --cached shows reverse mode change' '
    (cd repo && chmod -x file.txt && grit add file.txt) &&
    (cd repo && grit diff --cached >../actual) &&
    grep "old mode 100755" actual &&
    grep "new mode 100644" actual
'

test_expect_success 'commit mode change back to 644' '
    (cd repo && grit commit -m "remove executable")
'

test_expect_success 'diff-tree shows mode change 755->644' '
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    grep "100755" actual &&
    grep "100644" actual
'

test_expect_success 'setup: add second file as executable from start' '
    (cd repo &&
     echo script >run.sh &&
     chmod +x run.sh &&
     grit add run.sh &&
     grit commit -m "add executable script"
    )
'

test_expect_success 'diff-tree shows new executable file as 100755' '
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    grep "100755" actual &&
    grep "run.sh" actual
'

test_expect_success 'mode change plus content change in same file' '
    (cd repo &&
     chmod +x file.txt &&
     echo modified >file.txt &&
     grit add file.txt
    ) &&
    (cd repo && grit diff --cached >../actual) &&
    grep "old mode 100644" actual &&
    grep "new mode 100755" actual &&
    grep "+modified" actual &&
    grep -- "-content" actual
'

test_expect_success 'diff --cached --stat shows insertions for combined change' '
    (cd repo && grit diff --cached --stat >../actual) &&
    grep "file.txt" actual
'

test_expect_success 'commit combined mode+content change' '
    (cd repo && grit commit -m "mode and content change")
'

test_expect_success 'diff-tree shows M for combined change' '
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    grep "M" actual &&
    grep "file.txt" actual
'

test_expect_success 'mode change on multiple files at once' '
    (cd repo &&
     echo a >a.sh &&
     echo b >b.sh &&
     grit add a.sh b.sh &&
     grit commit -m "add two scripts"
    ) &&
    (cd repo &&
     chmod +x a.sh b.sh &&
     grit add a.sh b.sh
    ) &&
    (cd repo && grit diff --cached --name-status >../actual) &&
    grep "a.sh" actual &&
    grep "b.sh" actual
'

test_expect_success 'diff --cached shows both mode changes' '
    (cd repo && grit diff --cached >../actual) &&
    test "$(grep -c "old mode 100644" actual)" = "2" &&
    test "$(grep -c "new mode 100755" actual)" = "2"
'

test_expect_success 'commit multiple mode changes' '
    (cd repo && grit commit -m "make both executable")
'

test_expect_success 'diff --exit-code returns 0 with no changes' '
    (cd repo && grit diff --exit-code)
'

test_expect_success 'diff --cached --exit-code returns 0 with nothing staged' '
    (cd repo && grit diff --cached --exit-code)
'

test_expect_success 'mode change in working tree detected by diff' '
    (cd repo && chmod -x a.sh) &&
    ! (cd repo && grit diff --exit-code)
'

test_expect_success 'working tree mode change shows in diff output' '
    (cd repo && grit diff >../actual) &&
    grep "a.sh" actual &&
    grep "100755" actual &&
    grep "100644" actual
'

test_expect_success 'restore working tree mode' '
    (cd repo && chmod +x a.sh) &&
    (cd repo && grit diff --exit-code)
'

test_expect_success 'new file with executable mode shows 100755 in diff --cached' '
    (cd repo &&
     echo newscript >new.sh &&
     chmod +x new.sh &&
     grit add new.sh
    ) &&
    (cd repo && grit diff --cached >../actual) &&
    grep "new file mode 100755" actual
'

test_expect_success 'commit new executable file' '
    (cd repo && grit commit -m "add new.sh")
'

test_expect_success 'remove executable bit and add content simultaneously' '
    (cd repo &&
     chmod -x run.sh &&
     echo updated >>run.sh &&
     grit add run.sh
    ) &&
    (cd repo && grit diff --cached >../actual) &&
    grep "old mode 100755" actual &&
    grep "new mode 100644" actual &&
    grep "+updated" actual
'

test_expect_success 'commit reverse combined change' '
    (cd repo && grit commit -m "de-exec and update run.sh")
'

test_expect_success 'diff-tree -p shows patch for mode+content change' '
    (cd repo && grit diff-tree -p HEAD~1 HEAD >../actual) &&
    grep "run.sh" actual
'

test_expect_success 'diff --cached is empty after commit' '
    (cd repo && grit diff --cached >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'mode-only change with diff --cached --stat shows 0 changes' '
    (cd repo &&
     chmod +x run.sh &&
     grit add run.sh &&
     grit diff --cached --stat >../actual
    ) &&
    grep "1 file changed" actual &&
    grep "0 insertions(+), 0 deletions(-)" actual
'

test_expect_success 'commit mode-only change' '
    (cd repo && grit commit -m "re-exec run.sh")
'

test_expect_success 'add non-executable and executable files together' '
    (cd repo &&
     echo plain >plain.txt &&
     echo exec >exec.sh && chmod +x exec.sh &&
     grit add plain.txt exec.sh
    ) &&
    (cd repo && grit diff --cached >../actual) &&
    grep "new file mode 100644" actual &&
    grep "new file mode 100755" actual
'

test_expect_success 'commit mixed mode new files' '
    (cd repo && grit commit -m "add plain and exec")
'

test_expect_success 'diff-tree shows different modes for new files' '
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    grep "100644.*plain.txt" actual &&
    grep "100755.*exec.sh" actual
'

test_expect_success 'status is clean' '
    (cd repo && grit status --porcelain >../actual) &&
    ! grep -v "^##" actual
'

test_done
