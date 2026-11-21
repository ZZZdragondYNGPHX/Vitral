const $ = s => document.querySelector(s);
const all = s => [...document.querySelectorAll(s)];
const invoke = window.__TAURI__?.core.invoke;
const notice = text => { $("#notice").textContent = text; };
const read = input => input.type === "checkbox" ? input.checked : (input.type === "range" || input.name === "frecency_half_life_sec") ? Number(input.value) : input.value;
let saved = {}, busy = false;
function preview() {
    const mode = $("[name=theme]").value;
    document.documentElement.dataset.theme = mode === "system" ? matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light" : mode;
    for (const input of all("input[type=range]")) input.nextElementSibling.value = input.value + (input.name === "candidate_font_px" ? " px" : " 条");
    document.documentElement.style.setProperty("--candidate-font", $("[name=candidate_font_px]").value + "px");
    all(".online").forEach(el => el.disabled = busy || $("[name=pure_offline]").checked);
}
function apply(settings) {
    saved = settings;
    for (const el of all("main [name]")) {
        if (el.type === "password") el.value = "";
        else if (settings[el.name] != null) {
            if (el.type === "checkbox") el.checked = settings[el.name];
            else el.value = settings[el.name];
        }
    }
    preview();
}
const api = (command, payload = {}) => invoke("engine", { command, payload });
async function run(fn) {
    if (busy) return;
    if (!invoke) { notice("请在 Vitral 桌面应用中执行此操作。"); return; }
    busy = true;
    const buttons = all("main button"); buttons.forEach(b => b.disabled = true);
    notice("处理中…");
    try { await fn(); } catch (e) { notice(String(e)); }
    finally { busy = false; buttons.forEach(b => b.disabled = false); preview(); }
}
all("nav button").forEach(b => b.onclick = () => {
    all("nav button").forEach(item => item.removeAttribute("aria-current"));
    b.setAttribute("aria-current","page"); $("h1").textContent = b.textContent;
    all("main section").forEach(s => s.hidden = s.id !== b.dataset.tab); notice("");
});
all("main [name]").forEach(el => el.oninput = () => {preview(); notice("有未保存的设置");});
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", preview);
$("#save").onclick = () => run(async () => {
    const patch = {};
    for (const el of all("main [name]")) if (el.type !== "password" || el.value) patch[el.name] = read(el);
    const reply = await invoke("save_settings",{patch}); apply(reply.settings);
    notice(reply.active ? "设置已应用" : "设置已保存，下次启用 Vitral 时生效");
});
const showEntries = entries => {
    const list = $("#entries"); list.replaceChildren();
    for (const item of entries) {
        const row = document.createElement("li"), text = document.createElement("span");
        text.textContent = item.content; row.append(text);
        const code = document.createElement("input"); code.placeholder = "简码"; code.setAttribute("aria-label","置顶简码"); code.size = 5; code.value=item.shortcut_code || "";
        const pin=document.createElement("button");pin.textContent=item.pinned?"更新简码":"置顶";
        pin.onclick=()=>run(async()=>{const r=await api("clipboard-pin",{id:item.id,shortcut_code:code.value || null});showEntries(r.ring); notice("已置顶");});
        row.append(code,pin); list.append(row);
    }
};
all("[data-action]").forEach(b => b.onclick = () => run(async()=>{
    const cmd = b.dataset.action;
    const data = await api(cmd,cmd === "restore-from-koofr" ? {config:{}} : {});
    if (cmd === "restore-from-koofr" && data.preferences) {
        const reply = await invoke("save_settings", {patch: data.preferences});
        apply(reply.settings);
    }
    if (data.ring) showEntries(data.ring);
    if (cmd === "storage") {
        const usage = $("#usage"); usage.replaceChildren();
        for (const [key,label] of Object.entries({lexicon:"词库与配置",clipboard:"剪贴板",relay:"临时中转"})) {
            const dt=document.createElement("dt"),dd=document.createElement("dd");
            dt.textContent=label; dd.textContent=new Intl.NumberFormat("zh-CN",{style:"unit",unit:"kilobyte",maximumFractionDigits:1}).format(data[key]/1000);usage.append(dt,dd);
        }
    }
    if(cmd === "relay-poll-inbox"){
        const inbox=$("#inbox");
        for(const item of data.received_items){
            const row=document.createElement("li");
            if(item.is_file){
                const link=document.createElement("a");
                const blob=new Blob([Uint8Array.from(item.raw_bytes)]);
                link.href=URL.createObjectURL(blob);link.download=item.file_name || "Vitral";
                link.textContent=link.download;row.append(link);
                link.onclick=()=>setTimeout(()=>{URL.revokeObjectURL(link.href);row.remove();},1000);
            }else{row.textContent=new TextDecoder().decode(Uint8Array.from(item.raw_bytes));}
            inbox.append(row);
        }
    }
    if(cmd === "relay-discover"){
        const devices=$("#devices");devices.replaceChildren();for(const d of data){const li=document.createElement("li");li.textContent=d.device_name||d.device_id;
            const send=document.createElement("button");send.textContent="发送文件";send.className="online";
            send.onclick=()=>run(async()=>{const path=await invoke("choose_file",{lexicon:false});if(path){const r=await api("relay-send",{device_id:d.device_id,path_or_text:path});notice("已通过 "+r.transport+" 发送");}});
            li.append(send);devices.append(li);}
    }
    notice(cmd === "restore-from-koofr" ? `已恢复 ${data.snapshots_applied} 项` : data.ring?.length === 0 ? "剪贴板为空" : cmd === "relay-discover" && !data.length ? "尚未发现设备" : "已完成");
}));
async function importPaths(paths) {
    let total=0;
    for(const path of paths){const r=await api("import-lexicon",{path});total+=r.imported_count;}
    notice(`已导入 ${total} 个词条`);
}
$("#import").onclick=()=>run(async()=>{const path=await invoke("choose_file",{lexicon:true});if(path)await importPaths([path]);else notice("");});
window.__TAURI__?.event.listen("tauri://drag-drop", e=>run(()=>importPaths(e.payload.paths)));
all("[data-ai]").forEach(b=>b.onclick=()=>run(async()=>{const r=await api("ai-inline",{mode:b.dataset.ai,text:$("#source").value,target_lang:$("#target-lang").value});$("#result").textContent=r.output_text;notice("已完成");}));
let recorder;
$("#record").onclick=()=>run(async()=>{
    if(recorder){recorder=false;$("#record").textContent="开始录音";const r=await invoke("stop_recording");$("#source").value=r.transcript;notice(r.transcript?"已在本机转写":"未识别到语音");}
    else {await invoke("start_recording");recorder=true;$("#record").textContent="停止并转写";notice("正在录音");}
});
if(invoke) invoke("get_settings").then(apply).catch(e=>notice(String(e)));else preview();
