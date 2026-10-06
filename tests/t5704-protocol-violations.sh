#!/bin/sh

test_description='Test responses to violations of the network protocol. In most
of these cases it will generally be acceptable for one side to break off
communications if the other side says something unexpected. We are mostly
making sure that we do not segfault or otherwise behave badly.'

. ./test-lib.sh

test_expect_success 'extra delim packet in v2 ls-refs args' '
	{
		packetize command=ls-refs &&
		packetize "object-format=$(test_oid algo)" &&
		printf 0001 &&
		# protocol expects 0000 flush here
		printf 0001
	} >input &&
	test_must_fail env GIT_PROTOCOL=version=2 \
		git upload-pack . <input 2>err &&
	test_grep "expected flush after ls-refs arguments" err
'

test_expect_success 'extra delim packet in v2 fetch args' '
	{
		packetize command=fetch &&
		packetize "object-format=$(test_oid algo)" &&
		printf 0001 &&
		# protocol expects 0000 flush here
		printf 0001
	} >input &&
	test_must_fail env GIT_PROTOCOL=version=2 \
		git upload-pack . <input 2>err &&
	test_grep "expected flush after fetch arguments" err
'

test_expect_success 'bogus symref in v0 capabilities' '
	test_commit foo &&
	oid=$(git rev-parse HEAD) &&
	dst=refs/heads/foo &&
	{
		printf "%s HEAD\0symref object-format=%s symref=HEAD:%s\n" \
			"$oid" "$GIT_DEFAULT_HASH" "$dst" |
			test-tool pkt-line pack-raw-stdin &&
		printf "0000"
	} >input &&
	git ls-remote --symref --upload-pack="cat input; read junk;:" . >actual &&
	printf "ref: %s\tHEAD\n%s\tHEAD\n" "$dst" "$oid" >expect &&
	test_cmp expect actual
'

test_expect_success PYTHON 'fetch rejects invalid v2 advertised refname' '
	test_when_finished "rm -rf fetch-seed fetch-client fake-upload-pack" &&
	git init fetch-seed &&
	git -C fetch-seed config user.name "Fetch Fixture" &&
	git -C fetch-seed config user.email "fixture@example.invalid" &&
	test_commit -C fetch-seed base &&
	oid=$(git -C fetch-seed rev-parse HEAD) &&
	git clone fetch-seed fetch-client &&
	cp fetch-client/.git/config expect.config &&
	write_script fake-upload-pack "$PYTHON_PATH" <<-\PY &&
	import os
	import sys

	stdin = getattr(sys.stdin, "buffer", sys.stdin)
	stdout = getattr(sys.stdout, "buffer", sys.stdout)

	def pkt(payload):
	    if isinstance(payload, str):
	        payload = payload.encode()
	    stdout.write(("%04x" % (len(payload) + 4)).encode() + payload)
	    stdout.flush()

	for line in [
	    "version 2\n",
	    "agent=git/2.47.0\n",
	    "ls-refs=unborn\n",
	    "fetch=shallow\n",
	    "object-format=sha1\n",
	]:
	    pkt(line)
	stdout.write(b"0000")
	stdout.flush()

	while True:
	    header = stdin.read(4)
	    if not header:
	        sys.exit(0)
	    if header in (b"0000", b"0001", b"0002"):
	        if header == b"0000":
	            break
	        continue
	    size = int(header, 16)
	    stdin.read(size - 4)

	pkt(os.environ["MALICIOUS_OID"] + " refs/heads/../../../config\n")
	stdout.write(b"0000")
	stdout.flush()
	PY
	test_must_fail env MALICIOUS_OID=$oid git -C fetch-client -c protocol.version=2 \
		fetch --upload-pack="$(pwd)/fake-upload-pack" origin >out 2>err &&
	test_cmp expect.config fetch-client/.git/config &&
	test_grep "invalid ref advertisement" err
'

test_done
