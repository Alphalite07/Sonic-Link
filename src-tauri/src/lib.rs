mod audio_io;

use audio_io::{AudioCommand, AudioEngineHandle, RxMode};
use rand::{rngs::OsRng, RngCore};
use soniclink_core::crypto::{encrypt_packet, KeyExchangeSession, MasterKey};
use soniclink_core::drawing::StrokePoint;
use soniclink_core::fec::fec_encode;
use soniclink_core::modem::{modulate_fsk, AudioBand};
use soniclink_core::rx::FHSS_HOP_NONCE;
use soniclink_core::sstv_rgb::modulate_encrypted_rgb_sstv;
use std::sync::Mutex;
use tauri::{Manager, State};

struct AppState {
    key: Mutex<MasterKey>,
    kx: Mutex<Option<KeyExchangeSession>>,
    band: Mutex<AudioBand>,
    audio: AudioEngineHandle,
}

#[tauri::command]
fn set_passphrase(passphrase: String, salt: String, state: State<AppState>) -> Result<String, String> {
    let new_key = MasterKey::from_passphrase(&passphrase, salt.as_bytes()).map_err(|e| e.to_string())?;
    let fingerprint = new_key.safety_fingerprint();
    *state.key.lock().unwrap() = new_key.clone();
    state.audio.send(AudioCommand::UpdateKey(new_key))?;
    Ok(fingerprint)
}

#[tauri::command]
fn generate_psk(state: State<AppState>) -> Result<String, String> {
    let new_key = MasterKey::generate_random();
    let hex_key = hex::encode(new_key.as_bytes());
    *state.key.lock().unwrap() = new_key.clone();
    state.audio.send(AudioCommand::UpdateKey(new_key))?;
    Ok(hex_key)
}

#[tauri::command]
fn import_psk(hex_key: String, state: State<AppState>) -> Result<String, String> {
    let bytes = hex::decode(hex_key.trim()).map_err(|_| "Invalid hex key")?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| "Key must be 64 hex chars (256-bit)")?;
    let new_key = MasterKey::from_raw(arr);
    let fingerprint = new_key.safety_fingerprint();
    *state.key.lock().unwrap() = new_key.clone();
    state.audio.send(AudioCommand::UpdateKey(new_key))?;
    Ok(fingerprint)
}

#[tauri::command]
fn init_x25519(state: State<AppState>) -> Result<String, String> {
    let session = KeyExchangeSession::new();
    let qr = session.qr_payload();
    *state.kx.lock().unwrap() = Some(session);
    Ok(qr)
}

#[tauri::command]
fn complete_x25519(peer_qr: String, state: State<AppState>) -> Result<String, String> {
    let session = state
        .kx
        .lock()
        .unwrap()
        .take()
        .ok_or("Click 'X25519 QR Pair' first to generate your keypair")?;
    let (new_key, fp) = session
        .derive_session_key(&peer_qr)
        .map_err(|e| e.to_string())?;
    *state.key.lock().unwrap() = new_key.clone();
    state.audio.send(AudioCommand::UpdateKey(new_key))?;
    Ok(fp)
}

#[tauri::command]
fn set_band(ultrasonic: bool, state: State<AppState>) -> Result<(), String> {
    let band = if ultrasonic { AudioBand::Ultrasonic } else { AudioBand::Audible };
    *state.band.lock().unwrap() = band;
    state.audio.send(AudioCommand::SetBand(band))
}

#[tauri::command]
fn set_rx_mode(mode: String, state: State<AppState>) -> Result<(), String> {
    let m = if mode == "analog" { RxMode::AnalogRgb } else { RxMode::Digital };
    state.audio.send(AudioCommand::SetMode(m))
}

#[tauri::command]
fn set_loopback(enabled: bool, state: State<AppState>) -> Result<(), String> {
    state.audio.send(AudioCommand::SetLoopback(enabled))
}

#[tauri::command]
fn toggle_mic(enable: bool, state: State<AppState>) -> Result<(), String> {
    let cmd = if enable { AudioCommand::StartMic } else { AudioCommand::StopMic };
    state.audio.send(cmd)
}

fn transmit_encrypted_digital(payload: &[u8], state: &State<AppState>) -> Result<usize, String> {
    let key = state.key.lock().unwrap().clone();
    let band = *state.band.lock().unwrap();
    let encrypted = encrypt_packet(&key, payload).map_err(|e| e.to_string())?;
    let fec_bytes = fec_encode(&encrypted).map_err(|e| e.to_string())?;
    let pcm = modulate_fsk(&fec_bytes, &key, &FHSS_HOP_NONCE, band, state.audio.sample_rate);
    let len = fec_bytes.len();
    state.audio.send(AudioCommand::PlayPcm(pcm))?;
    Ok(len)
}

#[tauri::command]
fn stop_tx(state: State<AppState>) -> Result<(), String> {
    state.audio.send(AudioCommand::StopTx)
}

#[tauri::command]
fn send_encrypted_text(text: String, state: State<AppState>) -> Result<usize, String> {
    let mut payload = Vec::with_capacity(1 + text.len());
    payload.push(0x01); // Text / ASCII type
    payload.extend_from_slice(text.as_bytes());
    transmit_encrypted_digital(&payload, &state)
}

#[tauri::command]
fn send_drawing_strokes(strokes: Vec<StrokePoint>, state: State<AppState>) -> Result<usize, String> {
    let mut payload = Vec::with_capacity(1 + strokes.len() * 3);
    payload.push(0x02); // Live Drawing Burst type
    for pt in strokes {
        payload.extend_from_slice(&pt.pack());
    }
    transmit_encrypted_digital(&payload, &state)
}

#[tauri::command]
fn send_encrypted_rgb_image(rgb_bytes: Vec<u8>, state: State<AppState>) -> Result<(), String> {
    let key = state.key.lock().unwrap().clone();
    let band = *state.band.lock().unwrap();
    let sr = state.audio.sample_rate;

    // 1. Generate fresh 12-byte random frame nonce & send it encrypted via FSK+FEC
    let mut frame_nonce = [0u8; 12];
    OsRng.fill_bytes(&mut frame_nonce);
    let mut header_payload = Vec::with_capacity(13);
    header_payload.push(0x03); // RGB Nonce Sync type
    header_payload.extend_from_slice(&frame_nonce);

    let enc_hdr = encrypt_packet(&key, &header_payload).map_err(|e| e.to_string())?;
    let fec_hdr = fec_encode(&enc_hdr).map_err(|e| e.to_string())?;
    let mut pcm = modulate_fsk(&fec_hdr, &key, &FHSS_HOP_NONCE, band, sr);

    // 2. 120ms guard silence so RX decrypts nonce & arms RGB scrambler
    pcm.extend(vec![0.0f32; (0.120 * sr as f32) as usize]);

    // 3. Append full-color cryptographically scrambled RGB scanlines
    let sstv_pcm = modulate_encrypted_rgb_sstv(&rgb_bytes, &key, &frame_nonce, sr);
    pcm.extend(sstv_pcm);
    state.audio.send(AudioCommand::StopTx)?;
    state.audio.send(AudioCommand::PlayPcm(pcm))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let default_key = MasterKey::from_passphrase("SONICLINK-DEFAULT", b"TACTICAL-ROOM-01")
                .expect("default key");
            let audio = AudioEngineHandle::spawn(app.handle().clone(), default_key.clone());
            app.manage(AppState {
                key: Mutex::new(default_key),
                kx: Mutex::new(None),
                band: Mutex::new(AudioBand::Audible),
                audio,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            set_passphrase,
            generate_psk,
            import_psk,
            init_x25519,
            complete_x25519,
            set_band,
            set_rx_mode,
            set_loopback,
            toggle_mic,
            send_encrypted_text,
            send_drawing_strokes,
            send_encrypted_rgb_image,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}