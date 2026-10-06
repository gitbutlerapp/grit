#!/bin/sh
# Tests for grit write-tree with clean and dirty index states.

test_description='grit write-tree: clean index, dirty index, --prefix, --missing-ok'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with files' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "root" >root.txt &&
	mkdir -p sub/deep &&
	echo "nested" >sub/nested.txt &&
	echo "deep" >sub/deep/file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial"
	)
'

###########################################################################
# Section 2: Basic write-tree on clean index
###########################################################################

test_expect_success 'write-tree on clean index produces valid tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit cat-file -t "$tree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'write-tree matches HEAD tree on clean index' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	head_tree=$(grit rev-parse HEAD^{tree}) &&
	test "$tree" = "$head_tree"
	)
'

test_expect_success 'write-tree matches git write-tree' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

test_expect_success 'write-tree output is 40-char hex' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	echo "$tree" | grep -qE "^[0-9a-f]{40}$"
	)
'

###########################################################################
# Section 3: write-tree after staging changes
###########################################################################

test_expect_success 'write-tree after adding a new file' '
	(
	cd repo &&
	echo "new" >new.txt &&
	"$REAL_GIT" add new.txt &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "new.txt" actual
	)
'

test_expect_success 'write-tree after adding new file matches git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

test_expect_success 'write-tree after modifying existing file' '
	(
	cd repo &&
	echo "modified root" >root.txt &&
	"$REAL_GIT" add root.txt &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "root.txt" actual
	)
'

test_expect_success 'write-tree modified file has different blob' '
	(
	cd repo &&
	old_blob=$(grit rev-parse HEAD:root.txt) &&
	tree=$(grit write-tree) &&
	new_blob=$(grit ls-tree "$tree" | grep "root.txt" | awk "{print \$3}") &&
	test "$old_blob" != "$new_blob"
	)
'

test_expect_success 'write-tree after modifying matches git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

test_expect_success 'commit staged changes for clean state' '
	(
	cd repo &&
	"$REAL_GIT" commit -m "updated"
	)
'

###########################################################################
# Section 4: write-tree after removing files
###########################################################################

test_expect_success 'write-tree after git rm' '
	(
	cd repo &&
	"$REAL_GIT" rm new.txt &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	! grep "new.txt" actual
	)
'

test_expect_success 'write-tree after rm matches git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

test_expect_success 'restore state after rm test' '
	(
	cd repo &&
	"$REAL_GIT" checkout HEAD -- . 2>/dev/null ||
	"$REAL_GIT" reset HEAD -- new.txt &&
	"$REAL_GIT" checkout -- new.txt
	)
'

###########################################################################
# Section 5: write-tree --prefix (known stack overflow bug in grit)
###########################################################################

test_expect_success 'write-tree --prefix for subdirectory' '
	(
	cd repo &&
	tree=$(grit write-tree --prefix sub/) &&
	grit cat-file -t "$tree" >actual &&
	echo "tree" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'write-tree --prefix matches git --prefix' '
	(
	cd repo &&
	grit_tree=$(grit write-tree --prefix sub/) &&
	git_tree=$("$REAL_GIT" write-tree --prefix sub/) &&
	test "$grit_tree" = "$git_tree"
	)
'

###########################################################################
# Section 6: write-tree idempotency
###########################################################################

test_expect_success 'write-tree called twice returns same hash' '
	(
	cd repo &&
	t1=$(grit write-tree) &&
	t2=$(grit write-tree) &&
	test "$t1" = "$t2"
	)
'

test_expect_success 'write-tree same index twice is still same hash' '
	(
	cd repo &&
	t1=$(grit write-tree) &&
	t2=$(grit write-tree) &&
	test "$t1" = "$t2"
	)
'

###########################################################################
# Section 7: write-tree with update-index
###########################################################################

test_expect_success 'write-tree after update-index add' '
	(
	cd repo &&
	echo "via-update-index" >ui.txt &&
	grit update-index --add ui.txt &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "ui.txt" actual
	)
'

test_expect_success 'write-tree after update-index matches git' '
	(
	cd repo &&
	"$REAL_GIT" add ui.txt &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

test_expect_success 'write-tree after update-index remove' '
	(
	cd repo &&
	grit update-index --remove ui.txt &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	! grep "ui.txt" actual
	)
'

###########################################################################
# Section 8: Tree structure verification
###########################################################################

test_expect_success 'write-tree creates proper subtree for sub/' '
	(
	cd repo &&
	"$REAL_GIT" checkout HEAD -- . 2>/dev/null &&
	"$REAL_GIT" add . &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "040000 tree" actual | grep "sub"
	)
'

test_expect_success 'write-tree top-level has correct entry count' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	entries=$(wc -l <actual | tr -d " ") &&
	git_tree=$("$REAL_GIT" write-tree) &&
	"$REAL_GIT" ls-tree "$git_tree" >git_actual &&
	git_entries=$(wc -l <git_actual | tr -d " ") &&
	test "$entries" = "$git_entries"
	)
'

test_expect_success 'write-tree recursive listing matches git' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >grit_out &&
	git_tree=$("$REAL_GIT" write-tree) &&
	"$REAL_GIT" ls-tree -r "$git_tree" >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 9: Empty directory edge cases
###########################################################################

test_expect_success 'write-tree ignores empty directories' '
	(
	cd repo &&
	mkdir -p emptydir &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >actual &&
	! grep "emptydir" actual
	)
'

###########################################################################
# Section 10: Multiple files in same directory
###########################################################################

test_expect_success 'write-tree with many files in one dir' '
	(
	cd repo &&
	mkdir -p multi &&
	for i in 1 2 3 4 5; do echo "f$i" >multi/f$i.txt; done &&
	"$REAL_GIT" add multi/ &&
	tree=$(grit write-tree) &&
	grit ls-tree -r "$tree" >actual &&
	grep -c "multi/" actual >count &&
	test "$(cat count)" = "5"
	)
'

test_expect_success 'write-tree many files matches git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

###########################################################################
# Section 11: write-tree with executable files
###########################################################################

test_expect_success 'write-tree preserves executable bit' '
	(
	cd repo &&
	echo "#!/bin/sh" >run.sh &&
	chmod +x run.sh &&
	"$REAL_GIT" add run.sh &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "100755" actual | grep "run.sh"
	)
'

test_expect_success 'write-tree executable matches git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

###########################################################################
# Section 12: write-tree with symlinks
###########################################################################

test_expect_success 'write-tree preserves symlinks' '
	(
	cd repo &&
	ln -sf root.txt link.txt &&
	"$REAL_GIT" add link.txt &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "120000" actual | grep "link.txt"
	)
'

test_expect_success 'write-tree symlink matches git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

###########################################################################
# Section 13: write-tree after read-tree
###########################################################################

test_expect_success 'write-tree after read-tree of older commit' '
	(
	cd repo &&
	old_tree=$(grit rev-parse HEAD~1^{tree}) &&
	grit read-tree "$old_tree" &&
	tree=$(grit write-tree) &&
	test "$tree" = "$old_tree"
	)
'

test_expect_success 'restore index after read-tree test' '
	(
	cd repo &&
	head_tree=$(grit rev-parse HEAD^{tree}) &&
	grit read-tree "$head_tree"
	)
'

test_expect_success 'write-tree ls-tree output shows correct modes' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "100644" actual >blobs &&
	test $(wc -l <blobs | tr -d " ") -ge 1
	)
'

test_expect_success 'write-tree tree can be used by commit-tree' '
	(
	cd repo &&
	tree=$(grit write-tree) &&
	commit=$(grit commit-tree "$tree" -m "test commit from write-tree") &&
	grit cat-file -t "$commit" >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'write-tree after adding binary file' '
	(
	cd repo &&
	printf "\000\001\002" >binary.dat &&
	"$REAL_GIT" add binary.dat &&
	tree=$(grit write-tree) &&
	grit ls-tree "$tree" >actual &&
	grep "binary.dat" actual
	)
'

test_expect_success 'write-tree binary file matches git' '
	(
	cd repo &&
	grit_tree=$(grit write-tree) &&
	git_tree=$("$REAL_GIT" write-tree) &&
	test "$grit_tree" = "$git_tree"
	)
'

test_done
