use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::mpsc;
use std::time::Duration;

fn main() {
    let host = cpal::default_host();
    let devices: Vec<_> = host.input_devices().expect("enum").collect();
    for d in &devices {
        let name = d.name().unwrap_or_default();
        let cfg = match d.default_input_config() { Ok(c) => c, Err(_) => continue };
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let config: cpal::StreamConfig = cfg.clone().into();
        let ch = cfg.channels() as usize;
        let stream = match cfg.sample_format() {
            cpal::SampleFormat::F32 => d.build_input_stream(&config, move |data: &[f32], _| {
                let mut mono = Vec::with_capacity(data.len() / ch);
                for f in data.chunks_exact(ch) { mono.push(f.iter().sum::<f32>() / ch as f32); }
                let _ = tx.send(mono);
            }, |e| eprintln!("err {e}"), None),
            cpal::SampleFormat::I16 => d.build_input_stream(&config, move |data: &[i16], _| {
                let mut mono = Vec::with_capacity(data.len() / ch);
                for f in data.chunks_exact(ch) { mono.push(f.iter().map(|&s| s as f32 / 32768.0).sum::<f32>() / ch as f32); }
                let _ = tx.send(mono);
            }, |e| eprintln!("err {e}"), None),
            _ => continue,
        }.expect("stream");
        stream.play().unwrap();
        println!("[{name}] listening 6s (quiet 3s + SPEAK LOUDLY 3s)...");
        std::thread::sleep(Duration::from_secs(6));
        drop(stream);
        let mut samples = Vec::new();
        while let Ok(v) = rx.try_recv() { samples.extend(v); }
        let half = samples.len() / 2;
        let rms = |s: &[f32]| (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt();
        let peak = |s: &[f32]| s.iter().map(|x| x.abs()).fold(0f32, f32::max);
        println!("[{name}] samples={} quietRMS={:.4} speakRMS={:.4} quietPeak={:.4} speakPeak={:.4}",
            samples.len(), rms(&samples[..half]), rms(&samples[half..]), peak(&samples[..half]), peak(&samples[half..]));
    }
}
