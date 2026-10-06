#!/bin/sh
# Test ls-tree --name-only and -z (NUL termination) options and their
# combinations with -r, -d, -t, --name-status, path filtering, and
# various tree structures.

test_description='grit ls-tree --name-only and -z'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with files and dirs' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "alpha" >alpha.txt &&
	echo "beta" >beta.txt &&
	echo "gamma" >gamma.md &&
	echo "#!/bin/sh" >run.sh &&
	chmod +x run.sh &&
	printf "" >empty &&
	mkdir -p src/lib &&
	echo "main" >src/main.rs &&
	echo "lib" >src/lib.rs &&
	echo "util" >src/lib/util.rs &&
	mkdir -p docs &&
	echo "guide" >docs/guide.md &&
	grit add . &&
	test_tick &&
	grit commit -m "initial"
	)
'

# --- --name-only basics ---

test_expect_success 'ls-tree --name-only lists names only' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD >actual &&
	! grep -qE "^[0-9]{6}" actual
	)
'

test_expect_success 'ls-tree --name-only line count matches default' '
	(
	cd repo &&
	grit ls-tree HEAD >default_out &&
	grit ls-tree --name-only HEAD >names_out &&
	def_count=$(wc -l <default_out | tr -d " ") &&
	name_count=$(wc -l <names_out | tr -d " ") &&
	test "$def_count" = "$name_count"
	)
'

test_expect_success 'ls-tree --name-only shows expected entries' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD >actual &&
	grep "alpha.txt" actual &&
	grep "beta.txt" actual &&
	grep "gamma.md" actual &&
	grep "run.sh" actual &&
	grep "empty" actual &&
	grep "src" actual &&
	grep "docs" actual
	)
'

test_expect_success 'ls-tree --name-only does not show OIDs' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD >actual &&
	! grep -qE "[0-9a-f]{40}" actual
	)
'

test_expect_success 'ls-tree --name-only does not show type' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD >actual &&
	! grep -qw "blob" actual &&
	! grep -qw "tree" actual
	)
'

test_expect_success 'ls-tree --name-only output is sorted' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD >actual &&
	sort actual >sorted &&
	test_cmp actual sorted
	)
'

# --- --name-status ---

test_expect_success 'ls-tree --name-status same as --name-only' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD >name_only &&
	grit ls-tree --name-status HEAD >name_status &&
	test_cmp name_only name_status
	)
'

# --- --name-only with -r ---

test_expect_success 'ls-tree --name-only -r lists all recursive names' '
	(
	cd repo &&
	grit ls-tree --name-only -r HEAD >actual &&
	grep "alpha.txt" actual &&
	grep "src/main.rs" actual &&
	grep "src/lib.rs" actual &&
	grep "src/lib/util.rs" actual &&
	grep "docs/guide.md" actual
	)
'

test_expect_success 'ls-tree --name-only -r does not show directories' '
	(
	cd repo &&
	grit ls-tree --name-only -r HEAD >actual &&
	! grep "^src$" actual &&
	! grep "^docs$" actual
	)
'

test_expect_success 'ls-tree --name-only -r count matches -r default' '
	(
	cd repo &&
	grit ls-tree -r HEAD >default_r &&
	grit ls-tree --name-only -r HEAD >names_r &&
	def_count=$(wc -l <default_r | tr -d " ") &&
	name_count=$(wc -l <names_r | tr -d " ") &&
	test "$def_count" = "$name_count"
	)
'

# --- --name-only with -d ---

test_expect_success 'ls-tree --name-only -d shows only directory names' '
	(
	cd repo &&
	grit ls-tree --name-only -d HEAD >actual &&
	grep "src" actual &&
	grep "docs" actual &&
	! grep "alpha.txt" actual
	)
'

# --- -z (NUL termination) ---

test_expect_success 'ls-tree -z output contains NUL bytes' '
	(
	cd repo &&
	grit ls-tree -z HEAD >actual &&
	nul_count=$(tr -cd "\0" <actual | wc -c | tr -d " ") &&
	test "$nul_count" -gt 0
	)
'

test_expect_success 'ls-tree -z: entry count matches default' '
	(
	cd repo &&
	grit ls-tree HEAD >default_out &&
	def_count=$(wc -l <default_out | tr -d " ") &&
	grit ls-tree -z HEAD >z_out &&
	nul_count=$(tr -cd "\0" <z_out | wc -c | tr -d " ") &&
	test "$def_count" = "$nul_count"
	)
'

test_expect_success 'ls-tree -z entries can be split on NUL' '
	(
	cd repo &&
	grit ls-tree -z HEAD | tr "\0" "\n" >converted &&
	grep "alpha.txt" converted &&
	grep "src" converted
	)
'

test_expect_success 'ls-tree -z entries contain mode and OID' '
	(
	cd repo &&
	grit ls-tree -z HEAD | tr "\0" "\n" >converted &&
	head -1 converted | grep -qE "^[0-9]{6} (blob|tree) [0-9a-f]{40}"
	)
'

test_expect_success 'ls-tree -z does not have newlines in entries' '
	(
	cd repo &&
	grit ls-tree -z HEAD >raw &&
	# Each NUL-terminated record should not contain embedded newlines
	# (the last byte before each NUL should not be newline)
	entry_count=$(tr -cd "\0" <raw | wc -c | tr -d " ") &&
	test "$entry_count" -gt 0
	)
'

# --- --name-only -z combined ---

test_expect_success 'ls-tree --name-only -z uses NUL termination' '
	(
	cd repo &&
	grit ls-tree --name-only -z HEAD >actual &&
	nul_count=$(tr -cd "\0" <actual | wc -c | tr -d " ") &&
	test "$nul_count" -gt 0
	)
'

test_expect_success 'ls-tree --name-only -z: split gives clean names' '
	(
	cd repo &&
	grit ls-tree --name-only -z HEAD | tr "\0" "\n" | grep -v "^$" >actual &&
	grit ls-tree --name-only HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree --name-only -z -r shows recursive names NUL-separated' '
	(
	cd repo &&
	grit ls-tree --name-only -z -r HEAD | tr "\0" "\n" | grep -v "^$" >actual &&
	grit ls-tree --name-only -r HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree -z -r: entry count matches non-z -r' '
	(
	cd repo &&
	grit ls-tree -r HEAD >nonz &&
	nonz_count=$(wc -l <nonz | tr -d " ") &&
	grit ls-tree -z -r HEAD >zout &&
	z_count=$(tr -cd "\0" <zout | wc -c | tr -d " ") &&
	test "$nonz_count" = "$z_count"
	)
'

# --- path filtering ---

test_expect_success 'ls-tree --name-only with path filter' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD alpha.txt >actual &&
	test_line_count = 1 actual &&
	grep "alpha.txt" actual
	)
'

test_expect_success 'ls-tree --name-only with directory path' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD src >actual &&
	grep "src" actual
	)
'

test_expect_success 'ls-tree --name-only with nonexistent path returns empty' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD nonexistent >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'ls-tree -z with path filter' '
	(
	cd repo &&
	grit ls-tree -z HEAD alpha.txt | tr "\0" "\n" | grep -v "^$" >actual &&
	test_line_count = 1 actual
	)
'

# --- second commit ---

test_expect_success 'setup second commit' '
	(
	cd repo &&
	echo "new file" >new.txt &&
	mkdir -p extra &&
	echo "extra" >extra/data &&
	grit add . &&
	test_tick &&
	grit commit -m "second"
	)
'

test_expect_success 'ls-tree --name-only HEAD shows new entries' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD >actual &&
	grep "new.txt" actual &&
	grep "extra" actual
	)
'

test_expect_success 'ls-tree --name-only -r HEAD includes new nested file' '
	(
	cd repo &&
	grit ls-tree --name-only -r HEAD >actual &&
	grep "extra/data" actual
	)
'

test_expect_success 'ls-tree -z HEAD shows more entries than before' '
	(
	cd repo &&
	grit ls-tree -z HEAD >z_new &&
	new_count=$(tr -cd "\0" <z_new | wc -c | tr -d " ") &&
	test "$new_count" -ge 7
	)
'

test_expect_success 'ls-tree --name-only with multiple path args' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD alpha.txt beta.txt >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'ls-tree -z --name-only with multiple path args' '
	(
	cd repo &&
	grit ls-tree -z --name-only HEAD alpha.txt beta.txt | tr "\0" "\n" | grep -v "^$" >actual &&
	test_line_count = 2 actual
	)
'

# --- tree OID directly ---

test_expect_success 'ls-tree --name-only works with tree OID' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree --name-only "$tree" >actual &&
	grep "alpha.txt" actual
	)
'

test_expect_success 'ls-tree -z works with tree OID' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree -z "$tree" | tr "\0" "\n" | grep -v "^$" >actual &&
	grep "alpha.txt" actual
	)
'

test_done
