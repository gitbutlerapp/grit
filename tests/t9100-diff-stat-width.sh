#!/bin/sh
# Test diff --stat output formatting and width behavior.

test_description='grit diff --stat output formatting'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup base repository' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Tester" &&
	echo "line1" >file1.txt &&
	grit add file1.txt &&
	grit commit -m "initial"
	)
'

###########################################################################
# Section 1: Basic --stat output
###########################################################################

test_expect_success 'diff --stat shows single file addition' '
	(
	cd repo &&
	echo "new content" >new.txt &&
	grit add new.txt &&
	grit commit -m "add new" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "new.txt" out &&
	grep "1 file changed" out
	)
'

test_expect_success 'diff --stat shows insertion count' '
	(
	cd repo &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "1 insertion" out
	)
'

test_expect_success 'diff --stat shows multiple files changed' '
	(
	cd repo &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	echo "c" >c.txt &&
	grit add a.txt b.txt c.txt &&
	grit commit -m "add three" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "3 files changed" out
	)
'

test_expect_success 'diff --stat shows deletions' '
	(
	cd repo &&
	grit rm c.txt &&
	grit commit -m "remove c" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "deletion" out
	)
'

test_expect_success 'diff --stat shows file modification' '
	(
	cd repo &&
	echo "extra line" >>a.txt &&
	grit add a.txt &&
	grit commit -m "modify a" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "a.txt" out &&
	grep "1 file changed" out
	)
'

###########################################################################
# Section 2: --stat with different file types
###########################################################################

test_expect_success 'diff --stat with renamed file via add/rm' '
	(
	cd repo &&
	cp a.txt renamed.txt &&
	grit rm a.txt &&
	grit add renamed.txt &&
	grit commit -m "rename a to renamed" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "renamed.txt" out
	)
'

test_expect_success 'diff --stat with binary-like content is handled' '
	(
	cd repo &&
	printf "\000binary\000data" >bin.dat &&
	grit add bin.dat &&
	grit commit -m "add binary" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "bin.dat" out
	)
'

test_expect_success 'diff --stat with empty file addition' '
	(
	cd repo &&
	: >empty.txt &&
	grit add empty.txt &&
	grit commit -m "add empty" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "empty.txt" out &&
	grep "1 file changed" out
	)
'

###########################################################################
# Section 3: Long filenames in stat
###########################################################################

test_expect_success 'diff --stat with long filename shows filename' '
	(
	cd repo &&
	echo "content" >this_is_a_rather_long_filename_for_testing_stat_display.txt &&
	grit add this_is_a_rather_long_filename_for_testing_stat_display.txt &&
	grit commit -m "long name" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "this_is_a_rather_long_filename_for_testing_stat_display.txt" out
	)
'

test_expect_success 'diff --stat with multiple long filenames' '
	(
	cd repo &&
	echo "x" >long_name_file_alpha_something_extended_really.txt &&
	echo "y" >long_name_file_beta_something_extended_really.txt &&
	grit add long_name_file_alpha_something_extended_really.txt &&
	grit add long_name_file_beta_something_extended_really.txt &&
	grit commit -m "two long names" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "long_name_file_alpha" out &&
	grep "long_name_file_beta" out &&
	grep "2 files changed" out
	)
'

test_expect_success 'diff --stat with deeply nested path' '
	(
	cd repo &&
	mkdir -p very/deep/nested/directory/structure &&
	echo "deep" >very/deep/nested/directory/structure/file.txt &&
	grit add very/ &&
	grit commit -m "deep path" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "very/deep/nested/directory/structure/file.txt" out
	)
'

###########################################################################
# Section 4: --stat with multi-line changes
###########################################################################

test_expect_success 'diff --stat shows correct count for multi-line addition' '
	(
	cd repo &&
	printf "line1\nline2\nline3\nline4\nline5\n" >multi.txt &&
	grit add multi.txt &&
	grit commit -m "add multi" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "5 insertions" out
	)
'

test_expect_success 'diff --stat shows insertions and deletions on modify' '
	(
	cd repo &&
	printf "replaced1\nreplaced2\nline3\nline4\nline5\n" >multi.txt &&
	grit add multi.txt &&
	grit commit -m "modify multi" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "insertion" out &&
	grep "deletion" out
	)
'

test_expect_success 'diff --stat with large insertion count' '
	(
	cd repo &&
	seq 1 100 >hundred.txt &&
	grit add hundred.txt &&
	grit commit -m "100 lines" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "100 insertions" out
	)
'

###########################################################################
# Section 5: --stat on cached/staged changes
###########################################################################

test_expect_success 'diff --cached --stat shows staged change' '
	(
	cd repo &&
	echo "staged change" >>renamed.txt &&
	grit add renamed.txt &&
	grit diff --cached --stat >out &&
	grep "renamed.txt" out &&
	grep "1 file changed" out
	)
'

test_expect_success 'diff --cached --stat shows no output when clean' '
	(
	cd repo &&
	grit commit -m "commit staged" &&
	grit diff --cached --stat >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 6: --stat summary line format
###########################################################################

test_expect_success 'diff --stat summary says "file changed" for 1 file' '
	(
	cd repo &&
	echo "one" >one.txt &&
	grit add one.txt &&
	grit commit -m "one file" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "1 file changed" out
	)
'

test_expect_success 'diff --stat summary says "files changed" for multiple' '
	(
	cd repo &&
	echo "p" >p.txt &&
	echo "q" >q.txt &&
	grit add p.txt q.txt &&
	grit commit -m "two files" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "2 files changed" out
	)
'

test_expect_success 'diff --stat with only deletions shows no insertions' '
	(
	cd repo &&
	grit rm p.txt &&
	grit commit -m "del p" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "deletion" out
	)
'

test_expect_success 'diff --stat with only insertions shows no deletions' '
	(
	cd repo &&
	echo "fresh" >fresh.txt &&
	grit add fresh.txt &&
	grit commit -m "add fresh" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "insertion" out
	)
'

###########################################################################
# Section 7: --stat combined with other diff options
###########################################################################

test_expect_success 'diff --stat between two explicit commits' '
	(
	cd repo &&
	first=$(git rev-parse HEAD~5) &&
	last=$(git rev-parse HEAD) &&
	grit diff --stat $first $last >out &&
	test -s out
	)
'

test_expect_success 'diff --stat on identical commits produces no output' '
	(
	cd repo &&
	grit diff --stat HEAD HEAD >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff --numstat shows machine-readable output' '
	(
	cd repo &&
	echo "numstat" >ns.txt &&
	grit add ns.txt &&
	grit commit -m "ns" &&
	grit diff --numstat HEAD~1 HEAD >out &&
	grep "^1	0	ns.txt$" out
	)
'

test_expect_success 'diff --name-only lists just filenames' '
	(
	cd repo &&
	grit diff --name-only HEAD~1 HEAD >out &&
	grep "^ns.txt$" out
	)
'

test_expect_success 'diff --name-status shows status letter and name' '
	(
	cd repo &&
	grit diff --name-status HEAD~1 HEAD >out &&
	grep "^A	ns.txt$" out
	)
'

###########################################################################
# Section 8: --stat with file mode changes
###########################################################################

test_expect_success 'diff --stat with permission change shows file' '
	(
	cd repo &&
	chmod +x fresh.txt &&
	grit add fresh.txt &&
	grit commit -m "make executable" &&
	grit diff --stat HEAD~1 HEAD >out &&
	grep "fresh.txt" out
	)
'

test_expect_success 'diff --stat pipe character alignment' '
	(
	cd repo &&
	grit diff --stat HEAD~3 HEAD >out &&
	grep "|" out
	)
'

test_done
