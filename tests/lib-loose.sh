# Support routines for hand-crafting loose objects.

# The minimal test-lib does not set test_hash_algo; default to sha1 so loose_obj
# and tests that configure extensions.objectformat work.
test_hash_algo=${test_hash_algo:-sha1}
export test_hash_algo
# test-lib.sh does not set upstream-style hash prereqs; satisfy `test_have_prereq SHA1`
# for scripts that source this helper (e.g. t1512) when using the default algorithm.
if test "$test_hash_algo" = sha1
then
	test_set_prereq SHA1
fi

# Match git/t/oid-info entries used by t1006 et al. for $(test_oid deadbeef).
if test -n "${TEST_OID_CACHE_FILE:-}" && test -d "$(dirname "$TEST_OID_CACHE_FILE")"
then
	test_oid_cache <<'OIDCACHE'
deadbeef	sha1:deadbeefdeadbeefdeadbeefdeadbeefdeadbeef
deadbeef_short	sha1:deadbeefdeadbeefdeadbeefdeadbeefdeadbee
OIDCACHE
fi

# Write a loose object into the odb at $1, with object type $2 and contents
# from stdin. Writes the oid to stdout. Example:
#
#   oid=$(echo foo | loose_obj .git/objects blob)
#
loose_obj () {
	cat >tmp_loose.content &&
	size=$(wc -c <tmp_loose.content) &&
	{
		# Do not quote $size here; we want the shell
		# to strip whitespace that "wc" adds on some platforms.
		printf "%s %s\0" "$2" $size &&
		cat tmp_loose.content
	} >tmp_loose.raw &&

	oid=$(test-tool $test_hash_algo <tmp_loose.raw) &&
	suffix=${oid#??} &&
	prefix=${oid%$suffix} &&
	dir=$1/$prefix &&
	file=$dir/$suffix &&

	test-tool zlib deflate <tmp_loose.raw >tmp_loose.zlib &&
	mkdir -p "$dir" &&
	mv tmp_loose.zlib "$file" &&

	rm tmp_loose.raw tmp_loose.content &&
	echo "$oid"
}
