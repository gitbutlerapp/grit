#!/bin/sh
# Tests for grit diff with permission changes (644↔755)

test_description='grit diff permission changes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup base repo' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "script content" >script.sh &&
	echo "regular file" >regular.txt &&
	echo "another" >other.txt &&
	git add . &&
	git commit -m "initial (all 644)"
	)
'

# === basic permission change detection ===

test_expect_success 'diff detects 644 to 755 change' '
	(
	cd repo &&
	chmod +x script.sh &&
	git diff >../actual &&
	grep "old mode 100644" ../actual &&
	grep "new mode 100755" ../actual
	)
'

test_expect_success 'diff --name-only shows permission-changed file' '
	(
	cd repo &&
	git diff --name-only >../actual &&
	grep "script.sh" ../actual
	)
'

test_expect_success 'diff --name-status shows file with mode change' '
	(
	cd repo &&
	git diff --name-status >../actual &&
	grep "script.sh" ../actual
	)
'

test_expect_success 'diff --stat shows permission change' '
	(
	cd repo &&
	git diff --stat >../actual &&
	grep "script.sh" ../actual
	)
'

test_expect_success 'diff --numstat shows permission change' '
	(
	cd repo &&
	git diff --numstat >../actual &&
	grep "script.sh" ../actual
	)
'

test_expect_success 'diff --exit-code detects mode change' '
	(
	cd repo &&
	test_must_fail git diff --exit-code
	)
'

test_expect_success 'diff --quiet detects mode change' '
	(
	cd repo &&
	test_must_fail git diff --quiet
	)
'

# === staged permission change ===

test_expect_success 'stage permission change' '
	(
	cd repo &&
	git add script.sh &&
	git diff --cached >../actual &&
	grep "old mode 100644" ../actual &&
	grep "new mode 100755" ../actual
	)
'

test_expect_success 'diff --cached --name-status for permission change' '
	(
	cd repo &&
	git diff --cached --name-status >../actual &&
	grep "script.sh" ../actual
	)
'

test_expect_success 'commit permission change' '
	(
	cd repo &&
	git commit -m "make script executable"
	)
'

# === reverse permission change 755 to 644 ===

test_expect_success 'diff detects 755 to 644 change' '
	(
	cd repo &&
	chmod -x script.sh &&
	git diff >../actual &&
	grep "old mode 100755" ../actual &&
	grep "new mode 100644" ../actual
	)
'

test_expect_success 'stage reverse permission change' '
	(
	cd repo &&
	git add script.sh &&
	git diff --cached >../actual &&
	grep "old mode 100755" ../actual &&
	grep "new mode 100644" ../actual
	)
'

test_expect_success 'commit reverse permission change' '
	(
	cd repo &&
	git commit -m "remove executable bit"
	)
'

# === permission + content change combined ===

test_expect_success 'diff shows permission + content change together' '
	(
	cd repo &&
	chmod +x script.sh &&
	echo "#!/bin/sh" >>script.sh &&
	git diff >../actual &&
	grep "old mode 100644" ../actual &&
	grep "new mode 100755" ../actual &&
	grep "script.sh" ../actual
	)
'

test_expect_success 'diff --stat for permission + content change' '
	(
	cd repo &&
	git diff --stat >../actual &&
	grep "script.sh" ../actual
	)
'

test_expect_success 'stage and commit permission + content' '
	(
	cd repo &&
	git add script.sh &&
	git commit -m "add shebang and make executable"
	)
'

# === multiple files with permission changes ===

test_expect_success 'diff detects permission changes on multiple files' '
	(
	cd repo &&
	chmod +x regular.txt &&
	chmod +x other.txt &&
	git diff >../actual &&
	grep "regular.txt" ../actual &&
	grep "other.txt" ../actual
	)
'

test_expect_success 'diff --name-only shows all permission-changed files' '
	(
	cd repo &&
	git diff --name-only >../actual &&
	grep "regular.txt" ../actual &&
	grep "other.txt" ../actual
	)
'

test_expect_success 'restore permissions' '
	(
	cd repo &&
	chmod -x regular.txt &&
	chmod -x other.txt
	)
'

# === between commits with permission changes ===

test_expect_success 'setup commits for permission diff' '
	(
	cd repo &&
	chmod +x regular.txt &&
	git add regular.txt &&
	git commit -m "make regular executable"
	)
'

test_expect_success 'diff between commits shows permission change' '
	(
	cd repo &&
	git diff HEAD~1 HEAD >../actual &&
	grep "old mode 100644" ../actual &&
	grep "new mode 100755" ../actual
	)
'

test_expect_success 'diff between commits --stat' '
	(
	cd repo &&
	git diff --stat HEAD~1 HEAD >../actual &&
	grep "regular.txt" ../actual
	)
'

test_expect_success 'diff between commits --name-only' '
	(
	cd repo &&
	git diff --name-only HEAD~1 HEAD >../actual &&
	grep "regular.txt" ../actual
	)
'

test_expect_success 'diff between commits --exit-code with perm change' '
	(
	cd repo &&
	test_must_fail git diff --exit-code HEAD~1 HEAD
	)
'

# === permission-only change: no content diff lines ===

test_expect_success 'permission-only diff has no +/- content lines' '
	(
	cd repo &&
	chmod -x regular.txt &&
	git diff >../actual &&
	grep "old mode" ../actual &&
	! grep "^+[^+]" ../actual &&
	! grep "^-[^-]" ../actual
	)
'

test_expect_success 'restore to clean' '
	(
	cd repo &&
	git checkout -- .
	)
'

test_done
