#!/bin/sh
# Test grit diff-index raw output, --cached, --quiet, --exit-code,
# --abbrev, path filtering, and status letters (M, A, D).

test_description='grit diff-index name and status output'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: repo with initial files' '
	(
	grit init di-repo &&
	cd di-repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "alpha" >alpha.txt &&
	echo "beta" >beta.txt &&
	echo "gamma" >gamma.txt &&
	grit add alpha.txt beta.txt gamma.txt &&
	test_tick &&
	grit commit -m "initial commit with three files"
	)
'

# --- clean state ---

test_expect_success 'diff-index --cached HEAD empty for clean index' '
	(
	cd di-repo &&
	grit diff-index --cached HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff-index HEAD empty for clean working tree' '
	(
	cd di-repo &&
	grit diff-index HEAD >actual &&
	test_must_be_empty actual
	)
'

# --- modifications in working tree ---

test_expect_success 'diff-index HEAD shows modified file' '
	(
	cd di-repo &&
	echo "alpha modified" >alpha.txt &&
	grit diff-index HEAD >actual &&
	grep "alpha.txt" actual
	)
'

test_expect_success 'diff-index HEAD shows M status' '
	(
	cd di-repo &&
	grit diff-index HEAD >actual &&
	grep "M	alpha.txt" actual
	)
'

test_expect_success 'diff-index HEAD does not show unmodified files' '
	(
	cd di-repo &&
	grit diff-index HEAD >actual &&
	! grep "beta.txt" actual &&
	! grep "gamma.txt" actual
	)
'

test_expect_success 'diff-index HEAD shows multiple modified files' '
	(
	cd di-repo &&
	echo "beta modified" >beta.txt &&
	grit diff-index HEAD >actual &&
	grep "alpha.txt" actual &&
	grep "beta.txt" actual
	)
'

# --- staged changes with --cached ---

test_expect_success 'diff-index --cached HEAD shows staged modification' '
	(
	cd di-repo &&
	grit add alpha.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "M	alpha.txt" actual
	)
'

test_expect_success 'diff-index --cached shows correct hash format' '
	(
	cd di-repo &&
	grit diff-index --cached HEAD >actual &&
	grep "^:100644 100644" actual
	)
'

test_expect_success 'diff-index (unstaged) still shows beta modification' '
	(
	cd di-repo &&
	grit diff-index HEAD >actual &&
	grep "beta.txt" actual
	)
'

test_expect_success 'stage remaining changes and commit' '
	(
	cd di-repo &&
	grit add beta.txt &&
	test_tick &&
	grit commit -m "modify alpha and beta"
	)
'

# --- new file (A status) ---

test_expect_success 'diff-index --cached HEAD shows A for new file' '
	(
	cd di-repo &&
	echo "new content" >new.txt &&
	grit add new.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "A	new.txt" actual
	)
'

test_expect_success 'diff-index --cached shows 000000 as old hash for new file' '
	(
	cd di-repo &&
	grit diff-index --cached HEAD >actual &&
	grep "^:000000 100644 0000000000000000000000000000000000000000" actual
	)
'

test_expect_success 'commit new file' '
	(
	cd di-repo &&
	test_tick &&
	grit commit -m "add new file"
	)
'

# --- deletion (D status) ---

test_expect_success 'diff-index --cached HEAD shows D for deleted file' '
	(
	cd di-repo &&
	grit rm gamma.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "D	gamma.txt" actual
	)
'

test_expect_success 'diff-index --cached shows 000000 as new hash for deleted file' '
	(
	cd di-repo &&
	grit diff-index --cached HEAD >actual &&
	grep "0000000000000000000000000000000000000000 D" actual
	)
'

test_expect_success 'commit deletion' '
	(
	cd di-repo &&
	test_tick &&
	grit commit -m "remove gamma"
	)
'

# --- --quiet flag ---

test_expect_success 'diff-index --quiet exits 0 for clean state' '
	(
	cd di-repo &&
	grit diff-index --quiet HEAD
	)
'

test_expect_success 'diff-index --quiet exits 1 for dirty state' '
	(
	cd di-repo &&
	echo "dirty" >>alpha.txt &&
	test_must_fail grit diff-index --quiet HEAD
	)
'

test_expect_success 'diff-index --quiet produces no output' '
	(
	cd di-repo &&
	grit diff-index --quiet HEAD >actual 2>&1 || true &&
	test_must_be_empty actual
	)
'

# --- --exit-code flag ---

test_expect_success 'diff-index --exit-code exits 1 with differences' '
	(
	cd di-repo &&
	test_must_fail grit diff-index --exit-code HEAD
	)
'

test_expect_success 'diff-index --exit-code produces output' '
	(
	cd di-repo &&
	grit add alpha.txt &&
	grit diff-index --exit-code --cached HEAD >actual 2>&1 || true &&
	test -s actual
	)
'

test_expect_success 'commit changes for exit-code clean test' '
	(
	cd di-repo &&
	test_tick &&
	grit commit -m "update alpha"
	)
'

test_expect_success 'diff-index --exit-code exits 0 for clean state' '
	(
	cd di-repo &&
	grit diff-index --exit-code --cached HEAD
	)
'

# --- --abbrev flag ---

test_expect_success 'diff-index --abbrev shortens hashes' '
	(
	cd di-repo &&
	echo "abbrev test" >>beta.txt &&
	grit add beta.txt &&
	grit diff-index --abbrev=7 --cached HEAD >actual &&
	! grep "0000000000000000000000000000000000000000" actual
	)
'

test_expect_success 'diff-index --abbrev=7 shows 7-char hashes' '
	(
	cd di-repo &&
	grit diff-index --abbrev=7 --cached HEAD >actual &&
	awk "{print \$3}" actual >hash &&
	len=$(head -1 hash | wc -c) &&
	test "$len" -eq 8
	)
'

test_expect_success 'commit abbrev changes' '
	(
	cd di-repo &&
	test_tick &&
	grit commit -m "abbrev test"
	)
'

# --- path filtering ---

test_expect_success 'setup: modify multiple files for path filter test' '
	(
	cd di-repo &&
	echo "alpha again" >>alpha.txt &&
	echo "beta again" >>beta.txt &&
	grit add alpha.txt beta.txt
	)
'

test_expect_success 'diff-index --cached HEAD with path filter' '
	(
	cd di-repo &&
	grit diff-index --cached HEAD -- alpha.txt >actual &&
	grep "alpha.txt" actual &&
	! grep "beta.txt" actual
	)
'

test_expect_success 'diff-index --cached HEAD with different path' '
	(
	cd di-repo &&
	grit diff-index --cached HEAD -- beta.txt >actual &&
	grep "beta.txt" actual &&
	! grep "alpha.txt" actual
	)
'

test_expect_success 'commit path filter test changes' '
	(
	cd di-repo &&
	test_tick &&
	grit commit -m "path filter test"
	)
'

# --- subdirectory files ---

test_expect_success 'setup: create subdirectory with files' '
	(
	cd di-repo &&
	mkdir -p sub/dir &&
	echo "sub content" >sub/dir/file.txt &&
	grit add sub &&
	test_tick &&
	grit commit -m "add subdirectory file"
	)
'

test_expect_success 'diff-index shows full path for nested file' '
	(
	cd di-repo &&
	echo "modified" >sub/dir/file.txt &&
	grit diff-index HEAD >actual &&
	grep "sub/dir/file.txt" actual
	)
'

test_expect_success 'diff-index --cached shows full path for nested staged' '
	(
	cd di-repo &&
	grit add sub/dir/file.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "sub/dir/file.txt" actual
	)
'

test_expect_success 'commit nested change' '
	(
	cd di-repo &&
	test_tick &&
	grit commit -m "modify nested"
	)
'

# --- mixed operations ---

test_expect_success 'diff-index --cached shows mixed A, M, D statuses' '
	(
	cd di-repo &&
	echo "modified alpha" >alpha.txt &&
	grit add alpha.txt &&
	echo "brand new" >brand.txt &&
	grit add brand.txt &&
	grit rm new.txt &&
	grit diff-index --cached HEAD >actual &&
	grep "M	alpha.txt" actual &&
	grep "A	brand.txt" actual &&
	grep "D	new.txt" actual
	)
'

test_expect_success 'diff-index --cached shows correct count of changes' '
	(
	cd di-repo &&
	grit diff-index --cached HEAD >actual &&
	count=$(wc -l <actual) &&
	test "$count" -eq 3
	)
'

test_expect_success 'commit mixed operations' '
	(
	cd di-repo &&
	test_tick &&
	grit commit -m "mixed ops"
	)
'

# --- diff-index against non-HEAD tree ---

test_expect_success 'diff-index --cached against older commit' '
	(
	cd di-repo &&
	old=$(grit log --oneline --reverse | head -1 | cut -d" " -f1) &&
	grit diff-index --cached "$old" >actual &&
	test -s actual
	)
'

test_expect_success 'diff-index --cached against older commit shows many changes' '
	(
	cd di-repo &&
	old=$(grit log --oneline --reverse | head -1 | cut -d" " -f1) &&
	grit diff-index --cached "$old" >actual &&
	count=$(wc -l <actual) &&
	test "$count" -gt 1
	)
'

# --- -m flag ---

test_expect_success 'diff-index -m HEAD works on clean tree' '
	(
	cd di-repo &&
	grit diff-index -m HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff-index -m HEAD shows working tree changes' '
	(
	cd di-repo &&
	echo "m flag test" >>alpha.txt &&
	grit diff-index -m HEAD >actual &&
	grep "alpha.txt" actual
	)
'

test_expect_success 'clean up m flag changes' '
	(
	cd di-repo &&
	grit checkout -- alpha.txt
	)
'

test_done
