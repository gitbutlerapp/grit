#!/bin/sh

test_description='diff: symlink creation, deletion, mode display, and diff-tree behavior'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with regular file' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo content >file.txt &&
     grit add file.txt &&
     grit commit -m "initial"
    )
'

test_expect_success 'diff --cached shows new symlink as mode 120000' '
    (cd repo &&
     ln -s file.txt link.txt &&
     grit add link.txt
    ) &&
    (cd repo && grit diff --cached >../actual) &&
    grep "new file mode 120000" actual
'

test_expect_success 'diff --cached symlink content shows target path' '
    grep "+file.txt" actual
'

test_expect_success 'diff --cached --stat shows symlink addition' '
    (cd repo && grit diff --cached --stat >../actual) &&
    grep "link.txt" actual &&
    grep "1 file changed" actual
'

test_expect_success 'diff --cached --numstat on symlink' '
    (cd repo && grit diff --cached --numstat >../actual) &&
    grep "link.txt" actual
'

test_expect_success 'diff --cached --name-only for symlink' '
    (cd repo && grit diff --cached --name-only >../actual) &&
    echo "link.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'diff --cached --name-status shows A for new symlink' '
    (cd repo && grit diff --cached --name-status >../actual) &&
    grep "^A" actual &&
    grep "link.txt" actual
'

test_expect_success 'commit symlink' '
    (cd repo && grit commit -m "add symlink")
'

test_expect_success 'diff-tree between commits shows symlink addition' '
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    grep "120000" actual &&
    grep "A" actual &&
    grep "link.txt" actual
'

test_expect_success 'no diff when tree is clean' '
    (cd repo && grit diff >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'diff --exit-code returns 0 on clean tree' '
    (cd repo && grit diff --exit-code)
'

test_expect_success 'diff --cached returns empty on clean tree' '
    (cd repo && grit diff --cached >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'working tree diff shows changed symlink' '
    (cd repo &&
     rm link.txt &&
     ln -s nonexistent link.txt &&
     grit diff >../actual
    ) &&
    grep "link.txt" actual &&
    grep "120000" actual
'

test_expect_success 'diff --exit-code returns 1 on modified symlink' '
    ! (cd repo && grit diff --exit-code)
'

test_expect_success 'diff --quiet on modified symlink exits non-zero' '
    ! (cd repo && grit diff --quiet)
'

test_expect_success 'restore symlink and verify clean' '
    (cd repo && git checkout -- link.txt) &&
    (cd repo && grit diff --exit-code)
'

test_expect_success 'delete symlink with grit rm --cached' '
    (cd repo && grit rm --cached link.txt) &&
    (cd repo && grit diff --cached --name-status >../actual) &&
    grep "^D" actual &&
    grep "link.txt" actual
'

test_expect_success 'diff --cached shows deleted file mode 120000' '
    (cd repo && grit diff --cached >../actual) &&
    grep "deleted file mode 120000" actual
'

test_expect_success 'commit symlink deletion' '
    (cd repo && rm -f link.txt && grit commit -m "remove symlink")
'

test_expect_success 'diff-tree shows D for deleted symlink' '
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    grep "D" actual &&
    grep "link.txt" actual
'

test_expect_success 'add multiple symlinks at once' '
    (cd repo &&
     ln -s file.txt sym_a &&
     ln -s file.txt sym_b &&
     ln -s file.txt sym_c &&
     grit add sym_a sym_b sym_c
    )
'

test_expect_success 'diff --cached --name-only shows all three' '
    (cd repo && grit diff --cached --name-only >../actual) &&
    grep "sym_a" actual &&
    grep "sym_b" actual &&
    grep "sym_c" actual
'

test_expect_success 'diff --cached --name-status shows A for all three' '
    (cd repo && grit diff --cached --name-status >../actual) &&
    test "$(grep -c "^A" actual)" = "3"
'

test_expect_success 'diff --cached --stat shows 3 files changed' '
    (cd repo && grit diff --cached --stat >../actual) &&
    grep "3 files changed" actual
'

test_expect_success 'commit three symlinks' '
    (cd repo && grit commit -m "three symlinks")
'

test_expect_success 'diff-tree shows all three new symlinks' '
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    grep "sym_a" actual &&
    grep "sym_b" actual &&
    grep "sym_c" actual
'

test_expect_success 'all three entries are 120000 mode' '
    test "$(grep -c "120000" actual)" = "3"
'

test_expect_success 'delete two symlinks with grit rm' '
    (cd repo &&
     grit rm --cached sym_a &&
     grit rm --cached sym_b &&
     rm -f sym_a sym_b
    )
'

test_expect_success 'diff --cached shows two deletions' '
    (cd repo && grit diff --cached --name-status >../actual) &&
    test "$(grep -c "^D" actual)" = "2"
'

test_expect_success 'commit two deletions' '
    (cd repo && grit commit -m "remove two symlinks")
'

test_expect_success 'diff-tree for deletion commit' '
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    grep "sym_a" actual &&
    grep "sym_b" actual &&
    ! grep "sym_c" actual
'

test_expect_success 'setup: create target for replacement' '
    (cd repo &&
     echo target >target.txt &&
     grit add target.txt &&
     grit commit -m "add target"
    )
'

test_expect_success 'setup: replace file with symlink via git add' '
    (cd repo &&
     rm file.txt &&
     ln -s target.txt file.txt &&
     git add file.txt
    )
'

test_expect_success 'grit diff --cached shows mode transition to 120000' '
    (cd repo && grit diff --cached >../actual) &&
    grep "120000" actual &&
    grep "file.txt" actual
'

test_expect_success 'commit file-to-symlink replacement' '
    (cd repo && grit commit -m "file to symlink")
'

test_expect_success 'diff-tree shows file.txt changed' '
    (cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
    grep "file.txt" actual
'

test_expect_success 'replace symlink back to regular file via git add' '
    (cd repo &&
     rm file.txt &&
     echo replaced >file.txt &&
     git add file.txt
    )
'

test_expect_success 'grit diff --cached shows mode transition to 100644' '
    (cd repo && grit diff --cached >../actual) &&
    grep "100644" actual &&
    grep "file.txt" actual
'

test_expect_success 'commit symlink-to-file replacement' '
    (cd repo && grit commit -m "symlink to file")
'

test_expect_success 'status is clean after all operations' '
    (cd repo && grit status --porcelain >../actual) &&
    ! grep -v "^##" actual
'

test_expect_success 'diff-tree across multiple commits' '
    (cd repo && grit diff-tree -r HEAD~4 HEAD >../actual) &&
    grep "file.txt" actual
'

test_done
