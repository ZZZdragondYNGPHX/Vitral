const invoke = window.__TAURI__?.core.invoke;
const shell = document.querySelector("#shell");
const capture = document.querySelector("#capture");
const restoreRaw = document.querySelector("#restore-raw");
const copyBtn = document.querySelector("#copy");
const summary = document.querySelector("#summary");
const cancelSummary = document.querySelector("#cancel-summary");
const transcript = document.querySelector("#transcript");
const state = document.querySelector("#state");
const clock = document.querySelector("#clock");

let recording = false;
let offline = true;
let hasLocalLlm = false;
let preferLocal = true;
let hasRemoteAi = false;
let settingsKnown = false;
let modelsKnown = false;
let liveRoute = null;
let syncSeq = 0;
let activeVoiceLabel = "本机离线语音";
let lastCleanedTranscript = "";
let lastRawTranscript = "";
let showingRaw = false;
let activeAiStreamId = 0;
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

async function syncRuntimeState() {
  if (!invoke) return false;
  const seq = ++syncSeq;
  let status;
  try {
    status = await invoke("probe_engine", { includeModels: true });
  } catch {
    if (seq === syncSeq) {
      settingsKnown = false;
      modelsKnown = false;
      liveRoute = null;
      hasLocalLlm = false;
      refresh();
    }
    return false;
  }
  if (seq !== syncSeq) {
    return settingsKnown && modelsKnown;
  }
  const settings = status?.settings;
  if (settings) {
    offline =
      typeof status.live_pure_offline === "boolean"
        ? status.live_pure_offline
        : Boolean(settings.pure_offline);
    preferLocal = settings.ai_prefer_local !== false;
    hasRemoteAi = Boolean((settings.ai_endpoint || "").trim());
    settingsKnown = Boolean(status.connected && typeof status.live_pure_offline === "boolean");
    window._themeMode = settings.theme || "system";
    applyTheme(window._themeMode);
  } else {
    settingsKnown = false;
  }
  if (status?.connected && status.models_state?.models) {
    const models = status.models_state.models;
    hasLocalLlm = models.some(m => m.capability === "local_llm" && m.active);
    const voiceModel = models.find(m => m.capability === "voice" && m.active);
    activeVoiceLabel = voiceModel?.name || "本机离线语音";
    liveRoute = typeof status.ai_route === "string" ? status.ai_route : null;
    modelsKnown = true;
  } else {
    hasLocalLlm = false;
    liveRoute = null;
    modelsKnown = false;
  }
  refresh();
  return settingsKnown && modelsKnown;
}

function summaryRoute() {
  if (!settingsKnown || !modelsKnown) return "unknown";
  if (liveRoute === "local" || liveRoute === "remote" || liveRoute === "none") {
    return liveRoute;
  }
  return "unknown";
}

function refresh() {
  const hasText = Boolean(transcript.value.trim());
  const route = summaryRoute();
  const canSummary = route === "local" || route === "remote";
  const streaming = activeAiStreamId > 0;
  capture.disabled = !invoke || busy;
  capture.textContent = recording ? "停止并转写" : "开始录音";
  copyBtn.disabled = !hasText;
  summary.disabled = !invoke || busy || recording || !hasText || !canSummary;
  summary.hidden = streaming;
  cancelSummary.hidden = !streaming;
  cancelSummary.disabled = !streaming;
  const hasToggleRaw = Boolean(lastRawTranscript && lastCleanedTranscript && lastRawTranscript !== lastCleanedTranscript);
  restoreRaw.hidden = !hasToggleRaw || recording;
  restoreRaw.disabled = busy;
  restoreRaw.textContent = showingRaw ? "切回规整" : "还原原话";
  summary.title =
    route === "local"
      ? "将使用本机 Qwen3 离线提炼摘要"
      : route === "remote"
        ? "将使用已配置的远端 AI 服务提炼摘要"
        : route === "unknown"
          ? "无法确认当前模型与离线状态，请确认 Vitral 输入服务已运行"
          : offline
            ? "纯离线模式下请先在模型中心启用本地 Qwen3，或关闭纯离线后配置远端 AI"
            : "请先在模型中心启用本地 Qwen3 或配置远端 AI 服务地址";
  if (recording) shell.dataset.phase = "recording";
  else if (busy) shell.dataset.phase = "busy";
  else if (shell.dataset.phase === "busy") shell.dataset.phase = hasText ? "done" : "idle";
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
    activeAiStreamId = 0;
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
      state.textContent = `正在通过 ${activeVoiceLabel} 离线转写…`;
      const result = await invoke("stop_recording");
      const text = result.transcript || "";
      lastCleanedTranscript = text;
      lastRawTranscript = result.cleaned ? (result.raw_transcript || "") : "";
      showingRaw = false;
      transcript.value = text;
      shell.dataset.phase = text ? "done" : "idle";
      state.textContent = text
        ? (result.cleaned ? "本机离线转写并完成轻度规整（可随时还原原话）" : "本机离线转写完成")
        : "未识别到有效语音";
      syncRuntimeState().catch(() => {});
    } else {
      lastCleanedTranscript = "";
      lastRawTranscript = "";
      showingRaw = false;
      await invoke("start_recording");
      recording = true;
      startClock();
      shell.dataset.phase = "recording";
      state.textContent = "正在录音…";
    }
  });

restoreRaw.onclick = () => {
  if (!lastRawTranscript || !lastCleanedTranscript) return;
  showingRaw = !showingRaw;
  transcript.value = showingRaw ? lastRawTranscript : lastCleanedTranscript;
  state.classList.remove("is-summary");
  state.textContent = showingRaw ? "已切回原始听写文本" : "已恢复轻度规整文本";
  refresh();
};

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

cancelSummary.onclick = () => {
  const id = activeAiStreamId;
  if (!id || !invoke) return;
  cancelSummary.disabled = true;
  invoke("engine", { command: "ai-inline-cancel", payload: { id } })
    .then(res => {
      if (!res?.cancelled && activeAiStreamId === id) {
        cancelSummary.disabled = false;
      }
    })
    .catch(() => {
      if (activeAiStreamId === id) cancelSummary.disabled = false;
    });
};

summary.onclick = () =>
  run(async () => {
    const synced = await syncRuntimeState();
    const route = summaryRoute();
    if (!synced || (route !== "local" && route !== "remote")) {
      shell.dataset.phase = transcript.value.trim() ? "done" : "idle";
      state.classList.remove("is-summary");
      state.textContent = summary.title || "当前无法确认可用的本地或远端 AI 摘要服务";
      return;
    }
    state.classList.remove("is-summary");
    const routeTag = route === "local" ? "[本机离线]" : "[远端 API]";
    state.textContent = `${routeTag} 正在提炼摘要…`;
    const started = await invoke("engine", {
      command: "ai-inline-start",
      payload: { mode: "summary", text: transcript.value, target_lang: "zh" },
    });
    activeAiStreamId = Number(started.id) || 0;
    refresh();
    let current = started;
    while (current.status === "running" && activeAiStreamId > 0) {
      if (current.output_text) {
        state.classList.add("is-summary");
        state.textContent = `${current.route === "local" ? "[本机离线]" : "[远端 API]"}\n${current.output_text}`;
      }
      await new Promise(resolve => setTimeout(resolve, 90));
      current = await invoke("engine", {
        command: "ai-inline-poll",
        payload: { id: activeAiStreamId },
      });
    }
    activeAiStreamId = 0;
    if (current.status === "cancelled") {
      shell.dataset.phase = transcript.value.trim() ? "done" : "idle";
      state.classList.remove("is-summary");
      state.textContent = "已停止生成摘要";
      return;
    }
    if (current.status === "error") {
      throw new Error(current.error || "摘要生成失败");
    }
    const finalTag = current.route === "local" ? "[本机离线]" : "[远端 API]";
    shell.dataset.phase = "done";
    state.classList.add("is-summary");
    state.textContent = current.output_text ? `${finalTag}\n${current.output_text}` : "未返回摘要";
  });

transcript.oninput = () => {
  if (transcript.value !== lastCleanedTranscript && transcript.value !== lastRawTranscript) {
    lastCleanedTranscript = "";
    lastRawTranscript = "";
    showingRaw = false;
  }
  refresh();
};
applyTheme("system");
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => applyTheme(window._themeMode || "system"));
refresh();

if (invoke) {
  syncRuntimeState().catch(error => (state.textContent = String(error)));
  window.addEventListener("focus", () => {
    if (!busy && !recording) syncRuntimeState().catch(() => {});
  });
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden && !busy && !recording) syncRuntimeState().catch(() => {});
  });
} else {
  state.textContent = "请在 Vitral 桌面应用中打开录音笔。";
}
