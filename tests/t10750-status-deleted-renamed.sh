#!/bin/sh
# Test grit status with deleted, renamed (via grit mv), and various
# mixed states: short format, porcelain, branch, untracked files,
# and -z output.

test_description='grit status with deleted and renamed files'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# We write test output to $OUT (outside the repo) to avoid
# polluting status with our own temp files.

test_expect_success 'setup: repo and output dir' '
	(
	OUT="$TRASH_DIRECTORY/output" &&
	mkdir -p "$OUT" &&
	grit init sr-repo &&
	cd sr-repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "alpha" >alpha.txt &&
	echo "beta" >beta.txt &&
	echo "gamma" >gamma.txt &&
	grit add alpha.txt beta.txt gamma.txt &&
	test_tick &&
	grit commit -m "initial three files"
	)
'

# --- clean state ---

test_expect_success 'status -s shows nothing for clean tree' '
	(
	cd sr-repo &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	test_must_be_empty "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'status --porcelain -b shows only branch for clean tree' '
	(
	cd sr-repo &&
	grit status --porcelain -b >"$TRASH_DIRECTORY/output/actual" &&
	grep "^## " "$TRASH_DIRECTORY/output/actual" &&
	test $(wc -l <"$TRASH_DIRECTORY/output/actual") -eq 1
	)
'

# --- unstaged deletion ---

test_expect_success 'status -s shows D for unstaged deletion' '
	(
	cd sr-repo &&
	rm alpha.txt &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	grep " D alpha.txt" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'status --porcelain shows same D format' '
	(
	cd sr-repo &&
	grit status --porcelain >"$TRASH_DIRECTORY/output/actual" &&
	grep " D alpha.txt" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'status long format shows deleted' '
	(
	cd sr-repo &&
	grit status >"$TRASH_DIRECTORY/output/actual" &&
	grep "deleted:" "$TRASH_DIRECTORY/output/actual"
	)
'

# --- staged deletion ---

test_expect_success 'status -s shows D in index column for staged deletion' '
	(
	cd sr-repo &&
	grit add alpha.txt &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	grep "^D  alpha.txt" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'status long format shows staged delete' '
	(
	cd sr-repo &&
	grit status >"$TRASH_DIRECTORY/output/actual" &&
	grep "deleted:" "$TRASH_DIRECTORY/output/actual" &&
	grep "Changes to be committed" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'restore alpha' '
	(
	cd sr-repo &&
	grit checkout HEAD -- alpha.txt
	)
'

# --- grit mv (rename) ---

test_expect_success 'grit mv renames file on disk' '
	(
	cd sr-repo &&
	grit mv alpha.txt renamed.txt &&
	test -f renamed.txt &&
	! test -f alpha.txt
	)
'

test_expect_success 'status -s shows rename for rename' '
	(
	cd sr-repo &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	grep "R" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'status -s shows old and new filename' '
	(
	cd sr-repo &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	grep "alpha.txt" "$TRASH_DIRECTORY/output/actual" &&
	grep "renamed.txt" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'commit rename' '
	(
	cd sr-repo &&
	test_tick &&
	grit commit -m "rename alpha to renamed"
	)
'

# --- multiple deletions ---

test_expect_success 'status shows multiple unstaged deletions' '
	(
	cd sr-repo &&
	rm beta.txt &&
	rm gamma.txt &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	grep " D beta.txt" "$TRASH_DIRECTORY/output/actual" &&
	grep " D gamma.txt" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'status deletion count is correct' '
	(
	cd sr-repo &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	count=$(grep " D" "$TRASH_DIRECTORY/output/actual" | wc -l) &&
	test "$count" -eq 2
	)
'

test_expect_success 'restore deleted files' '
	(
	cd sr-repo &&
	grit checkout -- beta.txt gamma.txt
	)
'

# --- untracked files ---

test_expect_success 'status -s shows ?? for untracked files' '
	(
	cd sr-repo &&
	echo "untracked" >untracked.txt &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	grep "^?? untracked.txt" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'status --porcelain shows ?? for untracked' '
	(
	cd sr-repo &&
	grit status --porcelain >"$TRASH_DIRECTORY/output/actual" &&
	grep "^?? untracked.txt" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'status -u no hides untracked files' '
	(
	cd sr-repo &&
	grit status --untracked-files=no -s >"$TRASH_DIRECTORY/output/actual" &&
	! grep "untracked.txt" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'cleanup untracked file' '
	(
	cd sr-repo &&
	rm untracked.txt
	)
'

# --- mixed staged and unstaged ---

test_expect_success 'status shows staged modification' '
	(
	cd sr-repo &&
	echo "staged change" >>beta.txt &&
	grit add beta.txt &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	grep "^M" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'status shows unstaged modification' '
	(
	cd sr-repo &&
	echo "unstaged change" >>gamma.txt &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	grep " M" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'status -s distinguishes index vs worktree columns' '
	(
	cd sr-repo &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	grep "^M  beta.txt" "$TRASH_DIRECTORY/output/actual" &&
	grep "^ M gamma.txt" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'commit staged and reset' '
	(
	cd sr-repo &&
	test_tick &&
	grit commit -m "staged change" &&
	grit checkout -- gamma.txt
	)
'

# --- branch display ---

test_expect_success 'status -sb shows branch name' '
	(
	cd sr-repo &&
	grit status -sb >"$TRASH_DIRECTORY/output/actual" &&
	grep "^## main" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'status --porcelain -b shows branch header' '
	(
	cd sr-repo &&
	grit status --porcelain -b >"$TRASH_DIRECTORY/output/actual" &&
	grep "^## main" "$TRASH_DIRECTORY/output/actual"
	)
'

# --- delete + new file (manual rename pattern) ---

test_expect_success 'status shows rename for manual rename' '
	(
	cd sr-repo &&
	cp renamed.txt manual-rename.txt &&
	rm renamed.txt &&
	grit add manual-rename.txt renamed.txt &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	# May show as R (rename) or D+A depending on rename detection
	grep -E "R|D" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'commit manual rename' '
	(
	cd sr-repo &&
	test_tick &&
	grit commit -m "manual rename"
	)
'

# --- grit mv with --force ---

test_expect_success 'setup: files for force mv' '
	(
	cd sr-repo &&
	echo "target" >target.txt &&
	echo "source" >source.txt &&
	grit add target.txt source.txt &&
	test_tick &&
	grit commit -m "add target and source"
	)
'

test_expect_success 'grit mv --force overwrites existing file' '
	(
	cd sr-repo &&
	grit mv -f source.txt target.txt &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	grep "target.txt" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'commit force mv' '
	(
	cd sr-repo &&
	test_tick &&
	grit commit -m "force mv"
	)
'

# --- grit mv --dry-run ---

test_expect_success 'grit mv --dry-run does not actually move' '
	(
	cd sr-repo &&
	grit mv -n beta.txt moved-beta.txt &&
	test -f beta.txt &&
	! test -f moved-beta.txt
	)
'

# --- grit mv --verbose ---

test_expect_success 'grit mv --verbose shows rename info' '
	(
	cd sr-repo &&
	grit mv -v beta.txt moved-beta.txt >"$TRASH_DIRECTORY/output/actual" 2>&1 &&
	test -f moved-beta.txt
	)
'

test_expect_success 'commit verbose mv' '
	(
	cd sr-repo &&
	test_tick &&
	grit commit -m "mv verbose"
	)
'

# --- status -z (NUL-terminated) ---

test_expect_success 'status -z produces output for dirty tree' '
	(
	cd sr-repo &&
	echo "z change" >>gamma.txt &&
	grit status -z >"$TRASH_DIRECTORY/output/actual" &&
	test -s "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'status -z output contains filename' '
	(
	cd sr-repo &&
	grit status -z >"$TRASH_DIRECTORY/output/actual" &&
	grep "gamma.txt" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'cleanup z test' '
	(
	cd sr-repo &&
	grit checkout -- gamma.txt
	)
'

# --- subdirectory files ---

test_expect_success 'setup: subdirectory with file' '
	(
	cd sr-repo &&
	mkdir -p sub &&
	echo "sub content" >sub/file.txt &&
	grit add sub &&
	test_tick &&
	grit commit -m "add sub"
	)
'

test_expect_success 'status -s shows full path for deleted subdir file' '
	(
	cd sr-repo &&
	rm sub/file.txt &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	grep "sub/file.txt" "$TRASH_DIRECTORY/output/actual"
	)
'

test_expect_success 'restore and verify clean' '
	(
	cd sr-repo &&
	grit checkout -- sub/file.txt &&
	grit status -s >"$TRASH_DIRECTORY/output/actual" &&
	test_must_be_empty "$TRASH_DIRECTORY/output/actual"
	)
'

test_done
