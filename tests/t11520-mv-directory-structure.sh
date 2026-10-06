#!/bin/sh
# Tests for grit mv: basic rename, directory moves, -f, -n, -k, -v,
# symlinks, nested dirs, and cross-directory moves.

test_description='grit mv: rename, directory structure, force, dry-run, skip'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: init repo with files and dirs' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	mkdir -p src/core &&
	echo "main" >src/main.c &&
	echo "util" >src/core/util.c &&
	grit add . &&
	grit commit -m "initial"
	)
'

# ---- basic rename ----
test_expect_success 'mv renames file in index' '
	(
	cd repo &&
	grit mv a.txt renamed.txt &&
	grit ls-files --error-unmatch renamed.txt &&
	! grit ls-files --error-unmatch a.txt 2>/dev/null
	)
'

test_expect_success 'mv renames file on disk' '
	(
	cd repo &&
	test -f renamed.txt &&
	! test -f a.txt
	)
'

test_expect_success 'mv shows rename in status' '
	(
	cd repo &&
	grit status --porcelain >st &&
	grep "renamed.txt" st
	)
'

test_expect_success 'commit after mv records rename' '
	(
	cd repo &&
	grit commit -m "rename a to renamed" &&
	grit ls-tree HEAD | grep "renamed.txt"
	)
'

# ---- mv to directory ----
test_expect_success 'mv file into existing directory' '
	(
	cd repo &&
	mkdir dest &&
	grit mv b.txt dest/ &&
	grit ls-files --error-unmatch dest/b.txt &&
	test -f dest/b.txt &&
	! test -f b.txt
	)
'

test_expect_success 'commit mv to directory' '
	(
	cd repo &&
	grit commit -m "mv b to dest"
	)
'

# ---- mv directory ----
test_expect_success 'mv directory to new name' '
	(
	cd repo &&
	grit mv src lib &&
	grit ls-files --error-unmatch lib/main.c &&
	grit ls-files --error-unmatch lib/core/util.c &&
	! grit ls-files --error-unmatch src/main.c 2>/dev/null &&
	test -f lib/main.c &&
	test -f lib/core/util.c
	)
'

test_expect_success 'commit directory mv' '
	(
	cd repo &&
	grit commit -m "mv src to lib"
	)
'

# ---- force ----
test_expect_success 'mv refuses to overwrite existing file' '
	(
	cd repo &&
	echo "target" >target.txt &&
	grit add target.txt &&
	grit commit -m "target" &&
	echo "source" >source.txt &&
	grit add source.txt &&
	grit commit -m "source" &&
	test_must_fail grit mv source.txt target.txt 2>err
	)
'

test_expect_success 'mv -f forces overwrite' '
	(
	cd repo &&
	grit mv -f source.txt target.txt &&
	grit ls-files --error-unmatch target.txt &&
	! grit ls-files --error-unmatch source.txt 2>/dev/null &&
	test "$(cat target.txt)" = "source"
	)
'

test_expect_success 'commit forced mv' '
	(
	cd repo &&
	grit commit -m "force mv"
	)
'

# ---- dry-run ----
test_expect_success 'mv --dry-run does not actually move' '
	(
	cd repo &&
	echo "dry" >dry.txt &&
	grit add dry.txt &&
	grit commit -m "dry" &&
	grit mv --dry-run dry.txt dry_moved.txt &&
	test -f dry.txt &&
	! test -f dry_moved.txt &&
	grit ls-files --error-unmatch dry.txt
	)
'

test_expect_success 'mv -n is synonym for --dry-run' '
	(
	cd repo &&
	grit mv -n dry.txt dry_moved.txt &&
	test -f dry.txt &&
	grit ls-files --error-unmatch dry.txt
	)
'

# ---- verbose ----
test_expect_success 'mv -v shows movement info' '
	(
	cd repo &&
	echo "verb" >verb.txt &&
	grit add verb.txt &&
	grit commit -m "verb" &&
	grit mv -v verb.txt verb_moved.txt >output 2>&1 &&
	grep "verb.txt" output &&
	grit commit -m "verb mv"
	)
'

# ---- skip errors (-k) ----
test_expect_success 'mv -k skips errors instead of aborting' '
	(
	cd repo &&
	echo "ok" >ok.txt &&
	grit add ok.txt &&
	grit commit -m "ok" &&
	mkdir targetdir &&
	grit mv -k nonexistent.txt ok.txt targetdir/ 2>err &&
	grit ls-files --error-unmatch targetdir/ok.txt
	)
'

test_expect_success 'commit after -k move' '
	(
	cd repo &&
	grit commit -m "k mv"
	)
'

# ---- mv into nested directory ----
test_expect_success 'mv file into deeply nested directory' '
	(
	cd repo &&
	mkdir -p deep/a/b/c &&
	echo "deep" >tomove.txt &&
	grit add tomove.txt &&
	grit commit -m "tomove" &&
	grit mv tomove.txt deep/a/b/c/ &&
	grit ls-files --error-unmatch deep/a/b/c/tomove.txt &&
	test -f deep/a/b/c/tomove.txt
	)
'

# ---- mv multiple files to directory ----
test_expect_success 'mv multiple files to directory' '
	(
	cd repo &&
	echo "m1" >m1.txt &&
	echo "m2" >m2.txt &&
	echo "m3" >m3.txt &&
	grit add m1.txt m2.txt m3.txt &&
	grit commit -m "multimv" &&
	mkdir bulk &&
	grit mv m1.txt m2.txt m3.txt bulk/ &&
	grit ls-files --error-unmatch bulk/m1.txt &&
	grit ls-files --error-unmatch bulk/m2.txt &&
	grit ls-files --error-unmatch bulk/m3.txt &&
	! grit ls-files --error-unmatch m1.txt 2>/dev/null
	)
'

test_expect_success 'commit multi mv' '
	(
	cd repo &&
	grit commit -m "bulk mv"
	)
'

# ---- mv nonexistent source fails ----
test_expect_success 'mv nonexistent source fails' '
	(
	cd repo &&
	test_must_fail grit mv nosuch.txt somewhere.txt 2>err
	)
'

# ---- mv to same name fails ----
test_expect_success 'mv file to itself is a no-op or error' '
	(
	cd repo &&
	echo "self" >self.txt &&
	grit add self.txt &&
	grit commit -m "self" &&
	if grit mv self.txt self.txt 2>/dev/null; then
		: ok, treated as no-op
	fi &&
	test -f self.txt &&
	grit ls-files --error-unmatch self.txt
	)
'

# ---- rename preserves content ----
test_expect_success 'mv preserves file content' '
	(
	cd repo &&
	echo "important content 12345" >preserve.txt &&
	grit add preserve.txt &&
	grit commit -m "preserve" &&
	grit mv preserve.txt preserved.txt &&
	test "$(cat preserved.txt)" = "important content 12345"
	)
'

# ---- mv across directories ----
test_expect_success 'mv file from one subdir to another' '
	(
	cd repo &&
	mkdir -p from to &&
	echo "cross" >from/cross.txt &&
	grit add from/cross.txt &&
	grit commit -m "cross setup" &&
	grit mv from/cross.txt to/cross.txt &&
	grit ls-files --error-unmatch to/cross.txt &&
	! grit ls-files --error-unmatch from/cross.txt 2>/dev/null &&
	test -f to/cross.txt
	)
'

# ---- mv directory into another directory ----
test_expect_success 'mv directory into another directory' '
	(
	cd repo &&
	mkdir -p moveme &&
	echo "inside" >moveme/inside.txt &&
	grit add moveme &&
	grit commit -m "moveme" &&
	mkdir -p container &&
	grit mv moveme container/ &&
	grit ls-files --error-unmatch container/moveme/inside.txt &&
	test -f container/moveme/inside.txt
	)
'

# ---- mv with -C flag ----
test_expect_success 'mv -C changes directory context' '
	(
	cd repo &&
	echo "cfile" >cfile.txt &&
	grit add cfile.txt &&
	grit commit -m "cfile" &&
	cd .. &&
	grit -C repo mv cfile.txt cfile_renamed.txt &&
	cd repo &&
	grit ls-files --error-unmatch cfile_renamed.txt &&
	test -f cfile_renamed.txt
	)
'

# ---- mv preserves executable bit ----
test_expect_success 'mv preserves executable permission' '
	(
	cd repo &&
	echo "exec" >exec.sh &&
	chmod +x exec.sh &&
	grit add exec.sh &&
	grit commit -m "exec" &&
	grit mv exec.sh run.sh &&
	grit ls-files -s run.sh >mode &&
	grep "^100755" mode
	)
'

# ---- mv then status shows R ----
test_expect_success 'status shows rename after mv' '
	(
	cd repo &&
	grit commit -m "snapshot" --allow-empty &&
	echo "rename_track" >ren.txt &&
	grit add ren.txt &&
	grit commit -m "ren" &&
	grit mv ren.txt ren2.txt &&
	grit status --porcelain >st &&
	grep "ren2.txt" st
	)
'

# ---- mv symlink ----
test_expect_success 'mv works with symlink' '
	(
	cd repo &&
	echo "real" >real.txt &&
	ln -s real.txt link.txt &&
	grit add real.txt link.txt &&
	grit commit -m "link" &&
	grit mv link.txt link_moved.txt &&
	test -L link_moved.txt &&
	grit ls-files --error-unmatch link_moved.txt
	)
'

test_expect_success 'mv and then ls-tree HEAD still shows old name' '
	(
	cd repo &&
	echo "old" >old.txt &&
	grit add old.txt &&
	grit commit -m "old" &&
	grit mv old.txt new.txt &&
	grit ls-tree HEAD | grep "old.txt" &&
	! grit ls-tree HEAD | grep "new.txt"
	)
'

test_expect_success 'mv then commit shows new name in tree' '
	(
	cd repo &&
	grit commit -m "renamed old" &&
	grit ls-tree HEAD | grep "new.txt" &&
	! grit ls-tree HEAD | grep "old.txt"
	)
'

test_done
