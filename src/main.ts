import QRCode from "qrcode";
import jsQR from "jsqr";
import { isTauri, SonicBackend, StrokePoint } from "./backend";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const txRgbCtx = $<HTMLCanvasElement>("txRgbCanvas").getContext("2d")!;
const rxRgbCtx = $<HTMLCanvasElement>("rxRgbCanvas").getContext("2d")!;
const txDrawCanvas = $<HTMLCanvasElement>("txDrawCanvas");
const txDrawCtx = txDrawCanvas.getContext("2d")!;
const rxDrawCtx = $<HTMLCanvasElement>("rxDrawCanvas").getContext("2d")!;

const PALETTE = [
  "#00f0a0",
  "#00c2ff",
  "#ff4769",
  "#ffe600",
  "#c77dff",
  "#ff8800",
  "#ffffff",
  "#05080b",
];

let micOn = false;
let lastRxPt: { x: number; y: number } | null = null;

function clearRxRgbCanvas() {
  rxRgbCtx.fillStyle = "#000";
  rxRgbCtx.fillRect(0, 0, 48, 48);
}
clearRxRgbCanvas();

const backend = new SonicBackend({
  onMicStatus: (s) => ($("micBadge").textContent = s),
  onMuteState: (muted) => $("muteBadge").classList.toggle("hidden", !muted),
  onStatus: (s) => ($("rxRgbStatus").textContent = s),
  onError: (e) => ($("rxTerminal").textContent += `\n[BLOCKED / NOISE] ${e}`),
  onText: (t) => {
    const ts = new Date().toLocaleTimeString();
    $("rxTerminal").textContent += `\n[${ts} VERIFIED]\n${t}\n`;
  },
  onDrawing: (pts) => {
    for (const pt of pts) {
      if (pt.pen_down && lastRxPt) {
        rxDrawCtx.strokeStyle = PALETTE[pt.color_idx] ?? "#00f0a0";
        rxDrawCtx.lineWidth = 3;
        rxDrawCtx.lineCap = "round";
        rxDrawCtx.beginPath();
        rxDrawCtx.moveTo(lastRxPt.x, lastRxPt.y);
        rxDrawCtx.lineTo(pt.x, pt.y);
        rxDrawCtx.stroke();
      }
      lastRxPt = pt.pen_down ? { x: pt.x, y: pt.y } : null;
    }
  },
  onRgbFrame: (rgb) => {
    if (!micOn) return;
    const imgData = rxRgbCtx.createImageData(48, 48);
    for (let i = 0, j = 0; i < rgb.length; i += 3, j += 4) {
      imgData.data[j] = rgb[i];
      imgData.data[j + 1] = rgb[i + 1];
      imgData.data[j + 2] = rgb[i + 2];
      imgData.data[j + 3] = 255;
    }
    rxRgbCtx.putImageData(imgData, 0, 0);
  },
});

backend.init().then(() => {
  $("runtimeSub").textContent =
    (isTauri() ? "[NATIVE CPAL] " : "[OFFLINE WASM PWA] ") +
    "XChaCha20-Poly1305 • RS(255,223) • X25519 • RGB SSTV";
});

/* ---------------- TABS, MIC, STOP ALL, ULTRASONIC, LOOPBACK ---------------- */
document.querySelectorAll<HTMLButtonElement>(".tab").forEach((btn) => {
  btn.onclick = async () => {
    const tab = btn.dataset.tab!;
    document.querySelectorAll(".tab").forEach((b) => b.classList.remove("active"));
    btn.classList.add("active");
    ["digital", "rgb", "draw"].forEach((t) =>
      $(`panel-${t}`).classList.toggle("hidden", t !== tab)
    );
    await backend.setRxMode(tab === "rgb" ? "analog" : "digital");
  };
});

$("btnMic").onclick = async () => {
  const nextState = !micOn;
  try {
    micOn = nextState;
    await backend.toggleMic(nextState);
    $("btnMic").textContent = micOn ? "Stop Receiver" : "Start Receiver (Mic)";
    $("btnMic").className = micOn ? "danger" : "primary";
  } catch (err) {
    micOn = false;
    $("btnMic").textContent = "Start Receiver (Mic)";
    $("btnMic").className = "primary";
    $("micBadge").textContent = "Mic Error";
    alert(`Mic error: ${err}`);
  }
};

$("btnStopAll").onclick = async () => {
  pendingStrokes = [];
  drawing = false;
  lastTxPt = null;
  await backend.stopTx();
  if (micOn) {
    micOn = false;
    await backend.toggleMic(false);
    $("btnMic").textContent = "Start Receiver (Mic)";
    $("btnMic").className = "primary";
  }
  clearRxRgbCanvas();
  $("txTextStatus").textContent = "Stopped";
  $("txRgbStatus").textContent = "Stopped";
  $("rxRgbStatus").textContent = "Stopped";
  $("txDrawStatus").textContent = "Stopped";
};

$("chkUltrasonic").onchange = (e) =>
  backend.setBand((e.target as HTMLInputElement).checked);
$("chkLoopback").onchange = (e) =>
  backend.setLoopback((e.target as HTMLInputElement).checked);

/* ---------------- KEY VAULT & X25519 QR PAIRING ---------------- */
$("btnDerive").onclick = async () => {
  const passphrase = $<HTMLInputElement>("inpPass").value;
  if (!passphrase) return;
  const fp = await backend.setPassphrase(passphrase, "SONICLINK-VAULT-V1");
  $("keyFp").textContent = `KEY: ${fp}`;
  $("pskDisplay").classList.add("hidden");
};

$("btnGenPsk").onclick = async () => {
  const hexKey = await backend.generatePsk();
  $("keyFp").textContent = `KEY: PSK-SET`;
  $("pskDisplay").textContent = `256-bit PSK: ${hexKey}`;
  $("pskDisplay").classList.remove("hidden");
};

$("btnImportPsk").onclick = async () => {
  try {
    const fp = await backend.importPsk($<HTMLInputElement>("inpPass").value);
    $("keyFp").textContent = `KEY: ${fp}`;
  } catch (err) {
    alert(String(err));
  }
};

let myQrPayload = "";
let camStream: MediaStream | null = null;
let scanRaf = 0;

async function rotateX25519() {
  myQrPayload = await backend.initX25519();
  await QRCode.toCanvas($<HTMLCanvasElement>("myQrCanvas"), myQrPayload, {
    width: 190,
    margin: 1,
  });
  $("qrStatus").textContent = "Ephemeral X25519 ready. Scan peer QR to complete ECDH.";
}

function stopQrCamera() {
  cancelAnimationFrame(scanRaf);
  camStream?.getTracks().forEach((t) => t.stop());
  camStream = null;
}

$("btnOpenQr").onclick = async () => {
  const drawer = $("qrDrawer");
  const opening = drawer.classList.contains("hidden");
  drawer.classList.toggle("hidden");
  if (opening) {
    if (!myQrPayload) await rotateX25519();
  } else {
    stopQrCamera();
  }
};

$("btnNewX25519").onclick = rotateX25519;

$("btnCopyMyQr").onclick = () => {
  navigator.clipboard.writeText(myQrPayload);
  $("qrStatus").textContent = "Copied your X25519 Public Key!";
};

async function finalizePairing(peerPayload: string) {
  try {
    const fp = await backend.completeX25519(peerPayload);
    stopQrCamera();
    $("keyFp").textContent = `ECDH: ${fp}`;
    $("qrStatus").textContent = `PAIRED! Verify both devices show: ${fp}`;
    myQrPayload = "";
  } catch (err) {
    $("qrStatus").textContent = `Pairing failed: ${err}`;
  }
}

$("btnPairPasted").onclick = () =>
  finalizePairing($<HTMLInputElement>("inpPass").value);

$("btnStartScan").onclick = async () => {
  if (camStream) {
    stopQrCamera();
    $("btnStartScan").textContent = "Start Camera Scan";
    return;
  }
  try {
    camStream = await navigator.mediaDevices.getUserMedia({
      video: { facingMode: "environment" },
    });
    const video = $<HTMLVideoElement>("qrVideo");
    video.srcObject = camStream;
    await video.play();
    $("btnStartScan").textContent = "Stop Camera";

    const off = document.createElement("canvas");
    const offCtx = off.getContext("2d", { willReadFrequently: true })!;
    const tick = () => {
      if (!camStream) return;
      if (video.readyState === video.HAVE_ENOUGH_DATA) {
        off.width = video.videoWidth;
        off.height = video.videoHeight;
        offCtx.drawImage(video, 0, 0);
        const img = offCtx.getImageData(0, 0, off.width, off.height);
        const code = jsQR(img.data, img.width, img.height);
        if (code?.data?.startsWith("soniclink://x25519/")) {
          finalizePairing(code.data);
          $("btnStartScan").textContent = "Start Camera Scan";
          return;
        }
      }
      scanRaf = requestAnimationFrame(tick);
    };
    tick();
  } catch (err) {
    $("qrStatus").textContent = `Camera unavailable (${err}). Paste peer PubKey instead.`;
  }
};

/* ---------------- TAB 1: DIGITAL TEXT & ASCII ---------------- */
$("btnSendText").onclick = async () => {
  const text = $<HTMLTextAreaElement>("txText").value;
  if (!text) return;
  $("txTextStatus").textContent = "Encrypting + RS-FEC + Modulating...";
  const bytes = await backend.sendText(text);
  $("txTextStatus").textContent = `Transmitted (${bytes} FEC bytes)`;
};

$("btnClearTerm").onclick = () => ($("rxTerminal").textContent = "");

$("fileAscii").onchange = (e) => {
  const file = (e.target as HTMLInputElement).files?.[0];
  if (!file) return;
  const img = new Image();
  img.onload = () => {
    const cols = 20,
      rows = 10;
    const c = document.createElement("canvas");
    c.width = cols;
    c.height = rows;
    const ctx = c.getContext("2d")!;
    ctx.drawImage(img, 0, 0, cols, rows);
    const d = ctx.getImageData(0, 0, cols, rows).data;
    const ramp = " .:-=+*#%@";
    let out = "";
    for (let y = 0; y < rows; y++) {
      for (let x = 0; x < cols; x++) {
        const i = (y * cols + x) * 4;
        const lum = 0.299 * d[i] + 0.587 * d[i + 1] + 0.114 * d[i + 2];
        out += ramp[Math.floor((lum / 256) * ramp.length)];
      }
      if (y < rows - 1) out += "\n";
    }
    $<HTMLTextAreaElement>("txText").value = out;
  };
  img.src = URL.createObjectURL(file);
};

/* ---------------- TAB 2: ENCRYPTED COLOR RGB SSTV ---------------- */
function drawColorTestCard() {
  const bars = [
    "#ffffff",
    "#ffe600",
    "#00e5ff",
    "#00ff66",
    "#ff00dd",
    "#ff2233",
    "#0055ff",
  ];
  const bw = 48 / bars.length;
  bars.forEach((col, i) => {
    txRgbCtx.fillStyle = col;
    txRgbCtx.fillRect(i * bw, 0, Math.ceil(bw), 34);
  });
  txRgbCtx.fillStyle = "#111";
  txRgbCtx.fillRect(0, 34, 48, 14);
  txRgbCtx.fillStyle = "#00f0a0";
  txRgbCtx.fillRect(8, 38, 32, 6);
}
drawColorTestCard();
$("btnColorPattern").onclick = drawColorTestCard;
$("btnClearRgb").onclick = clearRxRgbCanvas;

$("fileRgb").onchange = (e) => {
  const file = (e.target as HTMLInputElement).files?.[0];
  if (!file) return;
  const img = new Image();
  img.onload = () => txRgbCtx.drawImage(img, 0, 0, 48, 48);
  img.src = URL.createObjectURL(file);
};

$("btnSendRgb").onclick = async () => {
  const rgba = txRgbCtx.getImageData(0, 0, 48, 48).data;
  const rgb: number[] = [];
  for (let i = 0; i < rgba.length; i += 4) {
    rgb.push(rgba[i], rgba[i + 1], rgba[i + 2]);
  }
  $("txRgbStatus").textContent = "Sending encrypted nonce + scrambled RGB scanlines...";
  await backend.sendRgbImage(rgb);
};

/* ---------------- TAB 3: LIVE ACOUSTIC DRAWING PAD ---------------- */
let activeColorIdx = 0;
let drawing = false;
let pendingStrokes: StrokePoint[] = [];
let lastTxPt: { x: number; y: number } | null = null;

PALETTE.forEach((hex, idx) => {
  const s = document.createElement("div");
  s.className = "swatch" + (idx === 0 ? " active" : "");
  s.style.background = hex;
  s.onclick = () => {
    activeColorIdx = idx;
    document.querySelectorAll(".swatch").forEach((el) => el.classList.remove("active"));
    s.classList.add("active");
  };
  $("paletteRow").appendChild(s);
});

function pushPoint(x: number, y: number, penDown: boolean) {
  const pt: StrokePoint = {
    x: Math.max(0, Math.min(255, Math.round(x))),
    y: Math.max(0, Math.min(255, Math.round(y))),
    color_idx: activeColorIdx,
    pen_down: penDown,
  };
  pendingStrokes.push(pt);
  if (penDown && lastTxPt) {
    txDrawCtx.strokeStyle = PALETTE[activeColorIdx];
    txDrawCtx.lineWidth = 3;
    txDrawCtx.lineCap = "round";
    txDrawCtx.beginPath();
    txDrawCtx.moveTo(lastTxPt.x, lastTxPt.y);
    txDrawCtx.lineTo(pt.x, pt.y);
    txDrawCtx.stroke();
  }
  lastTxPt = penDown ? { x: pt.x, y: pt.y } : null;
}

async function flushStrokes() {
  if (pendingStrokes.length === 0) return;
  const batch = pendingStrokes.splice(0, 32);
  $("txDrawStatus").textContent = `Streaming ${batch.length} encrypted stroke pts...`;
  await backend.sendStrokes(batch);
}

txDrawCanvas.onpointerdown = (e) => {
  drawing = true;
  const r = txDrawCanvas.getBoundingClientRect();
  pushPoint(
    ((e.clientX - r.left) / r.width) * 255,
    ((e.clientY - r.top) / r.height) * 255,
    true
  );
};

txDrawCanvas.onpointermove = (e) => {
  if (!drawing) return;
  const r = txDrawCanvas.getBoundingClientRect();
  const x = ((e.clientX - r.left) / r.width) * 255;
  const y = ((e.clientY - r.top) / r.height) * 255;
  if (!lastTxPt || Math.hypot(x - lastTxPt.x, y - lastTxPt.y) >= 6) {
    pushPoint(x, y, true);
  }
};

window.onpointerup = () => {
  if (drawing) {
    drawing = false;
    if (lastTxPt) pushPoint(lastTxPt.x, lastTxPt.y, false);
    flushStrokes();
  }
};

setInterval(flushStrokes, 900);

$("btnClearTxDraw").onclick = () => txDrawCtx.clearRect(0, 0, 256, 256);
$("btnClearRxDraw").onclick = () => rxDrawCtx.clearRect(0, 0, 256, 256);