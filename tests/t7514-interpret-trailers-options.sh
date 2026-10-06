#!/bin/sh
# Tests for interpret-trailers --where, --if-exists, --if-missing options.

test_description='interpret-trailers placement and conditional options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	cat >message_with_trailers <<-\EOF
	subject

	body

	Signed-off-by: A
	EOF
'

test_expect_success '--where=end appends new trailer after existing (default)' '
	cat >expected <<-\EOF &&
	subject

	body

	Signed-off-by: A
	Reviewed-by: B
	EOF
	git interpret-trailers --where=end --trailer "Reviewed-by: B" \
		<message_with_trailers >actual &&
	test_cmp expected actual
'

test_expect_success '--where=start prepends new trailer before existing' '
	cat >expected <<-\EOF &&
	subject

	body

	Reviewed-by: B
	Signed-off-by: A
	EOF
	git interpret-trailers --where=start --trailer "Reviewed-by: B" \
		<message_with_trailers >actual &&
	test_cmp expected actual
'

test_expect_success '--where=after appends (same as end)' '
	cat >expected <<-\EOF &&
	subject

	body

	Signed-off-by: A
	Reviewed-by: B
	EOF
	git interpret-trailers --where=after --trailer "Reviewed-by: B" \
		<message_with_trailers >actual &&
	test_cmp expected actual
'

test_expect_success '--where=before prepends (same as start)' '
	cat >expected <<-\EOF &&
	subject

	body

	Reviewed-by: B
	Signed-off-by: A
	EOF
	git interpret-trailers --where=before --trailer "Reviewed-by: B" \
		<message_with_trailers >actual &&
	test_cmp expected actual
'

test_expect_success '--if-exists=replace replaces matching trailer' '
	cat >expected <<-\EOF &&
	subject

	body

	Signed-off-by: B
	EOF
	git interpret-trailers --if-exists=replace --trailer "Signed-off-by: B" \
		<message_with_trailers >actual &&
	test_cmp expected actual
'

test_expect_success '--if-exists=donothing skips if key exists' '
	cat >expected <<-\EOF &&
	subject

	body

	Signed-off-by: A
	EOF
	git interpret-trailers --if-exists=donothing --trailer "Signed-off-by: B" \
		<message_with_trailers >actual &&
	test_cmp expected actual
'

test_expect_success '--if-exists=add always adds even if key exists' '
	cat >expected <<-\EOF &&
	subject

	body

	Signed-off-by: A
	Signed-off-by: B
	EOF
	git interpret-trailers --if-exists=add --trailer "Signed-off-by: B" \
		<message_with_trailers >actual &&
	test_cmp expected actual
'

test_expect_success '--if-missing=add adds trailer when key is missing (default)' '
	cat >expected <<-\EOF &&
	subject

	body

	Signed-off-by: A
	Reviewed-by: B
	EOF
	git interpret-trailers --if-missing=add --trailer "Reviewed-by: B" \
		<message_with_trailers >actual &&
	test_cmp expected actual
'

test_expect_success '--if-missing=donothing skips trailer when key is missing' '
	cat >expected <<-\EOF &&
	subject

	body

	Signed-off-by: A
	EOF
	git interpret-trailers --if-missing=donothing --trailer "Reviewed-by: B" \
		<message_with_trailers >actual &&
	test_cmp expected actual
'

test_done
