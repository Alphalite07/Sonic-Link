# SonicLink // Encrypted Multiplatform Acoustic Modem

**SonicLink** is an air-gapped, multiplatform data-over-sound communicator built with **Rust**, **Tauri v2**, and **WebAssembly (Offline PWA)**. It transmits encrypted text, ASCII art, full-color analog images, and live drawings purely through everyday speakers and microphones—with zero Wi-Fi, Bluetooth, or cellular required.

## Core Features

- **Military-Grade Digital Encryption:**
  - **XChaCha20-Poly1305** authenticated encryption (256-bit key + 192-bit random nonce per packet).
  - **Argon2id** passphrase key derivation, **256-bit PSK**, and **X25519 Ephemeral ECDH + HKDF-SHA256** pairing via live **QR Code** scanner/generator.
  - **Zeroize-on-drop** secret memory handling in Rust.
  - **Keyed Frequency Hopping (FHSS-lite):** Symbol-to-frequency mapping shuffles cryptographically using a `ChaCha20` keystream.
- **Forward Error Correction:** CCSDS **Reed-Solomon `RS(255, 223)`** repairs up to 16 corrupted bytes per block *before* AEAD authentication.
- **Audible & Ultrasonic Modes:** Switch between audible chirps (`1.1–3.0 kHz`) and near-silent ultrasonic (`18.0–19.8 kHz`).
- **Encrypted Color Analog Mode (RGB SSTV):** Martin-M1-inspired sequential R/G/B scanlines with keyed row, channel, pixel, and polarity scrambling. Without the key, receivers see pure rainbow static; with the key, full-color images materialize organically out of ambient noise.
- **Live Acoustic Drawing Pad:** Streams quantized 3-byte vector strokes as encrypted acoustic micro-bursts in real time.
- **Acoustic Echo Self-Mute:** Automatically mutes the microphone during TX (+320ms room reverb guard), with a **Loopback Test** toggle for single-device testing.
- **True Multiplatform:**
  - **Native Desktop & Mobile (Tauri v2 + `cpal`):** Windows, macOS, Linux, Android, iOS.
  - **Installable Offline PWA (`wasm-bindgen` + `vite-plugin-pwa`):** Runs directly in mobile/desktop browsers with the exact same Rust DSP/crypto core compiled to WebAssembly.

## Architecture

```text
soniclink/
├── crates/
│   ├── soniclink-core/   # Pure Rust: Crypto, RS FEC, FHSS-FSK, RGB SSTV, Drawing Codec
│   └── soniclink-wasm/   # wasm-bindgen bridge for Browser / Offline PWA
├── src-tauri/            # Tauri v2 native backend + cpal low-latency audio I/O
└── src/                  # Unified TypeScript frontend (auto-switches Native vs WASM)