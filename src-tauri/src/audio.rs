use base64::Engine;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, StreamConfig};
use std::sync::mpsc;
use std::thread::JoinHandle;
use tracing::{info, warn};

const TARGET_SAMPLE_RATE: u32 = 24_000;

pub struct AudioCapture {
    receiver: mpsc::Receiver<String>,
    _handle: JoinHandle<()>,
}

impl AudioCapture {
    pub fn start() -> Result<Self, String> {
        let (tx, rx) = mpsc::channel::<String>();
        let tx_clone = tx.clone();

        let handle = std::thread::spawn(move || {
            let host = cpal::default_host();
            let device = match host.default_input_device() {
                Some(d) => d,
                None => {
                    let _ = tx_clone.send(String::new());
                    return;
                }
            };

            let supported = match device.supported_input_configs() {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "config query failed");
                    let _ = tx_clone.send(String::new());
                    return;
                }
            };

            let config_match = supported
                .into_iter()
                .find(|c| c.channels() >= 1 && c.min_sample_rate().0 <= TARGET_SAMPLE_RATE);

            let supported_config = match config_match {
                Some(c) => c.with_sample_rate(SampleRate(TARGET_SAMPLE_RATE)),
                None => {
                    let _ = tx_clone.send(String::new());
                    return;
                }
            };

            let config = StreamConfig {
                channels: 1,
                sample_rate: SampleRate(TARGET_SAMPLE_RATE),
                buffer_size: cpal::BufferSize::Default,
            };

            let stream_result = match supported_config.sample_format() {
                SampleFormat::I16 => {
                    let sender = tx_clone;
                    device
                        .build_input_stream(
                            &config,
                            move |data: &[i16], _: &_| {
                                let bytes: Vec<u8> =
                                    data.iter().flat_map(|s| s.to_le_bytes()).collect();
                                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                let _ = sender.send(b64);
                            },
                            |e| warn!(error = %e, "audio stream error"),
                            None,
                        )
                }
                SampleFormat::F32 => {
                    let sender = tx_clone;
                    device.build_input_stream(
                        &config,
                        move |data: &[f32], _: &_| {
                            let bytes: Vec<u8> = data
                                .iter()
                                .flat_map(|&s| {
                                    let v = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
                                    v.to_le_bytes()
                                })
                                .collect();
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            let _ = sender.send(b64);
                        },
                        |e| warn!(error = %e, "audio stream error"),
                        None,
                    )
                }
                fmt => {
                    warn!(format = ?fmt, "unsupported sample format");
                    return;
                }
            };

            let stream = match stream_result {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "build stream failed");
                    return;
                }
            };

            if let Err(e) = stream.play() {
                warn!(error = %e, "stream play failed");
                return;
            }

            info!("audio capture started: 24kHz mono i16");
            stream.pause().ok();
            stream.play().ok();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        });

        Ok(Self {
            receiver: rx,
            _handle: handle,
        })
    }

    pub fn read_all_pending(&self) -> Vec<String> {
        let mut chunks = Vec::new();
        while let Ok(chunk) = self.receiver.try_recv() {
            if !chunk.is_empty() {
                chunks.push(chunk);
            }
        }
        chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_to_base64_roundtrip() {
        let pcm = vec![0i16, 100, -100, 32767, -32768];
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let decoded = base64::engine::general_purpose::STANDARD.decode(&b64).unwrap();
        let restored: Vec<i16> = decoded
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(pcm, restored);
    }

    #[test]
    fn empty_pcm_produces_empty_base64() {
        let bytes: Vec<u8> = Vec::new();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        assert_eq!(b64, "");
    }
}
