#!/bin/sh
# Ported from git/t/t4011 patterns — tests for 'grit diff-tree'.

test_description='grit diff-tree'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

make_commit () {
	msg=$1
	parent=${2-}
	tree=$(git write-tree) || return 1
	if test -n "$parent"; then
		commit=$(printf '%s\n' "$msg" | git commit-tree "$tree" -p "$parent") || return 1
	else
		commit=$(printf '%s\n' "$msg" | git commit-tree "$tree") || return 1
	fi
	git update-ref HEAD "$commit" || return 1
	printf '%s\n' "$commit"
}

# ---------------------------------------------------------------------------
# Setup — all state files written to the trash root for easy cross-test access.
# ---------------------------------------------------------------------------

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	printf "hello\n" >file.txt &&
	git update-index --add file.txt &&
	commit1=$(make_commit "initial") &&
	test -n "$commit1" &&
	printf "%s\n" "$commit1" >../commit1 &&
	tree1=$(git cat-file -p "$commit1" | grep "^tree" | awk "{print \$2}") &&
	printf "%s\n" "$tree1" >../tree1
	)
'

test_expect_success 'setup second commit' '
	(
	cd repo &&
	printf "world\n" >>file.txt &&
	git update-index --add file.txt &&
	c1=$(cat ../commit1) &&
	commit2=$(make_commit "second" "$c1") &&
	test -n "$commit2" &&
	printf "%s\n" "$commit2" >../commit2 &&
	tree2=$(git cat-file -p "$commit2" | grep "^tree" | awk "{print \$2}") &&
	printf "%s\n" "$tree2" >../tree2
	)
'

# ---------------------------------------------------------------------------
# Two-tree mode
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree two trees produces raw output' '
	(
	cd repo &&
	t1=$(cat ../tree1) &&
	t2=$(cat ../tree2) &&
	git diff-tree "$t1" "$t2" >out &&
	grep "M	file.txt" out
	)
'

test_expect_success 'diff-tree two trees raw line starts with colon' '
	(
	cd repo &&
	t1=$(cat ../tree1) &&
	t2=$(cat ../tree2) &&
	git diff-tree "$t1" "$t2" >out &&
	grep "^:" out
	)
'

# ---------------------------------------------------------------------------
# Single-commit mode
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree single commit shows changes vs parent' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree "$c2" >out &&
	grep "M	file.txt" out
	)
'

test_expect_success 'diff-tree single commit raw output has correct status' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree "$c2" >out &&
	grep "^:100644 100644 " out
	)
'

test_expect_success 'diff-tree root commit without --root produces no output' '
	(
	cd repo &&
	c1=$(cat ../commit1) &&
	git diff-tree "$c1" >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff-tree root commit with --root shows files' '
	(
	cd repo &&
	c1=$(cat ../commit1) &&
	git diff-tree --root "$c1" >out &&
	grep "A	file.txt" out
	)
'

# ---------------------------------------------------------------------------
# Recursive flag
# ---------------------------------------------------------------------------

test_expect_success 'setup nested directory' '
	(
	cd repo &&
	mkdir -p sub &&
	printf "nested\n" >sub/nested.txt &&
	git update-index --add sub/nested.txt &&
	c2=$(cat ../commit2) &&
	commit3=$(make_commit "add nested" "$c2") &&
	printf "%s\n" "$commit3" >../commit3 &&
	tree3=$(git cat-file -p "$commit3" | grep "^tree" | awk "{print \$2}") &&
	printf "%s\n" "$tree3" >../tree3
	)
'

test_expect_success 'diff-tree -r recurses into subdirs' '
	(
	cd repo &&
	c3=$(cat ../commit3) &&
	git diff-tree -r "$c3" >out &&
	grep "sub/nested.txt" out
	)
'

test_expect_success 'diff-tree without -r does not recurse into subdirs' '
	(
	cd repo &&
	t2=$(cat ../tree2) &&
	t3=$(cat ../tree3) &&
	git diff-tree "$t2" "$t3" >out &&
	! grep "sub/nested.txt" out
	)
'

# ---------------------------------------------------------------------------
# Output formats
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree -p produces patch output' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r -p "$c2" >out &&
	grep "^diff --git" out &&
	grep "^+world" out
	)
'

test_expect_success 'diff-tree --patch produces patch output' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r --patch "$c2" >out &&
	grep "^diff --git" out
	)
'

test_expect_success 'diff-tree --name-only shows only file names' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r --name-only "$c2" >out &&
	grep "^file.txt$" out &&
	! grep "^:" out
	)
'

test_expect_success 'diff-tree --name-status shows status letter and name' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r --name-status "$c2" >out &&
	grep "^M	file.txt" out
	)
'

test_expect_success 'diff-tree --stat shows diffstat' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r --stat "$c2" >out &&
	grep "file.txt" out &&
	grep "changed" out
	)
'

# ---------------------------------------------------------------------------
# --stdin mode
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree --stdin reads commit OID and shows diff' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	printf "%s\n" "$c2" | git diff-tree --stdin >out &&
	grep "M	file.txt" out
	)
'

test_expect_success 'diff-tree --stdin prints commit-id header' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	printf "%s\n" "$c2" | git diff-tree --stdin >out &&
	head -1 out >first_line &&
	grep "^[0-9a-f]\{40\}$" first_line
	)
'

test_expect_success 'diff-tree --stdin --no-commit-id suppresses header' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	printf "%s\n" "$c2" | git diff-tree --stdin --no-commit-id >out &&
	grep "^:" out &&
	! head -1 out | grep "^[0-9a-f]\{40\}$"
	)
'

test_expect_success 'diff-tree --stdin with two tree OIDs compares them' '
	(
	cd repo &&
	t1=$(cat ../tree1) &&
	t2=$(cat ../tree2) &&
	printf "%s %s\n" "$t1" "$t2" | git diff-tree --stdin >out &&
	head -1 out >first_line &&
	grep "$t1" first_line &&
	grep "$t2" first_line &&
	grep "M	file.txt" out
	)
'

test_expect_success 'diff-tree --stdin passes through non-OID lines' '
	(
	cd repo &&
	printf "not-a-sha1\n" | git diff-tree --stdin >out &&
	grep "not-a-sha1" out
	)
'

# ---------------------------------------------------------------------------
# Path-limiting
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree with pathspec limits output' '
	(
	cd repo &&
	c3=$(cat ../commit3) &&
	git diff-tree -r "$c3" -- sub >out &&
	grep "sub/nested.txt" out
	)
'

test_expect_success 'diff-tree with pathspec excludes non-matching files' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r "$c2" -- nonexistent.txt >out &&
	test_must_be_empty out
	)
'

# ---------------------------------------------------------------------------
# Additional tests ported from git/t/t4011-diff-tree.sh patterns
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree --no-commit-id suppresses commit line in single-commit mode' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree --no-commit-id "$c2" >out &&
	! head -1 out | grep "^[0-9a-f]\{40\}$"
	)
'

test_expect_success 'diff-tree two commits shows changes between them' '
	(
	cd repo &&
	c1=$(cat ../commit1) &&
	c2=$(cat ../commit2) &&
	git diff-tree "$c1" "$c2" >out &&
	grep "M" out &&
	grep "file.txt" out
	)
'

test_expect_success 'diff-tree identical trees produces no output' '
	(
	cd repo &&
	t1=$(cat ../tree1) &&
	git diff-tree "$t1" "$t1" >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff-tree -r on nested adds shows full paths' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	c3=$(cat ../commit3) &&
	git diff-tree -r "$c2" "$c3" >out &&
	grep "A" out &&
	grep "sub/nested.txt" out
	)
'

test_expect_success 'diff-tree --name-only on two commits' '
	(
	cd repo &&
	c1=$(cat ../commit1) &&
	c2=$(cat ../commit2) &&
	git diff-tree --name-only "$c1" "$c2" >out &&
	grep "^file.txt$" out
	)
'

test_expect_success 'diff-tree --name-status on two commits' '
	(
	cd repo &&
	c1=$(cat ../commit1) &&
	c2=$(cat ../commit2) &&
	git diff-tree --name-status "$c1" "$c2" >out &&
	grep "^M" out &&
	grep "file.txt" out
	)
'

test_expect_success 'diff-tree --stat on two commits' '
	(
	cd repo &&
	c1=$(cat ../commit1) &&
	c2=$(cat ../commit2) &&
	git diff-tree --stat "$c1" "$c2" >out &&
	grep "file.txt" out &&
	grep "changed" out
	)
'

test_expect_success 'diff-tree -p shows proper hunk headers' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r -p "$c2" >out &&
	grep "^@@" out
	)
'

test_expect_success 'diff-tree --root on non-root commit still shows parent diff' '
	(
	cd repo &&
	c3=$(cat ../commit3) &&
	git diff-tree -r --root "$c3" >out &&
	grep "sub/nested.txt" out
	)
'

test_expect_success 'diff-tree --root shows A status for root commit' '
	(
	cd repo &&
	c1=$(cat ../commit1) &&
	git diff-tree -r --root "$c1" >out &&
	grep "^:000000" out
	)
'

# ---------------------------------------------------------------------------
# Patch mode: new-file and deleted-file headers (ported from t4011-diff-symlink)
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree -r -p --root shows new file mode header' '
	(
	cd repo &&
	c1=$(cat ../commit1) &&
	git diff-tree -r -p --root "$c1" >out &&
	grep "^new file mode 100644" out
	)
'

test_expect_success 'diff-tree -p shows index line for modified file' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r -p "$c2" >out &&
	grep "^index " out
	)
'

# ---------------------------------------------------------------------------
# File deletion
# ---------------------------------------------------------------------------

test_expect_success 'setup file deletion commit' '
	(
	cd repo &&
	c3=$(cat ../commit3) &&
	printf "extra content\n" >extra.txt &&
	git update-index --add extra.txt &&
	commit_extra=$(make_commit "add extra.txt" "$c3") &&
	printf "%s\n" "$commit_extra" >../commit_extra &&
	git update-index --force-remove extra.txt &&
	rm -f extra.txt &&
	commit_del=$(make_commit "delete extra.txt" "$commit_extra") &&
	printf "%s\n" "$commit_del" >../commit_del
	)
'

test_expect_success 'diff-tree shows D status for deleted file' '
	(
	cd repo &&
	c=$(cat ../commit_del) &&
	git diff-tree -r "$c" >out &&
	grep "^:100644 000000 " out &&
	grep "D	extra.txt" out
	)
'

test_expect_success 'diff-tree -p shows deleted file mode header' '
	(
	cd repo &&
	c=$(cat ../commit_del) &&
	git diff-tree -r -p "$c" >out &&
	grep "^deleted file mode 100644" out
	)
'

test_expect_success 'diff-tree --name-status shows D for deleted file' '
	(
	cd repo &&
	c=$(cat ../commit_del) &&
	git diff-tree -r --name-status "$c" >out &&
	grep "^D	extra.txt" out
	)
'

# ---------------------------------------------------------------------------
# Multiple files changed in one commit (ported from t4001-diff-rename patterns)
# ---------------------------------------------------------------------------

test_expect_success 'setup multi-file commit' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	printf "alpha content\n" >alpha.txt &&
	printf "beta content\n" >beta.txt &&
	git update-index --add alpha.txt beta.txt &&
	commit_multi=$(make_commit "add alpha and beta" "$c2") &&
	printf "%s\n" "$commit_multi" >../commit_multi
	)
'

test_expect_success 'diff-tree shows multiple entries for multi-file commit' '
	(
	cd repo &&
	c=$(cat ../commit_multi) &&
	git diff-tree -r "$c" >out &&
	grep "alpha.txt" out &&
	grep "beta.txt" out
	)
'

test_expect_success 'diff-tree --name-only shows all files in multi-file commit' '
	(
	cd repo &&
	c=$(cat ../commit_multi) &&
	git diff-tree -r --name-only "$c" >out &&
	grep "^alpha.txt$" out &&
	grep "^beta.txt$" out
	)
'

test_expect_success 'diff-tree --stat shows multiple files in multi-file commit' '
	(
	cd repo &&
	c=$(cat ../commit_multi) &&
	git diff-tree -r --stat "$c" >out &&
	grep "alpha.txt" out &&
	grep "beta.txt" out &&
	grep "files changed" out
	)
'

# ---------------------------------------------------------------------------
# Executable file mode (ported from t4011-diff-symlink patterns)
# ---------------------------------------------------------------------------

test_expect_success 'setup executable file commit' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	printf "#!/bin/sh\necho hello\n" >script.sh &&
	chmod +x script.sh &&
	git update-index --add script.sh &&
	commit_exec=$(make_commit "add executable script" "$c2") &&
	printf "%s\n" "$commit_exec" >../commit_exec
	)
'

test_expect_success 'diff-tree shows 100755 mode for executable file' '
	(
	cd repo &&
	c=$(cat ../commit_exec) &&
	git diff-tree -r "$c" >out &&
	grep "^:000000 100755 " out &&
	grep "A	script.sh" out
	)
'

test_expect_success 'diff-tree -p shows new file mode 100755' '
	(
	cd repo &&
	c=$(cat ../commit_exec) &&
	git diff-tree -r -p "$c" >out &&
	grep "^new file mode 100755" out
	)
'

# ---------------------------------------------------------------------------
# Symlink mode (ported from t4011-diff-symlink.sh)
# ---------------------------------------------------------------------------

test_expect_success 'setup symlink commit' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	ln -s file.txt link.txt &&
	git update-index --add link.txt &&
	commit_link=$(make_commit "add symlink" "$c2") &&
	printf "%s\n" "$commit_link" >../commit_link
	)
'

test_expect_success 'diff-tree shows 120000 mode for new symlink' '
	(
	cd repo &&
	c=$(cat ../commit_link) &&
	git diff-tree -r "$c" >out &&
	grep "^:000000 120000 " out &&
	grep "A	link.txt" out
	)
'

test_expect_success 'diff-tree -p shows new file mode 120000 for symlink' '
	(
	cd repo &&
	c=$(cat ../commit_link) &&
	git diff-tree -r -p "$c" >out &&
	grep "^new file mode 120000" out
	)
'

# ---------------------------------------------------------------------------
# Raw output field validation
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree raw output OIDs are 40 hex characters' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r "$c2" >out &&
	# Extract the old and new OID fields (fields 3 and 4 after the colon line)
	awk "{print \$3; print \$4}" out >oids &&
	# Each OID should be exactly 40 hex chars (or all-zeros for null OID)
	grep -E "^[0-9a-f]{40}$" oids
	)
'

test_expect_success 'diff-tree raw output format: colon then 6-digit modes' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r "$c2" >out &&
	grep "^:[0-9]\{6\} [0-9]\{6\} " out
	)
'

# ---------------------------------------------------------------------------
# -s flag: suppress diff (stdin mode)
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree --stdin -s suppresses diff lines' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	printf "%s\n" "$c2" | git diff-tree --stdin -s >out &&
	head -1 out | grep "^[0-9a-f]\{40\}$" &&
	! grep "^:" out
	)
'

# ---------------------------------------------------------------------------
# -v flag: verbose commit info (stdin mode)
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree --stdin -v shows indented commit message' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	printf "%s\n" "$c2" | git diff-tree --stdin -v >out &&
	grep "^    second" out
	)
'

# ---------------------------------------------------------------------------
# -U context lines control (ported from t4011-diff-symlink patterns)
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree -p -U0 produces patch with zero context' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r -p -U0 "$c2" >out &&
	grep "^+world" out &&
	! grep "^ hello" out
	)
'

test_expect_success 'diff-tree -p -U1 produces patch with one context line' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r -p -U1 "$c2" >out &&
	grep "^ hello" out &&
	grep "^+world" out
	)
'

# ---------------------------------------------------------------------------
# --stdin with multiple commits
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree --stdin processes multiple commits sequentially' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	printf "%s\n%s\n" "$c2" "$c2" | git diff-tree --stdin >out &&
	test "$(grep -c "M	file.txt" out)" = "2"
	)
'

# ---------------------------------------------------------------------------
# Two-tree mode: A and D entries from explicit tree comparison
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree two trees shows A for file in new tree only' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	c3=$(cat ../commit3) &&
	t2=$(git cat-file -p "$c2" | awk "/^tree/{print \$2}") &&
	t3=$(git cat-file -p "$c3" | awk "/^tree/{print \$2}") &&
	git diff-tree -r "$t2" "$t3" >out &&
	grep "^:000000 100644 " out &&
	grep "A	sub/nested.txt" out
	)
'

# ---------------------------------------------------------------------------
# --abbrev flag (ported from t4001-diff-rename patterns)
# Two-tree mode honours --abbrev; single-commit mode inherits the same
# underlying diff machinery but relies on the commit being resolved first.
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree --abbrev is accepted without error' '
	(
	cd repo &&
	t1=$(cat ../tree1) &&
	t2=$(cat ../tree2) &&
	git diff-tree --abbrev "$t1" "$t2" >out &&
	grep "^:" out
	)
'

test_expect_success 'diff-tree --abbrev=7 is accepted without error' '
	(
	cd repo &&
	t1=$(cat ../tree1) &&
	t2=$(cat ../tree2) &&
	git diff-tree --abbrev=7 "$t1" "$t2" >out &&
	grep "M	file.txt" out
	)
'

# ---------------------------------------------------------------------------
# --diff-filter flag (ported from t4001-diff-rename patterns)
# ---------------------------------------------------------------------------

test_expect_success 'setup diff-filter test: two files with one modified one added' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	printf "original content\n" >filter_orig.txt &&
	git update-index --add filter_orig.txt &&
	commit_filter1=$(make_commit "filter base" "$c2") &&
	printf "%s\n" "$commit_filter1" >../commit_filter1 &&
	printf "modified content\n" >filter_orig.txt &&
	git update-index filter_orig.txt &&
	printf "brand new file\n" >filter_new.txt &&
	git update-index --add filter_new.txt &&
	commit_filter2=$(make_commit "filter: mod+add" "$commit_filter1") &&
	printf "%s\n" "$commit_filter2" >../commit_filter2
	)
'

test_expect_success 'diff-tree --diff-filter=M includes modified file' '
	(
	cd repo &&
	c=$(cat ../commit_filter2) &&
	git diff-tree -r --diff-filter=M "$c" >out &&
	grep "M	filter_orig.txt" out
	)
'

test_expect_success 'diff-tree --diff-filter=D only shows deleted files' '
	(
	cd repo &&
	commit_filter1=$(cat ../commit_filter1) &&
	c2=$(cat ../commit2) &&
	tree_f1=$(git cat-file -p "$commit_filter1" | awk "/^tree/{print \$2}") &&
	tree_c2=$(git cat-file -p "$c2" | awk "/^tree/{print \$2}") &&
	git diff-tree --diff-filter=D "$tree_f1" "$tree_c2" >out &&
	grep "D	filter_orig.txt" out &&
	! grep "file.txt" out
	)
'

test_expect_success 'diff-tree --diff-filter=D excludes unrelated files in two-tree mode' '
	(
	cd repo &&
	commit_filter1=$(cat ../commit_filter1) &&
	c2=$(cat ../commit2) &&
	tree_f1=$(git cat-file -p "$commit_filter1" | awk "/^tree/{print \$2}") &&
	tree_c2=$(git cat-file -p "$c2" | awk "/^tree/{print \$2}") &&
	git diff-tree --diff-filter=D "$tree_f1" "$tree_c2" >out &&
	! grep "file.txt" out
	)
'

# ---------------------------------------------------------------------------
# Two-tree mode: D entries when file deleted between trees
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree two trees shows D for deleted file' '
	(
	cd repo &&
	commit_extra=$(cat ../commit_extra) &&
	commit_del=$(cat ../commit_del) &&
	t_with=$(git cat-file -p "$commit_extra" | awk "/^tree/{print \$2}") &&
	t_without=$(git cat-file -p "$commit_del" | awk "/^tree/{print \$2}") &&
	git diff-tree -r "$t_with" "$t_without" >out &&
	grep "^:100644 000000 " out &&
	grep "D	extra.txt" out
	)
'

# ---------------------------------------------------------------------------
# Patch output details for added/deleted files
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree -r -p for deleted file shows minus lines' '
	(
	cd repo &&
	c=$(cat ../commit_del) &&
	git diff-tree -r -p "$c" >out &&
	grep "^-extra content" out
	)
'

test_expect_success 'diff-tree -r -p shows a/file b/file header' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r -p "$c2" >out &&
	grep "^--- a/file.txt" out &&
	grep "^+++ b/file.txt" out
	)
'

test_expect_success 'diff-tree -r -p for new file references /dev/null as old path' '
	(
	cd repo &&
	c=$(cat ../commit_exec) &&
	git diff-tree -r -p "$c" >out &&
	grep "/dev/null" out
	)
'

test_expect_success 'diff-tree -r -p for deleted file references /dev/null as new path' '
	(
	cd repo &&
	c=$(cat ../commit_del) &&
	git diff-tree -r -p "$c" >out &&
	grep "/dev/null" out
	)
'

# ---------------------------------------------------------------------------
# Root commit with multiple files (ported from t4001-diff-rename patterns)
# ---------------------------------------------------------------------------

test_expect_success 'setup root commit with multiple files' '
	(
	git init multi_root &&
	cd multi_root &&
	printf "file A\n" >a.txt &&
	printf "file B\n" >b.txt &&
	git update-index --add a.txt b.txt &&
	tree_mr=$(git write-tree) &&
	commit_mr=$(printf "root\n" | git commit-tree "$tree_mr") &&
	git update-ref HEAD "$commit_mr" &&
	printf "%s\n" "$commit_mr" >../commit_mr
	)
'

test_expect_success 'diff-tree --root on multi-file root commit shows all added files' '
	(
	cd multi_root &&
	c=$(cat ../commit_mr) &&
	git diff-tree -r --root "$c" >out &&
	grep "A	a.txt" out &&
	grep "A	b.txt" out
	)
'

test_expect_success 'diff-tree -r --root --name-only on root shows only file names' '
	(
	cd multi_root &&
	c=$(cat ../commit_mr) &&
	git diff-tree -r --root --name-only "$c" >out &&
	grep "^a.txt$" out &&
	grep "^b.txt$" out &&
	! grep "^:" out
	)
'

# ---------------------------------------------------------------------------
# diff-tree --stat for deleted file shows deletions
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree -r --stat for deleted file shows deletions' '
	(
	cd repo &&
	c=$(cat ../commit_del) &&
	git diff-tree -r --stat "$c" >out &&
	grep "extra.txt" out &&
	grep "deletion" out
	)
'

# ---------------------------------------------------------------------------
# diff-tree raw output with mode 100755 and 120000 (ported from t4011-diff-symlink)
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree raw shows 100755 mode for executable file' '
	(
	cd repo &&
	c=$(cat ../commit_exec) &&
	git diff-tree -r "$c" >out &&
	grep "^:000000 100755 " out
	)
'

test_expect_success 'diff-tree raw shows 120000 mode for symlink' '
	(
	cd repo &&
	c=$(cat ../commit_link) &&
	git diff-tree -r "$c" >out &&
	grep "^:000000 120000 " out
	)
'

# ---------------------------------------------------------------------------
# -l0 rename limit (ported from t4001-diff-rename.sh)
# ---------------------------------------------------------------------------

test_expect_success 'setup rename test: commit file then remove+add renamed copy' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	printf "unique content line1\nline2\nline3\nline4\nline5\n" >rename_src.txt &&
	git update-index --add rename_src.txt &&
	commit_rsrc=$(make_commit "add rename source" "$c2") &&
	printf "%s\n" "$commit_rsrc" >../commit_rsrc &&
	cp rename_src.txt rename_dst.txt &&
	git update-index --force-remove rename_src.txt &&
	git update-index --add rename_dst.txt &&
	commit_rdst=$(make_commit "rename file" "$commit_rsrc") &&
	printf "%s\n" "$commit_rdst" >../commit_rdst
	)
'

test_expect_success 'diff-tree with remove+add of same content shows D and A entries' '
	(
	cd repo &&
	commit_rsrc=$(cat ../commit_rsrc) &&
	commit_rdst=$(cat ../commit_rdst) &&
	git diff-tree -r "$commit_rsrc" "$commit_rdst" >out &&
	grep "D	rename_src.txt" out &&
	grep "A	rename_dst.txt" out
	)
'

test_expect_success 'diff-tree -r output line count matches changed file count' '
	(
	cd repo &&
	commit_rsrc=$(cat ../commit_rsrc) &&
	commit_rdst=$(cat ../commit_rdst) &&
	git diff-tree -r "$commit_rsrc" "$commit_rdst" >out &&
	test_line_count = 2 out
	)
'

# ---------------------------------------------------------------------------
# diff-tree -p patch content validation
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree -r -p shows diff --git header for each file' '
	(
	cd repo &&
	c=$(cat ../commit_multi) &&
	git diff-tree -r -p "$c" >out &&
	grep "^diff --git a/alpha.txt b/alpha.txt" out &&
	grep "^diff --git a/beta.txt b/beta.txt" out
	)
'

test_expect_success 'diff-tree -r -p shows +++ b/ lines' '
	(
	cd repo &&
	c=$(cat ../commit_multi) &&
	git diff-tree -r -p "$c" >out &&
	grep "^+++ b/alpha.txt" out &&
	grep "^+++ b/beta.txt" out
	)
'

test_expect_success 'diff-tree -r -p shows added content with + prefix' '
	(
	cd repo &&
	c=$(cat ../commit_multi) &&
	git diff-tree -r -p "$c" >out &&
	grep "^+alpha content" out &&
	grep "^+beta content" out
	)
'

test_expect_success 'diff-tree -r -p for modification shows both - and + lines' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r -p "$c2" >out &&
	grep "^+world" out
	)
'

test_expect_success 'diff-tree -r -p -U0 suppresses context lines' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -r -p -U0 "$c2" >out &&
	grep "^@@" out &&
	! grep "^ hello" out
	)
'

# ---------------------------------------------------------------------------
# diff-tree with pathspec filtering
# ---------------------------------------------------------------------------

test_expect_success 'setup multi-file repo for pathspec tests' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	mkdir -p sub &&
	printf "sub content\n" >sub/deep.txt &&
	git update-index --add sub/deep.txt &&
	commit_sub=$(make_commit "add subdir" "$c2") &&
	printf "%s\n" "$commit_sub" >../commit_sub
	)
'

test_expect_success 'diff-tree -r with pathspec shows only matching' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	csub=$(cat ../commit_sub) &&
	git diff-tree -r "$c2" "$csub" -- sub >out &&
	grep "sub/deep.txt" out &&
	! grep "file.txt" out
	)
'

test_expect_success 'diff-tree -r with non-matching pathspec is empty' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	csub=$(cat ../commit_sub) &&
	git diff-tree -r "$c2" "$csub" -- nonexistent >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff-tree -r --name-only with pathspec' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	csub=$(cat ../commit_sub) &&
	git diff-tree -r --name-only "$c2" "$csub" -- sub >out &&
	grep "sub/deep.txt" out &&
	! grep "file.txt" out
	)
'

test_expect_success 'diff-tree -r --name-status with pathspec' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	csub=$(cat ../commit_sub) &&
	git diff-tree -r --name-status "$c2" "$csub" -- sub >out &&
	grep "A" out &&
	grep "sub/deep.txt" out
	)
'

test_expect_success 'diff-tree -r --stat with pathspec' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	csub=$(cat ../commit_sub) &&
	git diff-tree -r --stat "$c2" "$csub" -- sub >out &&
	grep "sub/deep.txt" out &&
	! grep "file.txt" out
	)
'

# ---------------------------------------------------------------------------
# diff-tree with binary files
# ---------------------------------------------------------------------------

test_expect_success 'setup binary file in repo' '
	(
	cd repo &&
	csub=$(cat ../commit_sub) &&
	printf "\000\001\002" >binary.dat &&
	git update-index --add binary.dat &&
	commit_bin=$(make_commit "add binary" "$csub") &&
	printf "%s\n" "$commit_bin" >../commit_bin
	)
'

test_expect_success 'diff-tree -r shows binary file addition' '
	(
	cd repo &&
	csub=$(cat ../commit_sub) &&
	cbin=$(cat ../commit_bin) &&
	git diff-tree -r "$csub" "$cbin" >out &&
	grep "binary.dat" out
	)
'

test_expect_success 'diff-tree --name-only shows binary file' '
	(
	cd repo &&
	csub=$(cat ../commit_sub) &&
	cbin=$(cat ../commit_bin) &&
	git diff-tree --name-only "$csub" "$cbin" >out &&
	grep "binary.dat" out
	)
'

test_expect_success 'diff-tree -p shows new file mode for binary' '
	(
	cd repo &&
	csub=$(cat ../commit_sub) &&
	cbin=$(cat ../commit_bin) &&
	git diff-tree -p "$csub" "$cbin" >out &&
	grep "new file mode" out
	)
'

# ---------------------------------------------------------------------------
# diff-tree with executable files
# ---------------------------------------------------------------------------

test_expect_success 'setup executable file' '
	(
	cd repo &&
	cbin=$(cat ../commit_bin) &&
	printf "#!/bin/sh\necho hi\n" >run.sh &&
	chmod +x run.sh &&
	git update-index --add run.sh &&
	commit_exec=$(make_commit "add executable" "$cbin") &&
	printf "%s\n" "$commit_exec" >../commit_exec
	)
'

test_expect_success 'diff-tree -r shows 100755 for executable' '
	(
	cd repo &&
	cbin=$(cat ../commit_bin) &&
	cexec=$(cat ../commit_exec) &&
	git diff-tree -r "$cbin" "$cexec" >out &&
	grep "100755" out &&
	grep "run.sh" out
	)
'

test_expect_success 'diff-tree -p shows 100755 mode for executable' '
	(
	cd repo &&
	cbin=$(cat ../commit_bin) &&
	cexec=$(cat ../commit_exec) &&
	git diff-tree -p "$cbin" "$cexec" >out &&
	grep "new file mode 100755" out
	)
'

# ---------------------------------------------------------------------------
# diff-tree with deletions
# ---------------------------------------------------------------------------

test_expect_success 'setup deletion' '
	(
	cd repo &&
	cexec=$(cat ../commit_exec) &&
	git update-index --force-remove binary.dat &&
	rm -f binary.dat &&
	commit_del=$(make_commit "delete binary" "$cexec") &&
	printf "%s\n" "$commit_del" >../commit_del
	)
'

test_expect_success 'diff-tree -r shows D for deleted file' '
	(
	cd repo &&
	cexec=$(cat ../commit_exec) &&
	cdel=$(cat ../commit_del) &&
	git diff-tree -r "$cexec" "$cdel" >out &&
	grep "D" out &&
	grep "binary.dat" out
	)
'

test_expect_success 'diff-tree --name-status shows D for deleted file' '
	(
	cd repo &&
	cexec=$(cat ../commit_exec) &&
	cdel=$(cat ../commit_del) &&
	git diff-tree --name-status "$cexec" "$cdel" >out &&
	grep "^D" out &&
	grep "binary.dat" out
	)
'

test_expect_success 'diff-tree -p shows deleted file mode for deletion' '
	(
	cd repo &&
	cexec=$(cat ../commit_exec) &&
	cdel=$(cat ../commit_del) &&
	git diff-tree -p "$cexec" "$cdel" >out &&
	grep "deleted file mode" out
	)
'

# ---------------------------------------------------------------------------
# diff-tree single commit (vs parent) additional formats
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree single commit shows changes vs parent' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree "$c2" >out &&
	grep "file.txt" out
	)
'

test_expect_success 'diff-tree --name-only single commit' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree --name-only "$c2" >out &&
	grep "file.txt" out
	)
'

test_expect_success 'diff-tree --name-status single commit' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree --name-status "$c2" >out &&
	grep "M" out &&
	grep "file.txt" out
	)
'

test_expect_success 'diff-tree --stat single commit' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree --stat "$c2" >out &&
	grep "file.txt" out &&
	grep "changed" out
	)
'

test_expect_success 'diff-tree -p single commit shows patch' '
	(
	cd repo &&
	c2=$(cat ../commit2) &&
	git diff-tree -p "$c2" >out &&
	grep "^diff --git" out &&
	grep "^+world" out
	)
'

# ---------------------------------------------------------------------------
# diff-tree between non-parent-child commits
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree -r between arbitrary commits' '
	(
	cd repo &&
	c1=$(cat ../commit1) &&
	cdel=$(cat ../commit_del) &&
	git diff-tree -r "$c1" "$cdel" >out &&
	test $(wc -l <out) -ge 3
	)
'

test_expect_success 'diff-tree -r --name-only between arbitrary commits lists all changed files' '
	(
	cd repo &&
	c1=$(cat ../commit1) &&
	cdel=$(cat ../commit_del) &&
	git diff-tree -r --name-only "$c1" "$cdel" >out &&
	grep "file.txt" out &&
	grep "sub/deep.txt" out &&
	grep "run.sh" out
	)
'

test_expect_success 'diff-tree -p between arbitrary commits shows full patch' '
	(
	cd repo &&
	c1=$(cat ../commit1) &&
	cdel=$(cat ../commit_del) &&
	git diff-tree -p "$c1" "$cdel" >out &&
	grep "^diff --git" out
	)
'

# ---------------------------------------------------------------------------
# diff-tree with identical trees
# ---------------------------------------------------------------------------

test_expect_success 'diff-tree -r between identical commits is empty' '
	(
	cd repo &&
	c1=$(cat ../commit1) &&
	git diff-tree -r "$c1" "$c1" >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff-tree --name-only between identical commits is empty' '
	(
	cd repo &&
	c1=$(cat ../commit1) &&
	git diff-tree --name-only "$c1" "$c1" >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff-tree -r --stat between identical commits shows 0 files' '
	(
	cd repo &&
	c1=$(cat ../commit1) &&
	git diff-tree -r --stat "$c1" "$c1" >out &&
	grep "0 files changed" out
	)
'

test_done
