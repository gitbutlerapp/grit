#!/bin/sh
# Tests for grit ls-files -s (stage) with various file modes and states.

test_description='grit ls-files --stage modes and stage numbers'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repository with various file types' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "regular" >regular.txt &&
	echo "another" >another.txt &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	mkdir -p sub/deep &&
	echo "nested" >sub/nested.txt &&
	echo "deeper" >sub/deep/file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial commit"
	)
'

###########################################################################
# Section 2: Basic ls-files -s output
###########################################################################

test_expect_success 'ls-files -s shows staged files' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	test $(wc -l <actual) -gt 0
	)
'

test_expect_success 'ls-files -s output has four columns' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	while IFS= read -r line; do
		mode=$(echo "$line" | awk "{print \$1}") &&
		oid=$(echo "$line" | awk "{print \$2}") &&
		stage=$(echo "$line" | awk "{print \$3}") &&
		echo "$mode" | grep -qE "^[0-9]{6}$" ||
			{ echo "bad mode: $mode"; return 1; } &&
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" ||
			{ echo "bad oid: $oid"; return 1; } &&
		echo "$stage" | grep -qE "^[0-9]+$" ||
			{ echo "bad stage: $stage"; return 1; }
	done <actual
	)
'

test_expect_success 'ls-files -s matches real git' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	"$REAL_GIT" ls-files -s >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-files --stage is same as -s' '
	(
	cd repo &&
	grit ls-files -s >s_out &&
	grit ls-files --stage >stage_out &&
	test_cmp s_out stage_out
	)
'

###########################################################################
# Section 3: Mode verification
###########################################################################

test_expect_success 'ls-files -s shows 100644 for regular files' '
	(
	cd repo &&
	grit ls-files -s regular.txt >actual &&
	grep "^100644" actual
	)
'

test_expect_success 'ls-files -s shows 100755 for executable' '
	(
	cd repo &&
	grit ls-files -s script.sh >actual &&
	grep "^100755" actual
	)
'

test_expect_success 'ls-files -s regular mode matches real git' '
	(
	cd repo &&
	grit ls-files -s regular.txt >actual &&
	"$REAL_GIT" ls-files -s regular.txt >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-files -s executable mode matches real git' '
	(
	cd repo &&
	grit ls-files -s script.sh >actual &&
	"$REAL_GIT" ls-files -s script.sh >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 4: Stage numbers
###########################################################################

test_expect_success 'ls-files -s stage number is 0 for normal files' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	while IFS= read -r line; do
		stage=$(echo "$line" | awk "{print \$3}") &&
		test "$stage" = "0" ||
			{ echo "unexpected stage: $stage"; return 1; }
	done <actual
	)
'

test_expect_success 'ls-files -s stage 0 matches real git stage 0' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	"$REAL_GIT" ls-files -s >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 5: Nested files in ls-files -s
###########################################################################

test_expect_success 'ls-files -s includes nested files' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	grep "sub/nested.txt" actual
	)
'

test_expect_success 'ls-files -s includes deeply nested files' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	grep "sub/deep/file.txt" actual
	)
'

test_expect_success 'ls-files -s nested files are all 100644' '
	(
	cd repo &&
	grit ls-files -s sub/ >actual &&
	while IFS= read -r line; do
		mode=$(echo "$line" | awk "{print \$1}") &&
		test "$mode" = "100644" ||
			{ echo "unexpected mode for sub/ file: $mode"; return 1; }
	done <actual
	)
'

test_expect_success 'ls-files -s nested matches real git' '
	(
	cd repo &&
	grit ls-files -s sub/ >actual &&
	"$REAL_GIT" ls-files -s sub/ >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 6: OID verification
###########################################################################

test_expect_success 'ls-files -s OID matches hash-object' '
	(
	cd repo &&
	expected_oid=$(grit hash-object regular.txt) &&
	actual_oid=$(grit ls-files -s regular.txt | awk "{print \$2}") &&
	test "$expected_oid" = "$actual_oid"
	)
'

test_expect_success 'ls-files -s OID matches hash-object for executable' '
	(
	cd repo &&
	expected_oid=$(grit hash-object script.sh) &&
	actual_oid=$(grit ls-files -s script.sh | awk "{print \$2}") &&
	test "$expected_oid" = "$actual_oid"
	)
'

test_expect_success 'ls-files -s OID matches hash-object for nested file' '
	(
	cd repo &&
	expected_oid=$(grit hash-object sub/nested.txt) &&
	actual_oid=$(grit ls-files -s sub/nested.txt | awk "{print \$2}") &&
	test "$expected_oid" = "$actual_oid"
	)
'

###########################################################################
# Section 7: ls-files -s after modifications
###########################################################################

test_expect_success 'ls-files -s unchanged after working tree modification' '
	(
	cd repo &&
	grit ls-files -s regular.txt >before &&
	echo "modified" >regular.txt &&
	grit ls-files -s regular.txt >after &&
	test_cmp before after
	)
'

test_expect_success 'ls-files -s changes after update-index' '
	(
	cd repo &&
	grit ls-files -s regular.txt >before &&
	echo "new content" >regular.txt &&
	grit update-index regular.txt &&
	grit ls-files -s regular.txt >after &&
	! test_cmp before after
	)
'

test_expect_success 'ls-files -s after update-index matches real git' '
	(
	cd repo &&
	"$REAL_GIT" add regular.txt &&
	grit ls-files -s regular.txt >actual &&
	"$REAL_GIT" ls-files -s regular.txt >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 8: ls-files -s with new files
###########################################################################

test_expect_success 'ls-files -s shows newly added file' '
	(
	cd repo &&
	echo "brand new" >new_file.txt &&
	grit update-index --add new_file.txt &&
	grit ls-files -s new_file.txt >actual &&
	grep "100644" actual &&
	grep "new_file.txt" actual
	)
'

test_expect_success 'ls-files -s new file matches real git' '
	(
	cd repo &&
	"$REAL_GIT" add new_file.txt &&
	grit ls-files -s new_file.txt >actual &&
	"$REAL_GIT" ls-files -s new_file.txt >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-files -s new executable file shows 100755' '
	(
	cd repo &&
	echo "#!/bin/sh" >new_exec.sh &&
	chmod +x new_exec.sh &&
	grit update-index --add new_exec.sh &&
	grit ls-files -s new_exec.sh >actual &&
	grep "^100755" actual
	)
'

###########################################################################
# Section 9: ls-files -s entry count
###########################################################################

test_expect_success 'ls-files -s count matches ls-files count' '
	(
	cd repo &&
	grit ls-files >plain &&
	grit ls-files -s >staged &&
	test $(wc -l <plain) -eq $(wc -l <staged)
	)
'

test_expect_success 'ls-files -s count matches real git count' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	"$REAL_GIT" ls-files -s >expect &&
	test $(wc -l <actual) -eq $(wc -l <expect)
	)
'

###########################################################################
# Section 10: ls-files -s sorted output
###########################################################################

test_expect_success 'ls-files -s output is sorted by path' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	awk -F"\t" "{print \$2}" actual >paths &&
	sort paths >sorted_paths &&
	test_cmp sorted_paths paths
	)
'

test_expect_success 'ls-files -s full output matches real git' '
	(
	cd repo &&
	"$REAL_GIT" add . &&
	grit ls-files -s >actual &&
	"$REAL_GIT" ls-files -s >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 11: ls-files -s with deleted staged files
###########################################################################

test_expect_success 'ls-files -s still shows file after working tree delete' '
	(
	cd repo &&
	grit ls-files -s another.txt >before &&
	rm another.txt &&
	grit ls-files -s another.txt >after &&
	test_cmp before after
	)
'

test_expect_success 'ls-files -s OID for blob is retrievable via cat-file' '
	(
	cd repo &&
	oid=$(grit ls-files -s sub/nested.txt | awk "{print \$2}") &&
	grit cat-file -p "$oid" >actual &&
	echo "nested" >expect &&
	test_cmp expect actual
	)
'

test_done
