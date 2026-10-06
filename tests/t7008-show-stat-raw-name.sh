#!/bin/sh
# Tests for grit show --stat, --raw, --name-only, --name-status,
# and format placeholders %ci, %ai, %cr, %ar, %D.

test_description='show --stat, --raw, --name-only, --name-status, format placeholders'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	echo "first" >file1.txt &&
	git add file1.txt &&
	GIT_AUTHOR_DATE="1000000000 +0000" GIT_COMMITTER_DATE="1000000000 +0000" \
		git commit -m "first commit" 2>/dev/null &&
	echo "second" >file2.txt &&
	echo "modified" >>file1.txt &&
	git add file1.txt file2.txt &&
	GIT_AUTHOR_DATE="1000000100 +0000" GIT_COMMITTER_DATE="1000000100 +0000" \
		git commit -m "second commit" 2>/dev/null
	)
'

# -- show --stat ---------------------------------------------------------------

test_expect_success 'show --stat includes file names' '
	(
	cd repo &&
	grit show --stat HEAD >actual &&
	grep "file1.txt" actual &&
	grep "file2.txt" actual
	)
'

test_expect_success 'show --stat includes insertion/deletion counts' '
	(
	cd repo &&
	grit show --stat HEAD >actual &&
	grep "changed" actual
	)
'

test_expect_success 'show --stat shows commit header before diffstat' '
	(
	cd repo &&
	grit show --stat HEAD >actual &&
	grep "^commit " actual &&
	grep "second commit" actual
	)
'

test_expect_success 'show --stat does not show unified diff' '
	(
	cd repo &&
	grit show --stat HEAD >actual &&
	! grep "^diff --git" actual &&
	! grep "^@@" actual
	)
'

# -- show --raw ----------------------------------------------------------------

test_expect_success 'show --raw produces colon-prefixed lines' '
	(
	cd repo &&
	grit show --raw HEAD >actual &&
	grep "^:" actual
	)
'

test_expect_success 'show --raw includes file modes and status' '
	(
	cd repo &&
	grit show --raw HEAD >actual &&
	grep "M" actual | grep "file1.txt" &&
	grep "A" actual | grep "file2.txt"
	)
'

test_expect_success 'show --raw does not show unified diff hunks' '
	(
	cd repo &&
	grit show --raw HEAD >actual &&
	! grep "^@@" actual &&
	! grep "^---" actual
	)
'

# -- show --name-only ----------------------------------------------------------

test_expect_success 'show --name-only lists file names' '
	(
	cd repo &&
	grit show --name-only HEAD >actual &&
	grep "^file1.txt$" actual &&
	grep "^file2.txt$" actual
	)
'

test_expect_success 'show --name-only does not show diff content' '
	(
	cd repo &&
	grit show --name-only HEAD >actual &&
	! grep "^diff --git" actual &&
	! grep "^@@" actual
	)
'

# -- show --name-status --------------------------------------------------------

test_expect_success 'show --name-status shows status and file names' '
	(
	cd repo &&
	grit show --name-status HEAD >actual &&
	grep "^M" actual | grep "file1.txt" &&
	grep "^A" actual | grep "file2.txt"
	)
'

test_expect_success 'show --name-status does not show diff content' '
	(
	cd repo &&
	grit show --name-status HEAD >actual &&
	! grep "^diff --git" actual &&
	! grep "^@@" actual
	)
'

# -- show --format=%ci (committer date ISO) ------------------------------------

test_expect_success 'show --format=%ci gives ISO-like committer date' '
	(
	cd repo &&
	grit show --format="format:%ci" --quiet >actual &&
	grep "2001-09-09" actual &&
	grep "+0000" actual
	)
'

# -- show --format=%ai (author date ISO) ---------------------------------------

test_expect_success 'show --format=%ai gives ISO-like author date' '
	(
	cd repo &&
	grit show --format="format:%ai" --quiet >actual &&
	grep "2001-09-09" actual &&
	grep "+0000" actual
	)
'

# -- show --format=%cr (relative committer date) -------------------------------

test_expect_success 'show --format=%cr gives relative committer date' '
	(
	cd repo &&
	grit show --format="format:%cr" --quiet >actual &&
	grep "ago" actual
	)
'

# -- show --format=%ar (relative author date) ----------------------------------

test_expect_success 'show --format=%ar gives relative author date' '
	(
	cd repo &&
	grit show --format="format:%ar" --quiet >actual &&
	grep "ago" actual
	)
'

# -- show --format=%D (decorations without parens) -----------------------------

test_expect_success 'show --format=%D does not crash' '
	(
	cd repo &&
	grit show --format="format:%D" --quiet >actual
	)
'

# -- show --stat on root commit ------------------------------------------------

test_expect_success 'show --stat on root commit works' '
	(
	cd repo &&
	first=$(git rev-list HEAD | tail -1) &&
	grit show --stat "$first" >actual &&
	grep "file1.txt" actual &&
	grep "changed" actual
	)
'

# -- show --raw on root commit -------------------------------------------------

test_expect_success 'show --raw on root commit shows added file' '
	(
	cd repo &&
	first=$(git rev-list HEAD | tail -1) &&
	grit show --raw "$first" >actual &&
	grep "^:" actual &&
	grep "A" actual | grep "file1.txt"
	)
'

# -- show --name-only on root commit ------------------------------------------

test_expect_success 'show --name-only on root commit lists file' '
	(
	cd repo &&
	first=$(git rev-list HEAD | tail -1) &&
	grit show --name-only "$first" >actual &&
	grep "^file1.txt$" actual
	)
'

# -- show --name-status on root commit ----------------------------------------

test_expect_success 'show --name-status on root commit shows A status' '
	(
	cd repo &&
	first=$(git rev-list HEAD | tail -1) &&
	grit show --name-status "$first" >actual &&
	grep "^A" actual | grep "file1.txt"
	)
'

test_done
