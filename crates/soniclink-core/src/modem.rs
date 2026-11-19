use crate::crypto::{CryptoScrambler, MasterKey};
use std::f32::consts::PI;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioBand {
    /// Audible sci-fi chirps (1.0 kHz – 3.0 kHz): longest range, works on all mics.
    Audible,
    /// Near-silent Ultrasonic (18.0 kHz – 19.9 kHz): inaudible to humans, requires 48kHz hardware.
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
        100.0 // 19 tones = 1.9 kHz total bandwidth (18.0..19.8 kHz in ultrasonic)
    }

    pub fn symbol_duration_sec(&self) -> f32 {
        match self {
            AudioBand::Audible => 0.060,    // ~8.3 bytes/sec
            AudioBand::Ultrasonic => 0.075, // slightly longer for ultrasonic SNR
        }
    }
}

pub const SYM_REPEAT: usize = 16;
pub const SYM_START: usize = 17;
pub const SYM_END: usize = 18;

/// Generates continuous-phase FSK PCM samples (f32 in -1.0..1.0)
/// with keyed symbol-table shuffling (Frequency Hopping).
pub fn modulate_fsk(
    data: &[u8],
    key: &MasterKey,
    hop_nonce: &[u8; 12],
    band: AudioBand,
    sample_rate: u32,
) -> Vec<f32> {
    let mut scrambler = CryptoScrambler::new(key, hop_nonce);
    let sym_samples = (band.symbol_duration_sec() * sample_rate as f32) as usize;

    // Convert bytes -> clockless symbol stream (START + nibbles/REPEAT + END)
    let mut symbols = Vec::with_capacity(data.len() * 2 + 2);
    symbols.push(SYM_START);
    let mut last_sym = SYM_START;

    for &byte in data {
        for nibble in [(byte >> 4) & 0x0F, byte & 0x0F] {
            let n = nibble as usize;
            if n == last_sym {
                symbols.push(SYM_REPEAT);
                last_sym = SYM_REPEAT;
            } else {
                symbols.push(n);
                last_sym = n;
            }
        }
    }
    symbols.push(SYM_END);

    // Synthesize phase-continuous sine wave
    let mut pcm = Vec::with_capacity(symbols.len() * sym_samples);
    let mut phase: f32 = 0.0;
    let two_pi_over_sr = 2.0 * PI / sample_rate as f32;

    for (idx, &sym) in symbols.iter().enumerate() {
        // Data symbols 0..15 hop frequencies cryptographically per step;
        // START/END/REPEAT stay fixed so receiver can acquire sync lock cleanly.
        let slot = if sym < 16 {
            let hop_table = scrambler.permutation(16);
            hop_table[sym]
        } else {
            sym
        };

        let freq = band.base_freq() + (slot as f32) * band.spacing_hz();
        let phase_inc = freq * two_pi_over_sr;

        for s in 0..sym_samples {
            // 3ms ramp on very first and very last symbol to prevent speaker pops
            let mut amp = 0.65;
            if idx == 0 && s < 150 {
                amp *= s as f32 / 150.0;
            } else if idx == symbols.len() - 1 && s > sym_samples.saturating_sub(150) {
                amp *= (sym_samples - s) as f32 / 150.0;
            }
            pcm.push(phase.sin() * amp);
            phase = (phase + phase_inc) % (2.0 * PI);
        }
    }
    pcm
}

/// Fast, zero-allocation Goertzel filter to measure exact power at a target frequency.
pub fn goertzel_power(samples: &[f32], target_freq: f32, sample_rate: u32) -> f32 {
    let n = samples.len() as f32;
    let k = (0.5 + (n * target_freq) / sample_rate as f32).floor();
    let omega = (2.0 * PI * k) / n;
    let coeff = 2.0 * omega.cos();

    let (mut q0, mut q1, mut q2) = (0.0f32, 0.0f32, 0.0f32);
    for &sample in samples {
        q0 = coeff * q1 - q2 + sample;
        q2 = q1;
        q1 = q0;
    }
    (q1 * q1 + q2 * q2 - q1 * q2 * coeff).max(0.0) / (n * n)
}