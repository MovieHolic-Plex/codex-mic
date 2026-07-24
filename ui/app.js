const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const label = document.getElementById("label");
const pill = document.getElementById("pill");
let hideTimer = null;
let pc = null;
let ctx = null;
let dest = null;
let audioTrack = null;
let unlisteners = [];

function setState(state, text) {
  pill.dataset.state = state;
  label.textContent = text;
  if (hideTimer) { clearTimeout(hideTimer); hideTimer = null; }
  if (state === "success") {
    hideTimer = setTimeout(() => { pill.dataset.state = "hidden"; label.textContent = "Ctrl+E"; }, 1500);
  } else if (state === "error") {
    hideTimer = setTimeout(() => { pill.dataset.state = "hidden"; label.textContent = "Ctrl+E"; }, 5000);
  }
}

function waitForIce(peer) {
  return new Promise((r) => {
    if (peer.iceGatheringState === "complete") return r();
    const check = () => {
      if (peer.iceGatheringState === "complete") {
        peer.removeEventListener("icegatheringstatechange", check);
        r();
      }
    };
    peer.addEventListener("icegatheringstatechange", check);
    setTimeout(r, 3500);
  });
}

function base64ToFloat32(b64) {
  const bin = atob(b64);
  const len = bin.length / 2;
  const f32 = new Float32Array(len);
  for (let i = 0; i < len; i++) {
    const lo = bin.charCodeAt(i * 2);
    const hi = bin.charCodeAt(i * 2 + 1);
    const s16 = (hi << 8) | lo;
    const signed = s16 > 32767 ? s16 - 65536 : s16;
    f32[i] = signed / 32768.0;
  }
  return f32;
}

async function onStarted() {
  setState("recording", "듣는 중…");
  try {
    ctx = new AudioContext({ sampleRate: 24000 });
    dest = ctx.createMediaStreamDestination();
    audioTrack = dest.stream.getAudioTracks()[0];

    pc = new RTCPeerConnection();
    pc.createDataChannel("oai-events");
    pc.addTrack(audioTrack, dest.stream);

    const offer = await pc.createOffer({ offerToReceiveAudio: false });
    await pc.setLocalDescription(offer);
    await waitForIce(pc);

    await invoke("realtime_start", { sdpOffer: pc.localDescription.sdp });
  } catch (e) {
    setState("error", "오류: " + String(e).slice(0, 60));
  }
}

function onPcm(data) {
  if (!ctx || !dest) return;
  const f32 = base64ToFloat32(data);
  const buffer = ctx.createBuffer(1, f32.length, 24000);
  buffer.getChannelData(0).set(f32);
  const src = ctx.createBufferSource();
  src.buffer = buffer;
  src.connect(dest);
  src.start();
}

async function onSdp(sdp) {
  if (!pc) return;
  try { await pc.setRemoteDescription({ type: "answer", sdp }); }
  catch (e) { console.error("SDP error:", e); }
}

function onStopped() {
  if (pc) { pc.close(); pc = null; }
  if (audioTrack) { audioTrack.stop(); audioTrack = null; }
  if (ctx) { ctx.close(); ctx = null; dest = null; }
  setState("hidden", "Ctrl+E");
}

async function setupListeners() {
  for (const u of unlisteners) await u();
  unlisteners = [];
  const subs = [
    ["dictate://started", () => onStarted()],
    ["dictate://processing", () => setState("processing", "변환 중…")],
    ["dictate://stopped", (e) => {
      const text = e.payload?.text || "";
      if (text) { setState("success", `✓ ${text.slice(0, 60)}`); }
      else { onStopped(); }
    }],
    ["dictate://error", (e) => setState("error", `⚠ ${e.payload?.message || "오류"}`)],
    ["realtime://sdp", (e) => onSdp(e.payload.sdp)],
    ["realtime://error", (e) => setState("error", `⚠ ${e.payload?.message || "오류"}`)],
    ["realtime://closed", () => {}],
    ["audio://pcm", (e) => onPcm(e.payload.data)],
  ];
  for (const [name, h] of subs) unlisteners.push(await listen(name, h));
}

setupListeners();
setState("hidden", "Ctrl+E");

setInterval(async () => {
  if (pill.dataset.state === "recording") {
    try {
      const b = await invoke("buffer");
      if (b) label.textContent = b.length > 80 ? b.slice(-80) : b;
      else label.textContent = "듣는 중…";
    } catch {}
  }
}, 200);
