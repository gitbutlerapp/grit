#!/bin/sh
# checkout-index with many files, edge cases, and performance.

test_description='grit checkout-index with many files and edge cases'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with many files' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "author@example.com" &&
	grit config user.name "A U Thor" &&
	for i in $(seq 1 100); do
		echo "content $i" >file_$i.txt
	done &&
	grit add file_*.txt &&
	test_tick &&
	grit commit -m "100 files"
	)
'

test_expect_success 'checkout-index --all restores all files' '
	(
	cd repo &&
	rm -f file_*.txt &&
	grit checkout-index --all &&
	count=$(ls file_*.txt 2>/dev/null | wc -l) &&
	test "$count" -eq 100
	)
'

test_expect_success 'checkout-index restores file content correctly' '
	(
	cd repo &&
	rm -f file_1.txt &&
	grit checkout-index file_1.txt &&
	echo "content 1" >expect &&
	test_cmp expect file_1.txt
	)
'

test_expect_success 'checkout-index --force overwrites existing files' '
	(
	cd repo &&
	echo "modified" >file_1.txt &&
	grit checkout-index -f file_1.txt &&
	echo "content 1" >expect &&
	test_cmp expect file_1.txt
	)
'

test_expect_success 'checkout-index without --force does not overwrite existing' '
	(
	cd repo &&
	echo "modified" >file_1.txt &&
	grit checkout-index file_1.txt 2>/dev/null &&
	echo "modified" >expect &&
	test_cmp expect file_1.txt
	)
'

test_expect_success 'checkout-index single file' '
	(
	cd repo &&
	rm -f file_50.txt &&
	grit checkout-index file_50.txt &&
	echo "content 50" >expect &&
	test_cmp expect file_50.txt
	)
'

test_expect_success 'checkout-index multiple named files' '
	(
	cd repo &&
	rm -f file_1.txt file_2.txt file_3.txt &&
	grit checkout-index file_1.txt file_2.txt file_3.txt &&
	test -f file_1.txt &&
	test -f file_2.txt &&
	test -f file_3.txt
	)
'

test_expect_success 'checkout-index with --all and --force' '
	(
	cd repo &&
	for i in $(seq 1 100); do
		echo "wrong" >file_$i.txt
	done &&
	grit checkout-index --all -f &&
	echo "content 42" >expect &&
	test_cmp expect file_42.txt
	)
'

test_expect_success 'checkout-index creates file in subdirectory' '
	(
	cd repo &&
	mkdir -p subdir &&
	echo "sub content" >subdir/sub.txt &&
	grit add subdir/sub.txt &&
	rm -f subdir/sub.txt &&
	grit checkout-index subdir/sub.txt &&
	echo "sub content" >expect &&
	test_cmp expect subdir/sub.txt
	)
'

test_expect_success 'checkout-index --mkdir creates leading directories' '
	(
	cd repo &&
	mkdir -p deep/nested/dir &&
	echo "deep" >deep/nested/dir/file.txt &&
	grit add deep/nested/dir/file.txt &&
	rm -rf deep &&
	grit checkout-index --mkdir deep/nested/dir/file.txt &&
	test -f deep/nested/dir/file.txt
	)
'

test_expect_success 'checkout-index -n does not create files' '
	(
	cd repo &&
	rm -f file_99.txt &&
	grit checkout-index -n file_99.txt &&
	! test -f file_99.txt
	)
'

test_expect_success 'checkout-index -q suppresses errors' '
	(
	cd repo &&
	echo "exists" >file_99.txt &&
	grit checkout-index -q file_99.txt 2>err &&
	test_must_be_empty err
	)
'

test_expect_success 'checkout-index preserves executable permission' '
	(
	cd repo &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	grit add script.sh &&
	rm -f script.sh &&
	grit checkout-index script.sh &&
	test -x script.sh
	)
'

test_expect_success 'checkout-index with 200 files' '
	(
	cd repo &&
	for i in $(seq 101 200); do
		echo "more content $i" >file_$i.txt
	done &&
	grit add file_*.txt &&
	rm -f file_*.txt &&
	grit checkout-index --all &&
	count=$(ls file_*.txt 2>/dev/null | wc -l) &&
	test "$count" -eq 200
	)
'

test_expect_success 'checkout-index --all speed: 200 files in reasonable time' '
	(
	cd repo &&
	rm -f file_*.txt &&
	start=$(date +%s) &&
	grit checkout-index --all &&
	end=$(date +%s) &&
	elapsed=$((end - start)) &&
	test "$elapsed" -lt 10
	)
'

test_expect_success 'checkout-index after read-tree reset' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	grit read-tree --reset "$tree" &&
	rm -f file_*.txt &&
	grit checkout-index --all -f &&
	test -f file_1.txt
	)
'

test_expect_success 'checkout-index restores file removed from working tree' '
	(
	cd repo &&
	rm file_10.txt &&
	grit checkout-index file_10.txt &&
	test -f file_10.txt
	)
'

test_expect_success 'checkout-index with existing file in the way without --force skips it' '
	(
	cd repo &&
	echo "untracked" >untracked.txt &&
	grit update-index --add untracked.txt &&
	echo "blocker" >untracked.txt &&
	grit checkout-index untracked.txt 2>/dev/null &&
	echo "blocker" >expect &&
	test_cmp expect untracked.txt
	)
'

test_expect_success 'checkout-index --force overwrites existing file' '
	(
	cd repo &&
	grit checkout-index -f untracked.txt &&
	echo "untracked" >expect &&
	test_cmp expect untracked.txt
	)
'

test_expect_success 'checkout-index with empty file' '
	(
	cd repo &&
	: >empty.txt &&
	grit add empty.txt &&
	rm -f empty.txt &&
	grit checkout-index empty.txt &&
	test -f empty.txt &&
	! test -s empty.txt
	)
'

test_expect_success 'checkout-index does not affect other index entries' '
	(
	cd repo &&
	grit ls-files -s >before &&
	rm -f file_5.txt &&
	grit checkout-index file_5.txt &&
	grit ls-files -s >after &&
	test_cmp before after
	)
'

test_expect_success 'checkout-index after modifying index' '
	(
	cd repo &&
	echo "new content" >file_1.txt &&
	grit update-index --add file_1.txt &&
	rm -f file_1.txt &&
	grit checkout-index file_1.txt &&
	echo "new content" >expect &&
	test_cmp expect file_1.txt
	)
'

test_expect_success 'checkout-index -u updates stat info' '
	(
	cd repo &&
	rm -f file_2.txt &&
	grit checkout-index -u file_2.txt &&
	test -f file_2.txt
	)
'

test_expect_success 'checkout-index with file containing special characters in content' '
	(
	cd repo &&
	printf "line1\nline2\ttab\n" >special.txt &&
	grit add special.txt &&
	rm -f special.txt &&
	grit checkout-index special.txt &&
	printf "line1\nline2\ttab\n" >expect &&
	test_cmp expect special.txt
	)
'

test_expect_success 'checkout-index --all restores various tracked files' '
	(
	cd repo &&
	rm -f file_1.txt file_50.txt empty.txt special.txt &&
	grit checkout-index --all -f &&
	test -f file_1.txt &&
	test -f file_50.txt &&
	test -f empty.txt &&
	test -f special.txt
	)
'

test_expect_success 'checkout-index of nonexistent index entry fails' '
	(
	cd repo &&
	test_must_fail grit checkout-index no_such_file.txt 2>/dev/null
	)
'

test_expect_success 'checkout-index with fresh index from read-tree' '
	(
	cd repo &&
	rm -f .git/index &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	grit read-tree "$tree" &&
	rm -f file_*.txt &&
	grit checkout-index --all &&
	count=$(ls file_*.txt 2>/dev/null | wc -l) &&
	test "$count" -ge 100
	)
'

test_expect_success 'checkout-index twice is idempotent with --force' '
	(
	cd repo &&
	grit checkout-index -f --all &&
	grit checkout-index -f --all &&
	echo "content 1" >expect &&
	test_cmp expect file_1.txt
	)
'

test_done
