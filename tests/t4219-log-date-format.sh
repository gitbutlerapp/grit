#!/bin/sh
test_description='grit log --date=<format>

Tests various date format options: short, iso, iso-strict, rfc, raw, unix, relative.'

. ./test-lib.sh

test_expect_success 'setup: repo with a commit at known time' '
	(
	git init repo &&
	cd repo &&
	git config user.name "A U Thor" &&
	git config user.email "author@example.com" &&
	test_tick &&
	echo a >file.txt && git add file.txt && git commit -m "initial"
	)
'

test_expect_success '--date=short shows YYYY-MM-DD' '
	(
	cd repo &&
	grit log -1 --date=short --format="tformat:%ad" >out &&
	grep "^[0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}$" out
	)
'

test_expect_success '--date=iso shows ISO-like format' '
	(
	cd repo &&
	grit log -1 --date=iso --format="tformat:%ad" >out &&
	grep "[0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\} [0-9]\{2\}:[0-9]\{2\}:[0-9]\{2\}" out
	)
'

test_expect_success '--date=iso-strict shows strict ISO 8601' '
	(
	cd repo &&
	grit log -1 --date=iso-strict --format="tformat:%ad" >out &&
	grep "[0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}T[0-9]\{2\}:[0-9]\{2\}:[0-9]\{2\}" out
	)
'

test_expect_success '--date=rfc shows RFC 2822 format' '
	(
	cd repo &&
	grit log -1 --date=rfc --format="tformat:%ad" >out &&
	grep "[A-Z][a-z]\{2\}," out
	)
'

test_expect_success '--date=raw shows unix timestamp + offset' '
	(
	cd repo &&
	grit log -1 --date=raw --format="tformat:%ad" >out &&
	grep "^[0-9]\+ [+-][0-9]\{4\}$" out
	)
'

test_expect_success '--date=unix shows bare unix timestamp' '
	(
	cd repo &&
	grit log -1 --date=unix --format="tformat:%ad" >out &&
	grep "^[0-9]\+$" out
	)
'

test_expect_success '--date=relative shows relative time' '
	(
	cd repo &&
	grit log -1 --date=relative --format="tformat:%ad" >out &&
	grep "ago\|seconds\|minutes\|hours\|days\|months\|years" out
	)
'

test_expect_success '--date=short in header (medium format)' '
	(
	cd repo &&
	grit log -1 --date=short >out &&
	grep "^Date:" out | grep "[0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}"
	)
'

test_expect_success '--date=iso in header (medium format)' '
	(
	cd repo &&
	grit log -1 --date=iso >out &&
	grep "^Date:" out | grep "[0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\} [0-9]\{2\}:[0-9]\{2\}:[0-9]\{2\}"
	)
'

test_expect_success '--date=raw in header' '
	(
	cd repo &&
	grit log -1 --date=raw >out &&
	grep "^Date:" out | grep "[0-9]\+ [+-][0-9]\{4\}"
	)
'

test_expect_success '%at shows unix timestamp (author)' '
	(
	cd repo &&
	grit log -1 --format="tformat:%at" >out &&
	grep "^[0-9]\+$" out
	)
'

test_expect_success '%ct shows unix timestamp (committer)' '
	(
	cd repo &&
	grit log -1 --format="tformat:%ct" >out &&
	grep "^[0-9]\+$" out
	)
'

test_expect_success '%at and %ct are consistent with --date=raw' '
	(
	cd repo &&
	grit log -1 --format="tformat:%at" >at_out &&
	grit log -1 --date=raw --format="tformat:%ad" >raw_out &&
	at_ts=$(cat at_out) &&
	raw_ts=$(cut -d" " -f1 <raw_out) &&
	test "$at_ts" = "$raw_ts"
	)
'

test_done
