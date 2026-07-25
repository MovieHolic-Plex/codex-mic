//! Persistent user configuration (`%APPDATA%/codex-mic/config.json`).
//!
//! Every field has a serde default, so a config written by an older version
//! always parses — unknown-or-missing just means default.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationMode {
    /// Press once to start, press again to stop (default).
    Toggle,
    /// Hold to record, release to commit.
    PushToTalk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionMode {
    /// Simulate keystrokes (works everywhere, slow for long text).
    Type,
    /// Set the clipboard and send Ctrl+V (instant; needs paste support).
    Clipboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Auto,
    Korean,
    English,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Dictation hotkey, tauri-global-shortcut syntax ("Ctrl+E", "Alt+Space").
    pub hotkey: String,
    /// Opens/closes the settings panel.
    pub settings_hotkey: String,
    pub activation_mode: ActivationMode,
    /// Auto-commit after this much continuous silence. 0 disables.
    pub silence_autostop_ms: u64,
    /// RMS (i16) below which audio counts as silence for the auto-stop.
    pub silence_threshold: i16,
    pub injection_mode: InjectionMode,
    /// After a clipboard paste, put the previous clipboard content back.
    pub restore_clipboard: bool,
    /// Append one space after the injected text.
    pub append_trailing_space: bool,
    pub language: Language,
    /// Input device name (substring match). None = system default.
    pub mic_device: Option<String>,
    /// Capture gain in dB. Realtek/USB mics need ~20; 0 disables the boost.
    pub mic_gain_db: f32,
    pub hallucination_filter: bool,
    /// Pill position (physical pixels), restored on launch. None = default spot.
    pub window_x: Option<i32>,
    pub window_y: Option<i32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Ctrl+E by default: it is deliberate enough that it never fires by
            // accident, needs no lock-key state juggling, and sits next to the
            // settings chord. CapsLock still works if you prefer a single key —
            // the caps state is restored for you — as does any other key or
            // chord set in the settings panel.
            hotkey: "Ctrl+E".to_string(),
            settings_hotkey: "Ctrl+Shift+E".to_string(),
            // Push-to-talk matches the universal "hold to record" intuition;
            // toggle mode remains available in settings.
            activation_mode: ActivationMode::PushToTalk,
            silence_autostop_ms: 0,
            silence_threshold: 500,
            injection_mode: InjectionMode::Type,
            restore_clipboard: true,
            append_trailing_space: false,
            language: Language::Auto,
            mic_device: None,
            mic_gain_db: 20.0,
            hallucination_filter: true,
            window_x: None,
            window_y: None,
        }
    }
}

fn config_path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(base).join("codex-mic").join("config.json")
}

static CONFIG: OnceLock<RwLock<Config>> = OnceLock::new();

fn cell() -> &'static RwLock<Config> {
    CONFIG.get_or_init(|| RwLock::new(Config::load()))
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
                tracing::warn!(error = %e, path = %path.display(), "config unreadable; using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create config dir: {e}"))?;
        }
        let raw = serde_json::to_string_pretty(self).map_err(|e| format!("serialize: {e}"))?;
        std::fs::write(&path, raw).map_err(|e| format!("write {}: {e}", path.display()))
    }
}

pub fn get() -> Config {
    cell().read().map(|c| c.clone()).unwrap_or_default()
}

/// Replace the live config and persist it.
pub fn set(next: Config) -> Result<(), String> {
    next.save()?;
    if let Ok(mut guard) = cell().write() {
        *guard = next;
    }
    Ok(())
}

/// Mutate the live config in place and persist (e.g. window position drags).
pub fn update(f: impl FnOnce(&mut Config)) -> Result<(), String> {
    let mut cfg = get();
    f(&mut cfg);
    set(cfg)
}

/// RMS amplitude of PCM16LE bytes — the silence detector's signal.
pub fn pcm_rms(pcm: &[u8]) -> i16 {
    if pcm.len() < 2 {
        return 0;
    }
    let (sum, n) = pcm.chunks_exact(2).fold((0f64, 0u64), |(s, n), c| {
        let v = i16::from_le_bytes([c[0], c[1]]) as f64;
        (s + v * v, n + 1)
    });
    ((sum / n as f64).sqrt()) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_json_uses_defaults() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.hotkey, "Ctrl+E");
        assert_ne!(
            cfg.hotkey, cfg.settings_hotkey,
            "identical hotkeys make apply_hotkeys refuse to register either"
        );
        assert_eq!(cfg.activation_mode, ActivationMode::PushToTalk);
        assert_eq!(cfg.silence_autostop_ms, 0);
        assert_eq!(cfg.injection_mode, InjectionMode::Type);
        assert_eq!(cfg.language, Language::Auto);
        assert!(cfg.hallucination_filter);
    }

    #[test]
    fn roundtrip_preserves_values() {
        let cfg = Config {
            hotkey: "Alt+Space".into(),
            activation_mode: ActivationMode::PushToTalk,
            silence_autostop_ms: 2500,
            injection_mode: InjectionMode::Clipboard,
            append_trailing_space: true,
            language: Language::Korean,
            mic_device: Some("USB麦克风".into()),
            ..Default::default()
        };
        let raw = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.hotkey, "Alt+Space");
        assert_eq!(back.activation_mode, ActivationMode::PushToTalk);
        assert_eq!(back.silence_autostop_ms, 2500);
        assert_eq!(back.injection_mode, InjectionMode::Clipboard);
        assert!(back.append_trailing_space);
        assert_eq!(back.language, Language::Korean);
        assert_eq!(back.mic_device.as_deref(), Some("USB麦克风"));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let cfg: Config = serde_json::from_str(r#"{"future_setting": 42, "hotkey": "F9"}"#).unwrap();
        assert_eq!(cfg.hotkey, "F9");
    }

    #[test]
    fn pcm_rms_distinguishes_silence_from_speech() {
        let silence = vec![0u8; 1920];
        assert_eq!(pcm_rms(&silence), 0);
        let loud: Vec<u8> = (0..960).flat_map(|_| 8000i16.to_le_bytes()).collect();
        assert_eq!(pcm_rms(&loud), 8000);
        assert!(pcm_rms(&[]) == 0);
    }
}
