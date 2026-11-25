"""Own the real headless host for the lifetime of an isolated native test."""
from contextlib import contextmanager
from pathlib import Path
import os
import socket
import subprocess
import sys
import tempfile
import time


def descendants(pid):
    """Live (non-zombie) processes below pid, from one process table snapshot."""
    text = subprocess.check_output(["ps", "-axo", "pid=,ppid=,stat="], text=True)
    rows = [line.split()[:3] for line in text.splitlines() if len(line.split()) >= 3]
    tree = {pid}
    while True:
        new = {int(c) for c, p, s in rows if int(p) in tree and not s.startswith("Z")} - tree
        if not new:
            return tree - {pid}
        tree |= new


def wait_gone(pids, seconds=10):
    """Wait for processes to exit (zombies count as gone); return any that remain."""
    until = time.monotonic() + seconds
    while pids:
        out = subprocess.run(["ps", "-p", ",".join(map(str, pids)), "-o", "pid=,stat="],
                             capture_output=True, text=True).stdout
        pids = {int(l.split()[0]) for l in out.splitlines() if l.split() and not l.split()[1].startswith("Z")}
        if not pids or time.monotonic() >= until:
            return pids
        time.sleep(0.05)
    return pids


@contextmanager
def session_host(binary, root, env, log=subprocess.DEVNULL):
    """Launch a fresh host, expose only its private socket, and reap exactly that process."""
    state = Path(env.get("XDG_STATE_HOME", root)) / "ox" / "host"
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="ox-host-sock-", dir="/tmp") as runtime:
        endpoint = str(Path(runtime) / "host.sock")
        env.update(OX_HOST_SOCKET=endpoint, OX_HOST_STATE_DIR=str(state))
        proc = subprocess.Popen([binary, "run", "--socket", endpoint, "--state-dir", state],
                                env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                start_new_session=True)
        try:
            until = time.monotonic() + 12
            while True:
                if proc.poll() is not None:
                    raise RuntimeError(f"Session host exited before binding ({proc.returncode})")
                try:
                    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as check:
                        check.connect(endpoint)
                    break
                except (FileNotFoundError, ConnectionRefusedError):
                    if time.monotonic() >= until:
                        raise TimeoutError("Session host did not bind its private Unix socket")
                    time.sleep(.005)
            yield proc
        finally:
            # The host's stock children exit, and Claude Code writes its session
            # files, only after the host stops. Wait for them before the caller
            # removes their directories; a child that outlives that is a leak.
            children = descendants(proc.pid) if proc.poll() is None else set()
            if proc.poll() is None:
                proc.terminate()
            try:
                proc.wait(timeout=8)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=3)
                raise RuntimeError("Session host failed to shut down on SIGTERM")
            # Never hide the test's own failure behind this one.
            if (survivors := wait_gone(children)) and sys.exc_info()[0] is None:
                raise RuntimeError(f"Stock processes outlived the session host: {sorted(survivors)}")


if __name__ == "__main__" and "__file__" in globals():
    import sys
    environment = os.environ.copy()
    with tempfile.TemporaryDirectory(prefix="ox-host-command-", dir="/tmp") as tmp:
        with session_host(Path(sys.argv[1]), Path(tmp), environment):
            raise SystemExit(subprocess.run(sys.argv[2:], env=environment).returncode)
