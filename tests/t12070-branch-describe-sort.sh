#!/bin/sh
test_description='grit branch listing, verbose output, create/delete/rename'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup repo with linear history' '
	grit init repo &&
	(cd repo &&
	 $REAL_GIT config user.email "t@t.com" &&
	 $REAL_GIT config user.name "T" &&
	 echo a >file.txt &&
	 grit add file.txt &&
	 grit commit -m "first" &&
	 grit rev-parse HEAD >../../oid_first &&
	 echo b >file.txt &&
	 grit add file.txt &&
	 grit commit -m "second" &&
	 grit rev-parse HEAD >../../oid_second &&
	 echo c >file.txt &&
	 grit add file.txt &&
	 grit commit -m "third" &&
	 grit rev-parse HEAD >../../oid_third)
'

# ── basic listing ─────────────────────────────────────────────────────────────

test_expect_success 'branch with no args lists branches' '
	(cd repo && grit branch >../actual) &&
	grep "main" actual
'

test_expect_success 'current branch is marked with asterisk' '
	(cd repo && grit branch >../actual) &&
	grep "^\* main" actual
'

test_expect_success 'branch --show-current shows current branch' '
	(cd repo && grit branch --show-current >../actual) &&
	echo main >expect &&
	test_cmp expect actual
'

test_expect_success 'branch -l lists branches (same as no args)' '
	(cd repo && grit branch -l >../actual) &&
	grep "main" actual
'

# ── create branches ───────────────────────────────────────────────────────────

test_expect_success 'branch create makes a new branch at HEAD' '
	(cd repo && grit branch feature) &&
	(cd repo && grit branch >../actual) &&
	grep "feature" actual
'

test_expect_success 'new branch points to same commit as HEAD' '
	(cd repo && grit rev-parse feature >../actual) &&
	(cd repo && grit rev-parse HEAD >../expect) &&
	test_cmp expect actual
'

test_expect_success 'branch create at specific SHA' '
	(cd repo && grit branch old-branch "$(cat ../../oid_first)") &&
	(cd repo && grit rev-parse old-branch >../actual) &&
	test_cmp actual ../oid_first
'

test_expect_success 'branch create does not switch to it' '
	(cd repo && grit branch --show-current >../actual) &&
	echo main >expect &&
	test_cmp expect actual
'

test_expect_success 'branch refuses to create duplicate name' '
	test_must_fail grit -C repo branch feature
'

test_expect_success 'branch -f overwrites existing branch to new target' '
	(cd repo && grit branch -f feature "$(cat ../../oid_second)") &&
	(cd repo && grit rev-parse feature >../actual) &&
	test_cmp actual ../oid_second
'

test_expect_success 'restore feature to HEAD' '
	(cd repo && grit branch -f feature "$(cat ../../oid_third)")
'

# ── verbose listing ───────────────────────────────────────────────────────────

test_expect_success 'branch -v shows commit subject' '
	(cd repo && grit branch -v >../actual) &&
	grep "third" actual
'

test_expect_success 'branch -v shows abbreviated hash for current branch' '
	(cd repo && grit branch -v >../actual) &&
	(cd repo && grit rev-parse --short HEAD >../short_hash) &&
	grep "$(cat short_hash)" actual
'

test_expect_success 'branch -v shows old-branch with first commit subject' '
	(cd repo && grit branch -v >../actual) &&
	grep "old-branch.*first" actual
'

# ── delete branches ───────────────────────────────────────────────────────────

test_expect_success 'branch -d deletes a branch' '
	(cd repo && grit branch to-delete) &&
	(cd repo && grit branch -d to-delete) &&
	(cd repo && grit branch >../actual) &&
	! grep "to-delete" actual
'

test_expect_success 'branch -d refuses to delete current branch' '
	test_must_fail grit -C repo branch -d main
'

test_expect_success 'branch -D force deletes' '
	(cd repo && grit branch force-del "$(cat ../../oid_second)") &&
	(cd repo && grit branch -D force-del) &&
	(cd repo && grit branch >../actual) &&
	! grep "force-del" actual
'

test_expect_success 'deleting non-existent branch fails' '
	test_must_fail grit -C repo branch -d no-such-branch
'

# ── rename/move ───────────────────────────────────────────────────────────────

test_expect_success 'branch -m renames a branch' '
	(cd repo && grit branch rename-me) &&
	(cd repo && grit branch -m rename-me renamed) &&
	(cd repo && grit branch >../actual) &&
	! grep "rename-me" actual &&
	grep "renamed" actual
'

test_expect_success 'renamed branch points to same commit' '
	(cd repo && grit rev-parse renamed >../actual) &&
	(cd repo && grit rev-parse HEAD >../expect) &&
	test_cmp expect actual
'

test_expect_success 'branch -M force renames over existing branch' '
	(cd repo && grit branch overwrite-target) &&
	(cd repo && grit branch -M renamed overwrite-target) &&
	(cd repo && grit branch >../actual) &&
	! grep "^  renamed$" actual &&
	grep "overwrite-target" actual
'

# ── create branches at various points ─────────────────────────────────────────

test_expect_success 'create branch at second commit' '
	(cd repo && grit branch beta "$(cat ../../oid_second)") &&
	(cd repo && grit rev-parse beta >../actual) &&
	test_cmp actual ../oid_second
'

test_expect_success 'create branch at first commit' '
	(cd repo && grit branch gamma "$(cat ../../oid_first)") &&
	(cd repo && grit rev-parse gamma >../actual) &&
	test_cmp actual ../oid_first
'

test_expect_success 'branch -v shows different subjects for branches at different commits' '
	(cd repo && grit branch -v >../actual) &&
	grep "beta.*second" actual &&
	grep "gamma.*first" actual
'

# ── multiple branch listing ──────────────────────────────────────────────────

test_expect_success 'branch list includes all created branches' '
	(cd repo && grit branch >../actual) &&
	grep "main" actual &&
	grep "feature" actual &&
	grep "old-branch" actual &&
	grep "beta" actual &&
	grep "gamma" actual
'

# ── branch -a ────────────────────────────────────────────────────────────────

test_expect_success 'branch -a lists all (no remotes means same as default)' '
	(cd repo && grit branch -a >../actual) &&
	grep "main" actual &&
	grep "feature" actual
'

# ── delete multiple branches ─────────────────────────────────────────────────

test_expect_success 'delete beta branch' '
	(cd repo && grit branch -d beta) &&
	(cd repo && grit branch >../actual) &&
	! grep "beta" actual
'

test_expect_success 'delete gamma branch' '
	(cd repo && grit branch -D gamma) &&
	(cd repo && grit branch >../actual) &&
	! grep "gamma" actual
'

# ── checkout and show-current ─────────────────────────────────────────────────

test_expect_success 'checkout switches branch' '
	(cd repo && grit checkout feature) &&
	(cd repo && grit branch --show-current >../actual) &&
	echo feature >expect &&
	test_cmp expect actual
'

test_expect_success 'switch back to main' '
	(cd repo && grit checkout main) &&
	(cd repo && grit branch --show-current >../actual) &&
	echo main >expect &&
	test_cmp expect actual
'

# ── branch from branch name ──────────────────────────────────────────────────

test_expect_success 'create branch from another branch name' '
	(cd repo && grit branch from-feature feature) &&
	(cd repo && grit rev-parse from-feature >../actual) &&
	(cd repo && grit rev-parse feature >../expect) &&
	test_cmp expect actual
'

test_expect_success 'clean up from-feature' '
	(cd repo && grit branch -d from-feature) &&
	(cd repo && grit branch >../actual) &&
	! grep "from-feature" actual
'

# ── final state ───────────────────────────────────────────────────────────────

test_expect_success 'final branch list is clean' '
	(cd repo && grit branch >../actual) &&
	grep "main" actual &&
	grep "feature" actual &&
	grep "old-branch" actual &&
	grep "overwrite-target" actual
'

test_done
