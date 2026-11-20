use argon2::Argon2;
use chacha20::cipher::{KeyIvInit, StreamCipher};
use chacha20::ChaCha20;
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    Key, XChaCha20Poly1305, XNonce,
};
use rand::{rngs::OsRng, RngCore};
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Error, Debug)]
pub enum CryptoError {
    #[error("Key derivation failed")]
    KdfFailed,
    #[error("Encryption failed")]
    EncryptFailed,
    #[error("Decryption/Authentication failed (wrong key or tampered audio)")]
    DecryptFailed,
}

/// 256-bit master key automatically wiped from RAM when dropped.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct MasterKey([u8; 32]);

impl MasterKey {
    /// Derive a 256-bit key from a user passphrase + shared room salt using Argon2id.
    pub fn from_passphrase(passphrase: &str, room_salt: &[u8]) -> Result<Self, CryptoError> {
        let mut key = [0u8; 32];
        // Pad or hash salt to at least 16 bytes for Argon2id
        let mut salt = [0u8; 16];
        for (i, b) in room_salt.iter().take(16).enumerate() {
            salt[i] = *b;
        }
        Argon2::default()
            .hash_password_into(passphrase.as_bytes(), &salt, &mut key)
            .map_err(|_| CryptoError::KdfFailed)?;
        Ok(Self(key))
    }

    /// Generate a fresh cryptographically random 256-bit PSK (for QR code pairing).
    pub fn generate_random() -> Self {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        Self(key)
    }

    pub fn from_raw(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Encrypts digital payloads with XChaCha20-Poly1305.
/// Output format: [24-byte random XNonce] || [Ciphertext + 16-byte Poly1305 Tag]
pub fn encrypt_packet(key: &MasterKey, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_bytes()));
    let mut nonce_bytes = [0u8; 24];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| CryptoError::EncryptFailed)?;

    let mut out = Vec::with_capacity(24 + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Verifies authenticity and decrypts an XChaCha20-Poly1305 packet.
pub fn decrypt_packet(key: &MasterKey, packet: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if packet.len() < 24 + 16 {
        return Err(CryptoError::DecryptFailed);
    }
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_bytes()));
    let nonce = XNonce::from_slice(&packet[..24]);
    let ciphertext = &packet[24..];

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| CryptoError::DecryptFailed)
}

/// Deterministic cryptographic PRNG backed by ChaCha20.
/// Used for BOTH Frequency-Hopping symbol tables AND Analog RGB Scanline Scrambling.
pub struct CryptoScrambler {
    cipher: ChaCha20,
}

impl CryptoScrambler {
    pub fn new(key: &MasterKey, nonce12: &[u8; 12]) -> Self {
        let cipher = ChaCha20::new(key.as_bytes().into(), nonce12.into());
        Self { cipher }
    }

    pub fn next_u32(&mut self) -> u32 {
        let mut buf = [0u8; 4];
        self.cipher.apply_keystream(&mut buf);
        u32::from_le_bytes(buf)
    }

    pub fn next_bool(&mut self) -> bool {
        (self.next_u32() & 1) == 1
    }

    /// Cryptographically secure Fisher-Yates shuffle of `0..len`.
    pub fn permutation(&mut self, len: usize) -> Vec<usize> {
        let mut order: Vec<usize> = (0..len).collect();
        for i in (1..len).rev() {
            let j = (self.next_u32() as usize) % (i + 1);
            order.swap(i, j);
        }
        order
    }
}

use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

pub const QR_PREFIX: &str = "soniclink://x25519/";

/// Ephemeral X25519 pairing session (secret automatically zeroized on drop).
pub struct KeyExchangeSession {
    secret: StaticSecret,
    public: PublicKey,
}

impl KeyExchangeSession {
    pub fn new() -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Returns QR-ready payload: "soniclink://x25519/<64-hex-chars>"
    pub fn qr_payload(&self) -> String {
        format!("{QR_PREFIX}{}", hex::encode(self.public.as_bytes()))
    }

    /// Completes X25519 ECDH + HKDF-SHA256 using the scanned peer QR / hex public key.
    /// Binds both public keys into the HKDF salt so Alice & Bob always derive the exact
    /// same 256-bit MasterKey and matching 8-char visual verification code.
    pub fn derive_session_key(&self, peer_input: &str) -> Result<(MasterKey, String), CryptoError> {
        let cleaned = peer_input.trim().strip_prefix(QR_PREFIX).unwrap_or(peer_input.trim());
        let peer_bytes = hex::decode(cleaned).map_err(|_| CryptoError::KdfFailed)?;
        let peer_arr: [u8; 32] = peer_bytes.try_into().map_err(|_| CryptoError::KdfFailed)?;
        let peer_pub = PublicKey::from(peer_arr);

        let shared = self.secret.diffie_hellman(&peer_pub);

        // Order-independent salt = min(pubA, pubB) || max(pubA, pubB)
        let my_bytes = self.public.as_bytes();
        let mut salt = [0u8; 64];
        if my_bytes <= &peer_arr {
            salt[..32].copy_from_slice(my_bytes);
            salt[32..].copy_from_slice(&peer_arr);
        } else {
            salt[..32].copy_from_slice(&peer_arr);
            salt[32..].copy_from_slice(my_bytes);
        }

        let hk = Hkdf::<Sha256>::new(Some(&salt), shared.as_bytes());
        let mut okm = [0u8; 32];
        hk.expand(b"SONICLINK-X25519-SESSION-V1", &mut okm)
            .map_err(|_| CryptoError::KdfFailed)?;

        let master = MasterKey::from_raw(okm);
        let code = master.safety_fingerprint();
        Ok((master, code))
    }
}

impl MasterKey {
    /// Short human-verifiable fingerprint (SHA-256 hash — never leaks raw key bytes).
    pub fn safety_fingerprint(&self) -> String {
        let hash = Sha256::digest(self.as_bytes());
        let h = hex::encode(&hash[..4]).to_uppercase();
        format!("{}-{}", &h[..4], &h[4..8])
    }
}