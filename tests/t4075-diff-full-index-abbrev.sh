#!/bin/sh

test_description='grit diff --full-index and --abbrev=N

Tests that --full-index shows full 40-char OIDs in patch index lines
and --abbrev=N controls the abbreviation length.'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "test@test.com" &&
	echo "hello world" >file.txt &&
	git add file.txt &&
	git commit -m "initial" &&
	echo "changed content" >file.txt &&
	git add file.txt &&
	git commit -m "modify"
	)
'

# --full-index in patch output
test_expect_success 'diff --full-index shows 40-char OIDs in index line' '
	(
	cd repo &&
	git diff --full-index HEAD~1 HEAD >out &&
	grep "^index " out >idx &&
	# Extract the old..new OID pair
	oid_part=$(sed "s/^index //; s/ .*//" idx) &&
	old_oid=$(echo "$oid_part" | cut -d. -f1) &&
	new_oid=$(echo "$oid_part" | cut -d. -f3) &&
	test ${#old_oid} -eq 40 &&
	test ${#new_oid} -eq 40
	)
'

# default abbrev (7 chars) in patch output
test_expect_success 'diff without --full-index shows 7-char OIDs in index line' '
	(
	cd repo &&
	git diff HEAD~1 HEAD >out &&
	grep "^index " out >idx &&
	oid_part=$(sed "s/^index //; s/ .*//" idx) &&
	old_oid=$(echo "$oid_part" | cut -d. -f1) &&
	test ${#old_oid} -eq 7
	)
'

# --abbrev=12 in patch output
test_expect_success 'diff --abbrev=12 shows 12-char OIDs in index line' '
	(
	cd repo &&
	git diff --abbrev=12 HEAD~1 HEAD >out &&
	grep "^index " out >idx &&
	oid_part=$(sed "s/^index //; s/ .*//" idx) &&
	old_oid=$(echo "$oid_part" | cut -d. -f1) &&
	test ${#old_oid} -eq 12
	)
'

# --full-index in raw output
test_expect_success 'diff --raw --full-index shows 40-char OIDs' '
	(
	cd repo &&
	git diff --raw --full-index HEAD~1 HEAD >out &&
	oid3=$(awk "{print \$3}" out) &&
	oid4=$(awk "{print \$4}" out) &&
	test ${#oid3} -eq 40 &&
	test ${#oid4} -eq 40
	)
'

# --abbrev=10 in raw output
test_expect_success 'diff --raw --abbrev=10 shows 10-char OIDs' '
	(
	cd repo &&
	git diff --raw --abbrev=10 HEAD~1 HEAD >out &&
	oid3=$(awk "{print \$3}" out) &&
	oid4=$(awk "{print \$4}" out) &&
	test ${#oid3} -eq 10 &&
	test ${#oid4} -eq 10
	)
'

# --no-abbrev in raw output (full OIDs)
test_expect_success 'diff --raw --no-abbrev shows full OIDs' '
	(
	cd repo &&
	git diff --raw --no-abbrev HEAD~1 HEAD >out &&
	oid3=$(awk "{print \$3}" out) &&
	test ${#oid3} -eq 40
	)
'

test_done
