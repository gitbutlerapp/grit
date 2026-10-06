# `test_commit_bulk` from upstream test-lib-functions.sh (fast-import path).
# Sourced after test-lib.sh (needs test_tick, git, error).

BUG () {
	error "bug in test script: $*"
}

# Efficiently create <nr> commits, each with a unique number (from 1 to <nr>
# by default) in the commit message.
#
# Usage: test_commit_bulk [options] <nr>
#   -C <dir>:
#	Run all git commands in directory <dir>
#   --ref=<n>:
#	ref on which to create commits (default: HEAD)
#   --start=<n>:
#	number commit messages from <n> (default: 1)
#   --message=<msg>:
#	use <msg> as the commit mesasge (default: "commit %s")
#   --filename=<fn>:
#	modify <fn> in each commit (default: %s.t)
#   --contents=<string>:
#	place <string> in each file (default: "content %s")
#   --id=<string>:
#	shorthand to use <string> and %s in message, filename, and contents
#
# The message, filename, and contents strings are evaluated by printf, with the
# first "%s" replaced by the current commit number.
test_commit_bulk () {
	tmpfile=.bulk-commit.input
	indir=.
	ref=HEAD
	n=1
	notick=
	message='commit %s'
	filename='%s.t'
	contents='content %s'
	while test $# -gt 0
	do
		case "$1" in
		-C)
			indir=$2
			shift
			;;
		--ref=*)
			ref=${1#--*=}
			;;
		--start=*)
			n=${1#--*=}
			;;
		--message=*)
			message=${1#--*=}
			;;
		--filename=*)
			filename=${1#--*=}
			;;
		--contents=*)
			contents=${1#--*=}
			;;
		--id=*)
			message="${1#--*=} %s"
			filename="${1#--*=}-%s.t"
			contents="${1#--*=} %s"
			;;
		--notick)
			notick=yes
			;;
		-*)
			BUG "invalid test_commit_bulk option: $1"
			;;
		*)
			break
			;;
		esac
		shift
	done
	total=$1

	# When importing onto HEAD, use the branch ref (e.g. refs/heads/main) in the
	# stream if HEAD is symbolic. Plain `commit HEAD` detaches HEAD and breaks
	# scripts that run `git branch -M` immediately after (t5326 lib-bitmap setup).
	commit_ref=$ref
	if test "$ref" = "HEAD"
	then
		commit_ref=$(git -C "$indir" symbolic-ref -q HEAD) || commit_ref=HEAD
	fi

	add_from=
	if git -C "$indir" rev-parse --quiet --verify "$ref"
	then
		add_from=t
	fi

	while test "$total" -gt 0
	do
		if test -z "$notick"
		then
			test_tick
		fi &&
		echo "commit $commit_ref"
		printf 'author %s <%s> %s\n' \
			"$GIT_AUTHOR_NAME" \
			"$GIT_AUTHOR_EMAIL" \
			"$GIT_AUTHOR_DATE"
		printf 'committer %s <%s> %s\n' \
			"$GIT_COMMITTER_NAME" \
			"$GIT_COMMITTER_EMAIL" \
			"$GIT_COMMITTER_DATE"
		echo "data <<EOF"
		printf "$message\n" $n
		echo "EOF"
		if test -n "$add_from"
		then
			echo "from $commit_ref^0"
			add_from=
		fi
		printf "M 644 inline $filename\n" $n
		echo "data <<EOF"
		printf "$contents\n" $n
		echo "EOF"
		echo
		n=$((n + 1))
		total=$((total - 1))
	done >"$tmpfile"

	git -C "$indir" \
	    -c fastimport.unpacklimit=0 \
	    fast-import <"$tmpfile" || return 1

	rm -f "$tmpfile"

	if test "$ref" = "HEAD"
	then
		git -C "$indir" checkout -f HEAD || return 1
	fi

	# Grit fast-import leaves loose objects; match Git fast-import + pack (t7900, t5332).
	git -C "$indir" repack -a -d -q || return 1

}
