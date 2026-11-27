"""Exercise the worker's real filesystem boundary through a stock app-server."""
import json
import os
import queue
import signal
import subprocess
import threading
import time


def run_worker(ox, launch, project, env):
    process = subprocess.Popen(
        [ox, "codex", "--chatgpt", "-c", 'forced_login_method="chatgpt"',
         "--no-daemon", "app-server", "--listen", "stdio://"],
        cwd=launch, env=env, start_new_session=True,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
    )
    inbox = queue.Queue()

    def read():
        try:
            for line in process.stdout:
                inbox.put(json.loads(line))
        finally:
            inbox.put(None)

    def send(frame):
        print(json.dumps(frame), file=process.stdin, flush=True)

    reader = threading.Thread(target=read, daemon=True)
    reader.start()
    try:
        send({"id": 1, "method": "initialize", "params": {
            "clientInfo": {"name": "ox_desktop", "title": "OX Desktop", "version": "0.1"},
            "capabilities": {"experimentalApi": True},
        }})
        approved = False
        deadline = time.monotonic() + 35
        while time.monotonic() < deadline:
            for directory in {launch, project}:
                marker = directory / "worker-marker"
                if marker.exists() and marker.read_text() == directory.name:
                    assert approved, "worker ran before its explicit approval"
                    return directory
            try:
                frame = inbox.get(timeout=0.05)
            except queue.Empty:
                continue
            assert frame is not None, "Codex closed before the worker wrote its marker"
            method = frame.get("method")
            if method and "id" in frame:
                assert method == "mcpServer/elicitation/request", method
                assert "start_ox_worker" in str(frame["params"])
                send({"id": frame["id"], "result": {"action": "accept", "content": {}}})
                approved = True
            elif "error" in frame and "id" in frame:
                raise AssertionError(frame["error"]["message"])
            elif frame.get("id") == 1:
                send({"method": "initialized"})
                send({"id": 2, "method": "thread/start", "params": {
                    "cwd": str(project), "approvalPolicy": "on-request",
                }})
            elif frame.get("id") == 2:
                thread = frame["result"]["thread"]
                assert thread["cwd"] == str(project), thread["cwd"]
                send({"id": 3, "method": "turn/start", "params": {
                    "threadId": thread["id"], "input": [{"type": "text", "text": "Start the fixture worker."}],
                }})
        raise AssertionError(f"No worker marker; launch={launch.name}, thread={project.name}, approved={approved}")
    finally:
        # OX's signal handler shuts its jobs down before exiting.
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        process.stdin.close()
        reader.join(timeout=1)


def verify_worker_directories(ox, launch, env):
    project = launch.parent / "second-project"
    project.mkdir()
    for directory in (launch, project):
        (directory / "fixture-input").write_text(directory.name)
    shared_destination = run_worker(ox, launch, project, env)
    print(f"Shared launch in {launch.name}, thread in {project.name}, worker wrote in {shared_destination.name}", flush=True)
    (shared_destination / "worker-marker").unlink()
    assert run_worker(ox, project, project, env) == project
    assert not (launch / "worker-marker").exists()


def verify_hosted_inheritance(project, env):
    """The subscription parent delegates a shell write to an API worker, with zero answers."""
    from contextlib import closing
    from host_codex_interrupt import ready
    from host_stock_session import Client, ask, change

    checkout = project.with_name("worker-checkout")
    checkout.mkdir()
    (checkout / "fixture-input").write_text("inherited")
    scope = {"project": str(project), "route": {"runtime": "codex", "billing": "subscription"}}
    with closing(Client(env["OX_HOST_SOCKET"])) as parent, closing(Client(env["OX_HOST_SOCKET"])) as catalog:
        ready(parent, scope)
        parent.act(scope, "mode", {"set_next_access": "yolo"})
        parent.until(lambda f: f["type"] == "ack" and f["id"] == "mode")
        parent.act(scope, "delegate", {"send_text": [None, "Start the isolated writer"]})
        thread = parent.until(lambda f: f["type"] == "opened")["thread"]

        def finished(frame):
            assert not ask(frame), "Unexpected operator approval on the YOLO parent"
            return (n := change(frame, "notification")) and n["event"] == "finished"

        parent.until(finished)
        deadline = time.monotonic() + 20
        marker = checkout / "worker-marker"
        # The worker's `cat >` creates the marker before it writes the content,
        # so wait for the bytes instead of just the file.
        while time.monotonic() < deadline:
            try:
                if marker.read_text() == "inherited":
                    break
            except FileNotFoundError:
                pass
            time.sleep(.05)
        assert marker.read_text() == "inherited"
        assert not (project / marker.name).exists()
        catalog.send(type="watch_catalog")
        states = catalog.until(lambda f: f["type"] == "snapshot")["scopes"]
        children = [(s["scope"], r) for s in states for r in s["sessions"] if r["parent"] == thread]
        assert len(children) == 1, children
        child_scope, child = children[0]
        assert child_scope == {"project": str(checkout), "route": {"runtime":"codex", "billing":{"api":"fixture/demo"}}}
        origin = child["detail"]["origin"]
        assert origin["parent"]["scope"] == scope and origin["parent"]["session"] == thread
        assert origin["started"] == origin["ceiling"] == "yolo", origin


class CatalogClient:
    """Request/reply client for finite discovery experiments in a synthetic profile."""

    def __init__(self, ox, project, env, api=False):
        route = (["--model", "fixture/demo", "-c", 'model_provider="codeox"'] if api else
                 ["--chatgpt", "-c", 'forced_login_method="chatgpt"'])
        self.process = subprocess.Popen(
            [ox, "codex", *route, "--no-daemon", "app-server", "--listen", "stdio://"],
            cwd=project, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, start_new_session=True,
        )
        self.inbox = queue.Queue()
        self.events = []
        self.seq = 0

    def __enter__(self):
        def read():
            for line in self.process.stdout:
                self.inbox.put(json.loads(line))
            self.inbox.put(None)
        self.reader = threading.Thread(target=read, daemon=True)
        self.reader.start()
        try:
            self.rpc("initialize", {"clientInfo": {"name": "ox_probe", "title": "OX probe", "version": "1"},
                                    "capabilities": {"experimentalApi": True}})
            self.send({"method": "initialized"})
        except BaseException:
            self.__exit__(None, None, None)
            raise
        return self

    def send(self, frame):
        print(json.dumps(frame), file=self.process.stdin, flush=True)

    def rpc(self, method, params):
        self.seq += 1
        self.send({"id": self.seq, "method": method, "params": params})
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            frame = self.inbox.get(timeout=max(.01, deadline-time.monotonic()))
            assert frame is not None, f"Codex closed during {method}"
            if "method" not in frame and frame.get("id") == self.seq:
                if "error" in frame:
                    raise RuntimeError(f"{method}: {frame['error']['message']}")
                return frame["result"]
            self.events.append(frame)
        raise AssertionError(f"Timed out during {method}")

    def __exit__(self, *_):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait()
        self.reader.join(timeout=1)


def verify_catalog(ox, project, env):
    with CatalogClient(ox, project, env) as owner, CatalogClient(ox, project, env) as observer:
        params = {"cwd": str(project), "modelProviders": ["openai"], "sortKey": "updated_at"}
        rows = owner.rpc("thread/list", {**params, "limit": 1})["data"]
        assert len(rows) == 1
        thread = rows[0]["id"]
        owner.rpc("thread/resume", {"threadId": thread, "excludeTurns": True})
        owner.rpc("thread/name/set", {"threadId": thread, "name": "Fable catalog fixture"})
        for state_only in [True, False]:
            start = time.monotonic()
            matches = observer.rpc("thread/list", {**params, "limit": 5000, "useStateDbOnly": state_only,
                                                   "searchTerm": "catalog fixt"})["data"]
            elapsed = (time.monotonic()-start)*1000
            print(f"Stock listing limit=5000 stateDbOnly={state_only} substring match={any(t['id']==thread for t in matches)} ms={elapsed:.2f}", flush=True)
            assert len(matches) == 1 and matches[0]["id"] == thread
        owner.rpc("turn/start", {"threadId": thread, "input": [{"type":"text","text":"Catalog fixture follow-up"}]})
        deadline = time.monotonic()+10
        while time.monotonic() < deadline:
            frame = owner.inbox.get(timeout=max(.01, deadline-time.monotonic()))
            if frame and frame.get("method") == "turn/completed":
                break
        else:
            raise AssertionError("Expected a completed fixture turn")
        # Fence the observation with an RPC on the observer's own connection.
        observer.rpc("thread/list", {**params, "limit": 1})
        seen = sorted({event["method"] for event in observer.events
                       if event.get("params", {}).get("threadId") == thread})
        print(f"Unsubscribed observer received {seen} for another connection's thread", flush=True)
        try:
            login = observer.rpc("account/login/start", {"type": "chatgpt"})
            assert login["loginId"] and login["authUrl"]
            observer.rpc("account/login/cancel", {"loginId": login["loginId"]})
            print("Native browser login initiated and cancelled through OX", flush=True)
        except RuntimeError as error:
            print(f"Stock login initiation in the synthetic profile: {error}", flush=True)


def verify_route_catalogs(ox, project, env):
    """Both secured homes need a real stock reader, including before a route is visited."""
    params = {"cwd": str(project), "modelProviders": [], "limit": 50}
    with CatalogClient(ox, project, env) as native, CatalogClient(ox, project, env, api=True) as relay:
        native_rows = native.rpc("thread/list", params)["data"]
        assert native_rows and all(row["modelProvider"] == "openai" for row in native_rows)
        opened = relay.rpc("thread/start", {"cwd": str(project), "model":"fixture/demo", "approvalPolicy": "on-request"})
        assert opened["modelProvider"] == "codeox" and opened["model"] == "fixture/demo", opened
        tid = opened["thread"]["id"]
        relay.rpc("turn/start", {"threadId": tid, "input": [{"type":"text","text":"Persist API route catalog fixture"}]})
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            frame = relay.inbox.get(timeout=max(.01,deadline-time.monotonic()))
            if frame.get("method") == "item/commandExecution/requestApproval":
                relay.send({"id":frame["id"], "result":{"decision":"decline"}})
            if frame.get("method") == "turn/completed":
                break
        else:
            raise AssertionError("API fixture turn did not complete")
        assert tid not in {row["id"] for row in native.rpc("thread/list",params)["data"]}
    with CatalogClient(ox, project, env, api=True) as cold:
        rows = cold.rpc("thread/list",params)["data"]
        row = next(r for r in rows if r["id"] == tid)
        assert row["modelProvider"] == "codeox" and row["cwd"] == str(project)
        assert not {r["id"] for r in native_rows}.intersection(r["id"] for r in rows)
        opened = cold.rpc("thread/resume",{"threadId":tid,"excludeTurns":True,"cwd":str(project)})
        assert opened["modelProvider"] == "codeox" and opened["model"] == "fixture/demo"
        print(f"Cold route catalogs: native={len(native_rows)}, relay={len(rows)}, listed alias={row.get('model')!r}, verified relay resume", flush=True)
