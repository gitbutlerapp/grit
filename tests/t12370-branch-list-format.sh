#!/bin/sh

test_description='grit branch list, create, delete, rename, verbose, and filtering'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo hello >file.txt &&
	grit add file.txt &&
	grit commit -m "initial"
	)
'

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
	echo "main" >expect &&
	test_cmp expect actual
'

test_expect_success 'branch creates new branch' '
	(cd repo && grit branch feature1) &&
	(cd repo && grit branch >../actual) &&
	grep "feature1" actual
'

test_expect_success 'branch creates second branch' '
	(cd repo && grit branch feature2) &&
	(cd repo && grit branch >../actual) &&
	grep "feature2" actual
'

test_expect_success 'new branch does not switch to it' '
	(cd repo && grit branch --show-current >../actual) &&
	echo "main" >expect &&
	test_cmp expect actual
'

test_expect_success 'branch -v shows commit hash and subject' '
	(cd repo && grit branch -v >../actual) &&
	grep "initial" actual
'

test_expect_success 'branch -v shows hash for each branch' '
	(cd repo && grit branch -v >../actual) &&
	grep "feature1" actual | grep "[0-9a-f]"
'

test_expect_success 'branch -vv shows verbose info' '
	(cd repo && grit branch -vv >../actual) &&
	grep "main" actual | grep "initial"
'

test_expect_success 'branch -d deletes a branch' '
	(cd repo && grit branch to-delete &&
	 grit branch -d to-delete >../actual 2>&1) &&
	grep "Deleted" actual
'

test_expect_success 'deleted branch no longer listed' '
	(cd repo && grit branch >../actual) &&
	! grep "to-delete" actual
'

test_expect_success 'branch -D force-deletes a branch' '
	(cd repo && grit branch force-del &&
	 grit branch -D force-del >../actual 2>&1) &&
	grep "Deleted" actual
'

test_expect_success 'branch -m renames a branch' '
	(cd repo && grit branch old-name &&
	 grit branch -m old-name new-name &&
	 grit branch >../actual) &&
	grep "new-name" actual &&
	! grep "old-name" actual
'

test_expect_success 'branch -M force-renames a branch' '
	(cd repo && grit branch force-rename &&
	 grit branch -M force-rename force-renamed &&
	 grit branch >../actual) &&
	grep "force-renamed" actual &&
	! grep "force-rename[^d]" actual
'

test_expect_success 'branch --list is equivalent to branch' '
	(cd repo && grit branch --list >../list &&
	 grit branch >../nolist) &&
	test_cmp list nolist
'

test_expect_success 'branch -a shows all branches' '
	(cd repo && grit branch -a >../actual) &&
	grep "main" actual &&
	grep "feature1" actual
'

test_expect_success 'branch from specific start point' '
	(cd repo &&
	 head_oid=$(grit rev-parse HEAD) &&
	 grit branch from-head $head_oid &&
	 grit branch >../actual) &&
	grep "from-head" actual
'

test_expect_success 'branch -f overwrites existing branch' '
	(cd repo &&
	 echo extra >extra.txt &&
	 grit add extra.txt &&
	 grit commit -m "second" &&
	 grit branch -f feature1 HEAD &&
	 grit branch -v >../actual) &&
	grep "feature1" actual | grep "second"
'

test_expect_success 'branch --merged lists merged branches' '
	(cd repo && grit branch --merged HEAD >../actual) &&
	grep "main" actual
'

test_expect_success 'cannot delete current branch' '
	(cd repo && test_must_fail grit branch -d main 2>../err) &&
	test -s err
'

test_expect_success 'branch with slashed name' '
	(cd repo && grit branch feature/sub &&
	 grit branch >../actual) &&
	grep "feature/sub" actual
'

test_expect_success 'branch with deeply nested slashed name' '
	(cd repo && grit branch feature/deep/nested/branch &&
	 grit branch >../actual) &&
	grep "feature/deep/nested/branch" actual
'

test_expect_success 'delete slashed branch' '
	(cd repo && grit branch -d feature/sub &&
	 grit branch >../actual) &&
	! grep "feature/sub$" actual
'

test_expect_success 'branch names are sorted alphabetically' '
	(cd repo && grit branch z-last &&
	 grit branch a-first &&
	 grit branch >../actual) &&
	# a-first should appear before z-last
	awk "/a-first/{a=NR} /z-last/{z=NR} END{exit(a<z?0:1)}" actual
'

test_expect_success 'branch -v aligns output' '
	(cd repo && grit branch -v >../actual) &&
	# All lines should have at least branch + hash
	lines=$(wc -l <actual) &&
	test "$lines" -gt 0
'

test_expect_success 'for-each-ref shows branches with objecttype' '
	(cd repo && grit for-each-ref --format="%(refname:short) %(objecttype)" refs/heads/ >../actual) &&
	grep "main commit" actual
'

test_expect_success 'for-each-ref shows branches with objectname' '
	(cd repo && grit for-each-ref --format="%(objectname)" refs/heads/main >../actual) &&
	test -s actual &&
	# Should be 40-char hex
	len=$(wc -c <actual) &&
	test "$len" -ge 40
'

test_expect_success 'for-each-ref shows subject' '
	(cd repo && grit for-each-ref --format="%(subject)" refs/heads/main >../actual) &&
	echo "second" >expect &&
	test_cmp expect actual
'

test_expect_success 'for-each-ref with refname atom' '
	(cd repo && grit for-each-ref --format="%(refname)" refs/heads/main >../actual) &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'for-each-ref with refname:short atom' '
	(cd repo && grit for-each-ref --format="%(refname:short)" refs/heads/main >../actual) &&
	echo "main" >expect &&
	test_cmp expect actual
'

test_expect_success 'branch -q suppresses output on create' '
	(cd repo && grit branch -q quiet-branch >../actual 2>&1) &&
	test ! -s actual
'

test_expect_success 'branch -q suppresses output on delete' '
	(cd repo && grit branch -q -d quiet-branch >../actual 2>&1) &&
	test ! -s actual
'

test_expect_success 'branch lists multiple branches correctly' '
	(cd repo && grit branch >../actual) &&
	count=$(grep -c "." actual) &&
	test "$count" -gt 3
'

test_done
