"""Open a notification in an existing paired window through the service worker message path."""

import base64
import hashlib
import json


def check_notification_links(js, wait, scope):
    thread = "disk-report"
    identity = [scope["project"], scope["route"]["runtime"], None, thread]
    digest = hashlib.sha256(json.dumps(identity, separators=(",", ":"), ensure_ascii=False).encode()).digest()
    ref = thread + "~" + base64.urlsafe_b64encode(digest).decode().rstrip("=")
    tap = "navigator.serviceWorker.dispatchEvent(new MessageEvent('message', {data:{openThread:arguments[0]}}));"
    # A notification that launches a new window arrives in its URL fragment.
    js("location.hash = new URLSearchParams({thread: arguments[0]}); location.reload()", ref)
    wait(lambda: js("return !location.hash && !document.getElementById('conversation')?.hidden && document.getElementById('title')?.innerText==='Rust build disk usage'"))
    js(tap, ref)
    wait(lambda: js("return !document.getElementById('conversation').hidden && document.getElementById('title').innerText==='Rust build disk usage'"))
    js(tap, thread + "~" + "A" * 43)
    wait(lambda: js("return !document.getElementById('catalog').hidden && document.getElementById('message').innerText.includes('no longer in the list')"))
    js(tap + "document.getElementById('home').click();", ref)
    # A newer tap must still be independently actionable after a cancellation.
    js(tap, ref)
    wait(lambda: js("return !document.getElementById('conversation').hidden && document.getElementById('title').innerText==='Rust build disk usage'"))
    js("document.getElementById('home').click()")
    print("PASS scoped notification tap and stale notification feedback in a paired browser", flush=True)
