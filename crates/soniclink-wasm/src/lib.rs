use rand::{rngs::OsRng, RngCore};
use serde::Serialize;
use soniclink_core::crypto::{encrypt_packet, KeyExchangeSession, MasterKey};
use soniclink_core::drawing::StrokePoint;
use soniclink_core::fec::fec_encode;
use soniclink_core::modem::{modulate_fsk, AudioBand};
use soniclink_core::rx::{DecodedPacket, FskReceiver, RgbSstvReceiver, FHSS_HOP_NONCE};
use soniclink_core::sstv_rgb::modulate_encrypted_rgb_sstv;
use wasm_bindgen::prelude::*;

#[derive(Serialize)]
#[serde(tag = "kind", content = "data")]
enum WasmEvent {
    Text(String),
    Drawing(Vec<StrokePoint>),
    Status(String),
    Error(String),
}

#[wasm_bindgen]
pub struct WasmSonicEngine {
    key: MasterKey,
    kx: Option<KeyExchangeSession>,
    band: AudioBand,
    analog_mode: bool,
    fsk_rx: FskReceiver,
    rgb_rx: RgbSstvReceiver,
}

#[wasm_bindgen]
impl WasmSonicEngine {
    #[wasm_bindgen(constructor)]
    pub fn new(sample_rate: u32) -> Self {
        let key = MasterKey::from_passphrase("SONICLINK-DEFAULT", b"TACTICAL-ROOM-01")
            .expect("default key");
        Self {
            fsk_rx: FskReceiver::new(sample_rate, AudioBand::Audible),
            rgb_rx: RgbSstvReceiver::new(sample_rate, &key),
            key,
            kx: None,
            band: AudioBand::Audible,
            analog_mode: false,
        }
    }

    pub fn set_passphrase(&mut self, passphrase: &str, salt: &str) -> Result<String, JsValue> {
        let k = MasterKey::from_passphrase(passphrase, salt.as_bytes())
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let fp = k.safety_fingerprint();
        self.key = k;
        Ok(fp)
    }

    pub fn generate_psk(&mut self) -> String {
        let k = MasterKey::generate_random();
        let hex_key = hex::encode(k.as_bytes());
        self.key = k;
        hex_key
    }

    pub fn import_psk(&mut self, hex_key: &str) -> Result<String, JsValue> {
        let bytes = hex::decode(hex_key.trim()).map_err(|_| JsValue::from_str("Invalid hex key"))?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| JsValue::from_str("Key must be 64 hex chars"))?;
        self.key = MasterKey::from_raw(arr);
        Ok(self.key.safety_fingerprint())
    }

    pub fn init_x25519(&mut self) -> String {
        let session = KeyExchangeSession::new();
        let qr = session.qr_payload();
        self.kx = Some(session);
        qr
    }

    pub fn complete_x25519(&mut self, peer_qr: &str) -> Result<String, JsValue> {
        let session = self.kx.take().ok_or_else(|| JsValue::from_str("Generate your QR first"))?;
        let (new_key, fp) = session
            .derive_session_key(peer_qr)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        self.key = new_key;
        Ok(fp)
    }

    pub fn set_band(&mut self, ultrasonic: bool) {
        self.band = if ultrasonic { AudioBand::Ultrasonic } else { AudioBand::Audible };
        self.fsk_rx.set_band(self.band);
    }

    pub fn set_analog_mode(&mut self, enabled: bool) {
        self.analog_mode = enabled;
    }

    pub fn reset_rx(&mut self) {
        self.fsk_rx.reset_sync();
    }

    fn encode_digital(&self, payload: &[u8], sample_rate: u32) -> Result<Vec<f32>, JsValue> {
        let enc = encrypt_packet(&self.key, payload).map_err(|e| JsValue::from_str(&e.to_string()))?;
        let fec = fec_encode(&enc).map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(modulate_fsk(&fec, &self.key, &FHSS_HOP_NONCE, self.band, sample_rate))
    }

    pub fn encode_text(&self, text: &str, sample_rate: u32) -> Result<Vec<f32>, JsValue> {
        let mut payload = Vec::with_capacity(1 + text.len());
        payload.push(0x01);
        payload.extend_from_slice(text.as_bytes());
        self.encode_digital(&payload, sample_rate)
    }

    pub fn encode_strokes(&self, strokes_json: &str, sample_rate: u32) -> Result<Vec<f32>, JsValue> {
        let strokes: Vec<StrokePoint> =
            serde_json::from_str(strokes_json).map_err(|e| JsValue::from_str(&e.to_string()))?;
        let mut payload = Vec::with_capacity(1 + strokes.len() * 3);
        payload.push(0x02);
        for pt in strokes {
            payload.extend_from_slice(&pt.pack());
        }
        self.encode_digital(&payload, sample_rate)
    }

    pub fn encode_rgb_image(&self, rgb_bytes: &[u8], sample_rate: u32) -> Result<Vec<f32>, JsValue> {
        let mut frame_nonce = [0u8; 12];
        OsRng.fill_bytes(&mut frame_nonce);
        let mut hdr = Vec::with_capacity(13);
        hdr.push(0x03);
        hdr.extend_from_slice(&frame_nonce);

        let mut pcm = self.encode_digital(&hdr, sample_rate)?;
        pcm.extend(vec![0.0f32; (0.120 * sample_rate as f32) as usize]);
        pcm.extend(modulate_encrypted_rgb_sstv(rgb_bytes, &self.key, &frame_nonce, sample_rate));
        Ok(pcm)
    }

    /// Feed raw mic PCM samples from Web Audio AudioWorklet; returns JSON array of WasmEvents.
    pub fn push_mic_samples(&mut self, samples: &[f32]) -> String {
        let mut events = Vec::new();
        for res in self.fsk_rx.push_samples(samples, &self.key) {
            match res {
                Ok(DecodedPacket::Text(t)) => events.push(WasmEvent::Text(t)),
                Ok(DecodedPacket::DrawingStrokes(pts)) => events.push(WasmEvent::Drawing(pts)),
                Ok(DecodedPacket::RgbFrameNonce(nonce)) => {
                    self.rgb_rx.arm_with_nonce(&self.key, &nonce);
                    events.push(WasmEvent::Status("Encrypted RGB Nonce Authenticated!".into()));
                }
                Err(e) => events.push(WasmEvent::Error(e)),
            }
        }
        if self.analog_mode {
            self.rgb_rx.push_samples(samples);
        }
        serde_json::to_string(&events).unwrap_or_else(|_| "[]".into())
    }

    pub fn get_rgb_frame(&self) -> Vec<u8> {
        self.rgb_rx.canvas_rgb.clone()
    }
}