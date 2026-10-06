#!/bin/sh
# Tests for cherry with complex branch topologies.

test_description='cherry advanced branch topologies'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- basic cherry usage --------------------------------------------------------

test_expect_success 'setup basic topology: master + topic diverge' '
	(
	git init basic &&
	cd basic &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "base" >base.txt &&
	git add base.txt &&
	test_tick &&
	git commit -m "base" &&
	git branch topic &&
	echo "m1" >m1.txt && git add m1.txt && test_tick && git commit -m "master-1" &&
	echo "m2" >m2.txt && git add m2.txt && test_tick && git commit -m "master-2" &&
	git checkout topic &&
	echo "t1" >t1.txt && git add t1.txt && test_tick && git commit -m "topic-1" &&
	echo "t2" >t2.txt && git add t2.txt && test_tick && git commit -m "topic-2" &&
	echo "t3" >t3.txt && git add t3.txt && test_tick && git commit -m "topic-3"
	)
'

test_expect_success 'cherry lists all topic commits as +' '
	(
	cd basic &&
	git cherry master topic >out.txt &&
	count=$(grep -c "^+" out.txt) &&
	test "$count" -eq 3
	)
'

test_expect_success 'cherry -v shows commit subjects' '
	(
	cd basic &&
	git cherry -v master topic >out.txt &&
	grep "topic-1" out.txt &&
	grep "topic-2" out.txt &&
	grep "topic-3" out.txt
	)
'

test_expect_success 'cherry in reverse shows master commits' '
	(
	cd basic &&
	git cherry topic master >out.txt &&
	count=$(grep -c "^+" out.txt) &&
	test "$count" -eq 2
	)
'

test_expect_success 'cherry -v in reverse shows master subjects' '
	(
	cd basic &&
	git cherry -v topic master >out.txt &&
	grep "master-1" out.txt &&
	grep "master-2" out.txt
	)
'

# -- cherry-pick detection (- marker) -----------------------------------------

test_expect_success 'setup: cherry-pick a topic commit to master' '
	(
	cd basic &&
	topic_first=$(git log --format="%H" topic | tail -2 | head -1) &&
	git checkout master &&
	git cherry-pick "$topic_first"
	)
'

test_expect_success 'cherry marks cherry-picked commit with -' '
	(
	cd basic &&
	git cherry master topic >out.txt &&
	minus=$(grep -c "^-" out.txt) &&
	plus=$(grep -c "^+" out.txt) &&
	test "$minus" -eq 1 &&
	test "$plus" -eq 2
	)
'

test_expect_success 'cherry -v marks cherry-picked with - and shows subject' '
	(
	cd basic &&
	git cherry -v master topic >out.txt &&
	grep "^- .* topic-1$" out.txt
	)
'

# -- LIMIT argument ------------------------------------------------------------

test_expect_success 'cherry with LIMIT restricts output' '
	(
	cd basic &&
	limit=$(git log --format="%H" topic | head -2 | tail -1) &&
	git cherry master topic "$limit" >out.txt &&
	count=$(wc -l <out.txt | tr -d " ") &&
	test "$count" -eq 1
	)
'

test_expect_success 'cherry with LIMIT shows only commits after limit' '
	(
	cd basic &&
	limit=$(git log --format="%H" topic | head -2 | tail -1) &&
	git cherry -v master topic "$limit" >out.txt &&
	grep "topic-3" out.txt &&
	! grep "topic-1" out.txt
	)
'

# -- empty result --------------------------------------------------------------

test_expect_success 'setup: cherry-pick all topic commits' '
	(
	cd basic &&
	git checkout master &&
	for h in $(git log --format="%H" topic | head -2); do
		git cherry-pick "$h" 2>/dev/null || true
	done
	)
'

test_expect_success 'cherry shows all - when all commits are picked' '
	(
	cd basic &&
	git cherry master topic >out.txt &&
	plus=$(grep -c "^+" out.txt || true) &&
	test "$plus" -eq 0
	)
'

# -- linear topology (no divergence) ------------------------------------------

test_expect_success 'setup linear topology' '
	(
	git init linear &&
	cd linear &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "l0" >l0.txt && git add l0.txt && test_tick && git commit -m "l0" &&
	git branch old-master &&
	echo "l1" >l1.txt && git add l1.txt && test_tick && git commit -m "l1" &&
	echo "l2" >l2.txt && git add l2.txt && test_tick && git commit -m "l2"
	)
'

test_expect_success 'cherry on linear: new commits show as +' '
	(
	cd linear &&
	git cherry old-master master >out.txt &&
	count=$(grep -c "^+" out.txt) &&
	test "$count" -eq 2
	)
'

test_expect_success 'cherry on linear reverse: nothing unique in old' '
	(
	cd linear &&
	git cherry master old-master >out.txt &&
	test_must_be_empty out.txt
	)
'

# -- multiple branches ---------------------------------------------------------

test_expect_success 'setup multiple branches' '
	(
	git init multi &&
	cd multi &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "base" >base.txt && git add base.txt && test_tick && git commit -m "base" &&
	git branch branch-a &&
	git branch branch-b &&
	git checkout branch-a &&
	echo "a1" >a1.txt && git add a1.txt && test_tick && git commit -m "a1" &&
	echo "a2" >a2.txt && git add a2.txt && test_tick && git commit -m "a2" &&
	git checkout branch-b &&
	echo "b1" >b1.txt && git add b1.txt && test_tick && git commit -m "b1"
	)
'

test_expect_success 'cherry between sibling branches a vs b' '
	(
	cd multi &&
	git cherry branch-b branch-a >out.txt &&
	count=$(grep -c "^+" out.txt) &&
	test "$count" -eq 2
	)
'

test_expect_success 'cherry between sibling branches b vs a' '
	(
	cd multi &&
	git cherry branch-a branch-b >out.txt &&
	count=$(grep -c "^+" out.txt) &&
	test "$count" -eq 1
	)
'

# -- cherry with identical patches across branches ----------------------------

test_expect_success 'setup identical patches' '
	(
	git init identical &&
	cd identical &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "init" >init.txt && git add init.txt && test_tick && git commit -m "init" &&
	git branch side &&
	echo "change" >change.txt && git add change.txt && test_tick && git commit -m "add-change" &&
	git checkout side &&
	echo "change" >change.txt && git add change.txt && test_tick && git commit -m "add-change"
	)
'

test_expect_success 'identical patches detected as already applied (-)' '
	(
	cd identical &&
	git cherry master side >out.txt &&
	grep "^-" out.txt
	)
'

# -- cherry defaults HEAD when not specified -----------------------------------

test_expect_success 'cherry with only upstream defaults HEAD as branch' '
	(
	cd basic &&
	git checkout topic &&
	git cherry master >out.txt &&
	test -s out.txt
	)
'

# -- verbose output format -----------------------------------------------------

test_expect_success 'cherry -v output has hash and subject on each line' '
	(
	cd multi &&
	git cherry -v branch-b branch-a >out.txt &&
	while IFS= read -r line; do
		echo "$line" | grep -q "^[+-] [0-9a-f]" || {
			echo "bad line: $line"
			return 1
		}
	done <out.txt
	)
'

test_expect_success 'cherry without -v has hash only per line' '
	(
	cd multi &&
	git cherry branch-b branch-a >out.txt &&
	while IFS= read -r line; do
		echo "$line" | grep -q "^[+-] [0-9a-f]*$" || {
			echo "bad line: $line"
			return 1
		}
	done <out.txt
	)
'

# -- single commit on topic ---------------------------------------------------

test_expect_success 'cherry with single topic commit' '
	(
	git init single &&
	cd single &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "s0" >s0.txt && git add s0.txt && test_tick && git commit -m "s0" &&
	git branch feat &&
	git checkout feat &&
	echo "s1" >s1.txt && git add s1.txt && test_tick && git commit -m "s1" &&
	git cherry master feat >out.txt &&
	count=$(grep -c "^+" out.txt) &&
	test "$count" -eq 1
	)
'

# -- long chain ----------------------------------------------------------------

test_expect_success 'setup long topic chain' '
	(
	git init longchain &&
	cd longchain &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "lc-base" >lc.txt && git add lc.txt && test_tick && git commit -m "lc-base" &&
	git branch long-topic &&
	git checkout long-topic &&
	for i in 1 2 3 4 5 6 7 8 9 10; do
		echo "lc-$i" >"lc-$i.txt" && git add "lc-$i.txt" && test_tick && git commit -m "lc-$i" || return 1
	done
	)
'

test_expect_success 'cherry lists all 10 commits from long chain' '
	(
	cd longchain &&
	git cherry master long-topic >out.txt &&
	count=$(grep -c "^+" out.txt) &&
	test "$count" -eq 10
	)
'

test_expect_success 'cherry -v lists all 10 with subjects' '
	(
	cd longchain &&
	git cherry -v master long-topic >out.txt &&
	grep "lc-1$" out.txt &&
	grep "lc-10$" out.txt
	)
'

# -- cherry output is deterministic --------------------------------------------

test_expect_success 'cherry output is deterministic across runs' '
	(
	cd multi &&
	git cherry -v branch-b branch-a >run1.txt &&
	git cherry -v branch-b branch-a >run2.txt &&
	test_cmp run1.txt run2.txt
	)
'

test_done
