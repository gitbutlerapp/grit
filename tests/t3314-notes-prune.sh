#!/bin/sh

test_description='Test git notes prune'

. ./test-lib.sh

test_expect_success 'setup' '
	git init -q &&
	test_commit A &&
	test_commit B
'

test_expect_success 'notes prune with no stale notes is no-op' '
	git notes add -m "note A" A &&
	git notes add -m "note B" B &&
	git notes list >before &&
	git notes prune &&
	git notes list >after &&
	test_cmp before after
'

test_expect_success 'notes prune -v reports nothing when clean' '
	git notes prune -v 2>err &&
	test_must_be_empty err
'

test_done
