#!/bin/sh
# Test ls-tree -l (--long), --name-only, --format, -d, -r, -t, -z options.

test_description='grit ls-tree long format'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with various entries' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "hello world" >hello &&
	echo "short" >short &&
	printf "x%.0s" $(seq 1 1000) >big &&
	printf "" >empty &&
	mkdir -p sub/deep &&
	echo "nested file" >sub/nested &&
	echo "deep file" >sub/deep/leaf &&
	echo "#!/bin/sh" >exec.sh &&
	chmod +x exec.sh &&
	grit add . &&
	test_tick &&
	grit commit -m "initial" &&
	grit rev-parse HEAD >../initial_commit
	)
'

test_expect_success 'ls-tree default format shows mode type oid name' '
	(
	cd repo &&
	grit ls-tree HEAD >actual &&
	head -1 actual | grep -qE "^[0-9]{6} (blob|tree) [0-9a-f]{40}	"
	)
'

test_expect_success 'ls-tree -l produces output with extra column' '
	(
	cd repo &&
	grit ls-tree -l HEAD >actual &&
	# -l adds a size/dash column between oid and name
	grep "hello" actual | awk -F"	" "{print NF}" >fields &&
	nf=$(cat fields) &&
	test "$nf" -ge 2
	)
'

test_expect_success 'ls-tree -l has more columns than default' '
	(
	cd repo &&
	grit ls-tree HEAD | grep "hello" >default_line &&
	grit ls-tree -l HEAD | grep "hello" >long_line &&
	def_len=$(wc -c <default_line | tr -d " ") &&
	long_len=$(wc -c <long_line | tr -d " ") &&
	test "$long_len" -gt "$def_len"
	)
'

test_expect_success 'ls-tree -l tree entries show dash for size' '
	(
	cd repo &&
	grit ls-tree -l HEAD >actual &&
	grep "040000" actual >trees &&
	while read line; do
		echo "$line" | grep -q "-" || return 1
	done <trees
	)
'

test_expect_success 'ls-tree --long is same as -l' '
	(
	cd repo &&
	grit ls-tree -l HEAD >expected &&
	grit ls-tree --long HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'ls-tree --name-only shows only names' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD >actual &&
	while read name; do
		echo "$name" | grep -qvE "^[0-9]{6}" || return 1
	done <actual
	)
'

test_expect_success 'ls-tree --name-only entry count matches default' '
	(
	cd repo &&
	grit ls-tree HEAD >default_out &&
	grit ls-tree --name-only HEAD >names_out &&
	test_line_count = $(wc -l <default_out | tr -d " ") names_out
	)
'

test_expect_success 'ls-tree --name-only lists expected files' '
	(
	cd repo &&
	grit ls-tree --name-only HEAD >actual &&
	grep "hello" actual &&
	grep "short" actual &&
	grep "big" actual &&
	grep "empty" actual &&
	grep "sub" actual
	)
'

test_expect_success 'ls-tree -d shows only tree entries' '
	(
	cd repo &&
	grit ls-tree -d HEAD >actual &&
	while read mode type oid name; do
		test "$type" = "tree" || return 1
	done <actual
	)
'

test_expect_success 'ls-tree -d does not show blob entries' '
	(
	cd repo &&
	grit ls-tree -d HEAD >actual &&
	! grep "blob" actual
	)
'

test_expect_success 'ls-tree -r recurses into subdirectories' '
	(
	cd repo &&
	grit ls-tree -r HEAD >actual &&
	grep "sub/nested" actual &&
	grep "sub/deep/leaf" actual
	)
'

test_expect_success 'ls-tree -r shows only blobs by default' '
	(
	cd repo &&
	grit ls-tree -r HEAD >actual &&
	while read mode type oid rest; do
		test "$type" = "blob" || return 1
	done <actual
	)
'

test_expect_success 'ls-tree -r lists all files including top-level' '
	(
	cd repo &&
	grit ls-tree -r HEAD >actual &&
	grep "hello" actual &&
	grep "exec.sh" actual
	)
'

test_expect_success 'ls-tree -r -t shows trees while recursing' '
	(
	cd repo &&
	grit ls-tree -r -t HEAD >actual &&
	grep "tree" actual &&
	grep "blob" actual
	)
'

test_expect_success 'ls-tree -r -t shows sub/ as tree entry' '
	(
	cd repo &&
	grit ls-tree -r -t HEAD >actual &&
	grep "040000 tree.*	sub$" actual
	)
'

test_expect_success 'ls-tree -r -t shows sub/deep as tree entry' '
	(
	cd repo &&
	grit ls-tree -r -t HEAD >actual &&
	grep "040000 tree.*	sub/deep$" actual
	)
'

test_expect_success 'ls-tree with path restricts output' '
	(
	cd repo &&
	grit ls-tree HEAD hello >actual &&
	test_line_count = 1 actual &&
	grep "hello" actual
	)
'

test_expect_success 'ls-tree with directory path shows directory entry' '
	(
	cd repo &&
	grit ls-tree HEAD sub >actual &&
	grep "sub" actual
	)
'

test_expect_success 'ls-tree with nonexistent path shows nothing' '
	(
	cd repo &&
	grit ls-tree HEAD nonexistent >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'ls-tree -z uses NUL terminator' '
	(
	cd repo &&
	grit ls-tree -z HEAD >actual &&
	tr "\0" "\n" <actual >converted &&
	test_line_count -gt 0 converted
	)
'

test_expect_success 'ls-tree executable file shows 100755' '
	(
	cd repo &&
	grit ls-tree HEAD >actual &&
	grep "exec.sh" actual | grep -q "100755"
	)
'

test_expect_success 'ls-tree -l executable file also shows 100755' '
	(
	cd repo &&
	grit ls-tree -l HEAD >actual &&
	grep "exec.sh" actual | grep -q "100755"
	)
'

test_expect_success 'setup second commit with more files' '
	(
	cd repo &&
	echo "alpha" >alpha &&
	echo "omega" >omega &&
	mkdir -p another &&
	echo "file" >another/file &&
	grit add . &&
	test_tick &&
	grit commit -m "second"
	)
'

test_expect_success 'ls-tree HEAD shows updated tree' '
	(
	cd repo &&
	grit ls-tree HEAD >actual &&
	grep "alpha" actual &&
	grep "omega" actual &&
	grep "another" actual
	)
'

test_expect_success 'ls-tree of initial commit tree differs from HEAD' '
	(
	cd repo &&
	initial=$(cat ../initial_commit) &&
	initial_tree=$(grit rev-parse "${initial}^{tree}") &&
	head_tree=$(grit rev-parse HEAD^{tree}) &&
	test "$initial_tree" != "$head_tree" &&
	grit ls-tree "$initial_tree" >old_ls &&
	grit ls-tree "$head_tree" >new_ls &&
	! test_cmp old_ls new_ls >/dev/null 2>&1
	)
'

test_expect_success 'ls-tree --name-only -r lists all recursive names' '
	(
	cd repo &&
	grit ls-tree --name-only -r HEAD >actual &&
	grep "sub/deep/leaf" actual &&
	grep "another/file" actual
	)
'

test_expect_success 'ls-tree format: %(objectname)' '
	(
	cd repo &&
	grit ls-tree --format="%(objectname)" HEAD >actual &&
	while read oid; do
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" || return 1
	done <actual
	)
'

test_expect_success 'ls-tree format: %(objecttype)' '
	(
	cd repo &&
	grit ls-tree --format="%(objecttype)" HEAD >actual &&
	while read t; do
		case "$t" in blob|tree) ;; *) return 1 ;; esac
	done <actual
	)
'

test_expect_success 'ls-tree format: %(path)' '
	(
	cd repo &&
	grit ls-tree --format="%(path)" HEAD >actual &&
	grit ls-tree --name-only HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree format: combined custom format' '
	(
	cd repo &&
	grit ls-tree --format="%(objecttype) %(path)" HEAD >actual &&
	grep "blob hello" actual &&
	grep "tree sub" actual
	)
'

test_expect_success 'ls-tree format: %(objectname) matches default column' '
	(
	cd repo &&
	grit ls-tree --format="%(objectname)" HEAD >format_oids &&
	grit ls-tree HEAD | awk "{print \$3}" >default_oids &&
	test_cmp default_oids format_oids
	)
'

test_expect_success 'ls-tree HEAD^{tree} same as HEAD' '
	(
	cd repo &&
	tree_oid=$(grit rev-parse HEAD^{tree}) &&
	grit ls-tree HEAD >from_head &&
	grit ls-tree "$tree_oid" >from_oid &&
	test_cmp from_head from_oid
	)
'

test_expect_success 'ls-tree with multiple paths' '
	(
	cd repo &&
	grit ls-tree HEAD hello short >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'ls-tree -d shows correct number of tree entries' '
	(
	cd repo &&
	grit ls-tree -d HEAD >top_trees &&
	count=$(wc -l <top_trees | tr -d " ") &&
	test "$count" -ge 1
	)
'

test_expect_success 'ls-tree -r count is more than non-recursive' '
	(
	cd repo &&
	grit ls-tree HEAD >nonrec &&
	grit ls-tree -r HEAD >rec &&
	nr=$(wc -l <nonrec | tr -d " ") &&
	r=$(wc -l <rec | tr -d " ") &&
	test "$r" -gt "$nr"
	)
'

test_done
