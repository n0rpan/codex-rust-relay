const enc = new TextEncoder();
const b64 = (bytes) =>
  btoa(String.fromCharCode(...new Uint8Array(bytes)))
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replaceAll("=", "");
const unb64 = (str) =>
  Uint8Array.from(atob(str.replaceAll("-", "+").replaceAll("_", "/")), (v) =>
    v.charCodeAt(0),
  );
async function db() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open("ox-device", 1);
    req.onupgradeneeded = () => req.result.createObjectStore("keys");
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}
export async function deviceKey(value) {
  const store = await db();
  return new Promise((resolve, reject) => {
    const tx = store.transaction("keys", value ? "readwrite" : "readonly"),
      table = tx.objectStore("keys");
    const req = value ? table.put(value, "device") : table.get("device");
    tx.oncomplete = () => {
      store.close();
      resolve(req.result);
    };
    tx.onabort = () => {
      store.close();
      reject(tx.error);
    };
  });
}
async function post(path, body, timeout = 10000) {
  const reply = await fetch("/api/" + path, {
    method: "POST",
    signal: AbortSignal.timeout(timeout),
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!reply.ok) {
    const err = new Error(
      reply.status === 403
        ? "Device pairing is required."
        : "The host could not complete this request.",
    );
    err.status = reply.status;
    throw err;
  }
  return reply.status === 204 ? null : reply.json();
}
const sign = async (key, nonce) =>
  b64(
    await crypto.subtle.sign(
      { name: "ECDSA", hash: "SHA-256" },
      key,
      enc.encode(nonce),
    ),
  );
export async function pair(code, label) {
  const keys = await crypto.subtle.generateKey(
    { name: "ECDSA", namedCurve: "P-256" },
    false,
    ["sign", "verify"],
  );
  const key = b64(await crypto.subtle.exportKey("raw", keys.publicKey));
  await post("pair", {
    code,
    key,
    signature: await sign(keys.privateKey, code),
    label,
  });
  await deviceKey(keys.privateKey);
}
export async function proof(key, purpose) {
  const { nonce, vapid } = await post("challenge", { purpose });
  return { proof: { nonce, signature: await sign(key, nonce) }, vapid };
}
export async function settingsRequest(key, operation) {
  const request = JSON.stringify(operation);
  const proof = await signedOperation(key, "settings", request);
  const transfer = [
    "pair_key_destination",
    "review_key_transfer",
    "send_key_transfer",
  ].includes(operation.action);
  const reply = await post(
    "settings",
    { request, proof },
    transfer ? 60000 : 10000,
  );
  if (reply?.error) throw new Error(reply.error);
  return reply;
}
export async function notifications(key, explicit) {
  const permission =
    explicit && Notification.permission === "default"
      ? await Notification.requestPermission()
      : Notification.permission;
  if (permission !== "granted")
    throw new Error("Notifications are disabled for this device.");
  const reg = await Promise.race([
    navigator.serviceWorker.ready,
    new Promise((_, reject) =>
      setTimeout(
        () =>
          reject(new Error("Reload OX to finish setting up notifications.")),
        12000,
      ),
    ),
  ]);
  let subscription = await reg.pushManager.getSubscription();
  if (!explicit && !subscription) return false;
  const { vapid } = await proof(key, "push");
  if (
    subscription &&
    b64(subscription.options.applicationServerKey) !== vapid
  ) {
    if (!explicit) return false;
    await subscription.unsubscribe();
    subscription = null;
  }
  subscription ??= await reg.pushManager.subscribe({
    userVisibleOnly: true,
    applicationServerKey: unb64(vapid),
  });
  const json = subscription.toJSON();
  // Browser permission prompts may outlive a short-lived challenge. Sign a fresh one.
  const signed = await proof(key, "push");
  await post("push", {
    proof: signed.proof,
    subscription: { endpoint: json.endpoint, keys: json.keys },
  });
  return true;
}

export async function signedOperation(key, purpose, request) {
  const { nonce } = await post("challenge", { purpose });
  const digest = b64(
    new Uint8Array(await crypto.subtle.digest("SHA-256", enc.encode(request))),
  );
  return { nonce, signature: await sign(key, nonce + "." + digest) };
}
