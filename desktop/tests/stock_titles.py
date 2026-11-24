"""Verify native title inference using disposable profiles and loopback provider credentials."""

import argparse
from contextlib import ExitStack
import json
from pathlib import Path
import shutil
import ssl
import threading
import subprocess
import tempfile
import time

import claude_stock_session as claude
from stock_session import jwt, ACCOUNT, Fixture as ResponsesFixture
from http.server import ThreadingHTTPServer


class TitleProvider(claude.Provider):
    reply = ResponsesFixture.reply

    def do_GET(self):
        if self.path.startswith("/backend-api/wham/"):
            return ResponsesFixture.do_GET(self)
        self.reply(200, "application/json", b'{"models":[],"data":[]}')

    def do_POST(self):
        data = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        req = json.loads(data)
        if self.path.split("?")[0].endswith("count_tokens"):
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'{"input_tokens":32}')
            return
        native_claude = hasattr(self.server, "native_tls")
        if self.path.split("?")[0].endswith("/messages"):
            kind = "claude"
            msg = {
                "id": "msg_title",
                "model": req["model"],
                "role": "assistant",
                "type": "message",
                "content": [{"type": "text", "text": "Database connection timeout"}],
                "stop_reason": "end_turn",
                "stop_sequence": None,
                "usage": {"input_tokens": 10, "output_tokens": 4},
            }
            events = [
                {
                    "type": "message_start",
                    "message": dict(msg, content=[], stop_reason=None),
                },
                {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""},
                },
                {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {
                        "type": "text_delta",
                        "text": "Database connection timeout",
                    },
                },
                {"type": "content_block_stop", "index": 0},
                {
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": 4},
                },
                {"type": "message_stop"},
            ]
        elif self.path.split("?")[0].endswith("/responses"):
            kind = "codex"
            msg = {
                "id": "resp_title",
                "object": "response",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 10, "output_tokens": 4, "total_tokens": 14},
            }
            events = [
                {
                    "type": "response.created",
                    "response": dict(msg, status="in_progress"),
                },
                {
                    "type": "response.output_item.done",
                    "item": {
                        "id": "m",
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {
                                "type": "output_text",
                                "text": "Database connection timeout",
                            }
                        ],
                    },
                },
                {"type": "response.completed", "response": msg},
            ]
        else:
            self.send_error(404)
            return
        self.server.seen.append(
            (
                kind,
                req.get("model"),
                req.get("tools") or [],
                self.headers.get("Authorization") or self.headers.get("x-api-key"),
                "Name a conversation" in data.decode(),
                native_claude,
            )
        )
        body = (
            "".join(
                "event: " + e["type"] + "\ndata: " + json.dumps(e) + "\n\n"
                for e in events
            )
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def codex_endpoint(root, stack):
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
        "/CN=Title test CA",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-keyout",
        "title-ca.key",
        "-out",
        "title-ca.pem",
    )
    openssl(
        "req",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-subj",
        "/CN=127.0.0.1",
        "-keyout",
        "title-tls.key",
        "-out",
        "title.csr",
    )
    (root / "title.ext").write_text(
        "subjectAltName=IP:127.0.0.1\nextendedKeyUsage=serverAuth\n"
    )
    openssl(
        "x509",
        "-req",
        "-in",
        "title.csr",
        "-CA",
        "title-ca.pem",
        "-CAkey",
        "title-ca.key",
        "-CAcreateserial",
        "-days",
        "1",
        "-out",
        "title-tls.pem",
        "-extfile",
        "title.ext",
    )
    endpoint = stack.enter_context(ThreadingHTTPServer(("127.0.0.1", 0), TitleProvider))
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(root / "title-tls.pem", root / "title-tls.key")
    endpoint.socket = tls.wrap_socket(endpoint.socket, server_side=True)
    endpoint.seen = []
    threading.Thread(target=endpoint.serve_forever, daemon=True).start()
    stack.callback(endpoint.shutdown)
    return endpoint


def run(test, ox, codex_bin, claude_bin):
    with tempfile.TemporaryDirectory() as tmp, ExitStack() as stack:
        root = Path(tmp)
        # Both setup helpers bind only loopback. CONNECT allows only the synthetic native Claude endpoint.
        claude.Provider = TitleProvider
        server, env = claude.setup(root, claude_bin, stack)
        native, cert = claude.native_endpoint(root, stack)
        server.seen = []
        native.seen = []
        https = codex_endpoint(root, stack)
        server.tls_origin = f"https://127.0.0.1:{https.server_port}"
        https.tls_origin = server.tls_origin
        (root / "bin/codex").symlink_to(codex_bin)
        if node := shutil.which("node"):
            (root / "bin/node").symlink_to(node)
        expiry = int(time.time()) + 3600
        access = jwt({"exp": expiry})
        tokens = {
            "id_token": jwt(
                {
                    "exp": expiry,
                    "https://api.openai.com/auth": {
                        "chatgpt_account_id": ACCOUNT,
                        "chatgpt_plan_type": "plus",
                    },
                }
            ),
            "access_token": access,
            "refresh_token": "fixture",
            "account_id": ACCOUNT,
        }
        (root / "codex/auth.json").write_text(
            json.dumps(
                {
                    "OPENAI_API_KEY": None,
                    "tokens": tokens,
                    "last_refresh": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                }
            )
        )
        (root / "codex/config.toml").write_text(
            f'chatgpt_base_url="http://127.0.0.1:{server.server_port}/backend-api"\n'
            f'[mcp_servers.sentinel]\ncommand="/usr/bin/touch"\nargs=["{root}/unwanted-mcp"]\n'
        )
        (root / "codex/codeox.toml").write_text(
            "".join(
                f'[providers.{key}]\nname="Fixture"\nwire_api="{wire}"\n'
                f'base_url="http://127.0.0.1:{server.server_port}/v1"\ndiscovery=false\nenv_key="OX_CLAUDE_FIXTURE_KEY"\n'
                f'[[providers.{key}.models]]\nid="demo"\ncontext_window=131072\n'
                for key, wire in [
                    ("chat", "responses"),
                    ("messages", "anthropic_messages"),
                ]
            )
        )
        token = native.expected_auth.removeprefix("Bearer ")
        (root / "native/.credentials.json").write_text(
            json.dumps(
                {
                    "claudeAiOauth": {
                        "accessToken": token,
                        "refreshToken": "fixture",
                        "expiresAt": expiry * 1000,
                        "scopes": ["user:inference", "user:profile"],
                        "subscriptionType": "max",
                        "rateLimitTier": "default_claude_max_5x",
                    }
                }
            )
        )
        (root / "native/.credentials.json").chmod(0o600)
        (root / "native/.claude.json").write_text(
            json.dumps(
                {
                    "oauthAccount": {
                        "accountUuid": ACCOUNT,
                        "organizationUuid": ACCOUNT,
                        "emailAddress": "title@example.invalid",
                    }
                }
            )
        )
        for kind, billing, model in [
            ("codex", "api", "chat/demo"),
            ("claude", "api", "messages/demo"),
            ("codex", "subscription", "gpt-5.5"),
            ("claude", "subscription", "claude-sonnet-4-6"),
            ("codex", "legacy", "demo"),
        ]:
            current = dict(
                env,
                OX_TEST_OX_BIN=str(ox),
                OX_TITLE_RUNTIME=kind,
                OX_TITLE_BILLING=billing,
                OX_TITLE_MODEL=model,
                OX_TITLE_FIXTURE="loopback",
            )
            target = server
            if billing == "legacy":
                (root / "codex/codeox.toml").unlink()
                with (root / "codex/config.toml").open("a") as config:
                    config.write(
                        f'\n[model_providers.legacy]\nname="Fixture"\nbase_url="http://127.0.0.1:{server.server_port}/v1"\nenv_key="OX_CLAUDE_FIXTURE_KEY"\nwire_api="responses"\n'
                    )
                profile = root / "codex/config.toml"
                profile.write_text(
                    'model="demo"\nmodel_provider="legacy"\n' + profile.read_text()
                )
            if kind == "codex" and billing == "subscription":
                target = https
                current.update(
                    CODEX_CA_CERTIFICATE=str(root / "title-ca.pem"),
                    SSL_CERT_FILE=str(root / "title-ca.pem"),
                )
            if kind == "claude" and billing == "subscription":
                target = native
                current["NODE_EXTRA_CA_CERTS"] = str(cert)
                for k in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
                    current[k] = current[k.lower()] = (
                        f"http://127.0.0.1:{native.server_port}"
                    )
            before = len(target.seen)
            subprocess.run(
                [
                    test,
                    "--exact",
                    "titles::inference::tests::isolated_stock_title",
                    "--ignored",
                    "--nocapture",
                ],
                cwd=root / "Alpha",
                env=current,
                check=True,
                timeout=38,
            )
            records = target.seen[before:]
            assert len(records) == 1, (kind, billing, [(r[0], r[1]) for r in records])
            actual, wire_model, tools, auth, system, _ = records[0]
            assert (
                actual == kind
                and wire_model == model.split("/")[-1]
                and not tools
                and system
            ), (kind, billing, wire_model, tools, system)
            expected = (
                (access if kind == "codex" else token)
                if billing == "subscription"
                else server.expected_auth
            )
            assert auth in (expected, "Bearer " + expected), (
                kind,
                billing,
                "wrong account",
            )
            assert not (root / "unwanted-mcp").exists()
            print("PASS", kind, billing, flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("test", "ox", "codex", "claude"):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    run(
        *(p.resolve(strict=True) for p in (args.test, args.ox, args.codex, args.claude))
    )
