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

test_expect_success PYTHON 'smart HTTP redirect does not forward credentials to another host' '
	cat >redirect-credential-probe.py <<\PY &&
import json
import os
import pathlib
import socketserver
import subprocess
import sys
import threading


def pkt(payload):
    return ("%04x" % (len(payload) + 4)).encode() + payload


ADVERTISEMENT = (
    pkt(b"version 2\n") + pkt(b"ls-refs\n") + pkt(b"fetch=shallow\n") + b"0000"
)


class ThreadingServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def response(handler, status, headers=(), body=b""):
    reason = {200: "OK", 302: "Found", 401: "Unauthorized", 405: "Method Not Allowed"}[status]
    lines = [f"HTTP/1.1 {status} {reason}\r\n".encode()]
    for key, value in headers:
        lines.append(f"{key}: {value}\r\n".encode())
    lines.append(f"Content-Length: {len(body)}\r\n".encode())
    lines.append(b"Connection: close\r\n\r\n")
    handler.request.sendall(b"".join(lines) + body)


def read_request(sock):
    data = bytearray()
    while b"\r\n\r\n" not in data:
        chunk = sock.recv(4096)
        if not chunk:
            break
        data.extend(chunk)
    header_bytes, _, remainder = bytes(data).partition(b"\r\n\r\n")
    lines = header_bytes.decode("iso-8859-1").split("\r\n")
    headers = {}
    for line in lines[1:]:
        if ":" in line:
            key, value = line.split(":", 1)
            headers[key.strip().lower()] = value.strip()
    length = int(headers.get("content-length", "0"))
    while len(remainder) < length:
        chunk = sock.recv(length - len(remainder))
        if not chunk:
            break
        remainder += chunk
    return lines[0], headers


state = {"records": [], "redirect_url": ""}


class OriginalHandler(socketserver.BaseRequestHandler):
    def handle(self):
        request_line, headers = read_request(self.request)
        method, _path, _version = request_line.split(" ", 2)
        state["records"].append({"side": "original", "method": method, "headers": headers})
        if method != "GET":
            response(self, 405)
        elif not headers.get("authorization"):
            response(self, 401, (("WWW-Authenticate", "Basic"),))
        else:
            response(self, 302, (("Location", state["redirect_url"]),))


class RedirectedHandler(socketserver.BaseRequestHandler):
    def handle(self):
        request_line, headers = read_request(self.request)
        method, _path, _version = request_line.split(" ", 2)
        state["records"].append({"side": "redirected", "method": method, "headers": headers})
        if method == "GET":
            response(
                self,
                200,
                (("Content-Type", "application/x-git-upload-pack-advertisement"),),
                ADVERTISEMENT,
            )
        elif method == "POST":
            response(
                self,
                200,
                (("Content-Type", "application/x-git-upload-pack-result"),),
                b"0000",
            )
        else:
            response(self, 405)


def fail(message):
    print(message, file=sys.stderr)
    print(json.dumps(state["records"], sort_keys=True), file=sys.stderr)
    sys.exit(1)


root = pathlib.Path.cwd()
helper = root / "credential-helper.sh"
helper.write_text("#!/bin/sh\nprintf \"%s\\n\" username=user password=secret \"\"\n")
helper.chmod(0o700)
cookie_file = root / "cookies"
cookie_file.write_text("Set-Cookie: session=SECRETCOOKIE\n")
config = root / "redirect-credential.gitconfig"
config.write_text(
    "[credential]\n"
    + "\thelper = "
    + str(helper)
    + "\n[http]\n\tcookieFile = "
    + str(cookie_file)
    + "\n"
)

redirected = ThreadingServer(("localhost", 0), RedirectedHandler)
original = ThreadingServer(("127.0.0.1", 0), OriginalHandler)
state["redirect_url"] = (
    f"http://localhost:{redirected.server_address[1]}/repo/info/refs?service=git-upload-pack"
)
threads = []
for server in (redirected, original):
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    threads.append(thread)

env = os.environ.copy()
for key in list(env):
    upper = key.upper()
    if upper in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
        env.pop(key, None)
    if key == "GIT_CONFIG_COUNT" or key.startswith("GIT_CONFIG_KEY_") or key.startswith("GIT_CONFIG_VALUE_"):
        env.pop(key, None)
env["GIT_CONFIG_GLOBAL"] = str(config)
env["GIT_CONFIG_NOSYSTEM"] = "1"
env["GIT_TERMINAL_PROMPT"] = "0"
env["NO_PROXY"] = "127.0.0.1,localhost"

url = f"http://127.0.0.1:{original.server_address[1]}/repo"
try:
    completed = subprocess.run(
        ["git", "clone", url, "redirect-clone"],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=30,
    )
finally:
    for server in (original, redirected):
        server.shutdown()
        server.server_close()
    for thread in threads:
        thread.join(timeout=2)

if completed.returncode != 0:
    print(completed.stdout, file=sys.stderr)
    print(completed.stderr, file=sys.stderr)
    fail("clone failed")

redirected_records = [record for record in state["records"] if record["side"] == "redirected"]
if not any(record["method"] == "GET" for record in redirected_records):
    fail("redirected GET was not observed")
if not any(record["method"] == "POST" for record in redirected_records):
    fail("redirected POST was not observed")
if not any(
    record["side"] == "original" and record["headers"].get("authorization")
    for record in state["records"]
):
    fail("original server did not receive authorization")
if any(record["headers"].get("authorization") for record in redirected_records):
    fail("redirected server received authorization")
if any(record["headers"].get("cookie") for record in redirected_records):
    fail("redirected server received cookie")
PY
	"$PYTHON_PATH" redirect-credential-probe.py
'

test_done
