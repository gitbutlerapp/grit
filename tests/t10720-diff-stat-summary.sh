#!/bin/sh
# Test grit diff --stat, --numstat, --name-only, and --name-status
# output formatting and correctness across various scenarios.

test_description='grit diff --stat summary output'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: repo with initial file' '
	(
	grit init stat-repo &&
	cd stat-repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "line one" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "initial commit"
	)
'

# --- basic --stat output ---

test_expect_success 'diff --stat shows nothing for clean tree' '
	(
	cd stat-repo &&
	grit diff --stat >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff --stat shows modified file' '
	(
	cd stat-repo &&
	echo "line two" >>file.txt &&
	grit diff --stat >actual &&
	grep "file.txt" actual
	)
'

test_expect_success 'diff --stat summary line mentions file changed' '
	(
	cd stat-repo &&
	grit diff --stat >actual &&
	grep "1 file changed" actual
	)
'

test_expect_success 'diff --cached --stat shows staged change' '
	(
	cd stat-repo &&
	grit add file.txt &&
	grit diff --cached --stat >actual &&
	grep "file.txt" actual
	)
'

test_expect_success 'diff --cached --stat shows insertion count' '
	(
	cd stat-repo &&
	grit diff --cached --stat >actual &&
	grep "insertion" actual
	)
'

test_expect_success 'diff --stat empty after staging all changes' '
	(
	cd stat-repo &&
	grit diff --stat >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'commit staged changes' '
	(
	cd stat-repo &&
	test_tick &&
	grit commit -m "add line two"
	)
'

# --- multiple files ---

test_expect_success 'setup: create multiple files' '
	(
	cd stat-repo &&
	echo "alpha content" >alpha.txt &&
	echo "beta content" >beta.txt &&
	echo "gamma content" >gamma.txt &&
	grit add alpha.txt beta.txt gamma.txt &&
	test_tick &&
	grit commit -m "add alpha beta gamma"
	)
'

test_expect_success 'diff --stat shows all modified files' '
	(
	cd stat-repo &&
	echo "alpha changed" >>alpha.txt &&
	echo "beta changed" >>beta.txt &&
	grit diff --stat >actual &&
	grep "alpha.txt" actual &&
	grep "beta.txt" actual
	)
'

test_expect_success 'diff --stat does not show unmodified files' '
	(
	cd stat-repo &&
	grit diff --stat >actual &&
	! grep "gamma.txt" actual
	)
'

test_expect_success 'diff --stat summary line shows file count' '
	(
	cd stat-repo &&
	grit diff --stat >actual &&
	grep "2 files changed" actual
	)
'

test_expect_success 'stage and commit multi-file changes' '
	(
	cd stat-repo &&
	grit add alpha.txt beta.txt &&
	test_tick &&
	grit commit -m "modify alpha beta"
	)
'

# --- --cached with new file ---

test_expect_success 'diff --cached --stat shows newly added file' '
	(
	cd stat-repo &&
	echo "new content" >new-file.txt &&
	grit add new-file.txt &&
	grit diff --cached --stat >actual &&
	grep "new-file.txt" actual
	)
'

test_expect_success 'diff --cached --stat insertion for new file' '
	(
	cd stat-repo &&
	grit diff --cached --stat >actual &&
	grep "1 insertion" actual
	)
'

test_expect_success 'commit new file' '
	(
	cd stat-repo &&
	test_tick &&
	grit commit -m "add new-file"
	)
'

# --- deletions via --cached ---

test_expect_success 'diff --cached --stat shows staged deletion' '
	(
	cd stat-repo &&
	grit rm new-file.txt &&
	grit diff --cached --stat >actual &&
	grep "new-file.txt" actual &&
	grep "deletion" actual
	)
'

test_expect_success 'commit deletion' '
	(
	cd stat-repo &&
	test_tick &&
	grit commit -m "remove new-file"
	)
'

# --- numstat output ---

test_expect_success 'diff --numstat shows nothing for clean tree' '
	(
	cd stat-repo &&
	grit diff --numstat >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff --numstat shows changed file' '
	(
	cd stat-repo &&
	echo "numstat test" >>alpha.txt &&
	grit diff --numstat >actual &&
	grep "alpha.txt" actual
	)
'

test_expect_success 'diff --numstat tab-separated columns' '
	(
	cd stat-repo &&
	grit diff --numstat >actual &&
	awk -F"\t" "NF >= 3" actual | grep "alpha.txt"
	)
'

test_expect_success 'diff --cached --numstat shows staged additions' '
	(
	cd stat-repo &&
	grit add alpha.txt &&
	grit diff --cached --numstat >actual &&
	grep "^1	" actual &&
	grep "alpha.txt" actual
	)
'

test_expect_success 'commit numstat test changes' '
	(
	cd stat-repo &&
	test_tick &&
	grit commit -m "numstat test commit"
	)
'

# --- name-only output ---

test_expect_success 'diff --name-only shows nothing for clean tree' '
	(
	cd stat-repo &&
	grit diff --name-only >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff --name-only lists changed file' '
	(
	cd stat-repo &&
	echo "more" >>beta.txt &&
	grit diff --name-only >actual &&
	grep "^beta.txt$" actual
	)
'

test_expect_success 'diff --name-only does not show stat summary' '
	(
	cd stat-repo &&
	grit diff --name-only >actual &&
	! grep "insertion" actual &&
	! grep "deletion" actual &&
	! grep "file changed" actual
	)
'

test_expect_success 'diff --name-only with multiple files' '
	(
	cd stat-repo &&
	echo "also changed" >>gamma.txt &&
	grit diff --name-only >actual &&
	grep "beta.txt" actual &&
	grep "gamma.txt" actual
	)
'

# --- name-status output ---

test_expect_success 'diff --name-status shows M for modification' '
	(
	cd stat-repo &&
	grit diff --name-status >actual &&
	grep "^M" actual
	)
'

test_expect_success 'diff --name-status lists all modified files' '
	(
	cd stat-repo &&
	grit diff --name-status >actual &&
	grep "beta.txt" actual &&
	grep "gamma.txt" actual
	)
'

test_expect_success 'stage and commit for next tests' '
	(
	cd stat-repo &&
	grit add beta.txt gamma.txt &&
	test_tick &&
	grit commit -m "modify beta gamma"
	)
'

# --- subdirectory paths ---

test_expect_success 'setup: file in subdirectory' '
	(
	cd stat-repo &&
	mkdir -p sub/deep &&
	echo "deep content" >sub/deep/nested.txt &&
	grit add sub/deep/nested.txt &&
	test_tick &&
	grit commit -m "add nested file"
	)
'

test_expect_success 'diff --stat shows full path for nested file' '
	(
	cd stat-repo &&
	echo "changed" >>sub/deep/nested.txt &&
	grit diff --stat >actual &&
	grep "sub/deep/nested.txt" actual
	)
'

test_expect_success 'diff --name-only shows full path for nested file' '
	(
	cd stat-repo &&
	grit diff --name-only >actual &&
	grep "sub/deep/nested.txt" actual
	)
'

test_expect_success 'diff --name-status shows full path for nested file' '
	(
	cd stat-repo &&
	grit diff --name-status >actual &&
	grep "sub/deep/nested.txt" actual
	)
'

test_expect_success 'diff --numstat shows full path for nested file' '
	(
	cd stat-repo &&
	grit diff --numstat >actual &&
	grep "sub/deep/nested.txt" actual
	)
'

test_expect_success 'reset nested file' '
	(
	cd stat-repo &&
	grit checkout -- sub/deep/nested.txt
	)
'

# --- diff --cached with mixed adds and modifications ---

test_expect_success 'diff --cached --stat with mixed staged changes' '
	(
	cd stat-repo &&
	echo "new staged" >fresh.txt &&
	echo "modify alpha more" >>alpha.txt &&
	grit add fresh.txt alpha.txt &&
	grit diff --cached --stat >actual &&
	grep "fresh.txt" actual &&
	grep "alpha.txt" actual &&
	grep "2 files changed" actual
	)
'

test_expect_success 'commit mixed changes' '
	(
	cd stat-repo &&
	test_tick &&
	grit commit -m "mixed staged changes"
	)
'

test_done
