const invoke = window.__TAURI__?.core.invoke;
const capture = document.querySelector("#capture");
const summary = document.querySelector("#summary");
const transcript = document.querySelector("#transcript");
const state = document.querySelector("#state");
let recording = false;
let offline = true;
let busy = false;

function refresh() {
    capture.disabled = !invoke || busy;
    capture.textContent = recording ? "停止并转写" : "录音";
    summary.disabled = !invoke || busy || recording || offline || !transcript.value.trim();
}

async function run(operation) {
    if (busy) return;
    busy = true;
    refresh();
    try { await operation(); }
    catch (error) { state.textContent = String(error); }
    finally { busy = false; refresh(); }
}

capture.onclick = () => run(async () => {
    if (recording) {
        recording = false;
        state.textContent = "正在本机转写…";
        const result = await invoke("stop_recording");
        transcript.value = result.transcript;
        state.textContent = result.transcript ? "转写完成" : "未识别到语音";
    } else {
        await invoke("start_recording");
        recording = true;
        state.textContent = "正在录音";
    }
});

summary.onclick = () => run(async () => {
    const result = await invoke("engine", {
        command: "ai-inline", payload: {mode: "summary", text: transcript.value, target_lang: "zh"},
    });
    state.textContent = result.output_text;
});
transcript.oninput = refresh;
refresh();
if (invoke) {
    invoke("get_settings").then(settings => {
        offline = settings.pure_offline;
        refresh();
    }).catch(error => state.textContent = String(error));
} else {
    state.textContent = "请在 Vitral 桌面应用中打开录音笔。";
}
