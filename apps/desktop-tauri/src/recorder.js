const invoke = window.__TAURI__?.core.invoke;
const shell = document.querySelector("#shell");
const capture = document.querySelector("#capture");
const copyBtn = document.querySelector("#copy");
const summary = document.querySelector("#summary");
const transcript = document.querySelector("#transcript");
const state = document.querySelector("#state");
const clock = document.querySelector("#clock");

let recording = false;
let offline = true;
let busy = false;
let startedAt = 0;
let timer = null;

function applyTheme(mode = "system") {
  const resolved =
    mode === "system"
      ? matchMedia("(prefers-color-scheme: dark)").matches
        ? "dark"
        : "light"
      : mode;
  document.documentElement.dataset.theme = resolved;
}

function setClock(sec) {
  const mm = String(Math.floor(sec / 60)).padStart(2, "0");
  const ss = String(sec % 60).padStart(2, "0");
  clock.textContent = `${mm}:${ss}`;
}

function startClock() {
  startedAt = Date.now();
  setClock(0);
  clearInterval(timer);
  timer = setInterval(() => {
    setClock(Math.floor((Date.now() - startedAt) / 1000));
  }, 500);
}

function stopClock() {
  clearInterval(timer);
  timer = null;
}

function refresh() {
  const hasText = Boolean(transcript.value.trim());
  capture.disabled = !invoke || busy;
  capture.textContent = recording ? "停止并转写" : "开始录音";
  copyBtn.disabled = !hasText;
  summary.disabled = !invoke || busy || recording || offline || !hasText;
  summary.title = offline ? "纯离线模式下不可调用外部 AI 摘要" : "";
  if (recording) shell.dataset.phase = "recording";
  else if (busy) shell.dataset.phase = "busy";
}

async function run(operation) {
  if (busy) return;
  busy = true;
  refresh();
  try {
    await operation();
  } catch (error) {
    stopClock();
    recording = false;
    shell.dataset.phase = "idle";
    state.classList.remove("is-summary");
    state.textContent = String(error);
  } finally {
    busy = false;
    refresh();
  }
}

capture.onclick = () =>
  run(async () => {
    state.classList.remove("is-summary");
    if (recording) {
      recording = false;
      stopClock();
      shell.dataset.phase = "busy";
      state.textContent = "正在本机 SenseVoice 离线转写…";
      const result = await invoke("stop_recording");
      transcript.value = result.transcript || "";
      shell.dataset.phase = result.transcript ? "done" : "idle";
      state.textContent = result.transcript ? "本机离线转写完成" : "未识别到有效语音";
    } else {
      await invoke("start_recording");
      recording = true;
      startClock();
      shell.dataset.phase = "recording";
      state.textContent = "正在录音…";
    }
  });

copyBtn.onclick = async () => {
  const text = transcript.value.trim();
  if (!text) return;
  try {
    await navigator.clipboard.writeText(text);
    const old = copyBtn.textContent;
    copyBtn.textContent = "已复制";
    setTimeout(() => (copyBtn.textContent = old), 1200);
  } catch {
    state.textContent = "无法写入剪贴板";
  }
};

summary.onclick = () =>
  run(async () => {
    state.classList.remove("is-summary");
    state.textContent = "正在提炼摘要…";
    const result = await invoke("engine", {
      command: "ai-inline",
      payload: { mode: "summary", text: transcript.value, target_lang: "zh" },
    });
    shell.dataset.phase = "done";
    state.classList.add("is-summary");
    state.textContent = result.output_text || "未返回摘要";
  });

transcript.oninput = refresh;
applyTheme("system");
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => applyTheme(window._themeMode || "system"));
refresh();

if (invoke) {
  invoke("get_settings")
    .then(settings => {
      offline = Boolean(settings.pure_offline);
      window._themeMode = settings.theme || "system";
      applyTheme(window._themeMode);
      refresh();
    })
    .catch(error => (state.textContent = String(error)));
} else {
  state.textContent = "请在 Vitral 桌面应用中打开录音笔。";
}
