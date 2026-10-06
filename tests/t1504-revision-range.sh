#!/bin/sh
# Tests for revision range parsing via rev-parse and rev-list.
# Covers HEAD~N, HEAD^N, HEAD^^, tag~N, --verify, --short, --short=N,
# --is-inside-work-tree, --git-dir, --show-toplevel, chained operators,
# and error handling for bad revisions.

test_description='revision range parsing (tilde, caret, tags, flags)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup revision range repo with 6 commits' '
	(
	git init revrange &&
	cd revrange &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	test_commit first &&
	test_commit second &&
	test_commit third &&
	test_commit fourth &&
	test_commit fifth &&
	test_commit sixth
	)
'

test_expect_success 'rev-parse HEAD resolves to a 40-char hex sha' '
	(
	cd revrange &&
	sha=$(git rev-parse HEAD) &&
	test $(echo "$sha" | wc -c) -ge 40 &&
	echo "$sha" | grep "^[0-9a-f]*$"
	)
'

test_expect_success 'rev-parse HEAD~0 equals HEAD' '
	(
	cd revrange &&
	head=$(git rev-parse HEAD) &&
	head0=$(git rev-parse HEAD~0) &&
	test "$head" = "$head0"
	)
'

test_expect_success 'rev-parse HEAD^0 equals HEAD' '
	(
	cd revrange &&
	head=$(git rev-parse HEAD) &&
	head0=$(git rev-parse HEAD^0) &&
	test "$head" = "$head0"
	)
'

test_expect_success 'rev-parse HEAD~1 is parent of HEAD' '
	(
	cd revrange &&
	head=$(git rev-parse HEAD) &&
	parent=$(git rev-parse HEAD~1) &&
	test "$head" != "$parent"
	)
'

test_expect_success 'rev-parse HEAD^1 equals HEAD~1' '
	(
	cd revrange &&
	tilde=$(git rev-parse HEAD~1) &&
	caret=$(git rev-parse HEAD^1) &&
	test "$tilde" = "$caret"
	)
'

test_expect_success 'rev-parse HEAD^ equals HEAD~1' '
	(
	cd revrange &&
	tilde=$(git rev-parse HEAD~1) &&
	caret=$(git rev-parse HEAD^) &&
	test "$tilde" = "$caret"
	)
'

test_expect_success 'rev-parse HEAD~2 goes two generations back' '
	(
	cd revrange &&
	g1=$(git rev-parse HEAD~1) &&
	g2=$(git rev-parse HEAD~2) &&
	test "$g1" != "$g2"
	)
'

test_expect_success 'rev-parse HEAD~N is consistent with chaining' '
	(
	cd revrange &&
	chain=$(git rev-parse HEAD^1^1) &&
	tilde2=$(git rev-parse HEAD~2) &&
	test "$chain" = "$tilde2"
	)
'

test_expect_success 'rev-parse HEAD^^ equals HEAD~2' '
	(
	cd revrange &&
	dblcaret=$(git rev-parse HEAD^^) &&
	tilde2=$(git rev-parse HEAD~2) &&
	test "$dblcaret" = "$tilde2"
	)
'

test_expect_success 'rev-parse HEAD~5 reaches root commit' '
	(
	cd revrange &&
	root=$(git rev-parse HEAD~5) &&
	test -n "$root" &&
	# root is the "first" commit
	msg=$(git log --format="%s" --max-count 1 $root) &&
	test "$msg" = "first"
	)
'

test_expect_success 'rev-parse HEAD~N fails for N beyond history' '
	(
	cd revrange &&
	test_must_fail git rev-parse HEAD~6
	)
'

test_expect_success 'rev-parse by tag name' '
	(
	cd revrange &&
	tag_sha=$(git rev-parse third) &&
	test -n "$tag_sha" &&
	msg=$(git log --format="%s" --max-count 1 $tag_sha) &&
	test "$msg" = "third"
	)
'

test_expect_success 'rev-parse tag~1' '
	(
	cd revrange &&
	tag_parent=$(git rev-parse third~1) &&
	msg=$(git log --format="%s" --max-count 1 $tag_parent) &&
	test "$msg" = "second"
	)
'

test_expect_success 'rev-parse tag^1' '
	(
	cd revrange &&
	tag_parent=$(git rev-parse third^1) &&
	expected=$(git rev-parse third~1) &&
	test "$tag_parent" = "$expected"
	)
'

test_expect_success 'rev-parse --verify HEAD succeeds' '
	(
	cd revrange &&
	sha=$(git rev-parse --verify HEAD) &&
	expected=$(git rev-parse HEAD) &&
	test "$sha" = "$expected"
	)
'

test_expect_success 'rev-parse --verify HEAD~1 succeeds' '
	(
	cd revrange &&
	sha=$(git rev-parse --verify HEAD~1) &&
	expected=$(git rev-parse HEAD~1) &&
	test "$sha" = "$expected"
	)
'

test_expect_success 'rev-parse --verify with bad ref fails' '
	(
	cd revrange &&
	test_must_fail git rev-parse --verify nonexistent-ref
	)
'

test_expect_success 'rev-parse --short gives abbreviated hash' '
	(
	cd revrange &&
	short=$(git rev-parse --short HEAD) &&
	full=$(git rev-parse HEAD) &&
	# Short should be a prefix of full
	echo "$full" | grep "^$short" &&
	# Short should be shorter than full
	short_len=$(echo "$short" | wc -c) &&
	full_len=$(echo "$full" | wc -c) &&
	test "$short_len" -lt "$full_len"
	)
'

test_expect_success 'rev-parse --short=4 gives 4-char hash' '
	(
	cd revrange &&
	short=$(git rev-parse --short=4 HEAD) &&
	len=$(echo -n "$short" | wc -c) &&
	test "$len" -eq 4
	)
'

test_expect_success 'rev-parse --is-inside-work-tree returns true' '
	(
	cd revrange &&
	result=$(git rev-parse --is-inside-work-tree) &&
	test "$result" = "true"
	)
'

test_expect_success 'rev-parse --git-dir shows .git' '
	(
	cd revrange &&
	result=$(git rev-parse --git-dir) &&
	test "$result" = ".git"
	)
'

test_expect_success 'rev-parse main resolves to HEAD' '
	(
	cd revrange &&
	main=$(git rev-parse main) &&
	head=$(git rev-parse HEAD) &&
	test "$main" = "$head"
	)
'

test_expect_success 'rev-parse main~1 works' '
	(
	cd revrange &&
	main_p=$(git rev-parse main~1) &&
	head_p=$(git rev-parse HEAD~1) &&
	test "$main_p" = "$head_p"
	)
'

test_expect_success 'all ancestors are distinct' '
	(
	cd revrange &&
	h0=$(git rev-parse HEAD) &&
	h1=$(git rev-parse HEAD~1) &&
	h2=$(git rev-parse HEAD~2) &&
	h3=$(git rev-parse HEAD~3) &&
	h4=$(git rev-parse HEAD~4) &&
	h5=$(git rev-parse HEAD~5) &&
	test "$h0" != "$h1" &&
	test "$h1" != "$h2" &&
	test "$h2" != "$h3" &&
	test "$h3" != "$h4" &&
	test "$h4" != "$h5"
	)
'

test_expect_success 'rev-parse with full SHA works' '
	(
	cd revrange &&
	full=$(git rev-parse HEAD) &&
	resolved=$(git rev-parse $full) &&
	test "$full" = "$resolved"
	)
'

test_expect_success 'rev-parse with abbreviated SHA works' '
	(
	cd revrange &&
	full=$(git rev-parse HEAD) &&
	abbrev=$(echo "$full" | cut -c1-7) &&
	resolved=$(git rev-parse $abbrev) &&
	test "$full" = "$resolved"
	)
'

test_expect_success 'rev-parse outside repo fails gracefully' '
	dir="$TRASH_DIRECTORY/no-repo-here" &&
	mkdir -p "$dir" &&
	(cd "$dir" &&
	 test_must_fail git rev-parse HEAD 2>&1)
'

test_done
