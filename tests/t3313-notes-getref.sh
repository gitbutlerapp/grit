#!/bin/sh

test_description='Test git notes get-ref'

. ./test-lib.sh

test_expect_success 'setup' '
	git init -q &&
	test_commit A
'

test_expect_success 'default notes ref' '
	echo "refs/notes/commits" >expect &&
	git notes get-ref >actual &&
	test_cmp expect actual
'

test_expect_success 'custom notes ref' '
	echo "refs/notes/custom" >expect &&
	git notes --ref=refs/notes/custom get-ref >actual &&
	test_cmp expect actual
'

test_expect_success 'short custom notes ref expanded' '
	echo "refs/notes/myref" >expect &&
	git notes --ref=refs/notes/myref get-ref >actual &&
	test_cmp expect actual
'

test_done
