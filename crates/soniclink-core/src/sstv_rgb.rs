use crate::crypto::{CryptoScrambler, MasterKey};
use std::f32::consts::PI;

pub const RGB_WIDTH: usize = 48;
pub const RGB_HEIGHT: usize = 48;
pub const FREQ_FRAME_SYNC: f32 = 750.0;
pub const FREQ_LINE_SYNC: f32 = 1000.0;
pub const FREQ_BLACK: f32 = 1400.0;
pub const FREQ_WHITE: f32 = 2800.0;

/// Precomputed cryptographic scramble plan for one 48x48 RGB frame.
/// Both TX and RX generate the exact same plan from (MasterKey, frame_nonce).
pub struct RgbScramblePlan {
    pub row_order: Vec<usize>,              // Shuffled row transmission order
    pub channel_orders: Vec<[usize; 3]>,    // Per-row R/G/B permutation
    pub pixel_orders: Vec<Vec<usize>>,      // Per-row horizontal pixel permutation
    pub invert_mask: Vec<Vec<bool>>,        // Per-pixel pitch polarity inversion
}

impl RgbScramblePlan {
    pub fn generate(key: &MasterKey, frame_nonce: &[u8; 12]) -> Self {
        let mut rng = CryptoScrambler::new(key, frame_nonce);
        let row_order = rng.permutation(RGB_HEIGHT);

        let mut channel_orders = Vec::with_capacity(RGB_HEIGHT);
        let mut pixel_orders = Vec::with_capacity(RGB_HEIGHT);
        let mut invert_mask = Vec::with_capacity(RGB_HEIGHT);

        for _ in 0..RGB_HEIGHT {
            let ch = rng.permutation(3);
            channel_orders.push([ch[0], ch[1], ch[2]]);
            pixel_orders.push(rng.permutation(RGB_WIDTH));

            let mut inv = Vec::with_capacity(RGB_WIDTH * 3);
            for _ in 0..(RGB_WIDTH * 3) {
                inv.push(rng.next_bool());
            }
            invert_mask.push(inv);
        }

        Self { row_order, channel_orders, pixel_orders, invert_mask }
    }
}

/// Modulates a 48x48 RGB image (len = 48*48*3) into encrypted Martin-M1-style
/// sequential R/G/B analog scanlines. Without `key`, it renders as pure rainbow static.
pub fn modulate_encrypted_rgb_sstv(
    rgb_pixels: &[u8],
    key: &MasterKey,
    frame_nonce: &[u8; 12],
    sample_rate: u32,
) -> Vec<f32> {
    assert_eq!(rgb_pixels.len(), RGB_WIDTH * RGB_HEIGHT * 3);
    let plan = RgbScramblePlan::generate(key, frame_nonce);

    let px_samples = (0.006 * sample_rate as f32) as usize;     // 6ms per color sub-pixel
    let line_sync_s = (0.025 * sample_rate as f32) as usize;    // 25ms line sync
    let frame_sync_s = (0.160 * sample_rate as f32) as usize;   // 160ms frame sync

    let mut pcm = Vec::new();
    let mut phase: f32 = 0.0;
    let two_pi_over_sr = 2.0 * PI / sample_rate as f32;

    let mut push_tone = |freq: f32, count: usize| {
        let inc = freq * two_pi_over_sr;
        for _ in 0..count {
            pcm.push(phase.sin() * 0.65);
            phase = (phase + inc) % (2.0 * PI);
        }
    };

    // Frame Start Sync Pulse
    push_tone(FREQ_FRAME_SYNC, frame_sync_s);

    // Transmit rows in cryptographically shuffled order
    for (tx_row_idx, &actual_y) in plan.row_order.iter().enumerate() {
        push_tone(FREQ_LINE_SYNC, line_sync_s);

        let ch_order = plan.channel_orders[tx_row_idx];
        let px_order = &plan.pixel_orders[tx_row_idx];
        let inv_row = &plan.invert_mask[tx_row_idx];

        let mut inv_cursor = 0;
        for &c in &ch_order {
            for &actual_x in px_order {
                let raw = rgb_pixels[(actual_y * RGB_WIDTH + actual_x) * 3 + c] as f32 / 255.0;
                let scrambled_val = if inv_row[inv_cursor] { 1.0 - raw } else { raw };
                inv_cursor += 1;

                let freq = FREQ_BLACK + scrambled_val * (FREQ_WHITE - FREQ_BLACK);
                push_tone(freq, px_samples);
            }
        }
    }
    pcm
}