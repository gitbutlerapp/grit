git_show_ref_exists=${git_show_ref_exists:-git show-ref --exists}

test_expect_success 'setup' '
	grit init repo &&
	(
	cd "$TRASH_DIRECTORY/repo" &&
	tree=$(git write-tree) &&
	commit=$(echo base | git commit-tree "$tree") &&
	grit update-ref refs/heads/master "$commit" &&
	grit update-ref refs/heads/main "$commit" &&
	grit update-ref refs/heads/side "$commit"
	)
'

test_expect_success '--exists with existing reference' '
	cd "$TRASH_DIRECTORY/repo" &&
	${git_show_ref_exists} refs/heads/side
'

test_expect_success '--exists with missing reference' '
	cd "$TRASH_DIRECTORY/repo" &&
	test_must_fail ${git_show_ref_exists} refs/heads/does-not-exist
'

test_expect_success '--exists does not use DWIM' '
	cd "$TRASH_DIRECTORY/repo" &&
	test_must_fail ${git_show_ref_exists} side 2>err &&
	grep "reference does not exist" err
'

test_expect_success '--exists with HEAD' '
	cd "$TRASH_DIRECTORY/repo" &&
	${git_show_ref_exists} HEAD
'

test_expect_success '--exists with arbitrary symref' '
	cd "$TRASH_DIRECTORY/repo" &&
	git symbolic-ref refs/symref refs/heads/side &&
	${git_show_ref_exists} refs/symref
'

test_expect_success '--exists with dangling symref' '
	cd "$TRASH_DIRECTORY/repo" &&
	git symbolic-ref refs/heads/dangling refs/heads/does-not-exist &&
	${git_show_ref_exists} refs/heads/dangling
'

test_expect_success '--exists with directory reports missing ref' '
	cd "$TRASH_DIRECTORY/repo" &&
	test_must_fail ${git_show_ref_exists} refs/heads 2>err &&
	grep "reference does not exist" err
'

test_expect_success '--exists with non-existent special ref' '
	cd "$TRASH_DIRECTORY/repo" &&
	test_must_fail ${git_show_ref_exists} FETCH_HEAD
'

test_expect_success '--exists with pseudoref (CHERRY_PICK_HEAD)' '
	cd "$TRASH_DIRECTORY/repo" &&
	oid=$(git rev-parse refs/heads/master) &&
	git update-ref CHERRY_PICK_HEAD "$oid" &&
	${git_show_ref_exists} CHERRY_PICK_HEAD
'

test_expect_success '--exists reports missing for full nonexistent path' '
	cd "$TRASH_DIRECTORY/repo" &&
	test_must_fail ${git_show_ref_exists} refs/tags/nonexistent 2>err &&
	grep "reference does not exist" err
'

test_expect_success '--exists succeeds for refs/heads/master' '
	cd "$TRASH_DIRECTORY/repo" &&
	${git_show_ref_exists} refs/heads/master
'

test_done
