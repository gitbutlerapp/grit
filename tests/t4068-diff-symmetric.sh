#!/bin/sh
test_description='grit diff symmetric (A...B) and merge-base diffs

The A...B symmetric diff syntax is not yet implemented in grit.
Tests here verify manual merge-base + diff-tree workaround and
mark the A...B syntax as expected failures.'

. ./test-lib.sh

test_expect_success 'setup diverged branches' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@test.com" &&
	echo "base" >base.txt &&
	git add base.txt &&
	git commit -m "base" &&
	git branch topic &&
	echo "main-change" >main.txt &&
	git add main.txt &&
	git commit -m "main commit" &&
	git checkout topic &&
	echo "topic-change" >topic.txt &&
	git add topic.txt &&
	git commit -m "topic commit" &&
	git checkout main
	)
'

# --- Manual merge-base approach (works today) ---

test_expect_success 'merge-base finds common ancestor' '
	(
	cd repo &&
	mb=$(git merge-base main topic) &&
	test -n "$mb"
	)
'

test_expect_success 'diff-tree from merge-base to topic shows only topic changes' '
	(
	cd repo &&
	mb=$(git merge-base main topic) &&
	topic_sha=$(git rev-parse topic) &&
	git diff-tree -r --name-only "$mb" "$topic_sha" >out &&
	grep "topic\.txt" out &&
	! grep "main\.txt" out
	)
'

test_expect_success 'diff-tree from merge-base to main shows only main changes' '
	(
	cd repo &&
	mb=$(git merge-base main topic) &&
	main_sha=$(git rev-parse main) &&
	git diff-tree -r --name-only "$mb" "$main_sha" >out &&
	grep "main\.txt" out &&
	! grep "topic\.txt" out
	)
'

test_expect_success 'diff-tree from merge-base shows name-status' '
	(
	cd repo &&
	mb=$(git merge-base main topic) &&
	topic_sha=$(git rev-parse topic) &&
	git diff-tree -r --name-status "$mb" "$topic_sha" >out &&
	grep "A" out &&
	grep "topic\.txt" out
	)
'

test_expect_success 'diff-tree from merge-base with --stat' '
	(
	cd repo &&
	mb=$(git merge-base main topic) &&
	topic_sha=$(git rev-parse topic) &&
	git diff-tree --stat "$mb" "$topic_sha" >out &&
	grep "topic" out
	)
'

test_expect_success 'setup branches with file modified on both sides' '
	(
	cd repo &&
	git checkout main &&
	echo "shared-line" >shared.txt &&
	git add shared.txt &&
	git commit -m "add shared" &&
	git branch topic2 &&
	echo "main-edit" >>shared.txt &&
	git add shared.txt &&
	git commit -m "main edits shared" &&
	git checkout topic2 &&
	echo "topic-edit" >>shared.txt &&
	git add shared.txt &&
	git commit -m "topic edits shared" &&
	git checkout main
	)
'

test_expect_success 'merge-base diff shows topic side of shared file change' '
	(
	cd repo &&
	mb=$(git merge-base main topic2) &&
	topic2_sha=$(git rev-parse topic2) &&
	git diff-tree -r --name-only "$mb" "$topic2_sha" >out &&
	grep "shared\.txt" out
	)
'

# --- Symmetric diff syntax A...B (not yet implemented) ---

test_expect_success 'diff main...topic shows only topic changes' '
	(
	cd repo &&
	git diff main...topic --name-only >out &&
	grep "topic\.txt" out &&
	! grep "main\.txt" out
	)
'

test_expect_success 'diff topic...main shows only main changes' '
	(
	cd repo &&
	git diff topic...main --name-only >out &&
	grep "main\.txt" out &&
	! grep "topic\.txt" out
	)
'

test_expect_success 'diff A...B --stat' '
	(
	cd repo &&
	git diff main...topic --stat >out &&
	grep "topic" out
	)
'

test_expect_success 'diff A...B --name-status' '
	(
	cd repo &&
	git diff main...topic --name-status >out &&
	grep "topic\.txt" out
	)
'

test_expect_success 'diff A...B --exit-code' '
	(
	cd repo &&
	test_must_fail git diff main...topic --exit-code
	)
'

test_expect_success 'diff A...B with pathspec' '
	(
	cd repo &&
	git diff main...topic -- topic.txt >out &&
	grep "topic\.txt" out
	)
'

# --- additional merge-base diff tests ---

test_expect_success 'diff from merge-base to topic with --numstat' '
	(
	cd repo &&
	mb=$(git merge-base main topic) &&
	topic_sha=$(git rev-parse topic) &&
	git diff --numstat "$mb" "$topic_sha" >out &&
	grep "topic\.txt" out
	)
'

test_expect_success 'diff-tree from merge-base shows full diff patch' '
	(
	cd repo &&
	mb=$(git merge-base main topic) &&
	topic_sha=$(git rev-parse topic) &&
	git diff "$mb" "$topic_sha" >out &&
	grep "^diff --git" out &&
	grep "topic" out
	)
'

test_expect_success 'diff --exit-code from merge-base to topic' '
	(
	cd repo &&
	mb=$(git merge-base main topic) &&
	topic_sha=$(git rev-parse topic) &&
	test_must_fail git diff --exit-code "$mb" "$topic_sha"
	)
'

test_expect_success 'diff --quiet from merge-base to topic' '
	(
	cd repo &&
	mb=$(git merge-base main topic) &&
	topic_sha=$(git rev-parse topic) &&
	test_must_fail git diff --quiet "$mb" "$topic_sha"
	)
'

test_expect_success 'diff from merge-base to topic shows topic.txt added' '
	(
	cd repo &&
	mb=$(git merge-base main topic) &&
	topic_sha=$(git rev-parse topic) &&
	git diff --name-status "$mb" "$topic_sha" >out &&
	grep "^A" out &&
	grep "topic\.txt" out
	)
'

test_expect_success 'diff-tree merge-base topic2 shows shared.txt change' '
	(
	cd repo &&
	mb=$(git merge-base main topic2) &&
	topic2_sha=$(git rev-parse topic2) &&
	git diff --numstat "$mb" "$topic2_sha" >out &&
	grep "shared\.txt" out
	)
'

test_done
