#!/bin/sh

test_description='--full-name does not affect scope in ls-files / ls-tree'

. ./test-lib.sh

test_expect_success 'setup' '
	test_commit root &&
	mkdir sub &&
	test_commit sub/a &&
	test_commit sub/b
'

test_expect_success 'ls-files: --full-name from subdirectory (no pathspec)' '
	(
		cd sub &&
		git ls-files --full-name >actual &&
		cat >expect <<-\EOF &&
		sub/a
		sub/b
		EOF
		test_cmp expect actual
	)
'

test_expect_success 'ls-files: --full-name from subdirectory (explicit pathspec)' '
	(
		cd sub &&
		git ls-files --full-name -- sub >actual &&
		cat >expect <<-\EOF &&
		sub/a
		sub/b
		EOF
		test_cmp expect actual
	)
'

test_expect_success 'ls-tree: --full-name HEAD from subdirectory (no pathspec)' '
	(
		cd sub &&
		git ls-tree --full-name HEAD >actual &&
		grep sub/a actual &&
		grep sub/b actual &&
		! grep root actual
	)
'

test_expect_success 'ls-tree: --full-name HEAD from subdirectory (explicit pathspec)' '
	(
		cd sub &&
		git ls-tree --full-name HEAD -- sub >actual &&
		grep sub/a actual &&
		grep sub/b actual
	)
'

test_done
