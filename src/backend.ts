export type StrokePoint = {
  x: number;
  y: number;
  color_idx: number;
  pen_down: boolean;
};

export interface SonicCallbacks {
  onMicStatus: (s: string) => void;
  onMuteState: (muted: boolean) => void;
  onStatus: (s: string) => void;
  onError: (s: string) => void;
  onText: (t: string) => void;
  onDrawing: (pts: StrokePoint[]) => void;
  onRgbFrame: (rgb: Uint8Array | number[]) => void;
}

export const isTauri = () =>
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export class SonicBackend {
  private cb: SonicCallbacks;

  // Browser / Offline WASM PWA runtime fields
  private audioCtx: AudioContext | null = null;
  private micStream: MediaStream | null = null;
  private micSource: MediaStreamAudioSourceNode | null = null;
  private workletNode: AudioWorkletNode | null = null;
  private silentGain: GainNode | null = null;
  private activeSources: AudioBufferSourceNode[] = [];
  private wasm: any = null;

  private loopback = false;
  private muteUntil = 0;
  private wasMuted = false;
  private rgbTimer: number | null = null;
  private analogMode = false;

  constructor(cb: SonicCallbacks) {
    this.cb = cb;
  }

  async init() {
    if (isTauri()) {
      const { listen } = await import("@tauri-apps/api/event");
      listen<string>("mic-status", (e) => this.cb.onMicStatus(e.payload));
      listen<boolean>("tx-mute-state", (e) => this.cb.onMuteState(e.payload));
      listen<string>("rx-status", (e) => this.cb.onStatus(e.payload));
      listen<string>("rx-error", (e) => this.cb.onError(e.payload));
      listen<string>("rx-text", (e) => this.cb.onText(e.payload));
      listen<StrokePoint[]>("rx-drawing", (e) => this.cb.onDrawing(e.payload));
      listen<number[]>("rx-rgb-frame", (e) => this.cb.onRgbFrame(e.payload));
    } else {
      const mod = await import("./wasm-pkg/soniclink_wasm.js");
      await mod.default();
      this.audioCtx = new (window.AudioContext ||
        (window as any).webkitAudioContext)({
        sampleRate: 48000,
      });
      this.wasm = new mod.WasmSonicEngine(this.audioCtx.sampleRate);
    }
  }

  private async tauriInvoke<T>(
    cmd: string,
    args?: Record<string, unknown>
  ): Promise<T> {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke<T>(cmd, args);
  }

  private playWebPcm(pcm: Float32Array) {
    if (!this.audioCtx) return;
    if (this.audioCtx.state === "suspended") this.audioCtx.resume();

    const buf = this.audioCtx.createBuffer(1, pcm.length, this.audioCtx.sampleRate);
    buf.copyToChannel(new Float32Array(pcm), 0);

    const src = this.audioCtx.createBufferSource();
    src.buffer = buf;
    src.connect(this.audioCtx.destination);

    this.activeSources.push(src);
    src.onended = () => {
      this.activeSources = this.activeSources.filter((s) => s !== src);
    };

    // Acoustic Echo Self-Mute window: playback duration + 320ms room reverb tail
    const durMs = (pcm.length / this.audioCtx.sampleRate) * 1000;
    this.muteUntil = Math.max(this.muteUntil, performance.now() + durMs + 320);
    if (!this.loopback) {
      this.wasm?.reset_rx();
      this.cb.onMuteState(true);
      this.wasMuted = true;
    }

    src.start();
  }

  async stopTx() {
    if (isTauri()) return this.tauriInvoke<void>("stop_tx");

    for (const src of this.activeSources) {
      try {
        src.stop();
        src.disconnect();
      } catch {
        // already stopped
      }
    }
    this.activeSources = [];
    this.muteUntil = 0;
    this.wasMuted = false;
    this.cb.onMuteState(false);
  }

  async setPassphrase(passphrase: string, salt: string): Promise<string> {
    if (isTauri()) return this.tauriInvoke("set_passphrase", { passphrase, salt });
    return this.wasm.set_passphrase(passphrase, salt);
  }

  async generatePsk(): Promise<string> {
    if (isTauri()) return this.tauriInvoke("generate_psk");
    return this.wasm.generate_psk();
  }

  async importPsk(hexKey: string): Promise<string> {
    if (isTauri()) return this.tauriInvoke("import_psk", { hexKey });
    return this.wasm.import_psk(hexKey);
  }

  async initX25519(): Promise<string> {
    if (isTauri()) return this.tauriInvoke("init_x25519");
    return this.wasm.init_x25519();
  }

  async completeX25519(peerQr: string): Promise<string> {
    if (isTauri()) return this.tauriInvoke("complete_x25519", { peerQr });
    return this.wasm.complete_x25519(peerQr);
  }

  async setBand(ultrasonic: boolean) {
    if (isTauri()) return this.tauriInvoke<void>("set_band", { ultrasonic });
    this.wasm.set_band(ultrasonic);
  }

  async setRxMode(mode: "digital" | "analog") {
    this.analogMode = mode === "analog";
    if (isTauri()) return this.tauriInvoke<void>("set_rx_mode", { mode });
    this.wasm.set_analog_mode(this.analogMode);
  }

  async setLoopback(enabled: boolean) {
    this.loopback = enabled;
    if (isTauri()) return this.tauriInvoke<void>("set_loopback", { enabled });
    if (enabled) {
      this.wasMuted = false;
      this.cb.onMuteState(false);
    }
  }

  private cleanupWebMic() {
    if (this.silentGain) {
    this.silentGain.disconnect();
    this.silentGain = null;
    }
    if (this.workletNode) {
      this.workletNode.port.onmessage = null;
      this.workletNode.disconnect();
      this.workletNode = null;
    }
    if (this.micSource) {
      this.micSource.disconnect();
      this.micSource = null;
    }
    if (this.micStream) {
      this.micStream.getTracks().forEach((t) => t.stop());
      this.micStream = null;
    }
    if (this.rgbTimer !== null) {
      clearInterval(this.rgbTimer);
      this.rgbTimer = null;
    }
  }

  async toggleMic(enable: boolean) {
    if (isTauri()) return this.tauriInvoke<void>("toggle_mic", { enable });

    if (!enable) {
      this.cleanupWebMic();
      this.cb.onMicStatus("Mic Off");
      return;
    }

    // Ensure no leftover nodes/timers exist before starting
    this.cleanupWebMic();

    const ctx = this.audioCtx!;
    if (ctx.state === "suspended") await ctx.resume();

    this.micStream = await navigator.mediaDevices.getUserMedia({
      audio: {
        echoCancellation: false,
        noiseSuppression: false,
        autoGainControl: false,
      },
    });

    const workletCode = `
      class MicTap extends AudioWorkletProcessor {
        process(inputs) {
          const ch = inputs[0]?.[0];
          if (ch) this.port.postMessage(new Float32Array(ch));
          return true;
        }
      }
      registerProcessor('mic-tap', MicTap);
    `;
    const url = URL.createObjectURL(
      new Blob([workletCode], { type: "application/javascript" })
    );
    try {
      await ctx.audioWorklet.addModule(url);
    } finally {
      URL.revokeObjectURL(url);
    }

    this.micSource = ctx.createMediaStreamSource(this.micStream);
    this.workletNode = new AudioWorkletNode(ctx, "mic-tap");
    this.workletNode.port.onmessage = (e) => {
      const muted = !this.loopback && performance.now() < this.muteUntil;
      if (muted !== this.wasMuted) {
        this.wasMuted = muted;
        this.cb.onMuteState(muted);
      }
      if (muted) return;

      const json = this.wasm.push_mic_samples(e.data as Float32Array);
      const events = JSON.parse(json) as { kind: string; data: any }[];
      for (const ev of events) {
        if (ev.kind === "Text") this.cb.onText(ev.data);
        if (ev.kind === "Drawing") this.cb.onDrawing(ev.data);
        if (ev.kind === "Status") this.cb.onStatus(ev.data);
        if (ev.kind === "Error") this.cb.onError(ev.data);
      }
    };

    this.silentGain = ctx.createGain();
    this.silentGain.gain.value = 0;
    this.micSource.connect(this.workletNode);
    this.workletNode.connect(this.silentGain);
    this.silentGain.connect(ctx.destination);
    this.cb.onMicStatus(`PWA Listening (${ctx.sampleRate} Hz)`);

    this.rgbTimer = window.setInterval(() => {
      if (this.analogMode && this.wasm) {
        this.cb.onRgbFrame(this.wasm.get_rgb_frame());
      }
    }, 60);
  }

  async sendText(text: string): Promise<number> {
    if (isTauri()) return this.tauriInvoke("send_encrypted_text", { text });
    const pcm: Float32Array = this.wasm.encode_text(
      text,
      this.audioCtx!.sampleRate
    );
    this.playWebPcm(pcm);
    return Math.round(pcm.length / 480);
  }

  async sendStrokes(strokes: StrokePoint[]): Promise<void> {
    if (isTauri()) {
      await this.tauriInvoke("send_drawing_strokes", { strokes });
      return;
    }
    const pcm: Float32Array = this.wasm.encode_strokes(
      JSON.stringify(strokes),
      this.audioCtx!.sampleRate
    );
    this.playWebPcm(pcm);
  }

  async sendRgbImage(rgbBytes: number[]): Promise<void> {
    if (isTauri()) {
      await this.tauriInvoke("send_encrypted_rgb_image", { rgbBytes });
      return;
    }
    await this.stopTx();
    const pcm: Float32Array = this.wasm.encode_rgb_image(
      new Uint8Array(rgbBytes),
      this.audioCtx!.sampleRate
    );
    this.playWebPcm(pcm);
  }
}