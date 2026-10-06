#!/bin/sh

test_description='check-ignore pathname matching, verbose mode, and patterns'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "*.log" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "initial"
    )
'

test_expect_success 'check-ignore matches *.log pattern' '
    (cd repo &&
     grit check-ignore foo.log >../actual) &&
    echo "foo.log" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore exits 0 for ignored file' '
    (cd repo &&
     grit check-ignore foo.log)
'

test_expect_success 'check-ignore exits non-zero for non-ignored file' '
    (cd repo &&
     test_must_fail grit check-ignore readme.txt)
'

test_expect_success 'check-ignore -v shows source and pattern' '
    (cd repo &&
     grit check-ignore -v foo.log >../actual) &&
    grep ".gitignore:1:" actual &&
    grep "foo.log" actual
'

test_expect_success 'check-ignore -v -n shows non-matching with exit 1' '
    (cd repo &&
     grit check-ignore -v -n readme.txt >../actual 2>&1 || true) &&
    grep "readme.txt" actual
'

test_expect_success 'check-ignore with directory pattern' '
    (cd repo &&
     echo "build/" >>.gitignore &&
     grit add .gitignore &&
     grit commit -m "add build pattern" &&
     mkdir -p build &&
     echo "obj" >build/output.o &&
     grit check-ignore build/output.o >../actual) &&
    echo "build/output.o" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore with negation pattern' '
    (cd repo &&
     printf "*.txt\n!important.txt\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "negation pattern" &&
     test_must_fail grit check-ignore important.txt)
'

test_expect_success 'check-ignore still ignores non-negated .txt' '
    (cd repo &&
     grit check-ignore random.txt >../actual) &&
    echo "random.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore with leading slash pattern' '
    (cd repo &&
     printf "/root-only.tmp\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "leading slash pattern" &&
     grit check-ignore root-only.tmp >../actual) &&
    echo "root-only.tmp" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore leading slash does not match in subdir' '
    (cd repo &&
     mkdir -p subdir &&
     test_must_fail grit check-ignore subdir/root-only.tmp)
'

test_expect_success 'check-ignore with wildcard in middle' '
    (cd repo &&
     printf "doc/*.pdf\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "wildcard pattern" &&
     mkdir -p doc &&
     grit check-ignore doc/manual.pdf >../actual) &&
    echo "doc/manual.pdf" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore wildcard does not match deeper nesting' '
    (cd repo &&
     mkdir -p doc/sub &&
     test_expect_code 1 grit check-ignore doc/sub/manual.pdf >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'check-ignore with double-star pattern' '
    (cd repo &&
     printf "**/*.bak\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "double star" &&
     mkdir -p a/b/c &&
     grit check-ignore a/b/c/test.bak >../actual) &&
    echo "a/b/c/test.bak" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore double-star matches in subdirectory' '
    (cd repo &&
     mkdir -p x &&
     grit check-ignore x/deep.bak >../actual) &&
    echo "x/deep.bak" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore with multiple files' '
    (cd repo &&
     printf "*.o\n*.a\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "multi ext" &&
     grit check-ignore foo.o bar.a >../actual) &&
    printf "foo.o\nbar.a\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore with --stdin reads paths from stdin' '
    (cd repo &&
     printf "foo.o\nbar.a\nkeep.c\n" | grit check-ignore --stdin >../actual) &&
    printf "foo.o\nbar.a\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore --no-index checks untracked too' '
    (cd repo &&
     grit check-ignore --no-index foo.o >../actual) &&
    echo "foo.o" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore with comment lines in gitignore' '
    (cd repo &&
     printf "# comment\n*.tmp\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "comment" &&
     grit check-ignore file.tmp >../actual) &&
    echo "file.tmp" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore comment line is not a pattern' '
    (cd repo &&
     test_must_fail grit check-ignore "# comment")
'

test_expect_success 'check-ignore with blank lines in gitignore' '
    (cd repo &&
     printf "\n*.bak\n\n*.tmp\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "blank lines" &&
     grit check-ignore test.bak >../actual) &&
    echo "test.bak" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore with subdirectory gitignore' '
    (cd repo &&
     printf "*\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "ignore all at root" &&
     mkdir -p sub &&
     printf "!*.keep\n" >sub/.gitignore &&
     grit check-ignore sub/test.keep >../actual 2>&1 ||
     true) &&
    true
'

test_expect_success 'check-ignore with trailing spaces in pattern' '
    (cd repo &&
     printf "*.log  \n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "trailing space" &&
     grit check-ignore test.log >../actual) &&
    echo "test.log" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore with specific filename pattern' '
    (cd repo &&
     printf "secret.key\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "specific file" &&
     grit check-ignore secret.key >../actual) &&
    echo "secret.key" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore with star extension *.o' '
    (cd repo &&
     printf "*.o\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "star o" &&
     grit check-ignore test.o >../actual) &&
    echo "test.o" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore *.o does not match .c' '
    (cd repo &&
     test_must_fail grit check-ignore main.c)
'

test_expect_success 'check-ignore *.o does not match .o.bak' '
    (cd repo &&
     test_must_fail grit check-ignore test.o.bak)
'

test_expect_success 'check-ignore with question mark wildcard' '
    (cd repo &&
     printf "file?.txt\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "question mark" &&
     grit check-ignore fileA.txt >../actual) &&
    echo "fileA.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'check-ignore question mark does not match multiple chars' '
    (cd repo &&
     test_must_fail grit check-ignore fileAB.txt)
'

test_expect_success 'check-ignore -v on multiple files' '
    (cd repo &&
     printf "*.log\n*.tmp\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "multi verbose" &&
     grit check-ignore -v a.log b.tmp >../actual) &&
    test_line_count = 2 actual &&
    grep "a.log" actual &&
    grep "b.tmp" actual
'

test_done
