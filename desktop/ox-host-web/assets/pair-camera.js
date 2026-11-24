// Frames stay in this document. Only a verified same-host invitation reaches the pairing form.
export function pairingCode(text, origin) {
  let url;
  try {
    url = new URL(text);
  } catch {
    throw new Error("Use the complete pairing link from OX Desktop.");
  }
  const params = new URLSearchParams(url.hash.slice(1));
  const code = params.get("pair");
  if (
    url.origin !== origin ||
    url.pathname !== "/" ||
    url.username ||
    url.password ||
    url.search ||
    params.size !== 1 ||
    !/^[A-Za-z0-9_-]{43}$/.test(code ?? "")
  )
    throw new Error("Scan a fresh OX pairing code for this laptop.");
  return code;
}

export function setupCamera(accept) {
  const scan = document.getElementById("scan-pair");
  const panel = document.getElementById("camera-panel");
  const preview = document.getElementById("camera-preview");
  const status = document.getElementById("camera-status");
  const canvas = document.createElement("canvas");
  const context = canvas.getContext("2d", { willReadFrequently: true });
  let generation = 0,
    timer,
    stream;
  function stop() {
    generation++;
    clearTimeout(timer);
    stream?.getTracks().forEach((track) => track.stop());
    stream = null;
    preview.srcObject = null;
    panel.hidden = true;
    scan.disabled = false;
    status.textContent = "";
  }
  document.getElementById("cancel-camera").onclick = () => {
    stop();
    scan.focus();
  };
  window.addEventListener("pagehide", stop);
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) stop();
  });
  scan.onclick = async () => {
    stop();
    const current = generation;
    scan.disabled = true;
    panel.hidden = false;
    status.textContent =
      "Point the camera at the QR code in OX Desktop → Settings → Remote access.";
    try {
      if (!navigator.mediaDevices?.getUserMedia || !context)
        throw new Error(
          "This browser cannot open a camera here. Paste the pairing link below.",
        );
      const camera = await navigator.mediaDevices.getUserMedia({
        audio: false,
        video: { facingMode: { ideal: "environment" } },
      });
      if (generation !== current) {
        camera.getTracks().forEach((track) => track.stop());
        return;
      }
      stream = camera;
      preview.srcObject = camera;
      // Local decoder also works on WebKit versions without BarcodeDetector.
      await Promise.all([preview.play(), import("./vendor/jsQR-1.4.0.js")]);
      function decode() {
        if (generation !== current) return;
        if (preview.readyState >= 2 && preview.videoWidth) {
          const scale = Math.min(
            1,
            720 / Math.max(preview.videoWidth, preview.videoHeight),
          );
          canvas.width = Math.max(1, Math.round(preview.videoWidth * scale));
          canvas.height = Math.max(1, Math.round(preview.videoHeight * scale));
          context.drawImage(preview, 0, 0, canvas.width, canvas.height);
          const frame = context.getImageData(0, 0, canvas.width, canvas.height);
          const result = window.jsQR(frame.data, frame.width, frame.height);
          if (result) {
            let secret;
            try {
              secret = pairingCode(result.data, location.origin);
            } catch {
              status.textContent =
                "That code is for something else. Scan the pairing QR from this laptop.";
            }
            if (secret) {
              stop();
              accept(secret);
              return;
            }
          }
        }
        timer = setTimeout(decode, 170);
      }
      decode();
    } catch (err) {
      if (generation !== current) return;
      stop();
      status.textContent =
        err.name === "NotAllowedError"
          ? "Camera access is off. Allow camera access in your browser or device settings, then try again. You can also paste the link."
          : "Camera unavailable. Try again or paste the pairing link below.";
    }
  };
  return stop;
}
