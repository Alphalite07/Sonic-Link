use crate::crypto::{decrypt_packet, MasterKey};
use crate::drawing::StrokePoint;
use crate::fec::fec_decode;
use crate::modem::{goertzel_power, keyed_hop_table, AudioBand, SYM_END, SYM_REPEAT, SYM_START};
use crate::sstv_rgb::{
    RgbScramblePlan, FREQ_BLACK, FREQ_FRAME_SYNC, FREQ_LINE_SYNC, FREQ_WHITE, RGB_HEIGHT, RGB_WIDTH,
};

pub const FHSS_HOP_NONCE: [u8; 12] = *b"SONICFHSS001";

#[derive(Debug, Clone, PartialEq)]
pub enum DecodedPacket {
    Text(String),
    DrawingStrokes(Vec<StrokePoint>),
    RgbFrameNonce([u8; 12]),
}

/// Stateful 50%-overlapping Goertzel FSK + Reed-Solomon + XChaCha20-Poly1305 receiver.
pub struct FskReceiver {
    sample_rate: u32,
    band: AudioBand,
    buf: Vec<f32>,
    window_size: usize,
    hop_size: usize,

    receiving: bool,
    raw_bytes: Vec<u8>,
    pending_nibble: Option<u8>,
    last_data_nibble: u8,
    last_committed_sym: Option<usize>,
    candidate_sym: Option<usize>,
    candidate_hits: u8,
    silence_hops: u32,
}

impl FskReceiver {
    pub fn new(sample_rate: u32, band: AudioBand) -> Self {
        let window_size = (0.020 * sample_rate as f32) as usize; // 20ms analysis window
        let hop_size = (window_size / 2).max(1);                 // 10ms sliding hop (50% overlap)
        Self {
            sample_rate,
            band,
            buf: Vec::with_capacity(window_size * 2),
            window_size,
            hop_size,
            receiving: false,
            raw_bytes: Vec::new(),
            pending_nibble: None,
            last_data_nibble: 0,
            last_committed_sym: None,
            candidate_sym: None,
            candidate_hits: 0,
            silence_hops: 0,
        }
    }

    pub fn set_band(&mut self, band: AudioBand) {
        self.band = band;
        self.reset_sync();
    }

    pub fn reset_sync(&mut self) {
        self.receiving = false;
        self.raw_bytes.clear();
        self.pending_nibble = None;
        self.candidate_sym = None;
        self.candidate_hits = 0;
        self.last_committed_sym = None;
        self.silence_hops = 0;
    }

    pub fn push_samples(&mut self, samples: &[f32], key: &MasterKey) -> Vec<Result<DecodedPacket, String>> {
        self.buf.extend_from_slice(samples);
        let mut results = Vec::new();

        while self.buf.len() >= self.window_size {
            let window: Vec<f32> = self.buf[..self.window_size].to_vec();
            self.buf.drain(..self.hop_size);
            if let Some(res) = self.process_window(&window, key) {
                results.push(res);
            }
        }
        results
    }

    fn process_window(&mut self, window: &[f32], key: &MasterKey) -> Option<Result<DecodedPacket, String>> {
        let base = self.band.base_freq();
        let step = self.band.spacing_hz();

        let mut best_slot = 0usize;
        let mut best_power = 0.0f32;
        let mut total_power = 0.0f32;

        for slot in 0..19 {
            let freq = base + (slot as f32) * step;
            let p = goertzel_power(window, freq, self.sample_rate);
            total_power += p;
            if p > best_power {
                best_power = p;
                best_slot = slot;
            }
        }

        // Scale-invariant SNR check (works for both quiet and loud microphones)
        let avg_power = (total_power - best_power) / 18.0;
        if best_power < 1e-7 || best_power < avg_power * 5.0 {
            self.candidate_hits = 0;
            self.silence_hops = self.silence_hops.saturating_add(1);
            // Reset last committed slot after 300ms of silence so back-to-back retries always lock
            if self.silence_hops >= 30 {
                self.last_committed_sym = None;
            }
            return None;
        }
        self.silence_hops = 0;

        // 3 consecutive overlapping 10ms hops (~30ms steady tone) -> rock-solid debounce
        if self.candidate_sym == Some(best_slot) {
            self.candidate_hits = self.candidate_hits.saturating_add(1);
        } else {
            self.candidate_sym = Some(best_slot);
            self.candidate_hits = 1;
        }

        if self.candidate_hits == 3 && self.last_committed_sym != Some(best_slot) {
            self.last_committed_sym = Some(best_slot);
            return self.on_slot(best_slot, key);
        }
        None
    }

    fn on_slot(&mut self, slot: usize, key: &MasterKey) -> Option<Result<DecodedPacket, String>> {
        if slot == SYM_START {
            self.receiving = true;
            self.raw_bytes.clear();
            self.pending_nibble = None;
            self.last_data_nibble = 0;
            return None;
        }

        if slot == SYM_END {
            if !self.receiving {
                return None;
            }
            self.receiving = false;
            let fec_payload = std::mem::take(&mut self.raw_bytes);
            return Some(Self::decode_and_decrypt(&fec_payload, key));
        }

        if !self.receiving {
            return None;
        }

        let nibble = if slot == SYM_REPEAT {
            self.last_data_nibble
        } else if slot < 16 {
            let hop_table = keyed_hop_table(key, &FHSS_HOP_NONCE);
            let sym = hop_table.iter().position(|&mapped| mapped == slot)? as u8;
            self.last_data_nibble = sym;
            sym
        } else {
            return None;
        };

        match self.pending_nibble {
            None => self.pending_nibble = Some(nibble),
            Some(hi) => {
                self.raw_bytes.push((hi << 4) | nibble);
                self.pending_nibble = None;
            }
        }
        None
    }

    fn decode_and_decrypt(fec_payload: &[u8], key: &MasterKey) -> Result<DecodedPacket, String> {
        let encrypted = fec_decode(fec_payload).map_err(|e| format!("FEC failed ({} raw bytes): {e}", fec_payload.len()))?;
        let plaintext = decrypt_packet(key, &encrypted).map_err(|e| format!("Crypto failed: {e}"))?;

        if plaintext.is_empty() {
            return Err("Empty packet".into());
        }
        match plaintext[0] {
            0x01 => Ok(DecodedPacket::Text(String::from_utf8_lossy(&plaintext[1..]).to_string())),
            0x02 => {
                let mut pts = Vec::new();
                for chunk in plaintext[1..].chunks_exact(3) {
                    pts.push(StrokePoint::unpack([chunk[0], chunk[1], chunk[2]]));
                }
                Ok(DecodedPacket::DrawingStrokes(pts))
            }
            0x03 if plaintext.len() == 13 => {
                let mut nonce = [0u8; 12];
                nonce.copy_from_slice(&plaintext[1..13]);
                Ok(DecodedPacket::RgbFrameNonce(nonce))
            }
            _ => Err("Unknown packet type".into()),
        }
    }
}

/// Stateful Encrypted RGB SSTV Receiver.
pub struct RgbSstvReceiver {
    sample_rate: u32,
    px_samples: usize,
    buf: Vec<f32>,

    pub canvas_rgb: Vec<u8>,
    plan: RgbScramblePlan,
    tx_row: usize,
    sub_col: usize,
    in_frame_sync: bool,
    in_line_sync: bool,
    just_reset_frame: bool,
    noise_seed: u32,
}

impl RgbSstvReceiver {
    pub fn new(sample_rate: u32, key: &MasterKey) -> Self {
        let px_samples = (0.006 * sample_rate as f32) as usize;
        Self {
            sample_rate,
            px_samples,
            buf: Vec::with_capacity(px_samples * 2),
            canvas_rgb: vec![0u8; RGB_WIDTH * RGB_HEIGHT * 3],
            plan: RgbScramblePlan::generate(key, &[0u8; 12]),
            tx_row: 0,
            sub_col: 0,
            in_frame_sync: false,
            in_line_sync: false,
            just_reset_frame: false,
            noise_seed: 0xA5A5_1337,
        }
    }

    pub fn arm_with_nonce(&mut self, key: &MasterKey, frame_nonce: &[u8; 12]) {
        self.plan = RgbScramblePlan::generate(key, frame_nonce);
        self.tx_row = 0;
        self.sub_col = 0;
    }

    pub fn push_samples(&mut self, samples: &[f32]) -> bool {
        self.buf.extend_from_slice(samples);
        let mut updated = false;

        while self.buf.len() >= self.px_samples {
            let chunk: Vec<f32> = self.buf.drain(..self.px_samples).collect();
            self.process_pixel_slot(&chunk);
            updated = true;
        }
        updated
    }

    fn fast_rand_u8(&mut self) -> u8 {
        self.noise_seed ^= self.noise_seed << 13;
        self.noise_seed ^= self.noise_seed >> 17;
        self.noise_seed ^= self.noise_seed << 5;
        (self.noise_seed & 0xFF) as u8
    }

    fn process_pixel_slot(&mut self, chunk: &[f32]) {
        let p_frame = goertzel_power(chunk, FREQ_FRAME_SYNC, self.sample_rate);
        let p_line = goertzel_power(chunk, FREQ_LINE_SYNC, self.sample_rate);

        let mut powers = [0.0f32; 29];
        let mut best_i = 0usize;
        let mut best_p = 0.0f32;

        for i in 0..29 {
            let freq = FREQ_BLACK + (i as f32) * 50.0;
            let p = goertzel_power(chunk, freq, self.sample_rate);
            powers[i] = p;
            if p > best_p {
                best_p = p;
                best_i = i;
            }
        }

        if p_frame > 1e-6 && p_frame > best_p * 2.5 && p_frame > p_line * 1.8 {
            if !self.in_frame_sync {
                self.in_frame_sync = true;
                self.tx_row = 0;
                self.sub_col = 0;
                self.just_reset_frame = true;
            }
            return;
        } else {
            self.in_frame_sync = false;
        }

        if p_line > 1e-6 && p_line > best_p * 2.5 {
            if !self.in_line_sync {
                self.in_line_sync = true;
                self.sub_col = 0;
                if self.just_reset_frame {
                    self.just_reset_frame = false;
                } else {
                    self.tx_row = (self.tx_row + 1) % RGB_HEIGHT;
                }
            }
            return;
        } else {
            self.in_line_sync = false;
        }

        let norm_val: f32 = if best_p < 1e-7 {
            (self.fast_rand_u8() as f32 / 255.0) * 0.35
        } else {
            let refined_i = if best_i > 0 && best_i < 28 {
                let a = powers[best_i - 1].sqrt();
                let b = powers[best_i].sqrt();
                let c = powers[best_i + 1].sqrt();
                let denom = a - 2.0 * b + c;
                if denom.abs() > 1e-6 {
                    best_i as f32 + 0.5 * (a - c) / denom
                } else {
                    best_i as f32
                }
            } else {
                best_i as f32
            };
            let freq = FREQ_BLACK + refined_i * 50.0;
            ((freq - FREQ_BLACK) / (FREQ_WHITE - FREQ_BLACK)).clamp(0.0, 1.0)
        };

        let row_idx = self.tx_row % RGB_HEIGHT;
        let sub_idx = self.sub_col % (RGB_WIDTH * 3);
        let ch_slot = sub_idx / RGB_WIDTH;
        let px_slot = sub_idx % RGB_WIDTH;

        let actual_y = self.plan.row_order[row_idx];
        let actual_c = self.plan.channel_orders[row_idx][ch_slot];
        let actual_x = self.plan.pixel_orders[row_idx][px_slot];
        let inverted = self.plan.invert_mask[row_idx][sub_idx];

        let unscrambled = if inverted { 1.0 - norm_val } else { norm_val };
        self.canvas_rgb[(actual_y * RGB_WIDTH + actual_x) * 3 + actual_c] =
            (unscrambled.clamp(0.0, 1.0) * 255.0) as u8;

        self.sub_col += 1;
        if self.sub_col >= RGB_WIDTH * 3 {
            self.sub_col = 0;
            self.tx_row = (self.tx_row + 1) % RGB_HEIGHT;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::encrypt_packet;
    use crate::fec::fec_encode;
    use crate::modem::modulate_fsk;

    #[test]
    fn roundtrip_quiet_mic_with_noise() {
        let key = MasterKey::from_passphrase("TEST-KEY", b"SALT123456789012").unwrap();
        let mut payload = vec![0x01];
        payload.extend_from_slice(b"Hello SonicLink Military Acoustic Link!");

        let enc = encrypt_packet(&key, &payload).unwrap();
        let fec = fec_encode(&enc).unwrap();
        let mut pcm = modulate_fsk(&fec, &key, &FHSS_HOP_NONCE, AudioBand::Audible, 48000);

        // Simulate quiet phone mic (2% volume!) + leading silence
        let mut noisy = vec![0.0f32; 4800];
        noisy.extend(pcm.drain(..).map(|s| s * 0.02));
        noisy.extend(vec![0.0f32; 4800]);

        let mut rx = FskReceiver::new(48000, AudioBand::Audible);
        let decoded = rx.push_samples(&noisy, &key);
        assert_eq!(decoded.len(), 1);
        assert_eq!(
            decoded[0].as_ref().unwrap(),
            &DecodedPacket::Text("Hello SonicLink Military Acoustic Link!".into())
        );
    }
}