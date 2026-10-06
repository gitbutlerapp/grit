#!/bin/sh
# Tests for ls-tree --format with custom format strings.

test_description='ls-tree --format custom output'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

SYS_GIT=$(command -v git)
REAL_GIT=${REAL_GIT:-$SYS_GIT}

# ── Setup ──────────────────────────────────────────────────────────────────

test_expect_success 'setup: repository with blobs, trees, and nested structure' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "hello" >file.txt &&
	echo "world" >other.txt &&
	mkdir -p sub/deep &&
	echo "nested" >sub/nested.txt &&
	echo "deeper" >sub/deep/file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial commit" &&
	echo "big content with more data" >big.txt &&
	"$REAL_GIT" add big.txt &&
	"$REAL_GIT" commit -m "add big file"
	)
'

# ── Basic --format with objectname ─────────────────────────────────────────

test_expect_success 'ls-tree --format with %(objectname)' '
	(
	cd repo &&
	git ls-tree --format="%(objectname)" HEAD >actual &&
	git ls-tree HEAD | awk "{print \$3}" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree --format with %(objecttype)' '
	(
	cd repo &&
	git ls-tree --format="%(objecttype)" HEAD >actual &&
	git ls-tree HEAD | awk "{print \$2}" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree --format with %(objectmode) gives octal modes' '
	(
	cd repo &&
	git ls-tree --format="%(objectmode)" HEAD >actual &&
	while read mode; do
		echo "$mode" | grep -qE "^[0-9]{6}$" ||
			{ echo "bad mode: $mode"; return 1; }
	done <actual
	)
'

test_expect_success 'ls-tree --format with %(objectmode)' '
	(
	cd repo &&
	git ls-tree --format="%(objectmode)" HEAD >actual &&
	git ls-tree HEAD | awk "{print \$1}" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree --format with %(path)' '
	(
	cd repo &&
	git ls-tree --format="%(path)" HEAD >actual &&
	git ls-tree --name-only HEAD >expect &&
	test_cmp expect actual
	)
'

# ── Combined format strings ────────────────────────────────────────────────

test_expect_success 'ls-tree --format combining objectname and path' '
	(
	cd repo &&
	git ls-tree --format="%(objectname) %(path)" HEAD >actual &&
	git ls-tree HEAD | awk "{print \$3, \$4}" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree --format with literal text around placeholders' '
	(
	cd repo &&
	git ls-tree --format="OBJ:%(objectname) FILE:%(path)" HEAD >actual &&
	git ls-tree HEAD | awk "{print \"OBJ:\" \$3, \"FILE:\" \$4}" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree --format with tab separator' '
	(
	cd repo &&
	git ls-tree --format="%(objectmode)	%(objecttype)	%(objectname)	%(path)" HEAD >actual &&
	git ls-tree HEAD | sed "s/ /	/g" >expect_raw &&
	test_line_count -gt 0 actual
	)
'

test_expect_success 'ls-tree --format with only literal text' '
	(
	cd repo &&
	git ls-tree --format="constant" HEAD >actual &&
	lines=$(wc -l <actual) &&
	expected=$(git ls-tree HEAD | wc -l) &&
	test "$lines" = "$expected"
	)
'

# ── Format with recursive ──────────────────────────────────────────────────

test_expect_success 'ls-tree -r --format with %(path) shows full paths' '
	(
	cd repo &&
	git ls-tree -r --format="%(path)" HEAD >actual &&
	git ls-tree -r --name-only HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree -r --format with %(objectname) recurses' '
	(
	cd repo &&
	git ls-tree -r --format="%(objectname)" HEAD >actual &&
	git ls-tree -r HEAD | awk "{print \$3}" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-tree -r --format shows blobs not trees' '
	(
	cd repo &&
	git ls-tree -r --format="%(objecttype) %(path)" HEAD >actual &&
	! grep "^tree " actual
	)
'

# ── Format with -d (trees only) ────────────────────────────────────────────

test_expect_success 'ls-tree -d --format shows only trees' '
	(
	cd repo &&
	git ls-tree -d --format="%(objecttype) %(path)" HEAD >actual &&
	! grep "^blob " actual &&
	grep "^tree " actual
	)
'

test_expect_success 'ls-tree -d --format with %(objectname)' '
	(
	cd repo &&
	git ls-tree -d --format="%(objectname)" HEAD >actual &&
	git ls-tree -d HEAD | awk "{print \$3}" >expect &&
	test_cmp expect actual
	)
'

# ── Format output count matches ────────────────────────────────────────────

test_expect_success 'ls-tree --format line count matches normal output' '
	(
	cd repo &&
	git ls-tree HEAD | wc -l >expect_count &&
	git ls-tree --format="%(objectname)" HEAD | wc -l >actual_count &&
	test_cmp expect_count actual_count
	)
'

test_expect_success 'ls-tree -r --format line count matches' '
	(
	cd repo &&
	git ls-tree -r HEAD | wc -l >expect_count &&
	git ls-tree -r --format="%(objectname)" HEAD | wc -l >actual_count &&
	test_cmp expect_count actual_count
	)
'

# ── Format with objectsize:padded ──────────────────────────────────────────

test_expect_success 'ls-tree --format objecttype distinguishes blob and tree' '
	(
	cd repo &&
	git ls-tree --format="%(objecttype) %(path)" HEAD >actual &&
	grep "^blob " actual &&
	grep "^tree " actual
	)
'

test_expect_success 'ls-tree --format %(objectname) matches -l output OIDs' '
	(
	cd repo &&
	git ls-tree --format="%(objectname)" HEAD >fmt_oids &&
	git ls-tree -l HEAD | awk "{print \$3}" >long_oids &&
	test_cmp long_oids fmt_oids
	)
'

# ── Format with various tree-ish ───────────────────────────────────────────

test_expect_success 'ls-tree --format works with parent commit' '
	(
	cd repo &&
	parent=$(git rev-parse HEAD^) &&
	git ls-tree --format="%(objectname) %(path)" "$parent" >actual &&
	test_line_count -gt 0 actual
	)
'

test_expect_success 'ls-tree --format works with tree hash directly' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	git ls-tree --format="%(path)" "$tree" >actual &&
	git ls-tree --format="%(path)" HEAD >expect &&
	test_cmp expect actual
	)
'

# ── Format with path restriction ───────────────────────────────────────────

test_expect_success 'ls-tree --format with path restriction' '
	(
	cd repo &&
	git ls-tree --format="%(path)" HEAD -- sub >actual &&
	test_line_count -eq 1 actual &&
	grep "^sub" actual
	)
'

test_expect_success 'ls-tree -r --format with path restriction' '
	(
	cd repo &&
	git ls-tree -r --format="%(path)" HEAD -- sub >actual &&
	while read path; do
		case "$path" in
		sub/*) ;;
		*) echo "unexpected: $path"; return 1 ;;
		esac
	done <actual
	)
'

# ── Edge cases ─────────────────────────────────────────────────────────────

test_expect_success 'ls-tree --format with empty tree' '
	(
	cd repo &&
	empty_tree=$(git mktree </dev/null) &&
	git ls-tree --format="%(objectname)" "$empty_tree" >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'ls-tree --format %(objectname) outputs valid hex OIDs' '
	(
	cd repo &&
	git ls-tree -r --format="%(objectname)" HEAD >actual &&
	while read oid; do
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" ||
			{ echo "bad OID: $oid"; return 1; }
	done <actual
	)
'

test_expect_success 'ls-tree --format same tree-ish gives deterministic output' '
	(
	cd repo &&
	git ls-tree --format="%(objectmode) %(objecttype) %(objectname) %(path)" HEAD >out1 &&
	git ls-tree --format="%(objectmode) %(objecttype) %(objectname) %(path)" HEAD >out2 &&
	test_cmp out1 out2
	)
'

test_expect_success 'ls-tree --format reproduces default output' '
	(
	cd repo &&
	git ls-tree --format="%(objectmode) %(objecttype) %(objectname)	%(path)" HEAD >formatted &&
	git ls-tree HEAD >default_out &&
	test_cmp default_out formatted
	)
'

test_expect_success 'ls-tree -r -t --format shows trees and blobs' '
	(
	cd repo &&
	git ls-tree -r -t --format="%(objecttype) %(path)" HEAD >actual &&
	grep "^tree " actual &&
	grep "^blob " actual
	)
'

test_expect_success 'ls-tree --format with newline in literal text' '
	(
	cd repo &&
	count=$(git ls-tree HEAD | wc -l) &&
	git ls-tree --format="%(path)" HEAD >actual &&
	test_line_count -eq "$count" actual
	)
'

test_done
