use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use soniclink_core::crypto::MasterKey;
use soniclink_core::modem::AudioBand;
use soniclink_core::rx::{DecodedPacket, FskReceiver, RgbSstvReceiver};
use std::collections::VecDeque;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

pub enum AudioCommand {
    PlayPcm(Vec<f32>),
    StopTx,
    StartMic,
    StopMic,
    SetBand(AudioBand),
    SetMode(RxMode),
    SetLoopback(bool),
    UpdateKey(MasterKey),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RxMode {
    Digital,
    AnalogRgb,
}

#[derive(Clone)]
pub struct AudioEngineHandle {
    tx: mpsc::Sender<AudioCommand>,
    pub sample_rate: u32,
}

impl AudioEngineHandle {
    pub fn spawn(app: AppHandle, initial_key: MasterKey) -> Self {
        let host = cpal::default_host();
        let default_sr = host
            .default_output_device()
            .and_then(|d| d.default_output_config().ok())
            .map(|c| c.sample_rate().0)
            .unwrap_or(48_000);

        let (tx, rx) = mpsc::channel::<AudioCommand>();

        thread::spawn(move || {
            run_audio_thread(app, rx, initial_key);
        });

        Self { tx, sample_rate: default_sr }
    }

    pub fn send(&self, cmd: AudioCommand) -> Result<(), String> {
        self.tx.send(cmd).map_err(|e| format!("Audio thread offline: {e}"))
    }
}

fn run_audio_thread(app: AppHandle, cmd_rx: mpsc::Receiver<AudioCommand>, initial_key: MasterKey) {
    let host = cpal::default_host();

    let play_queue: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
    let _out_stream = start_output_stream(&host, play_queue.clone());

    let shared_key = Arc::new(Mutex::new(initial_key));
    let shared_band = Arc::new(Mutex::new(AudioBand::Audible));
    let shared_mode = Arc::new(Mutex::new(RxMode::Digital));
    let mut in_stream: Option<cpal::Stream> = None;

    let (mic_tx, mic_rx) = mpsc::channel::<Vec<f32>>();
    let mut fsk_rx: Option<FskReceiver> = None;
    let mut rgb_rx: Option<RgbSstvReceiver> = None;
    let mut last_rgb_emit = Instant::now();

    let mut loopback_mode = false;
    let mut last_tx_active: Option<Instant> = None;
    let mut was_self_muted = false;

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                AudioCommand::PlayPcm(samples) => {
                    if let Ok(mut q) = play_queue.lock() {
                        q.extend(samples);
                    }
                }
                AudioCommand::SetBand(b) => {
                    *shared_band.lock().unwrap() = b;
                    if let Some(rx) = fsk_rx.as_mut() {
                        rx.set_band(b);
                    }
                }
                AudioCommand::SetMode(m) => {
                    *shared_mode.lock().unwrap() = m;
                }
                AudioCommand::SetLoopback(enabled) => {
                    loopback_mode = enabled;
                }
                AudioCommand::UpdateKey(k) => {
                    *shared_key.lock().unwrap() = k;
                }
                AudioCommand::StartMic => {
                    if in_stream.is_none() {
                        if let Some((stream, sr)) = start_input_stream(&host, mic_tx.clone()) {
                            let k = shared_key.lock().unwrap().clone();
                            let b = *shared_band.lock().unwrap();
                            fsk_rx = Some(FskReceiver::new(sr, b));
                            rgb_rx = Some(RgbSstvReceiver::new(sr, &k));
                            in_stream = Some(stream);
                            let _ = app.emit("mic-status", format!("Listening ({sr} Hz)"));
                        } else {
                            let _ = app.emit("mic-status", "Mic failed to open");
                        }
                    }
                }
                AudioCommand::StopTx => {
                    if let Ok(mut q) = play_queue.lock() {
                        q.clear();
                    }
                    last_tx_active = None;
                    was_self_muted = false;
                    let _ = app.emit("tx-mute-state", false);
                }
                AudioCommand::StopMic => {
                    in_stream = None;
                    while mic_rx.try_recv().is_ok() {} // flush any buffered mic static
                    let _ = app.emit("mic-status", "Mic Off");
                }
            }
        }

        // Acoustic Echo Self-Mute: pause mic while TX is playing + 320ms room reverb tail
        let tx_active = !play_queue.lock().unwrap().is_empty();
        if tx_active {
            last_tx_active = Some(Instant::now());
        }
        let self_muted = !loopback_mode
            && last_tx_active.map_or(false, |t| t.elapsed() < Duration::from_millis(320));

        if self_muted != was_self_muted {
            was_self_muted = self_muted;
            let _ = app.emit("tx-mute-state", self_muted);
            if self_muted {
                if let Some(fsk) = fsk_rx.as_mut() {
                    fsk.reset_sync();
                }
            }
        }

        while let Ok(mono_samples) = mic_rx.try_recv() {
            if self_muted {
                continue;
            }
            let key = shared_key.lock().unwrap().clone();
            let mode = *shared_mode.lock().unwrap();

            if let Some(fsk) = fsk_rx.as_mut() {
                for res in fsk.push_samples(&mono_samples, &key) {
                    match res {
                        Ok(DecodedPacket::Text(text)) => {
                            let _ = app.emit("rx-text", text);
                        }
                        Ok(DecodedPacket::DrawingStrokes(pts)) => {
                            let _ = app.emit("rx-drawing", pts);
                        }
                        Ok(DecodedPacket::RgbFrameNonce(nonce)) => {
                            if let Some(rgb) = rgb_rx.as_mut() {
                                rgb.arm_with_nonce(&key, &nonce);
                                let _ = app.emit("rx-status", "Encrypted RGB Frame Nonce Authenticated!");
                            }
                        }
                        Err(err) => {
                            let _ = app.emit("rx-error", err);
                        }
                    }
                }
            }

            if mode == RxMode::AnalogRgb {
                if let Some(rgb) = rgb_rx.as_mut() {
                    if rgb.push_samples(&mono_samples) && last_rgb_emit.elapsed() >= Duration::from_millis(50) {
                        let _ = app.emit("rx-rgb-frame", &rgb.canvas_rgb);
                        last_rgb_emit = Instant::now();
                    }
                }
            }
        }

        thread::sleep(Duration::from_millis(5));
    }
}

fn start_output_stream(host: &cpal::Host, queue: Arc<Mutex<VecDeque<f32>>>) -> Option<cpal::Stream> {
    let device = host.default_output_device()?;
    let config = device.default_output_config().ok()?;
    let channels = config.channels() as usize;

    let stream = match config.sample_format() {
        SampleFormat::F32 => device.build_output_stream(
            &config.into(),
            move |data: &mut [f32], _| {
                let mut q = queue.lock().unwrap();
                for frame in data.chunks_mut(channels) {
                    let s = q.pop_front().unwrap_or(0.0);
                    for ch in frame.iter_mut() { *ch = s; }
                }
            },
            |err| eprintln!("Audio output error: {err}"),
            None,
        ),
        SampleFormat::I16 => device.build_output_stream(
            &config.into(),
            move |data: &mut [i16], _| {
                let mut q = queue.lock().unwrap();
                for frame in data.chunks_mut(channels) {
                    let s = (q.pop_front().unwrap_or(0.0).clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    for ch in frame.iter_mut() { *ch = s; }
                }
            },
            |err| eprintln!("Audio output error: {err}"),
            None,
        ),
        _ => return None,
    }.ok()?;

    stream.play().ok()?;
    Some(stream)
}

fn start_input_stream(host: &cpal::Host, tx: mpsc::Sender<Vec<f32>>) -> Option<(cpal::Stream, u32)> {
    let device = host.default_input_device()?;
    let config = device.default_input_config().ok()?;
    let sr = config.sample_rate().0;
    let channels = config.channels() as usize;

    let stream = match config.sample_format() {
        SampleFormat::F32 => device.build_input_stream(
            &config.into(),
            move |data: &[f32], _| {
                let mono: Vec<f32> = data.chunks(channels).map(|f| f[0]).collect();
                let _ = tx.send(mono);
            },
            |err| eprintln!("Mic input error: {err}"),
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            &config.into(),
            move |data: &[i16], _| {
                let mono: Vec<f32> = data.chunks(channels).map(|f| f[0] as f32 / i16::MAX as f32).collect();
                let _ = tx.send(mono);
            },
            |err| eprintln!("Mic input error: {err}"),
            None,
        ),
        _ => return None,
    }.ok()?;

    stream.play().ok()?;
    Some((stream, sr))
}