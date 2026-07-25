const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// The frontend is purely an indicator + settings panel. Audio is captured by
// cpal and Opus-encoded in Rust, then streamed over WebRTC — no AudioContext,
// no RTCPeerConnection here.

const label = document.getElementById("label");
const pill = document.getElementById("pill");
const meterFill = document.getElementById("meter-fill");
const settings = document.getElementById("settings");
const settingsStatus = document.getElementById("settings-status");

const DEFAULT_HOTKEY = "Ctrl+E";

let hideTimer = null;
let unlisteners = [];
let config = null;
let settingsOpen = false;

function setState(state, text) {
  pill.dataset.state = state;
  label.textContent = text;
  if (state !== "recording") setLevel(0);
  if (hideTimer) { clearTimeout(hideTimer); hideTimer = null; }
  const idle = () => { pill.dataset.state = "hidden"; label.textContent = config?.hotkey || DEFAULT_HOTKEY; };
  if (state === "success") hideTimer = setTimeout(idle, 1500);
  else if (state === "error") hideTimer = setTimeout(idle, 5000);
}

// Mic level meter. RMS arrives on an i16 scale where speech sits far below
// full scale, so a square-root curve is what makes normal talking fill a
// useful part of the bar instead of a sliver. Decay is capped per frame so the
// bar reads as a level, not a strobe.
let level = 0;
function setLevel(pct) {
  level = pct >= level ? pct : Math.max(pct, level - 12);
  meterFill.style.width = `${level}%`;
}
function levelFromRms(rms) {
  return Math.min(100, Math.round(Math.sqrt(Math.max(0, rms) / 12000) * 100));
}

async function setupListeners() {
  for (const u of unlisteners) await u();
  unlisteners = [];
  const subs = [
    ["dictate://started", () => setState("recording", "듣는 중…")],
    ["dictate://level", (e) => {
      if (pill.dataset.state === "recording") setLevel(levelFromRms(e.payload?.rms ?? 0));
    }],
    ["dictate://processing", () => setState("processing", "변환 중…")],
    ["dictate://stopped", (e) => {
      const text = e.payload?.text || "";
      if (text) setState("success", `✓ ${text.slice(0, 60)}`);
      else setState("hidden", config?.hotkey || DEFAULT_HOTKEY);
    }],
    ["dictate://filtered", (e) => {
      const text = e.payload?.text || "";
      setState("error", `⚠ 필터됨: ${text.slice(0, 40)}`);
    }],
    ["dictate://error", (e) => setState("error", `⚠ ${e.payload?.message || "오류"}`)],
    ["realtime://error", (e) => setState("error", `⚠ ${e.payload?.message || "오류"}`)],
    ["realtime://closed", () => {}],
    ["settings://open", () => openSettings()],
    ["settings://close", () => closeSettings()],
  ];
  for (const [name, h] of subs) unlisteners.push(await listen(name, h));
}

// ---------- settings panel ----------

const fields = {
  hotkey: document.getElementById("cfg-hotkey"),
  settingsHotkey: document.getElementById("cfg-settings-hotkey"),
  activationMode: document.getElementById("cfg-activation-mode"),
  silenceAutostop: document.getElementById("cfg-silence-autostop"),
  injectionMode: document.getElementById("cfg-injection-mode"),
  restoreClipboard: document.getElementById("cfg-restore-clipboard"),
  appendSpace: document.getElementById("cfg-append-space"),
  language: document.getElementById("cfg-language"),
  micGain: document.getElementById("cfg-mic-gain"),
  mic: document.getElementById("cfg-mic"),
  hallucinationFilter: document.getElementById("cfg-hallucination-filter"),
};

function fillForm(cfg, mics) {
  fields.hotkey.value = cfg.hotkey;
  fields.settingsHotkey.value = cfg.settings_hotkey;
  fields.activationMode.value = cfg.activation_mode;
  fields.silenceAutostop.value = String(cfg.silence_autostop_ms);
  fields.injectionMode.value = cfg.injection_mode;
  fields.restoreClipboard.checked = cfg.restore_clipboard;
  fields.appendSpace.checked = cfg.append_trailing_space;
  fields.language.value = cfg.language;
  fields.micGain.value = String(cfg.mic_gain_db ?? 20);
  fields.hallucinationFilter.checked = cfg.hallucination_filter;

  fields.mic.innerHTML = "";
  const def = document.createElement("option");
  def.value = "";
  def.textContent = "시스템 기본값";
  fields.mic.appendChild(def);
  for (const name of mics) {
    const opt = document.createElement("option");
    opt.value = name;
    opt.textContent = name;
    fields.mic.appendChild(opt);
  }
  fields.mic.value = cfg.mic_device || "";

  syncInjectionRow();
}

function syncInjectionRow() {
  document.getElementById("row-restore-clipboard").style.display =
    fields.injectionMode.value === "clipboard" ? "" : "none";
}
fields.injectionMode.addEventListener("change", syncInjectionRow);

function readForm() {
  return {
    hotkey: fields.hotkey.value.trim() || DEFAULT_HOTKEY,
    settings_hotkey: fields.settingsHotkey.value.trim() || "Ctrl+Shift+E",
    activation_mode: fields.activationMode.value,
    silence_autostop_ms: parseInt(fields.silenceAutostop.value, 10) || 0,
    silence_threshold: config?.silence_threshold ?? 500,
    injection_mode: fields.injectionMode.value,
    restore_clipboard: fields.restoreClipboard.checked,
    append_trailing_space: fields.appendSpace.checked,
    language: fields.language.value,
    mic_gain_db: parseFloat(fields.micGain.value) || 0,
    mic_device: fields.mic.value || null,
    hallucination_filter: fields.hallucinationFilter.checked,
  };
}

async function openSettings() {
  settingsOpen = true;
  settings.hidden = false;
  settingsStatus.textContent = "";
  try {
    const [cfg, mics] = await Promise.all([invoke("get_config"), invoke("list_mics")]);
    config = cfg;
    fillForm(cfg, mics);
  } catch (e) {
    settingsStatus.textContent = `설정 로드 실패: ${e}`;
  }
}

function closeSettings() {
  settingsOpen = false;
  settings.hidden = true;
}

document.getElementById("close-settings").addEventListener("click", async () => {
  try { await invoke("close_settings"); } catch {}
  closeSettings();
});

document.getElementById("gear").addEventListener("click", async (e) => {
  e.stopPropagation();
  try { await invoke("toggle_settings"); } catch {}
});

document.getElementById("quit").addEventListener("click", async (e) => {
  e.stopPropagation();
  try { await invoke("quit_app"); } catch {}
});

// Drag the pill anywhere: mousedown on the pill background (not the buttons)
// kicks off an OS window drag. Position persists across launches via config.
pill.addEventListener("mousedown", async (e) => {
  if (e.button !== 0) return;
  if (e.target.closest(".pill-btn")) return;
  try { await invoke("start_drag"); } catch {}
});

// Injection test: 2s grace so the user can watch it land in the target app.
// (The pill window never takes focus in pill mode, so the text goes to
// whatever app was focused before the click.)
document.getElementById("test-inject").addEventListener("click", async () => {
  settingsStatus.textContent = "2초 후 주입…";
  setTimeout(async () => {
    try {
      const typed = await invoke("test_inject", { text: "codex-mic 주입 테스트 OK" });
      settingsStatus.textContent = `주입됨: ${typed}`;
    } catch (e) {
      settingsStatus.textContent = `주입 실패: ${e}`;
    }
  }, 2000);
});

document.getElementById("save-settings").addEventListener("click", async () => {
  const next = readForm();
  try {
    await invoke("set_config", { config: next });
    config = next;
    settingsStatus.textContent = "저장됨 ✓";
    setTimeout(() => { settingsStatus.textContent = ""; }, 2000);
  } catch (e) {
    settingsStatus.textContent = `저장 실패: ${e}`;
  }
});

// ---------- init ----------

async function init() {
  await setupListeners();
  try { config = await invoke("get_config"); } catch {}
  setState("hidden", config?.hotkey || DEFAULT_HOTKEY);
  try {
    if (!(await invoke("has_oauth"))) {
      setState("error", "⚠ codex login 필요");
    }
  } catch {}
}

init();

// Live transcript preview while recording.
setInterval(async () => {
  if (settingsOpen) return;
  if (pill.dataset.state !== "recording") return;
  try {
    // The pill is deliberately narrow now, so show the tail of the transcript
    // — the words just spoken — rather than a truncated beginning.
    const b = await invoke("buffer");
    label.textContent = b ? (b.length > 48 ? b.slice(-48) : b) : "듣는 중…";
  } catch {}
}, 200);
