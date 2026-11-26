"""Start a worker from a real parent host and reconnect after the parent exits."""

from contextlib import ExitStack, closing
import json
import os
from pathlib import Path
import shutil
import socket

from browser import admin, wait, managed_server
from native_client import session_host


def parent_restart(root, project, target, tls_env, restart_target):
    root.mkdir()
    home = root / "home"
    package = root / "bin"
    home.mkdir()
    package.mkdir()
    # session_host sets the endpoint separately for each start.
    wrapper = package / "fixture-host"
    wrapper.write_text(
        '#!/bin/sh\nexec "$OX_TEST_PARENT_HARNESS" --ignored --exact linked_parent_fixture --nocapture\n'
    )
    wrapper.chmod(0o700)
    shutil.copy2(os.environ["OX_TEST_PARENT_HARNESS"], package / "host-test")
    shutil.copy2(os.environ["OX_TEST_SETTINGS_OX"], package / "ox")
    env = {k: tls_env[k] for k in ("OX_TEST_ORIGIN", "OX_TEST_NATIVE_DIR", "OX_TEST_LOOPBACK", "PATH")}
    env.update(
        HOME=str(home),
        CODEX_HOME=str(home / ".codex"),
        OX_TEST_PARENT_HARNESS=str(package / "host-test"),
        XDG_STATE_HOME=str(home / "state"),
        XDG_CONFIG_HOME=str(home / "config"),
        SHELL="/bin/bash",
        OX_NO_BACKGROUND_UPDATE="1",
        DBUS_SESSION_BUS_ADDRESS="unix:path=" + str(root / "no-bus"),
    )
    caller = {
        "project": str(project),
        "runtime": "codex",
        "launch": "test-parent-launch",
        "session": "parent-session",
    }

    def exchange(frame):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(85)
            connection.connect(env["OX_HOST_SOCKET"])
            with connection.makefile("rwb") as stream:
                for msg in [{"type": "hello", "version": 1}, frame]:
                    stream.write(json.dumps(msg).encode() + b"\n")
                    stream.flush()
                    reply = json.loads(stream.readline())
                return reply

    def control(job, op, **fields):
        return exchange(
            {
                "type": "worker_call",
                "id": "control",
                "caller": caller,
                "call": {"op": op, "job_id": job, **fields},
            }
        )

    def launch(name=None, **placement):
        return exchange(
            {
                "type": "worker_launch",
                "id": "launch",
                "caller": caller,
                "start": {
                    "parent": "",
                    "runtime": "codex",
                    "mode": "session",
                    "model": "fixture/demo",
                    "permission": "read",
                    "host": name,
                    **placement,
                    "task": "linked-parent-offline-fixture. Read tracked.",
                },
            }
        )

    def start(stack):
        stack.enter_context(session_host(wrapper, root, env))
        state = Path(env["OX_HOST_STATE_DIR"])
        wait(lambda: (state / "remote-admin.sock").exists())
        return state

    with ExitStack() as first:
        (target / "host-profile.json").write_text(
            json.dumps({"name": "build", "labels": ["fixture-build"]})
        )
        state = start(first)
        invitation = admin(target, {"type": "worker_invitation", "login": "phone@example.test"})[
            "pairing"
        ]["url"]
        admin(
            state,
            {
                "type": "worker_links",
                "command": {"action": "add_worker_link", "address": invitation, "code": ""},
            },
        )
        (peer,) = json.loads((state / "worker-links.json").read_text())
        assert "No known worker host" in launch("never-paired")["error"]

        def inventory_refreshed():
            hosts = exchange({"type": "worker_hosts", "id": "sheet", "project": str(project)})[
                "result"
            ]["hosts"]
            return any(
                h["host"]["id"] == peer["id"]
                and h["has_project"]
                and "fixture-build" in h["host"].get("labels", [])
                for h in hosts
            )

        wait(inventory_refreshed, 25)
        refusal = launch(needs={"memory_gb": 4000000000}).get("error") or ""
        assert "4000000000 GB" in refusal, refusal
        receiver = Path(tls_env["OX_TEST_NATIVE_DIR"])
        (receiver / "hold-start").touch()
        result = launch("label:fixture-build")
        assert result.get("result") and result["result"]["status"] == "preparing", result
        job = result["result"]["job_id"]
        wait(lambda: (receiver / "start-rejected").exists(), 45)
        assert not (receiver / "request-received").exists()
        assert "linked-parent-offline-fixture" not in (state / "ownership-v1.json").read_text()
    # The queued Start was refused by the proxy before the target received it.
    (receiver / "hold-start").unlink()
    with ExitStack() as resumed:
        start(resumed)
        wait(lambda: (receiver / "request-received").exists(), 45)
        different = {**caller, "session": "stranger", "launch": "other-launch"}
        other = exchange(
            {
                "type": "worker_call",
                "id": "other",
                "caller": different,
                "call": {"op": "status", "job_id": job},
            }
        )
        assert "Unknown worker job" in other["error"], other
    # The real target finishes its native turn while the parent host is entirely stopped.
    (receiver / "release-request").touch()
    with ExitStack() as second:
        from http.server import ThreadingHTTPServer
        from stock_session import Fixture
        from host_stock_session import Client
        from host_codex_interrupt import ready

        source_vendor = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
        source_vendor.mode = "linked-parent"
        source_vendor.project = root
        managed_server(second, source_vendor)
        cfg = Path(env["CODEX_HOME"])
        cfg.mkdir(exist_ok=True)
        (cfg / "config.toml").write_text("[features]\ntool_search=false\n")
        (cfg / "codeox.toml").write_text(
            '[providers.parent]\nname="Parent"\nwire_api="chat_completions"\n'
            f'base_url="http://127.0.0.1:{source_vendor.server_port}/v1"\nenv_key="OX_PARENT_FIXTURE_KEY"\ndiscovery=false\n'
            '[[providers.parent.models]]\nid="demo"\ncontext_window=131072\n'
        )
        env.update(
            OX_PARENT_FIXTURE_KEY="fixture-only",
            SHELL="/bin/bash",
            PATH=str(package) + ":" + env["PATH"],
        )
        for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
            env[name] = env[name.lower()] = "http://127.0.0.1:1"
        env["NO_PROXY"] = env["no_proxy"] = "127.0.0.1,localhost"
        (package / "codex").symlink_to(Path(os.environ["CODEOX_TEST_CODEX_BIN"]).resolve())
        start(second)

        def finished():
            v = control(job, "status")
            if "error" in v and v["error"]:
                raise AssertionError(v)
            return (
                v["result"]
                if v["result"]["status"] == "completed" and "result" in v["result"]
                else None
            )

        final = wait(finished, 45)
        assert "uncommitted on laptop" in final["result"], final
        assert final["worker_host"]["id"] == peer["id"]
        assert final["snapshot_commit"]
        (receiver / "malformed-notices").touch()
        wait(lambda: "incompatible worker notices" in (control(job, "status").get("error") or ""), 15)
        (receiver / "malformed-notices").unlink()
        assert control(job, "status")["result"]["status"] == "completed"

        def notices_acked():
            notices = json.loads((target / "ownership-v1.json").read_text())["worker_notices"]
            return notices["observed"].get(str(job)) == "completed" and not any(
                n["job"] == job for n in notices.get("linked", [])
            )

        wait(notices_acked, 15)
        assert not list((state / "linked-work/outgoing/requests").glob("*/start.json"))
        listing = exchange(
            {"type": "worker_call", "id": "list", "caller": caller, "call": {"op": "list"}}
        )
        assert job in [x["job_id"] for x in listing["result"]["jobs"]]
        assert control(job, "cancel")["result"]["status"] == "completed"
        # A second job accepts control while the inference is in flight.
        (receiver / "request-received").unlink()
        (receiver / "release-request").unlink()
        rules = project.parent / "codeox.toml"
        rules.write_text(
            '[workers.placement]\ndefault = "parent"\nheavy = ' + json.dumps(peer["id"]) + "\n"
        )
        another = launch(placement="heavy")["result"]["job_id"]
        wait(lambda: (receiver / "request-received").exists(), 45)
        steer = control(another, "steer", text="Include the filename in your reply.")
        assert steer["result"]["accepted"], steer
        cancelled = control(another, "cancel")
        (receiver / "release-request").touch()
        assert cancelled["result"]["status"] == "cancelled", cancelled
        with closing(Client(env["OX_HOST_SOCKET"])) as parent:
            scope = {
                "project": str(project),
                "route": {"runtime": "codex", "billing": {"api": "parent/demo"}},
            }
            ready(parent, scope)
            parent.act(scope, "parent-mode", {"set_next_access": "yolo"})
            parent.until(lambda f: f["type"] == "ack" and f["id"] == "parent-mode")
            parent.act(
                scope,
                "delegate",
                {"send_text": [None, "Ask a worker on the build host to read tracked."]},
            )
            parent.until(lambda f: f["type"] == "opened")
            wait(lambda: (root / "parent-tool-result").exists(), 30)
            tool_result = (root / "parent-tool-result").read_text()
            assert "preparing" in tool_result, tool_result
            wait(lambda: (root / "parent-received-result").exists(), 45)
        (receiver / "request-received").unlink()
        (receiver / "release-request").unlink()
        stopped = launch(peer["id"])["result"]["job_id"]
        wait(lambda: (receiver / "request-received").exists(), 45)
        restart_target()
        (receiver / "release-request").touch()
        wait(lambda: control(stopped, "status")["result"]["status"] == "interrupted", 20)
        (receiver / "request-received").unlink()
        (receiver / "release-request").unlink()
        detached = launch(peer["id"])["result"]["job_id"]
        wait(lambda: (receiver / "request-received").exists(), 45)
        source_id = (state / "host-id").read_text().strip()
        registration = next(
            h
            for h in admin(target, {"type": "status"})["worker_hosts"]
            if h["worker"]["source"] == source_id
        )
        admin(target, {"type": "revoke", "id": registration["id"]})
        wait(lambda: control(detached, "status")["result"]["status"] == "detached", 20)
        assert control(detached, "cancel")["result"]["status"] == "detached"
        (receiver / "release-request").touch()
        wait(
            lambda: (
                json.loads((target / "ownership-v1.json").read_text())["worker_notices"][
                    "observed"
                ].get(str(detached))
                == "completed"
            ),
            20,
        )
        admin(
            state,
            {"type": "worker_links", "command": {"action": "remove_worker_link", "host": peer["id"]}},
        )
    print(
        "PASS named, labelled, project-rule and hardware-selected stock Codex worker, ownership, signed control, parent restart and completion delivered into a hosted parent",
        flush=True,
    )
