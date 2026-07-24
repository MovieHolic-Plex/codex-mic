const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const label = document.getElementById("label");
const pill = document.getElementById("pill");

let pc = null;
let micStream = null;
let unlisteners = [];

function setLabel(text, state) {
  label.textContent = text;
  pill.dataset.state = state || "";
}

async function startPeer() {
  try {
    micStream = await navigator.mediaDevices.getUserMedia({ audio: true });
  } catch (e) {
    setLabel("마이크 권한 필요 / mic blocked", "error");
    return;
  }
  pc = new RTCPeerConnection();
  pc.ontrack = () => {};
  pc.createDataChannel("oai-events");
  for (const t of micStream.getAudioTracks()) pc.addTrack(t, micStream);
  const offer = await pc.createOffer({ offerToReceiveAudio: false });
  await pc.setLocalDescription(offer);
  await waitForIce(pc);
  try {
    await invoke("realtime_start", { sdpOffer: pc.localDescription.sdp });
  } catch (e) {
    setLabel("연결 실패: " + String(e), "error");
    stopPeer();
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

function stopPeer() {
  if (micStream) {
    for (const t of micStream.getAudioTracks()) t.stop();
    micStream = null;
  }
  if (pc) {
    pc.close();
    pc = null;
  }
}

async function onStarted() {
  setLabel("듣는 중… Listening…", "on");
  await startPeer();
}

async function onStopped() {
  stopPeer();
  setLabel("Press Ctrl+E to dictate", "");
}

function onTranscriptDelta(p) {
  if ((p.role || "") !== "user") return;
  invoke("buffer").then((b) => {
    setLabel((b || "").slice(-120) || "듣는 중… Listening…", "on");
  });
}

async function setupListeners() {
  for (const u of unlisteners) await u();
  unlisteners = [];
  const subs = [
    ["dictate://started", () => onStarted()],
    ["dictate://stopped", () => onStopped()],
    ["realtime://sdp", (e) => pc && pc.setRemoteDescription({ type: "answer", sdp: e.payload.sdp }).catch(() => {})],
    ["realtime://transcript/delta", (e) => onTranscriptDelta(e.payload)],
    ["realtime://error", (e) => setLabel("오류: " + (e.payload.message || "").slice(0, 80), "error")],
    ["realtime://closed", () => {}],
  ];
  for (const [name, h] of subs) unlisteners.push(await listen(name, h));
}

(async () => {
  await setupListeners();
  setLabel("Press Ctrl+E to dictate", "");
})();
