use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{info, warn};

/// The realtime session expects 24 kHz mono PCM16. Capture devices rarely
/// offer that natively (WASAPI shared mode is pinned to the mixer format,
/// usually 48 kHz), so we open the device at *its* format and resample here.
pub const TARGET_SAMPLE_RATE: u32 = 24_000;

const INIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Capture gain comes from config (`mic_gain_db`). USB/laptop microphones
/// routinely deliver -40 dBFS speech, which the realtime VAD never fires on —
/// verified live: both this machine's USB mic and Realtek array produced
/// speech at that level and got zero transcription. Rust-side AGC fixes it
/// without asking the user to dig through Windows sound control panels.
fn capture_gain() -> f32 {
    10f32.powf(crate::config::get().mic_gain_db / 20.0)
}

/// Apply gain with a clip ceiling. Returns true if clipping occurred — the
/// caller halves the gain so loud input stays clean.
fn apply_gain(samples: &mut [f32], gain: f32) -> bool {
    let mut clipped = false;
    for s in samples.iter_mut() {
        let v = *s * gain;
        if v.abs() > 0.98 {
            clipped = true;
        }
        *s = v.clamp(-0.98, 0.98);
    }
    clipped
}

/// Linear resampler with cross-block continuity.
///
/// `pos` is the read cursor in source-sample units, relative to the start of the
/// current input block. It can be negative, in which case index `-1` refers to
/// `prev` — the last sample of the *previous* block — so blocks stitch together
/// without a discontinuity at the seam.
pub struct Resampler {
    ratio: f64,
    pos: f64,
    prev: f32,
}

impl Resampler {
    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            ratio: src_rate as f64 / dst_rate as f64,
            pos: 0.0,
            prev: 0.0,
        }
    }

    /// Resample `input` (mono f32 in [-1, 1]) into `out` as little-endian i16.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<i16>) {
        if input.is_empty() {
            return;
        }
        let len = input.len();
        loop {
            let base = self.pos.floor();
            let frac = (self.pos - base) as f32;
            let i = base as i64;
            let s0 = match i {
                i if i < 0 => self.prev,
                i if (i as usize) < len => input[i as usize],
                _ => break,
            };
            let j = i + 1;
            let s1 = match j {
                j if j < 0 => self.prev,
                j if (j as usize) < len => input[j as usize],
                // Need one sample past the end to interpolate; wait for the next block.
                _ => break,
            };
            let v = s0 + (s1 - s0) * frac;
            out.push((v.clamp(-1.0, 1.0) * 32767.0) as i16);
            self.pos += self.ratio;
        }
        self.prev = input[len - 1];
        self.pos -= len as f64;
    }
}

/// Average interleaved frames down to mono.
fn downmix(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    if channels <= 1 {
        out.extend_from_slice(input);
        return;
    }
    for frame in input.chunks_exact(channels) {
        out.push(frame.iter().sum::<f32>() / channels as f32);
    }
}

pub struct AudioCapture {
    receiver: mpsc::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioCapture {
    /// Opens an input device and starts streaming.
    ///
    /// `device_name` is a case-insensitive substring of the cpal device name;
    /// `None` uses the system default. Unlike a fire-and-forget spawn, this
    /// waits for the capture thread to report whether the stream actually came
    /// up, so a missing microphone surfaces as `Err` instead of silent dead air.
    pub fn start(device_name: Option<&str>) -> Result<Self, String> {
        let (pcm_tx, pcm_rx) = mpsc::channel::<Vec<u8>>();
        let (init_tx, init_rx) = mpsc::channel::<Result<String, String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        // Own the name before crossing the thread boundary — a &str borrow
        // would escape the caller's stack frame.
        let device_name = device_name.map(str::to_string);

        let handle = std::thread::spawn(move || {
            capture_thread(pcm_tx, init_tx, stop_thread, device_name);
        });

        match init_rx.recv_timeout(INIT_TIMEOUT) {
            Ok(Ok(desc)) => {
                info!(config = %desc, "audio capture started");
                Ok(Self {
                    receiver: pcm_rx,
                    stop,
                    handle: Some(handle),
                })
            }
            Ok(Err(e)) => {
                stop.store(true, Ordering::SeqCst);
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                Err("audio device did not start within 5s".to_string())
            }
        }
    }

    /// Drain every buffered chunk into one contiguous PCM16LE byte vector.
    ///
    /// The pump encodes whatever is here into whole Opus frames; partial frames
    /// stay buffered in the realtime session for the next tick.
    pub fn read_pending_bytes(&self) -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        while let Ok(chunk) = self.receiver.try_recv() {
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            return None;
        }
        Some(bytes)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
            info!("audio capture stopped");
        }
    }
}

/// Names of all capturable input devices, for the settings dropdown.
pub fn list_input_devices() -> Vec<String> {
    use cpal::traits::DeviceTrait;
    let host = cpal::default_host();
    host.input_devices()
        .map(|devices| {
            devices
                .filter_map(|d| d.name().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn pick_device(
    host: &cpal::Host,
    device_name: Option<&str>,
) -> Result<cpal::Device, String> {
    match device_name {
        Some(want) if !want.trim().is_empty() => {
            let want = want.to_lowercase();
            let mut devices = host
                .input_devices()
                .map_err(|e| format!("enumerate input devices: {e}"))?;
            devices
                .find(|d| {
                    d.name()
                        .map(|n| n.to_lowercase().contains(&want))
                        .unwrap_or(false)
                })
                .ok_or_else(|| format!("no input device matching '{want}'"))
        }
        _ => host
            .default_input_device()
            .ok_or_else(|| "no default input device (microphone)".to_string()),
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn capture_thread(
    pcm_tx: mpsc::Sender<Vec<u8>>,
    init_tx: mpsc::Sender<Result<String, String>>,
    stop: Arc<AtomicBool>,
    device_name: Option<String>,
) {
    let host = cpal::default_host();
    let device = match pick_device(&host, device_name.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };

    // Use the device's own default config. Forcing a rate/channel count the
    // device does not support is the single most common cause of capture
    // failing outright on Windows.
    let supported = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => {
            let _ = init_tx.send(Err(format!("input config query failed: {e}")));
            return;
        }
    };

    let src_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let desc = format!("{src_rate}Hz x{channels} {sample_format:?} -> {TARGET_SAMPLE_RATE}Hz mono i16");

    let mut mono: Vec<f32> = Vec::new();
    let mut resampled: Vec<i16> = Vec::new();
    let mut resampler = Resampler::new(src_rate, TARGET_SAMPLE_RATE);
    let mut gain = capture_gain();

    let mut emit = move |input: &[f32], tx: &mpsc::Sender<Vec<u8>>| {
        downmix(input, channels, &mut mono);
        if apply_gain(&mut mono, gain) && gain > 1.0 {
            gain = (gain / 2.0).max(1.0);
            info!(new_gain = gain, "capture gain reduced (clipping detected)");
        }
        resampled.clear();
        resampler.process(&mono, &mut resampled);
        if resampled.is_empty() {
            return;
        }
        let bytes: Vec<u8> = resampled.iter().flat_map(|s| s.to_le_bytes()).collect();
        let _ = tx.send(bytes);
    };

    let err_fn = |e| warn!(error = %e, "audio stream error");
    let stream_result = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _: &_| emit(data, &pcm_tx),
            err_fn,
            None,
        ),
        SampleFormat::I16 => {
            let mut scratch: Vec<f32> = Vec::new();
            device.build_input_stream(
                &config,
                move |data: &[i16], _: &_| {
                    scratch.clear();
                    scratch.extend(data.iter().map(|&s| s as f32 / 32768.0));
                    emit(&scratch, &pcm_tx);
                },
                err_fn,
                None,
            )
        }
        SampleFormat::U16 => {
            let mut scratch: Vec<f32> = Vec::new();
            device.build_input_stream(
                &config,
                move |data: &[u16], _: &_| {
                    scratch.clear();
                    scratch.extend(data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0));
                    emit(&scratch, &pcm_tx);
                },
                err_fn,
                None,
            )
        }
        fmt => {
            let _ = init_tx.send(Err(format!("unsupported sample format: {fmt:?}")));
            return;
        }
    };

    let stream = match stream_result {
        Ok(s) => s,
        Err(e) => {
            let _ = init_tx.send(Err(format!("failed to open microphone: {e}")));
            return;
        }
    };

    if let Err(e) = stream.play() {
        let _ = init_tx.send(Err(format!("failed to start microphone: {e}")));
        return;
    }

    let _ = init_tx.send(Ok(desc));

    // The cpal stream must be dropped on the thread that owns it, so park here
    // until asked to stop rather than leaking the stream for the process
    // lifetime.
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = stream.pause();
    drop(stream);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_boosts_quiet_signal_without_clipping() {
        let mut quiet = vec![0.008f32; 100]; // Realtek-level speech
        assert!(!apply_gain(&mut quiet, 10.0));
        assert!(quiet.iter().all(|&s| (s - 0.08).abs() < 1e-6));
        let mut loud = vec![0.5f32; 100];
        assert!(apply_gain(&mut loud, 10.0));
        assert!(loud.iter().all(|&s| s <= 0.98));
    }

    #[test]
    fn downmix_stereo_averages_channels() {
        let interleaved = [1.0f32, 0.0, 0.5, 0.5, -1.0, 1.0];
        let mut out = Vec::new();
        downmix(&interleaved, 2, &mut out);
        assert_eq!(out, vec![0.5, 0.5, 0.0]);
    }

    #[test]
    fn downmix_mono_is_passthrough() {
        let input = [0.25f32, -0.25];
        let mut out = Vec::new();
        downmix(&input, 1, &mut out);
        assert_eq!(out, vec![0.25, -0.25]);
    }

    #[test]
    fn resampler_halves_rate_48k_to_24k() {
        let mut r = Resampler::new(48_000, 24_000);
        let input: Vec<f32> = (0..480).map(|i| i as f32 / 480.0).collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        // 2:1 decimation, minus the tail sample held back for interpolation.
        assert!((239..=240).contains(&out.len()), "got {}", out.len());
    }

    /// At a 1:1 ratio samples pass through untouched. The final sample of each
    /// block is held back — interpolation needs its successor — and is emitted
    /// once the next block arrives, so nothing is lost across the seam.
    #[test]
    fn resampler_passthrough_when_rates_match() {
        let mut r = Resampler::new(24_000, 24_000);
        let mut out = Vec::new();
        r.process(&[0.0f32, 0.5, -0.5, 1.0], &mut out);
        assert_eq!(out, vec![0, 16383, -16383]);

        out.clear();
        r.process(&[0.25f32, -0.25], &mut out);
        assert_eq!(out, vec![32767, 8191]);
    }

    /// Feeding many blocks must produce a rate close to the target, with no
    /// drift and no seam discontinuity — the property that actually determines
    /// whether transcription works.
    #[test]
    fn resampler_holds_rate_across_many_blocks() {
        let mut r = Resampler::new(44_100, 24_000);
        let mut total = 0usize;
        let block: Vec<f32> = (0..441).map(|i| (i as f32 * 0.01).sin()).collect();
        for _ in 0..100 {
            let mut out = Vec::new();
            r.process(&block, &mut out);
            total += out.len();
        }
        // 100 blocks of 10ms at 44.1kHz == 1s of audio == ~24000 samples.
        let drift = (total as i64 - 24_000).abs();
        assert!(drift <= 2, "resampled {total}, drift {drift}");
    }

    #[test]
    fn resampler_preserves_sine_amplitude() {
        let mut r = Resampler::new(48_000, 24_000);
        let input: Vec<f32> = (0..4800)
            .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin())
            .collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let peak = out.iter().map(|s| s.abs() as i32).max().unwrap();
        // A 440Hz tone survives 2:1 decimation with near-full scale intact.
        assert!(peak > 30_000, "peak {peak} — resampler is attenuating");
    }

    /// Opens the real default microphone. Off by default because CI machines
    /// have no input device; run with CODEX_MIC_AUDIO=1.
    #[test]
    fn microphone_capture_smoke() {
        if std::env::var("CODEX_MIC_AUDIO").is_err() {
            eprintln!("skipping; set CODEX_MIC_AUDIO=1 to run against a real mic");
            return;
        }
        let default = cpal::default_host()
            .default_input_device()
            .and_then(|d| d.default_input_config().ok());
        eprintln!("[mic] device default config: {default:?}");

        let mut capture = AudioCapture::start(None).expect("microphone should open");
        std::thread::sleep(Duration::from_millis(1500));
        let bytes = capture
            .read_pending_bytes()
            .expect("expected PCM within 1.5s of opening the mic");
        assert_eq!(bytes.len() % 2, 0, "PCM16 must be an even byte count");

        let pcm: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        let samples = pcm.len();
        let peak = pcm.iter().map(|s| (*s as i32).abs()).max().unwrap_or(0);
        let rms = (pcm.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / samples as f64).sqrt();
        eprintln!(
            "[mic] captured {samples} samples in ~1.5s ({:.1} Hz effective), peak {peak}, rms {rms:.1}",
            samples as f64 / 1.5
        );

        // 1.5s at 24kHz is 36000 samples; allow slack for thread scheduling.
        assert!(
            (30_000..=42_000).contains(&samples),
            "got {samples} samples in 1.5s (expected ~36000) — resampling is off-rate"
        );
        capture.stop();

        // After stop the device must actually be released: no new audio.
        std::thread::sleep(Duration::from_millis(300));
        let _ = capture.read_pending_bytes();
        assert!(
            capture.read_pending_bytes().is_none(),
            "capture kept producing audio after stop() — mic was not released"
        );
        eprintln!("[mic] device released cleanly after stop()");
    }

    /// The commit path drains the capture *after* releasing the device, to pick
    /// up everything cpal buffered since the pump's last 50 ms tick. If stop()
    /// dropped that audio the last syllable of every dictation would be lost —
    /// which is exactly what used to happen.
    #[test]
    fn buffered_audio_survives_stop_and_can_still_be_drained() {
        if std::env::var("CODEX_MIC_AUDIO").is_err() {
            eprintln!("skipping; set CODEX_MIC_AUDIO=1 to run against a real mic");
            return;
        }
        let mut capture = AudioCapture::start(None).expect("microphone should open");
        // Deliberately do not drain while recording: this stands in for the
        // audio captured between the pump's last tick and the key coming up.
        std::thread::sleep(Duration::from_millis(400));
        capture.stop();

        let tail = capture
            .read_pending_bytes()
            .expect("audio buffered before stop() must still be drainable after it");
        eprintln!("[mic] recovered {} bytes of tail after stop()", tail.len());
        assert_eq!(tail.len() % 2, 0, "PCM16 must be an even byte count");
        // 400ms at 48kHz mono PCM16 is ~38400 bytes; allow generous slack for
        // device start-up latency.
        assert!(
            tail.len() > 8_000,
            "only {} bytes recovered — the tail is being dropped",
            tail.len()
        );
    }
}
