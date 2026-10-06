#!/bin/sh
# Tests for stripspace with various whitespace inputs.

test_description='stripspace extra whitespace scenarios'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Basic trailing whitespace removal ──────────────────────────────────────

test_expect_success 'trailing spaces are removed' '
	printf "hello   \n" | git stripspace >actual &&
	printf "hello\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'trailing tabs are removed' '
	printf "hello\t\t\n" | git stripspace >actual &&
	printf "hello\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'trailing mixed whitespace is removed' '
	printf "hello \t \t\n" | git stripspace >actual &&
	printf "hello\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'trailing whitespace on multiple lines' '
	printf "line1   \nline2\t\nline3 \t \n" | git stripspace >actual &&
	printf "line1\nline2\nline3\n" >expect &&
	test_cmp expect actual
'

# ── Leading blank line removal ─────────────────────────────────────────────

test_expect_success 'leading blank lines are removed' '
	printf "\n\n\nhello\n" | git stripspace >actual &&
	printf "hello\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'leading whitespace-only lines are removed' '
	printf "   \n\t\n  \t  \nhello\n" | git stripspace >actual &&
	printf "hello\n" >expect &&
	test_cmp expect actual
'

# ── Trailing blank line removal ────────────────────────────────────────────

test_expect_success 'trailing blank lines are removed' '
	printf "hello\n\n\n\n" | git stripspace >actual &&
	printf "hello\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'trailing whitespace-only lines are removed' '
	printf "hello\n   \n\t\n" | git stripspace >actual &&
	printf "hello\n" >expect &&
	test_cmp expect actual
'

# ── Consecutive blank line collapsing ──────────────────────────────────────

test_expect_success 'multiple consecutive blank lines collapse to one' '
	printf "a\n\n\n\n\nb\n" | git stripspace >actual &&
	printf "a\n\nb\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'whitespace-only lines between text collapse to one blank' '
	printf "a\n   \n\t\n  \t  \nb\n" | git stripspace >actual &&
	printf "a\n\nb\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'three groups with blanks between them' '
	printf "a\n\n\n\nb\n\n\n\nc\n" | git stripspace >actual &&
	printf "a\n\nb\n\nc\n" >expect &&
	test_cmp expect actual
'

# ── Empty and whitespace-only inputs ───────────────────────────────────────

test_expect_success 'empty input produces empty output' '
	git stripspace </dev/null >actual &&
	test_must_be_empty actual
'

test_expect_success 'single newline produces empty output' '
	printf "\n" | git stripspace >actual &&
	test_must_be_empty actual
'

test_expect_success 'only whitespace produces empty output' '
	printf "   \t  \n  \n\t\t\n" | git stripspace >actual &&
	test_must_be_empty actual
'

test_expect_success 'only spaces (no newline) produces empty output' '
	printf "     " | git stripspace >actual &&
	test_must_be_empty actual
'

test_expect_success 'only tabs (no newline) produces empty output' '
	printf "\t\t\t" | git stripspace >actual &&
	test_must_be_empty actual
'

# ── Text without trailing newline ──────────────────────────────────────────

test_expect_success 'text without trailing newline gets newline appended' '
	printf "hello" | git stripspace >actual &&
	printf "hello\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'text with trailing spaces but no newline is cleaned' '
	printf "hello   " | git stripspace >actual &&
	printf "hello\n" >expect &&
	test_cmp expect actual
'

# ── Indentation preserved ─────────────────────────────────────────────────

test_expect_success 'leading spaces in content are preserved' '
	printf "  hello\n" | git stripspace >actual &&
	printf "  hello\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'leading tabs in content are preserved' '
	printf "\thello\n" | git stripspace >actual &&
	printf "\thello\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'internal spaces are preserved' '
	printf "hello   world\n" | git stripspace >actual &&
	printf "hello   world\n" >expect &&
	test_cmp expect actual
'

# ── Comment stripping (-s) ────────────────────────────────────────────────

test_expect_success '-s strips comment lines' '
	printf "# this is a comment\n" | git stripspace -s >actual &&
	test_must_be_empty actual
'

test_expect_success '-s preserves non-comment lines' '
	printf "hello\n# comment\nworld\n" | git stripspace -s >actual &&
	printf "hello\nworld\n" >expect &&
	test_cmp expect actual
'

test_expect_success '-s strips multiple comment lines' '
	printf "# one\n# two\n# three\n" | git stripspace -s >actual &&
	test_must_be_empty actual
'

test_expect_success '-s with mixed comment and non-comment' '
	printf "# header\ncode\n# middle\nmore code\n# footer\n" | git stripspace -s >actual &&
	printf "code\nmore code\n" >expect &&
	test_cmp expect actual
'

test_expect_success '-s does not strip lines with # in middle' '
	printf "not a # comment\n" | git stripspace -s >actual &&
	printf "not a # comment\n" >expect &&
	test_cmp expect actual
'

# ── Comment lines (-c) ────────────────────────────────────────────────────

test_expect_success '-c prefixes lines with comment char' '
	printf "hello\n" | git stripspace -c >actual &&
	printf "# hello\n" >expect &&
	test_cmp expect actual
'

test_expect_success '-c with multiple lines' '
	printf "a\nb\nc\n" | git stripspace -c >actual &&
	printf "# a\n# b\n# c\n" >expect &&
	test_cmp expect actual
'

test_expect_success '-c with blank line produces commented blank' '
	printf "a\n\nb\n" | git stripspace -c >actual &&
	printf "# a\n#\n# b\n" >expect &&
	test_cmp expect actual
'

test_expect_success '-c on empty input produces empty output' '
	git stripspace -c </dev/null >actual &&
	test_must_be_empty actual
'

test_expect_success '-c with text without trailing newline' '
	printf "hello" | git stripspace -c >actual &&
	printf "# hello\n" >expect &&
	test_cmp expect actual
'

# ── Comment char config ───────────────────────────────────────────────────

test_expect_success '-s with custom commentchar' '
	git init custom-comment &&
	cd custom-comment &&
	git config core.commentchar ";" &&
	printf "; this is a comment\nnot a comment\n" | git stripspace -s >actual &&
	printf "not a comment\n" >expect &&
	test_cmp expect actual
'

test_expect_success '-c with custom commentchar' '
	git init custom-comment-c &&
	cd custom-comment-c &&
	git config core.commentchar ";" &&
	printf "hello\n" | git stripspace -c >actual &&
	printf "; hello\n" >expect &&
	test_cmp expect actual
'

test_done
