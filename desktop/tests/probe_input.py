"""In-process input for a probe-enabled OX Desktop (OX_DESK_STATS_SOCKET set).

`press` clicks a marked control and `key` sends a GPUI keystroke to the active window,
both inside the app: no OS pointer or keyboard, so they work on Wayland without wtype.
"""
import json, os, socket, tempfile


def _send(sock, command, timeout=3):
    with tempfile.TemporaryDirectory() as d:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        try:
            s.bind(os.path.join(d, "c"))
            s.settimeout(timeout)
            s.sendto(command.encode(), str(sock))
            return json.loads(s.recv(1 << 20))
        finally:
            s.close()


def probe_press(sock, label):
    """Click the control with this id or label; raises RuntimeError with candidates if it fails."""
    reply = _send(sock, "press " + label)
    if "error" in reply:
        raise RuntimeError(reply["error"])
    return reply


def probe_key(sock, keystroke):
    """Send a keystroke such as "escape" or "ctrl-n" to the active window."""
    reply = _send(sock, "key " + keystroke)
    if "error" in reply:
        raise RuntimeError(reply["error"])
    return reply


if __name__ == "__main__":
    import sys
    sock, verb, arg = sys.argv[1:4]
    print(json.dumps((probe_press if verb == "press" else probe_key)(sock, arg)))
