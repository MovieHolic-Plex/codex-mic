const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const label = document.getElementById("label");
const pill = document.getElementById("pill");
let hideTimer = null;

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

async function pollBuffer() {
  if (pill.dataset.state !== "recording") return;
  try {
    const b = await invoke("buffer");
    if (b) {
      label.textContent = b.length > 80 ? b.slice(-80) : b;
    } else {
      label.textContent = "듣는 중…";
    }
  } catch {}
}

setInterval(pollBuffer, 200);

async function setupListeners() {
  await listen("dictate://started", () => setState("recording", "듣는 중…"));
  await listen("dictate://processing", () => setState("processing", "변환 중…"));
  await listen("dictate://stopped", (e) => {
    const text = e.payload?.text || "";
    if (text) { setState("success", `✓ ${text.slice(0, 60)}`); }
    else { setState("hidden", "Ctrl+E"); }
  });
  await listen("dictate://error", (e) => setState("error", `⚠ ${e.payload?.message || "오류"}`));
  await listen("realtime://error", (e) => setState("error", `⚠ ${e.payload?.message || "realtime 오류"}`));
}

setupListeners();
