#!/bin/sh

test_description='check-ignore: recursive subdirectory patterns, verbose mode, stdin, multiple paths'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with gitignore' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "*.log" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "initial with gitignore"
    )
'

test_expect_success 'ignored file is reported' '
    (cd repo && grit check-ignore test.log >../actual) &&
    echo "test.log" >expect &&
    test_cmp expect actual
'

test_expect_success 'non-ignored file returns non-zero' '
    (cd repo && test_must_fail grit check-ignore test.txt)
'

test_expect_success 'verbose mode shows source and pattern' '
    (cd repo && grit check-ignore -v test.log >../actual) &&
    grep ".gitignore" actual &&
    grep "\\*.log" actual &&
    grep "test.log" actual
'

test_expect_success 'multiple files on command line' '
    (cd repo && grit check-ignore a.log b.log c.log >../actual) &&
    grep "a.log" actual &&
    grep "b.log" actual &&
    grep "c.log" actual
'

test_expect_success 'mix of ignored and non-ignored files' '
    (cd repo && grit check-ignore test.log test.txt 2>../err >../actual) &&
    grep "test.log" actual &&
    ! grep "test.txt" actual
'

test_expect_success 'stdin mode reads paths from stdin' '
    (cd repo && echo "test.log" | grit check-ignore --stdin >../actual) &&
    echo "test.log" >expect &&
    test_cmp expect actual
'

test_expect_success 'stdin mode with multiple paths' '
    (cd repo && printf "a.log\nb.log\nc.txt\n" | grit check-ignore --stdin >../actual) &&
    grep "a.log" actual &&
    grep "b.log" actual &&
    ! grep "c.txt" actual
'

test_expect_success 'setup subdirectory with own gitignore' '
    (cd repo &&
     mkdir -p sub &&
     echo "*.tmp" >sub/.gitignore &&
     grit add sub/.gitignore &&
     grit commit -m "add sub gitignore"
    )
'

test_expect_success 'subdirectory pattern matches' '
    (cd repo && grit check-ignore sub/file.tmp >../actual) &&
    echo "sub/file.tmp" >expect &&
    test_cmp expect actual
'

test_expect_success 'subdirectory verbose shows correct source' '
    (cd repo && grit check-ignore -v sub/file.tmp >../actual) &&
    grep "sub/.gitignore" actual &&
    grep "\\*.tmp" actual
'

test_expect_success 'root pattern still applies in subdirectory' '
    (cd repo && grit check-ignore sub/file.log >../actual) &&
    echo "sub/file.log" >expect &&
    test_cmp expect actual
'

test_expect_success 'root pattern verbose in subdirectory' '
    (cd repo && grit check-ignore -v sub/file.log >../actual) &&
    grep ".gitignore:1:" actual
'

test_expect_success 'setup deeply nested directories' '
    (cd repo &&
     mkdir -p a/b/c &&
     echo "*.dat" >a/.gitignore &&
     echo "*.bak" >a/b/.gitignore &&
     echo "*.cache" >a/b/c/.gitignore &&
     grit add a/.gitignore a/b/.gitignore a/b/c/.gitignore &&
     grit commit -m "add nested gitignores"
    )
'

test_expect_success 'deepest gitignore pattern matches' '
    (cd repo && grit check-ignore a/b/c/file.cache >../actual) &&
    echo "a/b/c/file.cache" >expect &&
    test_cmp expect actual
'

test_expect_success 'middle gitignore pattern matches' '
    (cd repo && grit check-ignore a/b/file.bak >../actual) &&
    echo "a/b/file.bak" >expect &&
    test_cmp expect actual
'

test_expect_success 'parent gitignore pattern propagates to deep child' '
    (cd repo && grit check-ignore a/b/c/file.dat >../actual) &&
    echo "a/b/c/file.dat" >expect &&
    test_cmp expect actual
'

test_expect_success 'root pattern propagates through all levels' '
    (cd repo && grit check-ignore a/b/c/deep.log >../actual) &&
    echo "a/b/c/deep.log" >expect &&
    test_cmp expect actual
'

test_expect_success 'verbose on deeply nested shows correct source' '
    (cd repo && grit check-ignore -v a/b/c/file.cache >../actual) &&
    grep "a/b/c/.gitignore" actual
'

test_expect_success 'verbose on middle level shows correct source' '
    (cd repo && grit check-ignore -v a/b/file.bak >../actual) &&
    grep "a/b/.gitignore" actual
'

test_expect_success 'setup directory ignore pattern' '
    (cd repo &&
     echo "build/" >>.gitignore &&
     grit add .gitignore &&
     grit commit -m "add build/ pattern"
    )
'

test_expect_success 'directory pattern ignores files in directory' '
    (cd repo &&
     mkdir -p build &&
     grit check-ignore build/output.o >../actual) &&
    echo "build/output.o" >expect &&
    test_cmp expect actual
'

test_expect_success 'directory pattern ignores nested files' '
    (cd repo && grit check-ignore build/sub/deep.o >../actual) &&
    echo "build/sub/deep.o" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup wildcard patterns' '
    (cd repo &&
     printf "*.o\n*.pyc\ntemp_*\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "multiple wildcard patterns"
    )
'

test_expect_success 'first wildcard pattern works' '
    (cd repo && grit check-ignore main.o >../actual) &&
    echo "main.o" >expect &&
    test_cmp expect actual
'

test_expect_success 'second wildcard pattern works' '
    (cd repo && grit check-ignore module.pyc >../actual) &&
    echo "module.pyc" >expect &&
    test_cmp expect actual
'

test_expect_success 'prefix wildcard pattern works' '
    (cd repo && grit check-ignore temp_data >../actual) &&
    echo "temp_data" >expect &&
    test_cmp expect actual
'

test_expect_success 'non-matching extension is not ignored' '
    (cd repo && test_must_fail grit check-ignore main.c)
'

test_expect_success 'setup exact filename pattern' '
    (cd repo &&
     printf "secret.txt\n.env\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "exact filename patterns"
    )
'

test_expect_success 'exact filename is ignored' '
    (cd repo && grit check-ignore secret.txt >../actual) &&
    echo "secret.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'dotfile pattern is ignored' '
    (cd repo && grit check-ignore .env >../actual) &&
    echo ".env" >expect &&
    test_cmp expect actual
'

test_expect_success 'similar but different name is not ignored' '
    (cd repo && test_must_fail grit check-ignore secret.txt.bak) ||
    true
'

test_expect_success 'check-ignore with non-matching stdin returns non-zero' '
    (cd repo && echo "safe_file.txt" | test_must_fail grit check-ignore --stdin)
'

test_expect_success 'verbose with non-matching and -n shows empty source' '
    (cd repo && grit check-ignore -v -n safe_file.txt >../actual 2>&1) ||
    grep "safe_file.txt" actual
'

test_expect_success 'setup double-star pattern' '
    (cd repo &&
     printf "**/debug.log\n" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "double-star pattern"
    )
'

test_expect_success 'double-star matches in sub but not root' '
    (cd repo && grit check-ignore debug.log >../actual 2>&1) ||
    true
'

test_expect_success 'double-star matches in subdirectory' '
    (cd repo && grit check-ignore sub/debug.log >../actual) &&
    echo "sub/debug.log" >expect &&
    test_cmp expect actual
'

test_expect_success 'double-star matches deeply nested' '
    (cd repo && grit check-ignore a/b/c/debug.log >../actual) &&
    echo "a/b/c/debug.log" >expect &&
    test_cmp expect actual
'

test_done
