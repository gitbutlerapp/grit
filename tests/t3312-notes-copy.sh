#!/bin/sh

test_description='Test git notes copy'

. ./test-lib.sh

test_expect_success 'setup' '
	git init -q &&
	test_commit A &&
	test_commit B &&
	test_commit C
'

test_expect_success 'copy note from one object to another' '
	git notes add -m "note on A" A &&
	git notes copy A B &&
	echo "note on A" >expect &&
	git notes show B >actual &&
	test_cmp expect actual
'

test_expect_success 'copy note fails when target already has note' '
	git notes add -m "note on C" C &&
	test_must_fail git notes copy A C
'

test_expect_success 'copy note with --force overwrites existing' '
	git notes add -f -m "original C note" C &&
	git notes copy -f A C &&
	echo "note on A" >expect &&
	git notes show C >actual &&
	test_cmp expect actual
'

test_done
