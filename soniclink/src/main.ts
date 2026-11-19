import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

/* ---------------- STATE & TABS ---------------- */
let micOn = false;

document.querySelectorAll<HTMLButtonElement>(".tab").forEach((btn) => {
  btn.onclick = async () => {
    const tab = btn.dataset.tab!;
    document.querySelectorAll(".tab").forEach((b) => b.classList.remove("active"));
    btn.classList.add("active");
    ["digital", "rgb", "draw"].forEach((t) =>
      $(`panel-${t}`).classList.toggle("hidden", t !== tab)
    );
    await invoke("set_rx_mode", { mode: tab === "rgb" ? "analog" : "digital" });
  };
});

/* ---------------- MIC & ULTRASONIC ---------------- */
$("btnMic").onclick = async () => {
  micOn = !micOn;
  await invoke("toggle_mic", { enable: micOn });
  $("btnMic").textContent = micOn ? "Stop Receiver" : "Start Receiver (Mic)";
  $("btnMic").className = micOn ? "danger" : "primary";
};

$("chkUltrasonic").onchange = async (e) => {
  const ultrasonic = (e.target as HTMLInputElement).checked;
  await invoke("set_band", { ultrasonic });
};

/* ---------------- KEY VAULT ---------------- */
$("btnDerive").onclick = async () => {
  const passphrase = $<HTMLInputElement>("inpPass").value;
  if (!passphrase) return;
  const fp = await invoke<string>("set_passphrase", { passphrase, salt: "SONICLINK-VAULT-V1" });
  $("keyFp").textContent = `KEY: ${fp}`;
  $("pskDisplay").classList.add("hidden");
};

$("btnGenPsk").onclick = async () => {
  const hexKey = await invoke<string>("generate_psk");
  $("keyFp").textContent = `KEY: ${hexKey.slice(0, 8).toUpperCase()}`;
  $("pskDisplay").textContent = `256-bit PSK (share out-of-band): ${hexKey}`;
  $("pskDisplay").classList.remove("hidden");
};

$("btnImportPsk").onclick = async () => {
  const hexKey = $<HTMLInputElement>("inpPass").value;
  try {
    const fp = await invoke<string>("import_psk", { hexKey });
    $("keyFp").textContent = `KEY: ${fp}`;
    $("pskDisplay").classList.add("hidden");
  } catch (err) {
    alert(String(err));
  }
};

/* ---------------- TAB 1: DIGITAL TEXT & ASCII ---------------- */
$("btnSendText").onclick = async () => {
  const text = $<HTMLTextAreaElement>("txText").value;
  if (!text) return;
  $("txTextStatus").textContent = "Encrypting + RS-FEC + Modulating...";
  const bytes = await invoke<number>("send_encrypted_text", { text });
  $("txTextStatus").textContent = `Transmitted ${bytes} FEC-protected encrypted bytes`;
};

$("btnClearTerm").onclick = () => ($("rxTerminal").textContent = "");

$("fileAscii").onchange = (e) => {
  const file = (e.target as HTMLInputElement).files?.[0];
  if (!file) return;
  const img = new Image();
  img.onload = () => {
    const cols = 20, rows = 10;
    const c = document.createElement("canvas");
    c.width = cols; c.height = rows;
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
const txRgbCtx = $<HTMLCanvasElement>("txRgbCanvas").getContext("2d")!;
const rxRgbCtx = $<HTMLCanvasElement>("rxRgbCanvas").getContext("2d")!;

function drawColorTestCard() {
  const bars = ["#ffffff", "#ffe600", "#00e5ff", "#00ff66", "#ff00dd", "#ff2233", "#0055ff"];
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
  await invoke("send_encrypted_rgb_image", { rgbBytes: rgb });
};

/* ---------------- TAB 3: LIVE ACOUSTIC DRAWING PAD ---------------- */
const PALETTE = ["#00f0a0", "#00c2ff", "#ff4769", "#ffe600", "#c77dff", "#ff8800", "#ffffff", "#05080b"];
let activeColorIdx = 0;
let drawing = false;
let pendingStrokes: { x: number; y: number; color_idx: number; pen_down: boolean }[] = [];

const txDrawCanvas = $<HTMLCanvasElement>("txDrawCanvas");
const txDrawCtx = txDrawCanvas.getContext("2d")!;
const rxDrawCtx = $<HTMLCanvasElement>("rxDrawCanvas").getContext("2d")!;

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

let lastTxPt: { x: number; y: number } | null = null;
function pushPoint(x: number, y: number, penDown: boolean) {
  const pt = { x: Math.max(0, Math.min(255, Math.round(x))), y: Math.max(0, Math.min(255, Math.round(y))), color_idx: activeColorIdx, pen_down: penDown };
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
  const batch = pendingStrokes.splice(0, 32); // compact micro-burst
  $("txDrawStatus").textContent = `Streaming ${batch.length} encrypted stroke pts...`;
  await invoke("send_drawing_strokes", { strokes: batch });
}

txDrawCanvas.onpointerdown = (e) => {
  drawing = true;
  const r = txDrawCanvas.getBoundingClientRect();
  pushPoint(((e.clientX - r.left) / r.width) * 255, ((e.clientY - r.top) / r.height) * 255, true);
};
txDrawCanvas.onpointermove = (e) => {
  if (!drawing) return;
  const r = txDrawCanvas.getBoundingClientRect();
  const x = ((e.clientX - r.left) / r.width) * 255;
  const y = ((e.clientY - r.top) / r.height) * 255;
  // Subsample movement so we only transmit meaningful deltas (~6px apart)
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

/* ---------------- TAURI LIVE RECEIVER EVENTS ---------------- */
listen<string>("mic-status", (e) => ($("micBadge").textContent = e.payload));
listen<string>("rx-status", (e) => ($("rxRgbStatus").textContent = e.payload));
listen<string>("rx-error", (e) => {
  $("rxTerminal").textContent += `\n[BLOCKED / NOISE] ${e.payload}`;
});

listen<string>("rx-text", (e) => {
  const ts = new Date().toLocaleTimeString();
  $("rxTerminal").textContent += `\n[${ts} VERIFIED]\n${e.payload}\n`;
});

listen<number[]>("rx-rgb-frame", (e) => {
  const rgb = e.payload;
  const imgData = rxRgbCtx.createImageData(48, 48);
  for (let i = 0, j = 0; i < rgb.length; i += 3, j += 4) {
    imgData.data[j] = rgb[i];
    imgData.data[j + 1] = rgb[i + 1];
    imgData.data[j + 2] = rgb[i + 2];
    imgData.data[j + 3] = 255;
  }
  rxRgbCtx.putImageData(imgData, 0, 0);
});

let lastRxPt: { x: number; y: number } | null = null;
listen<{ x: number; y: number; color_idx: number; pen_down: boolean }[]>("rx-drawing", (e) => {
  for (const pt of e.payload) {
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
});