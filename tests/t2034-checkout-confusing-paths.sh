#!/bin/sh

test_description='checkout/clone refuse to write .git-aliasing paths (CVE-2014-9390)

A crafted tree whose entries name `.git` (or a case/Unicode fold of it) must not
be materialized into the working tree, or a `checkout`/`clone` could overwrite
the repository own `.git/config` and gain code execution via core.sshCommand.'

. ./test-lib.sh

test_expect_success 'set up base repo and a tree carrying .git/<file>' '
	echo content >file &&
	git add file &&
	git commit -m base &&
	blob=$(git hash-object -w file) &&
	# tree holding a single file named "config"
	dotgit_subtree=$(printf "100644 blob %s\tconfig" "$blob" | git mktree)
'

# For each dangerous directory name, build a tree `<name>/config`, commit it, and
# confirm checkout refuses it without disturbing the real .git/config.
while read name pretty; do
	: ${pretty:=$name}
	test_expect_success "checkout refuses tree with $pretty/ component" '
		evil_tree=$(printf "040000 tree %s\t%s" "$dotgit_subtree" "$name" | git mktree) &&
		evil_commit=$(git commit-tree "$evil_tree" -m evil) &&
		cp .git/config config.orig &&
		test_must_fail git checkout -f "$evil_commit" &&
		test_cmp config.orig .git/config &&
		git checkout -f main
	'
done <<-EOF
	.git
	.Git case-fold
	.GIT upper
	git~1 ntfs-short
EOF

test_expect_success 'enable protections explicitly too' '
	git config core.protectHFS true &&
	git config core.protectNTFS true
'

test_expect_success 'checkout refuses .git‌/ HFS ignorable-codepoint fold' '
	u200c=$(printf "\342\200\214") &&
	evil_tree=$(printf "040000 tree %s\t.git%s" "$dotgit_subtree" "$u200c" | git mktree) &&
	evil_commit=$(git commit-tree "$evil_tree" -m evil) &&
	cp .git/config config.orig &&
	test_must_fail git checkout -f "$evil_commit" &&
	test_cmp config.orig .git/config &&
	git checkout -f main
'

test_expect_success 'legitimate dotfiles still check out' '
	gi=$(echo "*.log" | git hash-object -w --stdin) &&
	good_tree=$(printf "100644 blob %s\t.gitignore\n100644 blob %s\t.gitmodules\n100644 blob %s\t.gitattributes\n100644 blob %s\t.mailmap" \
		"$gi" "$gi" "$gi" "$gi" | git mktree) &&
	good_commit=$(git commit-tree "$good_tree" -m good) &&
	git checkout -f "$good_commit" &&
	test_path_is_file .gitignore &&
	test_path_is_file .gitmodules &&
	test_path_is_file .gitattributes &&
	test_path_is_file .mailmap &&
	git checkout -f main
'

test_done
