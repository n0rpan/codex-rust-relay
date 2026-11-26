"""Two independent devices, actual stock Claude, local vendor, and SIGKILL recovery."""
from contextlib import ExitStack
import uuid
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from claude_stock_session import setup, native_endpoint
from host_fixture import session_host


class Client:
    def __init__(self, path):
        self.conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.conn.settimeout(25)
        self.conn.connect(path)
        self.input = self.conn.makefile("rb")
        self.send(type="hello", version=1)
        self.until(lambda f: f["type"] == "hello")

    def close(self):
        self.input.close()
        self.conn.close()

    def send(self, **message):
        self.conn.sendall(json.dumps(message).encode() + b"\n")

    def until(self, condition):
        frames = []
        while True:
            line = self.input.readline()
            if not line:
                raise RuntimeError("Host closed the socket")
            frame = json.loads(line)
            frames.append(frame["type"])
            result = condition(frame)
            if result:
                return frame if result is True else result
            if frame["type"] == "error":
                raise AssertionError(frame)

    def open(self, scope, cursor=None):
        self.send(type="subscribe", scope=scope, after=cursor)

    def act(self, scope, key, action, token=None):
        self.send(type="act", id=key, scope=scope, action=action, approval=token)


def change(frame, kind):
    return next((d for d in frame.get("changes", []) if d["kind"] == kind), None)


def ask(frame):
    update = change(frame, "asks")
    return update and next(iter(update["asks"]), None)


def worker_cases(root, stack, binary, env, log):
    native, cert = native_endpoint(root,stack)
    native.worker_case = True
    env = dict(env,NODE_EXTRA_CA_CERTS=str(cert),ENABLE_TOOL_SEARCH="false")
    for key in ("HTTP_PROXY","HTTPS_PROXY","ALL_PROXY"):
        env[key] = env[key.lower()] = f"http://127.0.0.1:{native.server_port}"
    cfg = root / "native"
    (cfg / ".credentials.json").write_text(json.dumps({"claudeAiOauth":{
        "accessToken":native.expected_auth.removeprefix("Bearer "), "refreshToken":"fixture-refresh",
        "expiresAt":int(time.time()*1000)+3600000,"scopes":["user:inference","user:profile"],
        "subscriptionType":"max","rateLimitTier":"default_claude_max_5x"}}))
    (cfg / ".credentials.json").chmod(0o600)
    (cfg / ".claude.json").write_text(json.dumps({"oauthAccount":{
        "accountUuid":str(uuid.uuid4()),"organizationUuid":str(uuid.uuid4()),"emailAddress":"desktop@example.invalid"}}))
    scope={"project":str(root/"Beta"),"route":{"runtime":"claude","billing":"subscription"}}
    with session_host(binary,root,env,log):
        client=Client(env["OX_HOST_SOCKET"])
        client.open(scope)
        state=client.until(lambda f:f["type"]=="snapshot")["scopes"][0]
        if "start" not in state["capabilities"]:
            client.until(lambda f:(c:=change(f,"state")) and "start" in c["capabilities"])
        client.act(scope,"worker-denied",{"send_text":[None,"HOST-WORKER write"]})
        thread=client.until(lambda f:f["type"]=="opened")["thread"]
        first_frames=[]
        def approve(frame):
            if pending:=ask(frame):
                client.act(scope,uuid.uuid4().hex,{"answer_ask":[pending["id"],0]},pending["token"])
            first_frames.extend(c for c in frame.get("changes",[]) if c["kind"]=="sessions")
            return (n:=change(frame,"notification")) and n["event"]=="finished"
        client.until(approve)
        # The parent runs in Claude Default, which is the read level: a write worker is above it.
        assert any("Requested mode write is above the source's mode read" in json.dumps(request["messages"])
                   for request in native.requests), [
            request["messages"][-1] for request in native.requests]
        assert all(not any(row["parent"]==thread for row in d["sessions"]) for d in first_frames)
        client.act(scope,"worker-ok",{"send_text":[thread,"HOST-WORKER read"]})
        # The parent's MCP call returns once the host has created the worker session.
        def launched(frame):
            if pending:=ask(frame):
                client.act(scope,uuid.uuid4().hex,{"answer_ask":[pending["id"],0]},pending["token"])
            return (n:=change(frame,"notification")) and n["event"]=="finished"
        client.until(launched)
        def launch_results():
            for request in native.requests:
                for message in request["messages"]:
                    for block in message["content"] if isinstance(message["content"], list) else []:
                        if block.get("type")=="tool_result" and isinstance(block.get("content"),str):
                            try:
                                yield json.loads(block["content"])
                            except ValueError:
                                pass
        assert any(r.get("owner")=="host" and r.get("job_id") and r.get("access_ceiling")=="read"
                   for r in launch_results()), [request["messages"][-1] for request in native.requests]
        # A host-owned worker is an ordinary session on its own route, stamped with
        # its origin and nested under the parent across scopes in the catalog.
        catalog=Client(env["OX_HOST_SOCKET"])
        catalog.send(type="watch_catalog")
        def stamped(frame):
            for state in frame.get("scopes",[]):
                for row in state["sessions"]:
                    if row["parent"]==thread and row["detail"].get("origin"):
                        return (state["scope"],row)
        worker_scope,worker=catalog.until(lambda f:f["type"]=="snapshot" and stamped(f))
        catalog.close()
        origin=worker["detail"]["origin"]
        assert origin["relation"]=="worker" and origin["runtime"]=="claude", origin
        assert origin["ceiling"]=="read" and origin["requested"]=="read", origin
        # Read on Claude Code is Plan; asking for read from a read parent is inheriting it.
        assert origin.get("started")=="plan" and origin.get("decided")=="inherited", origin
        assert origin["parent"]["session"]==thread and origin["parent"]["scope"]==scope, origin
        assert worker_scope["route"]=={"runtime":"claude","billing":{"api":"fixture/demo"}}, worker_scope
        operator=Client(env["OX_HOST_SOCKET"])
        operator.open(worker_scope)
        operator.until(lambda f:f["type"]=="snapshot")
        # The operator cannot raise a worker above its recorded read ceiling.
        operator.act(worker_scope,"child-mode",{"set_access":[worker["id"],"bypass_permissions"]})
        operator.until(lambda f:f["type"]=="error" and f["id"]=="child-mode" and "ceiling" in f["message"])
        def finished(frame):
            if pending:=ask(frame):
                operator.act(worker_scope,uuid.uuid4().hex,{"answer_ask":[pending["id"],0]},pending["token"])
            for state in frame.get("scopes",[]):
                if any(r["id"]==worker["id"] and r["status"]=="idle" and r["detail"].get("origin") for r in state["sessions"]):
                    return True
            for c in frame.get("changes",[]):
                for row in c.get("sessions",[]) + ([c["session"]] if "session" in c else []):
                    if row["id"]==worker["id"] and row["status"]=="idle":
                        return True
            return False
        operator.open(worker_scope)
        operator.until(finished)
        operator.close()
        # A normal agent call inherits the operator's active bypass mode. Both
        # runtime processes are stock Claude Code using the fixture subscription.
        checkout = root / "Beta" / "isolated-review"
        checkout.mkdir()
        def spawn_writer(body, prompt):
            if prompt == "Start the isolated writer":
                return [{"type":"tool_use", "id":"toolu_inherit_workspace",
                         "name":"mcp__ox_workers__start_ox_worker",
                         "input":{"runtime":"claude", "mode":"session", "checkout":str(checkout),
                                  "billing":"subscription", "subscription_model":"claude-fable-5-2",
                                  "task":"APPROVAL write the fixture marker"}}]
        native.script = spawn_writer
        client.act(scope, "parent-bypass", {"set_access":[thread, "bypass_permissions"]})
        def applied(frame):
            return any(d["kind"] == "session" and d["session"]["id"] == thread
                       and d["session"]["detail"].get("access") == "bypass_permissions"
                       for d in frame.get("changes", []))
        client.until(applied)
        client.act(scope, "inherit-workspace", {"send_text":[thread, "Start the isolated writer"]})
        def parent_finished(frame):
            assert not ask(frame), "parent or worker requested human approval for inherited access"
            return (notice := change(frame, "notification")) and notice["event"] == "finished"
        client.until(parent_finished)
        assert any(r.get("started_mode") == "bypass_permissions" for r in launch_results()), list(launch_results())
        deadline = time.monotonic() + 20
        while not (checkout / "approved.txt").exists() and time.monotonic() < deadline:
            time.sleep(.05)
        assert (checkout / "approved.txt").read_text().strip() == "accepted"
        assert not (root / "Beta" / "approved.txt").exists()
        expected = "claude-fable-5-2"
        assert any(r["model"] == expected and "APPROVAL write the fixture marker" in str(r["messages"])
                   for r in native.requests), "The mock provider did not receive the pinned model"
        catalog = Client(env["OX_HOST_SOCKET"])
        catalog.send(type="watch_catalog")
        states = catalog.until(lambda f: f["type"] == "snapshot")["scopes"]
        catalog.close()
        child, = [row for s in states if s["scope"]["project"] == str(checkout)
                  for row in s["sessions"] if row["parent"] == thread]
        report = child["detail"]["worker_model"]
        assert report["actual_model"] == report["pinned_model"] == report["requested_model"] == expected, report
        assert report["billing"] == "subscription" and report["model_state"] == "confirmed", report
        client.close()
    print("Stock Claude worker: launch above the parent's mode refused, host-owned read worker started in Plan and stamped with its parent, operator escalation above the ceiling refused.")


def live_descendants(pid):
    # Check only descendants of our known fixture process and ignore reaped/zombie entries.
    text=subprocess.check_output(["ps","-axo","pid=,ppid=,stat="],text=True)
    records=[line.split()[:3] for line in text.splitlines()]
    tree={pid}
    while True:
        children={int(child) for child,parent,state in records if int(parent) in tree and not state.startswith("Z")}
        new=children-tree
        if not new:
            return tree-{pid}
        tree.update(new)


def verify_stopped(pids):
    until=time.monotonic()+5
    while pids:
        states=subprocess.run(["ps","-p",",".join(map(str,pids)),"-o","pid=,stat="],capture_output=True,text=True).stdout
        pids={int(line.split()[0]) for line in states.splitlines() if not line.split()[1].startswith("Z")}
        if time.monotonic()>=until and pids:
            raise AssertionError(f"Stock runtime survived fixture host death: {pids}")
        if pids:
            time.sleep(.02)


def run(root, host, ox, claude):
    with ExitStack() as stack:
        _, env = setup(root, claude, stack)
        # The private package mirrors release helper discovery without altering the caller's binaries.
        host_path = root / "bin" / "ox-host"
        for source, target in ((host, host_path), (ox, root / "bin" / "ox")):
            try:
                os.link(source, target)
            except OSError:
                shutil.copy2(source, target)
        project = root / "Alpha"
        scope = {"project": str(project), "route": {"runtime": "claude", "billing":{"api":"fixture/demo"}}}
        env["XDG_STATE_HOME"] = str(root / "state")
        log = stack.enter_context((root / "host.log").open("w+"))
        with session_host(host_path, root, env, log) as owner:
            desktop = Client(env["OX_HOST_SOCKET"])
            desktop.open(scope)
            snap = desktop.until(lambda f: f["type"] == "snapshot")
            if "start" not in snap["scopes"][0]["capabilities"]:
                desktop.until(lambda f: (c := change(f, "state")) and "start" in c["capabilities"])
            phone = Client(env["OX_HOST_SOCKET"])
            phone.open(None)
            phone.until(lambda f: f["type"] == "snapshot")
            desktop.act(scope, "create", {"send_text": [None,"APPROVAL Alpha"]})
            opened = desktop.until(lambda f: f["type"] == "opened" and f["id"] == "create")
            thread = opened["thread"]
            first = phone.until(ask)
            assert first["thread"] == thread and first["token"]
            # Disconnect the creator while a stock approval is outstanding.
            desktop.close()
            rival = Client(env["OX_HOST_SOCKET"])
            rival.open(None)
            snap = rival.until(lambda f: f["type"] == "snapshot")
            assert snap["scopes"][0]["asks"][0]["token"] == first["token"]
            # A cursor from a different subscription on the same connection requires its own baseline.
            rival.open(scope, snap["cursor"])
            baseline = rival.until(lambda f: True)
            assert baseline["type"] == "snapshot" and [s["scope"] for s in baseline["scopes"]] == [scope], baseline
            # The creator disconnected. An exact retry recovers the same thread identity.
            rival.act(scope,"create",{"send_text":[None,"APPROVAL Alpha"]})
            assert rival.until(lambda f:f["type"]=="opened")["thread"]==thread
            rival.until(lambda f:f["type"]=="ack" and f["id"]=="create")
            outsider=Client(env["OX_HOST_SOCKET"])
            outsider.act(scope,"create",{"send_text":[None,"APPROVAL Alpha"]})
            outsider.until(lambda f:f["type"]=="error" and f["id"]=="create" and "Subscribe" in f["message"])
            outsider.close()
            cursor = snap["cursor"]
            action = {"answer_ask":[first["id"],0]}
            phone.act(scope,"decision-a",action,first["token"])
            rival.act(scope,"decision-b",action,first["token"])
            replies = [c.until(lambda f: f.get("id") == k and f["type"] in ("ack","error"))
                       for c,k in ((phone,"decision-a"),(rival,"decision-b"))]
            assert sorted(f["type"] for f in replies) == ["ack","error"], replies
            winner_index=next(i for i,f in enumerate(replies) if f["type"]=="ack")
            winner,winner_id=[(phone,"decision-a"),(rival,"decision-b")][winner_index]
            winner.act(scope,winner_id,action,first["token"])
            winner.until(lambda f:f["type"]=="ack" and f["id"]==winner_id)
            assert first["title"]
            replay = Client(env["OX_HOST_SOCKET"])
            replay.open(scope,cursor)
            resolved = replay.until(lambda f:(c:=change(f,"asks")) and not c["asks"])
            assert resolved["cursor"]["seq"] > cursor["seq"]
            replay.until(lambda f: (n:=change(f,"notification")) and n["event"]=="finished")
            assert (project / "approved.txt").read_text().strip()=="accepted"
            (project / "approved.txt").unlink()
            replay.act(scope,"next",{"send_text":[thread,"APPROVAL interrupted"]})
            old = replay.until(ask)
            # The durable cursor and retired approvals survive abrupt host death.
            children=live_descendants(owner.pid)
            assert children, "No stock children observed under the host"
            owner.kill()
            owner.wait(timeout=5)
            verify_stopped(children)
            for c in (replay,rival,phone):
                c.close()
        with session_host(host_path,root,env,log):
            restored=Client(env["OX_HOST_SOCKET"])
            restored.open(None,cursor)
            snap=restored.until(lambda f:f["type"]=="snapshot")
            assert snap["cursor"]["epoch"]!=cursor["epoch"]
            assert snap["cursor"]["seq"]>cursor["seq"]
            view=snap["scopes"][0]
            assert not view["asks"] and view["detail"]["retired_approvals"]==1
            assert next(r for r in view["sessions"] if r["id"]==thread)["status"]=="interrupted"
            restored.open(scope)
            restored.until(lambda f:f["type"]=="snapshot")
            restored.act(scope,"stale",{"answer_ask":[old["id"],0]},old["token"])
            restored.until(lambda f:f["type"]=="error" and f["id"]=="stale" and "earlier runtime" in f["message"])
            restored.close()
            assert not (project/"approved.txt").exists()
        worker_cases(root,stack,host_path,env,log)
    print("Stock Claude: desktop disconnect, two-device approval race, cursor replay, SIGKILL recovery, stale approval refusal passed.")


if __name__=="__main__":
    # The protocol identifies projects by realpath (on macOS /tmp is /private/tmp).
    # A short private root also fits Darwin's Unix-socket path limit.
    with tempfile.TemporaryDirectory(prefix="ox-host-stock-", dir="/tmp") as directory:
        run(Path(directory).resolve(),*(Path(p).resolve(strict=True) for p in sys.argv[1:]))
