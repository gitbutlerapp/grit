#!/bin/sh
# Test ls-files -z (NUL-terminated output) with various flags:
# --cached, --stage/-s, --deleted, --modified, --others, pathspecs,
# and combinations.

test_description='grit ls-files NUL-terminated output (-z)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "alpha" >alpha.txt &&
	echo "beta" >beta.txt &&
	echo "gamma" >gamma.md &&
	echo "delta" >delta.md &&
	echo "readme" >README.md &&
	mkdir -p src &&
	echo "main" >src/main.rs &&
	echo "lib" >src/lib.rs &&
	mkdir -p docs &&
	echo "guide" >docs/guide.md &&
	echo "api" >docs/api.md &&
	mkdir -p test &&
	echo "t1" >test/test1.sh &&
	echo "t2" >test/test2.sh &&
	grit add . &&
	test_tick &&
	grit commit -m "initial"
	)
'

# --- basic -z ---

test_expect_success 'ls-files -z produces NUL-separated output' '
	(
	cd repo &&
	grit ls-files -z >raw &&
	nul_count=$(tr -cd "\0" <raw | wc -c | tr -d " ") &&
	test "$nul_count" -gt 0
	)
'

test_expect_success 'ls-files -z entry count matches non-z' '
	(
	cd repo &&
	grit ls-files >nonz &&
	nonz_count=$(wc -l <nonz | tr -d " ") &&
	grit ls-files -z >zout &&
	z_count=$(tr -cd "\0" <zout | wc -c | tr -d " ") &&
	test "$nonz_count" = "$z_count"
	)
'

test_expect_success 'ls-files -z split on NUL matches normal output' '
	(
	cd repo &&
	grit ls-files -z | tr "\0" "\n" | grep -v "^$" >from_z &&
	grit ls-files >from_normal &&
	test_cmp from_normal from_z
	)
'

test_expect_success 'ls-files -z: all 11 files present' '
	(
	cd repo &&
	grit ls-files -z >raw &&
	z_count=$(tr -cd "\0" <raw | wc -c | tr -d " ") &&
	test "$z_count" = "11"
	)
'

test_expect_success 'ls-files -z: entries contain expected names' '
	(
	cd repo &&
	grit ls-files -z | tr "\0" "\n" >converted &&
	grep "alpha.txt" converted &&
	grep "src/main.rs" converted &&
	grep "docs/guide.md" converted &&
	grep "test/test1.sh" converted
	)
'

# --- -z with pathspec ---

test_expect_success 'ls-files -z with single file pathspec' '
	(
	cd repo &&
	grit ls-files -z alpha.txt | tr "\0" "\n" | grep -v "^$" >actual &&
	test_line_count = 1 actual &&
	grep "alpha.txt" actual
	)
'

test_expect_success 'ls-files -z with directory pathspec' '
	(
	cd repo &&
	grit ls-files -z src | tr "\0" "\n" | grep -v "^$" >actual &&
	test_line_count = 2 actual &&
	grep "src/main.rs" actual &&
	grep "src/lib.rs" actual
	)
'

test_expect_success 'ls-files -z with docs directory' '
	(
	cd repo &&
	grit ls-files -z docs | tr "\0" "\n" | grep -v "^$" >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'ls-files -z with multiple pathspecs' '
	(
	cd repo &&
	grit ls-files -z alpha.txt beta.txt | tr "\0" "\n" | grep -v "^$" >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'ls-files -z with nonexistent path returns empty' '
	(
	cd repo &&
	grit ls-files -z nonexistent >actual &&
	! tr "\0" "\n" <actual | grep -v "^$" | grep .
	)
'

# --- -z with -s (--stage) ---

test_expect_success 'ls-files -z -s produces NUL-separated staged output' '
	(
	cd repo &&
	grit ls-files -z -s >raw &&
	nul_count=$(tr -cd "\0" <raw | wc -c | tr -d " ") &&
	test "$nul_count" = "11"
	)
'

test_expect_success 'ls-files -z -s: split entries have mode and OID' '
	(
	cd repo &&
	grit ls-files -z -s | tr "\0" "\n" | grep -v "^$" >converted &&
	head -1 converted | grep -qE "^[0-9]{6} [0-9a-f]{40} [0-9]"
	)
'

test_expect_success 'ls-files -z -s: matches non-z -s output' '
	(
	cd repo &&
	grit ls-files -z -s | tr "\0" "\n" | grep -v "^$" >from_z &&
	grit ls-files -s >from_normal &&
	test_cmp from_normal from_z
	)
'

test_expect_success 'ls-files -z -s with pathspec' '
	(
	cd repo &&
	grit ls-files -z -s alpha.txt | tr "\0" "\n" | grep -v "^$" >actual &&
	test_line_count = 1 actual &&
	grep "alpha.txt" actual
	)
'

test_expect_success 'ls-files -z -s with directory pathspec' '
	(
	cd repo &&
	grit ls-files -z -s src | tr "\0" "\n" | grep -v "^$" >actual &&
	test_line_count = 2 actual
	)
'

# --- -z with --deleted ---

test_expect_success 'ls-files -z --deleted shows deleted files' '
	(
	cd repo &&
	rm alpha.txt &&
	grit ls-files -z --deleted | tr "\0" "\n" | grep -v "^$" >actual &&
	grep "alpha.txt" actual &&
	echo "alpha" >alpha.txt
	)
'

test_expect_success 'ls-files -z --deleted: multiple deletions' '
	(
	cd repo &&
	rm alpha.txt beta.txt &&
	grit ls-files -z --deleted | tr "\0" "\n" | grep -v "^$" >actual &&
	grep "alpha.txt" actual &&
	grep "beta.txt" actual &&
	echo "alpha" >alpha.txt &&
	echo "beta" >beta.txt
	)
'

test_expect_success 'ls-files -z --deleted with pathspec' '
	(
	cd repo &&
	rm gamma.md &&
	grit ls-files -z --deleted gamma.md | tr "\0" "\n" | grep -v "^$" >actual &&
	grep "gamma.md" actual &&
	echo "gamma" >gamma.md
	)
'

# --- -z with --modified ---

test_expect_success 'ls-files -z --modified shows modified files' '
	(
	cd repo &&
	echo "changed" >alpha.txt &&
	grit ls-files -z --modified | tr "\0" "\n" | grep -v "^$" >actual &&
	grep "alpha.txt" actual &&
	echo "alpha" >alpha.txt
	)
'

test_expect_success 'ls-files -z --modified with pathspec' '
	(
	cd repo &&
	echo "changed" >alpha.txt &&
	echo "also changed" >beta.txt &&
	grit ls-files -z --modified alpha.txt | tr "\0" "\n" | grep -v "^$" >actual &&
	grep "alpha.txt" actual &&
	! grep "beta.txt" actual &&
	echo "alpha" >alpha.txt &&
	echo "beta" >beta.txt
	)
'

# --- -z with --cached ---

test_expect_success 'ls-files -z --cached same as -z default' '
	(
	cd repo &&
	grit ls-files -z >default_z &&
	grit ls-files -z --cached >cached_z &&
	test_cmp default_z cached_z
	)
'

# --- after index changes ---

test_expect_success 'ls-files -z after adding new file' '
	(
	cd repo &&
	echo "new" >newfile.txt &&
	grit update-index --add newfile.txt &&
	grit ls-files -z | tr "\0" "\n" | grep -v "^$" >actual &&
	grep "newfile.txt" actual &&
	count=$(wc -l <actual | tr -d " ") &&
	test "$count" = "12"
	)
'

test_expect_success 'ls-files -z after removing file from index' '
	(
	cd repo &&
	grit update-index --force-remove newfile.txt &&
	grit ls-files -z | tr "\0" "\n" | grep -v "^$" >actual &&
	! grep "newfile.txt" actual &&
	count=$(wc -l <actual | tr -d " ") &&
	test "$count" = "11"
	)
'

# --- output order ---

test_expect_success 'ls-files -z output is sorted' '
	(
	cd repo &&
	grit ls-files -z | tr "\0" "\n" | grep -v "^$" >actual &&
	sort actual >sorted &&
	test_cmp actual sorted
	)
'

test_expect_success 'ls-files -z -s output is sorted by path' '
	(
	cd repo &&
	grit ls-files -z -s | tr "\0" "\n" | grep -v "^$" >actual &&
	# Extract path (after tab) and check sorted
	awk -F"\t" "{print \$2}" actual >paths &&
	sort paths >sorted_paths &&
	test_cmp paths sorted_paths
	)
'

# --- -z with nested directories ---

test_expect_success 'ls-files -z shows nested paths correctly' '
	(
	cd repo &&
	grit ls-files -z | tr "\0" "\n" >converted &&
	grep "src/main.rs" converted &&
	grep "src/lib.rs" converted &&
	grep "docs/guide.md" converted &&
	grep "docs/api.md" converted &&
	grep "test/test1.sh" converted &&
	grep "test/test2.sh" converted
	)
'

# --- after second commit ---

test_expect_success 'setup second commit with more files' '
	(
	cd repo &&
	echo "epsilon" >epsilon.txt &&
	mkdir -p deep/nested &&
	echo "deep" >deep/nested/file.txt &&
	grit add . &&
	test_tick &&
	grit commit -m "second"
	)
'

test_expect_success 'ls-files -z after second commit shows new files' '
	(
	cd repo &&
	grit ls-files -z | tr "\0" "\n" | grep -v "^$" >actual &&
	grep "epsilon.txt" actual &&
	grep "deep/nested/file.txt" actual &&
	count=$(wc -l <actual | tr -d " ") &&
	test "$count" -ge 13
	)
'

test_expect_success 'ls-files -z -s after second commit has at least 13 entries' '
	(
	cd repo &&
	grit ls-files -z -s >raw &&
	z_count=$(tr -cd "\0" <raw | wc -c | tr -d " ") &&
	test "$z_count" -ge 13
	)
'

test_expect_success 'ls-files -z with deep directory pathspec' '
	(
	cd repo &&
	grit ls-files -z deep | tr "\0" "\n" | grep -v "^$" >actual &&
	test_line_count = 1 actual &&
	grep "deep/nested/file.txt" actual
	)
'

test_done
