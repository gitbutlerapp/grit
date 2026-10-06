#!/bin/sh

test_description='Test git notes merge'

. ./test-lib.sh

test_expect_success 'setup' '
	git init -q &&
	test_commit A &&
	test_commit B &&
	test_commit C
'

test_expect_success 'merge notes from another ref' '
	git notes add -m "default note A" A &&
	git notes --ref=other add -m "other note B" B &&
	git notes --ref=other add -m "other note C" C &&
	git notes merge other &&
	git notes list >actual &&
	test_line_count = 3 actual
'

test_expect_success 'merged notes are readable' '
	echo "other note B" >expect &&
	git notes show B >actual &&
	test_cmp expect actual
'

test_expect_success 'merge does not overwrite existing notes' '
	echo "default note A" >expect &&
	git notes show A >actual &&
	test_cmp expect actual
'

test_done
