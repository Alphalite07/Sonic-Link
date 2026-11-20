use crate::crypto::{decrypt_packet, CryptoScrambler, MasterKey};
use crate::drawing::StrokePoint;
use crate::fec::fec_decode;
use crate::modem::{goertzel_power, AudioBand, SYM_END, SYM_REPEAT, SYM_START};
use crate::sstv_rgb::{
    RgbScramblePlan, FREQ_BLACK, FREQ_FRAME_SYNC, FREQ_LINE_SYNC, FREQ_WHITE, RGB_HEIGHT, RGB_WIDTH,
};

pub const FHSS_HOP_NONCE: [u8; 12] = *b"SONICFHSS001";

/// Unified encrypted digital packet types:
/// [0x01 || utf8_bytes]       -> Text or ASCII art
/// [0x02 || packed_strokes]   -> Live drawing pad micro-burst
/// [0x03 || 12-byte nonce]    -> Encrypted key-sync for upcoming Color RGB SSTV frame
#[derive(Debug, Clone)]
pub enum DecodedPacket {
    Text(String),
    DrawingStrokes(Vec<StrokePoint>),
    RgbFrameNonce([u8; 12]),
}

/// Stateful Goertzel-based FHSS-FSK + Reed-Solomon + XChaCha20-Poly1305 receiver.
pub struct FskReceiver {
    sample_rate: u32,
    band: AudioBand,
    buf: Vec<f32>,
    window_size: usize,

    receiving: bool,
    raw_bytes: Vec<u8>,
    pending_nibble: Option<u8>,
    last_data_nibble: u8,
    last_committed_sym: Option<usize>,
    candidate_sym: Option<usize>,
    candidate_hits: u8,
    scrambler: Option<CryptoScrambler>,
}

impl FskReceiver {
    pub fn new(sample_rate: u32, band: AudioBand) -> Self {
        // ~20ms Goertzel analysis window (fits 3+ windows inside each 60-75ms symbol)
        let window_size = (0.020 * sample_rate as f32) as usize;
        Self {
            sample_rate,
            band,
            buf: Vec::with_capacity(window_size * 2),
            window_size,
            receiving: false,
            raw_bytes: Vec::new(),
            pending_nibble: None,
            last_data_nibble: 0,
            last_committed_sym: None,
            candidate_sym: None,
            candidate_hits: 0,
            scrambler: None,
        }
    }

    pub fn set_band(&mut self, band: AudioBand) {
        self.band = band;
        self.receiving = false;
    }

    pub fn reset_sync(&mut self) {
        self.receiving = false;
        self.raw_bytes.clear();
        self.pending_nibble = None;
        self.candidate_sym = None;
        self.candidate_hits = 0;
        self.last_committed_sym = None;
    }

    /// Feed mono PCM mic samples. Returns any fully verified & decrypted packets.
    pub fn push_samples(&mut self, samples: &[f32], key: &MasterKey) -> Vec<Result<DecodedPacket, String>> {
        self.buf.extend_from_slice(samples);
        let mut results = Vec::new();

        while self.buf.len() >= self.window_size {
            let chunk: Vec<f32> = self.buf.drain(..self.window_size).collect();
            if let Some(res) = self.process_window(&chunk, key) {
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

        // Require clear SNR peak over average band energy
        let avg_power = (total_power - best_power) / 18.0;
        if best_power < 1e-5 || best_power < avg_power * 8.0 {
            self.candidate_hits = 0;
            return None;
        }

        // 2-window debounce (~40ms steady tone)
        if self.candidate_sym == Some(best_slot) {
            self.candidate_hits = self.candidate_hits.saturating_add(1);
        } else {
            self.candidate_sym = Some(best_slot);
            self.candidate_hits = 1;
        }

        if self.candidate_hits == 2 && self.last_committed_sym != Some(best_slot) {
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
            self.scrambler = Some(CryptoScrambler::new(key, &FHSS_HOP_NONCE));
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
            // Invert the keyed frequency-hop permutation for this step
            let scrambler = self.scrambler.as_mut()?;
            let hop_table = scrambler.permutation(16);
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
        let encrypted = fec_decode(fec_payload).map_err(|e| format!("FEC failed: {e}"))?;
        let plaintext = decrypt_packet(key, &encrypted).map_err(|e| format!("Crypto failed: {e}"))?;

        if plaintext.is_empty() {
            return Err("Empty packet".into());
        }
        match plaintext[0] {
            0x01 => {
                let text = String::from_utf8_lossy(&plaintext[1..]).to_string();
                Ok(DecodedPacket::Text(text))
            }
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
/// Continuously paints ambient acoustic static; when armed with a decrypted frame nonce
/// and locked onto 750Hz/1000Hz sync tones, unscrambles full-color pixels in real time!
pub struct RgbSstvReceiver {
    sample_rate: u32,
    px_samples: usize,
    buf: Vec<f32>,

    pub canvas_rgb: Vec<u8>, // 48 * 48 * 3
    plan: RgbScramblePlan,
    tx_row: usize,
    sub_col: usize, // 0 .. (RGB_WIDTH * 3)
    in_frame_sync: bool,
    in_line_sync: bool,
    just_reset_frame: bool,
    noise_seed: u32,
}

impl RgbSstvReceiver {
    pub fn new(sample_rate: u32, key: &MasterKey) -> Self {
        let px_samples = (0.006 * sample_rate as f32) as usize; // matches 6ms TX sub-pixel
        Self {
            sample_rate,
            px_samples,
            buf: Vec::with_capacity(px_samples * 2),
            canvas_rgb: vec![0u8; RGB_WIDTH * RGB_HEIGHT * 3],
            // Default dummy plan until an encrypted 0x03 nonce arrives
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

    /// Processes mic PCM and returns `true` whenever pixels updated.
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

        // Scan 1400..2800 Hz in 50 Hz steps (29 bins) with parabolic peak refinement
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

        // 1. Frame Sync (750 Hz)
        if p_frame > 1e-4 && p_frame > best_p * 3.0 && p_frame > p_line * 2.0 {
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

        // 2. Line Sync (1000 Hz)
        if p_line > 1e-4 && p_line > best_p * 3.0 {
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

        // 3. Measure pixel brightness (or render organic acoustic static if silent)
        let norm_val: f32 = if best_p < 2e-5 {
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

        // 4. Unscramble (row, channel, pixel X, and polarity) using secret plan
        let row_idx = self.tx_row % RGB_HEIGHT;
        let sub_idx = self.sub_col % (RGB_WIDTH * 3);
        let ch_slot = sub_idx / RGB_WIDTH;
        let px_slot = sub_idx % RGB_WIDTH;

        let actual_y = self.plan.row_order[row_idx];
        let actual_c = self.plan.channel_orders[row_idx][ch_slot];
        let actual_x = self.plan.pixel_orders[row_idx][px_slot];
        let inverted = self.plan.invert_mask[row_idx][sub_idx];

        let unscrambled = if inverted { 1.0 - norm_val } else { norm_val };
        let byte_val = (unscrambled.clamp(0.0, 1.0) * 255.0) as u8;

        self.canvas_rgb[(actual_y * RGB_WIDTH + actual_x) * 3 + actual_c] = byte_val;

        self.sub_col += 1;
        if self.sub_col >= RGB_WIDTH * 3 {
            self.sub_col = 0;
            self.tx_row = (self.tx_row + 1) % RGB_HEIGHT;
        }
    }
}
