use crate::crypto::{CryptoScrambler, MasterKey};
use std::f32::consts::PI;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioBand {
    /// Audible chirps (1.1 kHz – 2.9 kHz): longest range, works on all speakers/mics.
    Audible,
    /// Near-silent Ultrasonic (18.0 kHz – 19.8 kHz): requires 48kHz hardware.
    Ultrasonic,
}

impl AudioBand {
    pub fn base_freq(&self) -> f32 {
        match self {
            AudioBand::Audible => 1100.0,
            AudioBand::Ultrasonic => 18000.0,
        }
    }

    pub fn spacing_hz(&self) -> f32 {
        100.0 // 19 distinct tones across 1.8 kHz bandwidth
    }

    pub fn symbol_duration_sec(&self) -> f32 {
        match self {
            AudioBand::Audible => 0.080,    // 80ms per nibble (very reliable over air)
            AudioBand::Ultrasonic => 0.095, // slightly longer for ultrasonic SNR
        }
    }
}

pub const SYM_REPEAT: usize = 16;
pub const SYM_START: usize = 17;
pub const SYM_END: usize = 18;

/// Derives a secret, bijective 16-tone frequency map from MasterKey.
/// Guarantees zero collisions between adjacent symbols while hiding the tone map.
pub fn keyed_hop_table(key: &MasterKey, hop_nonce: &[u8; 12]) -> [usize; 16] {
    let mut scrambler = CryptoScrambler::new(key, hop_nonce);
    let perm = scrambler.permutation(16);
    let mut table = [0usize; 16];
    table.copy_from_slice(&perm);
    table
}

/// Generates continuous-phase FSK PCM samples (f32 in -1.0..1.0)
/// with keyed tone scrambling and long START/END preambles.
pub fn modulate_fsk(
    data: &[u8],
    key: &MasterKey,
    hop_nonce: &[u8; 12],
    band: AudioBand,
    sample_rate: u32,
) -> Vec<f32> {
    let hop_table = keyed_hop_table(key, hop_nonce);
    let sym_samples = (band.symbol_duration_sec() * sample_rate as f32) as usize;

    // Build clockless symbol stream: START + nibbles (with REPEAT for duplicates) + END
    let mut symbols: Vec<(usize, usize)> = Vec::with_capacity(data.len() * 2 + 2);
    // 2.5x duration START tone so receiver locks reliably
    symbols.push((SYM_START, (sym_samples * 5) / 2));

    let mut last_sym = SYM_START;
    for &byte in data {
        for nibble in [(byte >> 4) & 0x0F, byte & 0x0F] {
            let n = nibble as usize;
            if n == last_sym {
                symbols.push((SYM_REPEAT, sym_samples));
                last_sym = SYM_REPEAT;
            } else {
                symbols.push((n, sym_samples));
                last_sym = n;
            }
        }
    }
    // 2x duration END tone
    symbols.push((SYM_END, sym_samples * 2));

    let total_samples: usize = symbols.iter().map(|&(_, len)| len).sum();
    let mut pcm = Vec::with_capacity(total_samples);
    let mut phase: f32 = 0.0;
    let two_pi_over_sr = 2.0 * PI / sample_rate as f32;

    for (idx, &(sym, count)) in symbols.iter().enumerate() {
        let slot = if sym < 16 { hop_table[sym] } else { sym };
        let freq = band.base_freq() + (slot as f32) * band.spacing_hz();
        let phase_inc = freq * two_pi_over_sr;

        for s in 0..count {
            let mut amp = 0.75;
            if idx == 0 && s < 200 {
                amp *= s as f32 / 200.0;
            } else if idx == symbols.len() - 1 && s > count.saturating_sub(200) {
                amp *= (count - s) as f32 / 200.0;
            }
            pcm.push(phase.sin() * amp);
            phase = (phase + phase_inc) % (2.0 * PI);
        }
    }
    pcm
}

/// Exact Hann-windowed Goertzel power detector (clean sidelobe rejection at any sample rate).
pub fn goertzel_power(samples: &[f32], target_freq: f32, sample_rate: u32) -> f32 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let nf = n as f32;
    let omega = 2.0 * PI * target_freq / sample_rate as f32;
    let coeff = 2.0 * omega.cos();
    let hann_step = 2.0 * PI / (nf - 1.0).max(1.0);

    let (mut q1, mut q2) = (0.0f32, 0.0f32);
    for (i, &sample) in samples.iter().enumerate() {
        let w = 0.5 * (1.0 - (hann_step * i as f32).cos());
        let q0 = coeff * q1 - q2 + (sample * w);
        q2 = q1;
        q1 = q0;
    }
    (q1 * q1 + q2 * q2 - q1 * q2 * coeff).max(0.0) / (nf * nf)
}