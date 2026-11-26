"""Pair a second service and transfer a single reviewed key using the web controls."""
from pathlib import Path
import json
import os
import shutil
import ssl


def receiver(d, stack):
    from native_client import certificate, session_host
    from browser import Forward, ThreadedTCP, admin, managed_server, wait
    root = d / "receiver"
    root.mkdir()
    home = root / "home"
    home.mkdir()
    package = root / "bin"
    package.mkdir()
    for var, name in [("OX_TEST_HOST_BIN", "ox-host"), ("OX_TEST_SETTINGS_OX", "ox")]:
        shutil.copy2(os.environ[var], package / name)
    env = dict(HOME=str(home), CODEX_HOME=str(home / ".codex"), XDG_STATE_HOME=str(home / "state"),
               PATH=str(package), OX_NO_BACKGROUND_UPDATE="1", SHELL="/bin/sh")
    stack.enter_context(session_host(package / "ox-host", root, env))
    state = Path(env["OX_HOST_STATE_DIR"])
    wait(lambda: (state / "remote-admin.sock").exists())
    public = ThreadedTCP(("127.0.0.1", 0), Forward)
    port = public.server_address[1]
    name = "build.fixture.ts.net"
    origin = f"https://{name}:{port}"
    public.backend = admin(state, {"type": "enable", "port": 0, "origin": origin})["port"]
    invite = admin(state, {"type": "pair", "login": "phone@example.test"})["pairing"]["url"]
    certificate(root, name)
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.num_tickets = 0
    tls.load_cert_chain(root / "server.pem", root / "server.key")
    public.socket = tls.wrap_socket(public.socket, server_side=True)
    managed_server(stack, public)
    # The source checks the peer owner against its local Tailscale status.
    status = {"BackendState": "Running", "Self": {"UserID": 1}, "User": {"1": {"LoginName": "phone@example.test"}},
              "Peer": {"receiver": {"UserID": 1, "DNSName": name + "."},
                       "unrelated": {"UserID": 2, "DNSName": "other.fixture.ts.net."}}}
    ts_bin = root / "source-tools"
    ts_bin.mkdir()
    tool = ts_bin / "tailscale"
    tool.write_text("#!/bin/sh\ncat <<'STATUS'\n" + json.dumps(status) + "\nSTATUS\n")
    tool.chmod(0o700)
    return (dict(PATH=str(ts_bin) + os.pathsep + os.environ["PATH"], OX_TEST_ORIGIN=origin, OX_TEST_NATIVE_DIR=str(root),
                 OX_TEST_LOOPBACK=f"127.0.0.1:{port}"), invite, home / ".codex/ox-api-keys.json", state, d)


def exercise(js, wait, invite, saved, receiver_state, source):
    from browser import admin
    def go(label):
        wait(lambda: js("""const btn = [...document.querySelectorAll('#settings button')]
            .find(b => b.textContent.trim() === arguments[0] && b.getClientRects().length && !b.disabled);
            if (!btn) return false; btn.click(); return true;""", label))

    def select_brave():
        js("""for(const label of document.querySelectorAll('#settings label')) {
            const box=label.querySelector('input[type=checkbox]');
            if(box) box.checked=label.textContent.includes('Brave Search ·');
        } document.querySelector('#settings form').dispatchEvent(new Event('change',{bubbles:true}));""")

    js("""window.originalTransferFetch = window.fetch;
        window.transferExposedValue = false;
        window.observedKeySend = false;
        window.fetch = async function(resource, init) {
            const response = await originalTransferFetch.call(this, resource, init);
            if (new URL(resource, location.href).pathname === '/api/settings') {
                const operation = JSON.parse(JSON.parse(init.body).request);
                if (operation.action === 'send_key_transfer') window.observedKeySend = true;
                if (String(init?.body).includes('fixture-transfer-brave') ||
                    (await response.clone().text()).includes('fixture-transfer-brave'))
                    window.transferExposedValue = true;
            }
            return response;
        };""")
    js("document.getElementById('open-phone-settings').click()")
    go("Credentials")
    go("Copy keys to another host…")
    wait(lambda: js("return !!document.querySelector('#settings input[name=destination]')"))
    field = "document.querySelector('#settings input[name=destination]').value=arguments[0]"
    js(field, invite.replace("build.fixture.ts.net", "other.fixture.ts.net"))
    go("Pair destination")
    wait(lambda: js("return document.querySelector('#settings').innerText.includes('owned by your Tailscale login')"))
    js(field, invite)
    go("Pair destination")
    try:
        wait(lambda: js("return [...document.querySelectorAll('#settings label')].some(l=>l.textContent.includes('Brave Search ·'))"))
    except AssertionError as exc:
        raise AssertionError(js("return document.querySelector('#settings').innerText")) from exc
    assert js("return document.querySelector('#settings').innerText.includes('incur charges')")
    select_brave()
    # A key rotation between the review and send must leave the destination untouched.
    request_key = """const query=arguments[0]; return import('./device.js').then(async m=>
        m.settingsRequest(await m.deviceKey(),query))"""
    snapshot = js(request_key, {"action": "read_rows"})
    revision = snapshot["revisions"]["keys"]
    garden = next(k for k in snapshot["credentials"] if k["name"] == "GARDEN_TOKEN")
    js(request_key, {"action": "edit_key", "name": garden["name"], "scope": garden["scope"],
                     "value": "fixture-rotated", "revision": revision})
    go("Send selected keys")
    wait(lambda: js("return document.querySelector('#settings').innerText.includes('changed. Review the transfer again.')"))
    assert not saved.exists()
    go("Refresh review")
    wait(lambda: js("return !!document.querySelector('#settings input[type=checkbox]')"))
    select_brave()
    go("Send selected keys")
    wait(lambda: js("return document.querySelector('#settings').innerText.includes('Sent 1 key to ')"))
    content = saved.read_text()
    assert "fixture-transfer-brave" in content and "GARDEN_TOKEN" not in content
    assert not js("return [document.body.textContent,JSON.stringify(localStorage),JSON.stringify(sessionStorage)].some(t=>t.includes('fixture-transfer-brave'))")
    go("Copy keys to another host…")
    wait(lambda: js("return [...document.querySelectorAll('#settings button')].some(b=>b.textContent.includes('build.fixture.ts.net'))"))
    js("[...document.querySelectorAll('#settings button')].find(b=>b.textContent.includes('build.fixture.ts.net')).click()")
    wait(lambda: js("return [...document.querySelectorAll('#settings label')].some(l=>l.textContent.includes('Replace key'))"))
    assert js("return ![...document.querySelectorAll('#settings label')].find(l=>l.textContent.includes('Brave Search ·')).querySelector('input').checked")
    # The desktop directory has the same on-disk schema. Move the existing
    # working registration into it to exercise reuse and independent web repair.
    web = source / "key-destinations.json"
    desktop = source / "home/state/ox/desktop/hosts.json"
    assert not desktop.exists()
    web.rename(desktop)
    original = desktop.read_bytes()
    host = json.loads(original)[0]["id"]
    review_query = {"action": "review_key_transfer", "host": host}
    old = js(request_key, review_query)["pairing"]
    fresh = admin(receiver_state, {"type": "pair", "login": "phone@example.test"})["pairing"]["url"]
    js(request_key, {"action": "pair_key_destination", "address": fresh, "code": ""})
    assert js(request_key, review_query)["pairing"] != old
    assert desktop.read_bytes() == original
    assert len(admin(receiver_state, {"type": "status"})["devices"]) == 2
    web.unlink()
    assert js(request_key, review_query)["pairing"] == old
    assert js("window.fetch = window.originalTransferFetch; return window.observedKeySend && !window.transferExposedValue")
    print("PASS web UI sends only Brave Search, rejects stale reviews and foreign-owned peers, and preserves existing desktop pairings", flush=True)
