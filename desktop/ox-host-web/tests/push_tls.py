"""One-use, isolated TLS Web Push receiver for the Rust transport regression."""

import http.server
from pathlib import Path
import ssl
import subprocess
import sys

root = Path(sys.argv[1])


def openssl(*args):
    subprocess.run(
        ["openssl", *args],
        cwd=root,
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


openssl(
    "req",
    "-x509",
    "-newkey",
    "rsa:2048",
    "-nodes",
    "-days",
    "1",
    "-subj",
    "/CN=OX fixture CA",
    "-addext",
    "basicConstraints=critical,CA:TRUE",
    "-out",
    "ca.pem",
    "-keyout",
    "ca.key",
)
openssl(
    "req",
    "-new",
    "-newkey",
    "rsa:2048",
    "-nodes",
    "-subj",
    "/CN=web.push.apple.com",
    "-out",
    "leaf.csr",
    "-keyout",
    "leaf.key",
)
(root / "extensions").write_text(
    "subjectAltName=DNS:web.push.apple.com\nextendedKeyUsage=serverAuth\n"
)
openssl(
    "x509",
    "-req",
    "-in",
    "leaf.csr",
    "-CA",
    "ca.pem",
    "-CAkey",
    "ca.key",
    "-CAcreateserial",
    "-days",
    "1",
    "-out",
    "leaf.pem",
    "-extfile",
    "extensions",
)


class Receiver(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        assert self.path == "/synthetic"
        assert self.headers.get("Authorization", "").startswith("vapid t=")
        assert self.headers.get("Content-Encoding") == "aes128gcm"
        (root / "encrypted").write_bytes(
            self.rfile.read(int(self.headers["Content-Length"]))
        )
        self.send_response(201)
        self.end_headers()


with http.server.HTTPServer(("127.0.0.1", 0), Receiver) as server:
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(root / "leaf.pem", root / "leaf.key")
    server.socket = ctx.wrap_socket(server.socket, server_side=True)
    server.timeout = 8
    print(server.server_port, flush=True)
    server.handle_request()
