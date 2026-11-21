const $ = s => document.querySelector(s);
const all = s => [...document.querySelectorAll(s)];
const invoke = window.__TAURI__?.core.invoke;

const ACTION_DESCRIPTIONS = {
  toggle_language: "在中文输入与输入法内英文模式间快速切换",
  settings: "快速唤起 Vitral 桌面管理中心",
  clipboard: "原地展开剪贴板悬浮历史与置顶收藏",
  lookup: "打开单字手写查音板，显示带声调全拼与小鹤码",
  handwriting: "打开连续手写板，支持原位叠写与上下文纠偏",
};

const read = input =>
  input.type === "checkbox"
    ? input.checked
    : input.type === "range" || input.name === "frecency_half_life_sec"
      ? Number(input.value)
      : input.value;

let saved = {};
let busy = false;
let dirty = false;
let firstSettings = true;
let recordingShortcut = null;
let clipboardEntries = [];
let clipboardCapacity = 50;
let clipboardLoaded = false;
let confirmClearTimer = null;
let confirmHabitsTimer = null;
let recorderActive = false;
let recordTimer = null;
let recordStartedAt = 0;

function setNotice(text, tone = "idle") {
  $("#notice").textContent = text;
  $("#save-bar").dataset.tone = text ? tone : "idle";
}

function markDirty() {
  dirty = true;
  setNotice("有未保存的更改", "dirty");
}

function formatBytes(bytes) {
  const n = Number(bytes) || 0;
  if (n === 0) return "0 B";
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(2)} MB`;
}

function formatTime(ms) {
  if (!ms) return "";
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return "";
  const now = new Date();
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  return sameDay ? `${hh}:${mm}` : `${d.getMonth() + 1}/${d.getDate()} ${hh}:${mm}`;
}

function updateRangeFill(input) {
  const min = Number(input.min || 0);
  const max = Number(input.max || 100);
  const val = Number(input.value || 0);
  const pct = max > min ? ((val - min) / (max - min)) * 100 : 0;
  input.style.setProperty("--pct", `${pct}%`);
  const out = input.nextElementSibling;
  if (out) out.value = `${val}${input.name === "candidate_font_px" ? " px" : " 条"}`;
}

function syncSegmented(name, value) {
  const group = $(`.segmented[data-mirror="${name}"]`);
  if (!group) return;
  for (const btn of group.querySelectorAll("button")) {
    const selected = btn.dataset.val === String(value);
    btn.classList.toggle("is-selected", selected);
    btn.setAttribute("aria-pressed", String(selected));
  }
}

function preview() {
  const mode = $("[name=theme]").value;
  const resolved =
    mode === "system"
      ? matchMedia("(prefers-color-scheme: dark)").matches
        ? "dark"
        : "light"
      : mode;
  document.documentElement.dataset.theme = resolved;

  const fontPx = Number($("[name=candidate_font_px]").value);
  document.documentElement.style.setProperty("--candidate-font", `${fontPx}px`);
  for (const input of all("input[type=range]")) updateRangeFill(input);

  all("[data-font-preset]").forEach(btn => {
    btn.classList.toggle("is-active", Number(btn.dataset.fontPreset) === fontPx);
  });

  const schema = $("[name=schema]").value;
  syncSegmented("schema", schema);
  syncSegmented("theme", mode);

  const preedit = $("#preview-preedit");
  if (preedit) {
    preedit.innerHTML =
      (schema === "double_pinyin_flypy" ? "jr" : "jin'ri") +
      '<span class="caret"></span>';
  }

  const offline = $("[name=pure_offline]").checked;
  document.body.classList.toggle("is-offline", offline);
  $("#mode-pill").hidden = !offline;
  $("#offline-card").classList.toggle("is-offline", offline);
  const badge = $("#offline-badge");
  badge.textContent = offline ? "网络已阻断" : "协同就绪";
  badge.className = `badge ${offline ? "is-warn" : "is-ok"}`;

  all(".online").forEach(el => {
    el.disabled = busy || offline;
  });
}

function updateEngineStatus(connected) {
  const box = $("#engine-status");
  const text = box.querySelector(".status-text");
  const offline = $("[name=pure_offline]").checked;
  if (!invoke) {
    box.dataset.state = "disconnected";
    text.textContent = "界面预览模式";
    return;
  }
  if (connected) {
    box.dataset.state = offline ? "offline" : "connected";
    text.textContent = offline ? "输入服务运行中 · 纯离线" : "输入服务已连接";
  } else {
    box.dataset.state = "disconnected";
    text.textContent = "尚未检测到活动输入服务";
  }
}

function apply(settings) {
  saved = settings;
  dirty = false;
  clipboardCapacity = Number(settings.clipboard_ring_capacity || 50);

  const linuxEnglish = $("[name=linux_english_mode]");
  linuxEnglish.disabled = typeof settings.linux_english_mode !== "boolean";
  $("#linux-language").hidden = linuxEnglish.disabled;

  for (const el of all("main [name]")) {
    if (el.type === "password") {
      el.value = "";
    } else if (settings[el.name] != null) {
      if (el.type === "checkbox") el.checked = Boolean(settings[el.name]);
      else el.value = String(settings[el.name]);
    }
  }

  for (const pill of all("[data-secret]")) {
    const present = Boolean(settings[pill.dataset.secret]);
    pill.hidden = !present;
    const input = pill.closest(".row")?.querySelector("input");
    if (input && present) {
      input.placeholder = "已保存（留空保持不变）";
    }
  }

  renderShortcuts(settings);
  preview();

  if (firstSettings) {
    firstSettings = false;
    if (settings.launch_page === "library") {
      switchTab("library");
    }
  }
}

const api = (command, payload = {}) => invoke("engine", { command, payload });

async function run(fn, { silent = false } = {}) {
  if (busy) return;
  if (!invoke) {
    setNotice("请在 Vitral 桌面应用中执行此操作", "error");
    return;
  }
  busy = true;
  const buttons = all("main button");
  buttons.forEach(b => (b.disabled = true));
  if (!silent) setNotice("处理中…", "busy");
  try {
    await fn();
  } catch (e) {
    setNotice(String(e), "error");
  } finally {
    busy = false;
    buttons.forEach(b => (b.disabled = false));
    preview();
  }
}

function switchTab(tabId) {
  all("nav button").forEach(item => {
    const active = item.dataset.tab === tabId;
    if (active) {
      item.setAttribute("aria-current", "page");
      $("h1").textContent = item.querySelector("span")?.textContent || item.textContent;
    } else {
      item.removeAttribute("aria-current");
    }
  });
  all("main section").forEach(s => (s.hidden = s.id !== tabId));
  if (!dirty) setNotice("");
  if (tabId === "library" && invoke && !clipboardLoaded) {
    loadClipboard(true);
  }
}

all("nav button").forEach(b => (b.onclick = () => switchTab(b.dataset.tab)));

// Interactive preview candidates
all("#preview-capsule .cand").forEach(btn => {
  btn.onclick = () => {
    all("#preview-capsule .cand").forEach(c => c.classList.toggle("is-active", c === btn));
  };
});

// Segmented controls mirror hidden selects
all(".segmented").forEach(group => {
  const name = group.dataset.mirror;
  const select = $(`[name="${name}"]`);
  group.querySelectorAll("button").forEach(btn => {
    btn.onclick = () => {
      if (!select) return;
      select.value = btn.dataset.val;
      preview();
      markDirty();
    };
  });
});

// Font size presets
all("[data-font-preset]").forEach(btn => {
  btn.onclick = () => {
    const range = $("[name=candidate_font_px]");
    range.value = btn.dataset.fontPreset;
    preview();
    markDirty();
  };
});

all("main [name]").forEach(el => {
  el.oninput = () => {
    preview();
    markDirty();
  };
  el.onchange = () => {
    preview();
    markDirty();
  };
});

matchMedia("(prefers-color-scheme: dark)").addEventListener("change", preview);

async function saveAll() {
  if (recordingShortcut) finishShortcut(null);
  const conflicts = checkShortcutConflicts();
  if (conflicts.length) {
    setNotice(conflicts[0], "error");
    return;
  }
  await run(async () => {
    const patch = {};
    for (const el of all("main [name]")) {
      if (!el.disabled && (el.type !== "password" || el.value)) {
        patch[el.name] = read(el);
      }
    }
    patch.shortcuts = readShortcuts();
    const reply = await invoke("save_settings", { patch });
    apply(reply.settings);
    updateEngineStatus(Boolean(reply.active));
    setNotice(
      reply.active ? "设置已保存并实时应用" : "设置已保存，下次启用 Vitral 时生效",
      "ok"
    );
  });
}

$("#save").onclick = saveAll;

// Ctrl+S / Cmd+S shortcut
window.addEventListener("keydown", e => {
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s" && !recordingShortcut) {
    e.preventDefault();
    saveAll();
  }
});

// ================= CLIPBOARD =================
async function loadClipboard(silent = false) {
  await run(
    async () => {
      const data = await api("clipboard-state");
      clipboardLoaded = true;
      clipboardCapacity = data.capacity || clipboardCapacity;
      clipboardEntries = data.ring || [];
      renderClipboard();
      if (!silent) {
        setNotice(clipboardEntries.length ? "已刷新剪贴板历史" : "剪贴板为空", "ok");
      }
    },
    { silent }
  );
}

function createClipRow(item) {
  const row = document.createElement("li");
  row.className = `clip-item${item.pinned ? " is-pinned" : ""}`;

  const body = document.createElement("div");
  body.className = "clip-body";

  const text = document.createElement("div");
  text.className = "clip-text";
  text.textContent = item.content;
  text.title = item.content;

  const meta = document.createElement("div");
  meta.className = "clip-meta";
  if (item.pinned && item.shortcut_code) {
    const codeTag = document.createElement("span");
    codeTag.className = "clip-code-pill";
    codeTag.textContent = item.shortcut_code;
    meta.append(codeTag);
  }
  const bits = [];
  if (item.source_device) bits.push(item.source_device);
  const ts = formatTime(item.timestamp_ms);
  if (ts) bits.push(ts);
  bits.push(`${item.content.length} 字符`);
  const metaText = document.createElement("span");
  metaText.textContent = bits.join(" · ");
  meta.append(metaText);
  body.append(text, meta);

  const actions = document.createElement("div");
  actions.className = "clip-actions";

  const copyBtn = document.createElement("button");
  copyBtn.type = "button";
  copyBtn.className = "btn-mini";
  copyBtn.textContent = "复制";
  copyBtn.onclick = async () => {
    try {
      await navigator.clipboard.writeText(item.content);
      copyBtn.textContent = "已复制";
      setTimeout(() => (copyBtn.textContent = "复制"), 1200);
    } catch {
      setNotice("无法写入系统剪贴板", "error");
    }
  };

  const code = document.createElement("input");
  code.className = "clip-code-input";
  code.placeholder = "简码";
  code.setAttribute("aria-label", "置顶简码");
  code.value = item.shortcut_code || "";
  code.spellcheck = false;

  const pinBtn = document.createElement("button");
  pinBtn.type = "button";
  pinBtn.className = "btn-mini";
  pinBtn.textContent = item.pinned ? "保存简码" : "置顶";
  pinBtn.onclick = () =>
    run(async () => {
      const r = await api("clipboard-pin", {
        id: item.id,
        shortcut_code: code.value.trim() || null,
      });
      clipboardEntries = r.ring || [];
      renderClipboard();
      setNotice(item.pinned ? "已更新简码" : "已置顶收藏", "ok");
    });

  actions.append(copyBtn, code, pinBtn);

  if (item.pinned) {
    const unpinBtn = document.createElement("button");
    unpinBtn.type = "button";
    unpinBtn.className = "btn-mini";
    unpinBtn.textContent = "取消置顶";
    unpinBtn.onclick = () =>
      run(async () => {
        const r = await api("clipboard-unpin", { id: item.id });
        clipboardEntries = r.ring || [];
        renderClipboard();
        setNotice("已取消置顶", "ok");
      });
    actions.append(unpinBtn);
  }

  row.append(body, actions);
  return row;
}

function renderClipboard() {
  const q = ($("#clip-search").value || "").trim().toLowerCase();
  const filtered = clipboardEntries.filter(
    item =>
      !q ||
      item.content.toLowerCase().includes(q) ||
      (item.shortcut_code || "").toLowerCase().includes(q)
  );
  const pinned = filtered.filter(e => e.pinned);
  const recent = filtered.filter(e => !e.pinned);
  const totalPinned = clipboardEntries.filter(e => e.pinned).length;

  $("#clip-summary").textContent = `当前 ${clipboardEntries.length} / ${clipboardCapacity} 条 · ${totalPinned} 条置顶收藏（免淘汰）`;

  const pinnedList = $("#entries-pinned");
  const recentList = $("#entries");
  pinnedList.replaceChildren(...pinned.map(createClipRow));
  recentList.replaceChildren(...recent.map(createClipRow));

  $("#pinned-count").textContent = String(pinned.length);
  $("#recent-count").textContent = String(recent.length);
  $("#pinned-group").hidden = pinned.length === 0;
  $("#recent-group").hidden = recent.length === 0;
  $("#entries-empty").hidden = filtered.length > 0;
  $("#entries-empty").textContent =
    clipboardEntries.length === 0
      ? "暂无剪贴板记录，复制文本后将自动出现在这里"
      : "没有匹配的剪贴板记录";
}

$("#clip-search").oninput = renderClipboard;

// ================= ACTIONS =================
function renderHabitStats(stats) {
  const box = $("#habit-stats");
  if (!box || !stats) return;
  box.replaceChildren();
  box.hidden = false;
  const items = [
    ["个人词频", `${stats.vocab_count || 0} 词 · ${stats.bigram_count || 0} 搭配`],
    ["自造与识词", `${(stats.phrase_count || 0) + (stats.english_count || 0)} 项 · ${stats.app_count || 0} 应用`],
    ["专属模型", `重排 ${stats.reranker_updates || 0} 次 · 语义记忆 ${stats.semantic_memory_count || 0} 条`],
  ];
  for (const [label, value] of items) {
    const tile = document.createElement("div");
    tile.className = "usage-tile";
    const dt = document.createElement("dt");
    dt.textContent = label;
    const dd = document.createElement("dd");
    dd.textContent = value;
    tile.append(dt, dd);
    box.append(tile);
  }
}

function renderUsage(data) {
  const usage = $("#usage");
  usage.replaceChildren();
  usage.hidden = false;
  for (const [key, label] of Object.entries({
    lexicon: "词库与配置",
    clipboard: "双轨剪贴板",
    relay: "临时中转区",
  })) {
    const val = Number(data[key] || 0);
    const tile = document.createElement("div");
    tile.className = `usage-tile${key === "relay" && val === 0 ? " is-zero" : ""}`;
    const dt = document.createElement("dt");
    dt.textContent = label;
    const dd = document.createElement("dd");
    dd.textContent = key === "relay" && val === 0 ? "0 B（已清理）" : formatBytes(val);
    tile.append(dt, dd);
    usage.append(tile);
  }
}

function renderDevices(list) {
  const container = $("#devices");
  container.replaceChildren();
  $("#devices-empty").hidden = list.length > 0;
  for (const d of list) {
    const li = document.createElement("li");
    li.className = "device-card";

    const top = document.createElement("div");
    top.className = "device-top";
    const name = document.createElement("span");
    name.className = "device-name";
    name.textContent = d.device_name || d.device_id;
    name.title = name.textContent;

    const dot = document.createElement("span");
    const transport = d.lan_reachable ? "lan" : d.online ? "wan" : "off";
    dot.className = `device-dot ${transport}`;
    top.append(name, dot);

    const sub = document.createElement("div");
    sub.className = "device-sub";
    sub.textContent =
      transport === "lan"
        ? "局域网直连 · 极速"
        : transport === "wan"
          ? "跨网中转 · 阅后即焚"
          : "离线 · 投递待收";

    const sendBtn = document.createElement("button");
    sendBtn.type = "button";
    sendBtn.className = "online btn-accent-soft";
    sendBtn.textContent = "发送文件";
    sendBtn.onclick = () =>
      run(async () => {
        const path = await invoke("choose_file", { lexicon: false });
        if (!path) {
          setNotice("");
          return;
        }
        const r = await api("relay-send", { device_id: d.device_id, path_or_text: path });
        setNotice(`已通过 ${r.transport} 发送至 ${name.textContent}`, "ok");
      });

    li.append(top, sub, sendBtn);
    container.append(li);
  }
}

// Mouse wheel horizontal scrolling for device strip
$("#devices").addEventListener(
  "wheel",
  e => {
    if (e.deltaY !== 0) {
      e.currentTarget.scrollLeft += e.deltaY;
      e.preventDefault();
    }
  },
  { passive: false }
);

all("[data-action]").forEach(b => {
  b.onclick = () => {
    const cmd = b.dataset.action;

    // Two-step confirmation for clearing unpinned clipboard entries
    if (cmd === "clipboard-clear-unpinned" && !b.classList.contains("is-confirming")) {
      b.classList.add("is-confirming");
      b.textContent = "再次点击确认清空";
      clearTimeout(confirmClearTimer);
      confirmClearTimer = setTimeout(() => {
        b.classList.remove("is-confirming");
        b.textContent = "清空未置顶";
      }, 3500);
      return;
    }
    if (cmd === "clipboard-clear-unpinned") {
      clearTimeout(confirmClearTimer);
      b.classList.remove("is-confirming");
      b.textContent = "清空未置顶";
    }

    // Two-step confirmation for clearing learned typing habits
    if (cmd === "personalization-clear" && !b.classList.contains("is-confirming")) {
      b.classList.add("is-confirming");
      b.textContent = "再次点击确认清空";
      clearTimeout(confirmHabitsTimer);
      confirmHabitsTimer = setTimeout(() => {
        b.classList.remove("is-confirming");
        b.textContent = "清空习惯记忆";
      }, 3500);
      return;
    }
    if (cmd === "personalization-clear") {
      clearTimeout(confirmHabitsTimer);
      b.classList.remove("is-confirming");
      b.textContent = "清空习惯记忆";
    }

    if (cmd === "clipboard-state") {
      loadClipboard(false);
      return;
    }

    run(async () => {
      const data = await api(cmd, cmd === "restore-from-koofr" ? { config: {} } : {});
      if (cmd === "restore-from-koofr" && data.preferences) {
        const reply = await invoke("save_settings", { patch: data.preferences });
        apply(reply.settings);
      }
      if (data.ring) {
        clipboardLoaded = true;
        clipboardEntries = data.ring;
        renderClipboard();
      }
      if (cmd === "storage") renderUsage(data);
      if (cmd === "relay-discover") renderDevices(data || []);
      if (cmd === "relay-poll-inbox") {
        const inbox = $("#inbox");
        const items = data.received_items || [];
        if (items.length) $("#inbox-wrap").hidden = false;
        for (const item of items) {
          const row = document.createElement("li");
          if (item.is_file) {
            const link = document.createElement("a");
            const blob = new Blob([Uint8Array.from(item.raw_bytes)]);
            link.href = URL.createObjectURL(blob);
            link.download = item.file_name || "Vitral";
            link.textContent = `下载 ${link.download}`;
            const sizeSpan = document.createElement("span");
            sizeSpan.className = "device-sub";
            sizeSpan.textContent = formatBytes(item.raw_bytes.length);
            row.append(link, sizeSpan);
            link.onclick = () =>
              setTimeout(() => {
                URL.revokeObjectURL(link.href);
                row.remove();
              }, 1000);
          } else {
            const txt = new TextDecoder().decode(Uint8Array.from(item.raw_bytes));
            const span = document.createElement("span");
            span.textContent = txt;
            const copy = document.createElement("button");
            copy.type = "button";
            copy.className = "btn-mini";
            copy.textContent = "复制";
            copy.onclick = () => navigator.clipboard.writeText(txt);
            row.append(span, copy);
          }
          inbox.append(row);
        }
        setNotice(items.length ? `收到 ${items.length} 项新传输内容` : "收件箱暂无新内容", "ok");
        return;
      }

      if (cmd === "personalization-clear") {
        renderHabitStats(data);
        setNotice("已清空个人习惯记忆（置顶词已保留）", "ok");
      } else if (cmd === "poll-services") {
        try {
          const usage = await api("storage");
          renderUsage(usage);
        } catch {}
        setNotice("已完成与 Koofr 云端同步", "ok");
      } else if (cmd === "restore-from-koofr") {
        setNotice(`已从云端恢复 ${data.snapshots_applied} 项数据`, "ok");
      } else if (cmd === "clipboard-clear-unpinned") {
        setNotice("已清空未置顶剪贴板记录", "ok");
      } else if (cmd === "relay-discover") {
        setNotice(data.length ? `已发现 ${data.length} 台设备` : "已开启接收，尚未发现周边设备", "ok");
      } else if (cmd === "relay-stop-discovery") {
        setNotice("已关闭局域网接收", "ok");
      } else {
        setNotice("已完成", "ok");
      }
    });
  };
});

// ================= LEXICON IMPORT =================
async function importPaths(paths) {
  let total = 0;
  const log = $("#import-log");
  for (const path of paths) {
    const r = await api("import-lexicon", { path });
    total += r.imported_count || 0;
    const li = document.createElement("li");
    const name = path.split(/[/\\]/).pop() || path;
    const left = document.createElement("span");
    left.textContent = name;
    const right = document.createElement("strong");
    right.textContent = `+${r.imported_count} 词条`;
    li.append(left, right);
    log.prepend(li);
    log.hidden = false;
  }
  setNotice(`成功导入 ${total} 个词条`, "ok");
}

const dropBtn = $("#import");
dropBtn.onclick = () =>
  run(async () => {
    const path = await invoke("choose_file", { lexicon: true });
    if (path) await importPaths([path]);
    else setNotice("");
  });

window.__TAURI__?.event.listen("tauri://drag-enter", () => dropBtn.classList.add("is-dragover"));
window.__TAURI__?.event.listen("tauri://drag-leave", () => dropBtn.classList.remove("is-dragover"));
window.__TAURI__?.event.listen("tauri://drag-drop", e => {
  dropBtn.classList.remove("is-dragover");
  if (e.payload?.paths?.length) run(() => importPaths(e.payload.paths));
});

// ================= VOICE & AI WORKBENCH =================
const sourceInput = $("#source");
const updateCount = () => {
  $("#source-count").textContent = `${sourceInput.value.length} 字`;
};
sourceInput.addEventListener("input", updateCount);
updateCount();

const AI_LABELS = { translate: "翻译结果", polish: "润色结果", summary: "提炼摘要" };
all("[data-ai]").forEach(b => {
  b.onclick = () => {
    const text = sourceInput.value.trim();
    if (!text) {
      setNotice("请先输入或录制需要处理的原文", "error");
      sourceInput.focus();
      return;
    }
    run(async () => {
      const mode = b.dataset.ai;
      const r = await api("ai-inline", {
        mode,
        text,
        target_lang: $("#target-lang").value,
      });
      $("#result-title").textContent = AI_LABELS[mode] || "处理结果";
      $("#result").textContent = r.output_text;
      $("#result-card").hidden = false;
      setNotice("处理完成", "ok");
    });
  };
});

$("#copy-result").onclick = async () => {
  const txt = $("#result").textContent;
  if (!txt) return;
  try {
    await navigator.clipboard.writeText(txt);
    setNotice("已复制处理结果", "ok");
  } catch {
    setNotice("复制失败", "error");
  }
};
$("#apply-result").onclick = () => {
  const txt = $("#result").textContent;
  if (!txt) return;
  sourceInput.value = txt;
  updateCount();
  setNotice("已替换至原文输入框", "ok");
};

function startTimer() {
  recordStartedAt = Date.now();
  $("#rec-timer").hidden = false;
  const tick = () => {
    const secs = Math.floor((Date.now() - recordStartedAt) / 1000);
    $("#rec-elapsed").textContent =
      `${String(Math.floor(secs / 60)).padStart(2, "0")}:${String(secs % 60).padStart(2, "0")}`;
  };
  tick();
  recordTimer = setInterval(tick, 500);
}
function stopTimer() {
  clearInterval(recordTimer);
  recordTimer = null;
  $("#rec-timer").hidden = true;
}

$("#record").onclick = () =>
  run(async () => {
    if (recorderActive) {
      recorderActive = false;
      stopTimer();
      $("#record").textContent = "开始录音";
      const r = await invoke("stop_recording");
      sourceInput.value = r.transcript || "";
      updateCount();
      setNotice(r.transcript ? "本机 SenseVoice 转写完成" : "未识别到有效语音", "ok");
    } else {
      await invoke("start_recording");
      recorderActive = true;
      startTimer();
      $("#record").textContent = "停止并转写";
      setNotice("正在录音，再次点击即可离线转写", "busy");
    }
  });

// ================= SHORTCUTS =================
function checkShortcutConflicts() {
  const rows = all("[data-shortcut]");
  const seen = new Map();
  const errors = [];
  rows.forEach(r => r.classList.remove("has-conflict"));

  for (const row of rows) {
    const keyVal = row.querySelector("[data-binding=key]").value.trim();
    const textVal = row.querySelector("[data-binding=text]").value.trim().toLowerCase();
    const label = row.querySelector(".shortcut-title").textContent;
    if (keyVal) {
      if (seen.has(keyVal)) {
        const prev = seen.get(keyVal);
        prev.row.classList.add("has-conflict");
        row.classList.add("has-conflict");
        errors.push(`快捷键 ${keyVal} 同时绑定了「${prev.label}」和「${label}」`);
      } else {
        seen.set(keyVal, { row, label });
      }
    }
    if (textVal && !/^[a-z]+$/.test(textVal)) {
      row.classList.add("has-conflict");
      errors.push(`「${label}」的功能输入码仅支持英文字母（a–z）`);
    }
  }

  const alertBox = $("#shortcut-conflict");
  alertBox.hidden = errors.length === 0;
  alertBox.textContent = errors[0] || "";
  return errors;
}

function renderShortcuts(settings) {
  const rules = $("#shortcut-rules");
  rules.replaceChildren();
  recordingShortcut = null;

  for (const action of settings.shortcut_actions || []) {
    const binding = (settings.shortcuts || []).find(b => b.action === action.id) || {};
    const card = document.createElement("fieldset");
    card.className = "shortcut-card";
    card.dataset.shortcut = action.id;

    const legend = document.createElement("legend");
    legend.textContent = action.label;
    card.append(legend);

    const top = document.createElement("div");
    top.className = "shortcut-top";
    const title = document.createElement("span");
    title.className = "shortcut-title";
    title.textContent = action.label;
    const desc = document.createElement("span");
    desc.className = "shortcut-desc";
    desc.textContent = ACTION_DESCRIPTIONS[action.id] || "";
    top.append(title, desc);

    const grid = document.createElement("div");
    grid.className = "shortcut-grid";

    // Physical Key Field
    const keyField = document.createElement("div");
    keyField.className = "shortcut-field";
    const keyTitle = document.createElement("span");
    keyTitle.className = "shortcut-field-label";
    keyTitle.textContent = "全局实体按键";
    const group = document.createElement("div");
    group.className = "shortcut-key-controls";

    const key = document.createElement("input");
    key.dataset.binding = "key";
    key.value = binding.key || "";
    key.readOnly = true;
    key.placeholder = "未绑定";
    key.setAttribute("aria-label", `${action.label}按键`);

    const recordBtn = document.createElement("button");
    recordBtn.type = "button";
    recordBtn.className = "btn-mini";
    recordBtn.textContent = "录制";
    recordBtn.setAttribute("aria-label", `录制${action.label}按键`);
    recordBtn.onclick = () => {
      if (recordingShortcut) finishShortcut(null);
      recordingShortcut = { key, record: recordBtn, card, shift: false };
      card.classList.add("is-recording");
      recordBtn.textContent = "请按键…";
      setNotice("请按下组合键（或单按 Shift）；Esc 取消，Backspace 清除", "busy");
    };

    const clearBtn = document.createElement("button");
    clearBtn.type = "button";
    clearBtn.className = "btn-mini";
    clearBtn.textContent = "清除";
    clearBtn.setAttribute("aria-label", `清除${action.label}按键`);
    clearBtn.onclick = () => {
      key.value = "";
      checkShortcutConflicts();
      markDirty();
    };

    group.append(key, recordBtn, clearBtn);
    keyField.append(keyTitle, group);

    // Text Input Code Field
    const textField = document.createElement("div");
    textField.className = "shortcut-field";
    const textTitle = document.createElement("span");
    textTitle.className = "shortcut-field-label";
    textTitle.textContent = "拼音功能输入码（纯字母）";
    const text = document.createElement("input");
    text.dataset.binding = "text";
    text.value = binding.text || "";
    text.placeholder = action.id === "settings" ? "例如 setting" : "留空关闭";
    text.autocomplete = "off";
    text.spellcheck = false;
    text.autocapitalize = "none";
    text.setAttribute("aria-label", `${action.label}输入码`);
    text.oninput = () => {
      checkShortcutConflicts();
      markDirty();
    };
    textField.append(textTitle, text);

    grid.append(keyField, textField);
    card.append(top, grid);
    rules.append(card);
  }
  checkShortcutConflicts();
}

$("#reset-shortcuts").onclick = () => {
  const defaults = saved.shortcut_defaults || [
    { action: "toggle_language", key: "Shift", text: "" },
    { action: "settings", key: "", text: "setting" },
    { action: "clipboard", key: "", text: "" },
    { action: "lookup", key: "", text: "" },
    { action: "handwriting", key: "", text: "" },
  ];
  renderShortcuts({ ...saved, shortcuts: defaults });
  markDirty();
};

function readShortcuts() {
  return all("[data-shortcut]").map(row => ({
    action: row.dataset.shortcut,
    key: row.querySelector("[data-binding=key]").value,
    text: row.querySelector("[data-binding=text]").value.trim().toLowerCase(),
  }));
}

function finishShortcut(key) {
  const capture = recordingShortcut;
  if (!capture) return;
  capture.card.classList.remove("is-recording");
  capture.record.textContent = "录制";
  recordingShortcut = null;
  if (key !== null) {
    capture.key.value = key;
    checkShortcutConflicts();
    markDirty();
  } else {
    setNotice(dirty ? "有未保存的更改" : "已取消录制", dirty ? "dirty" : "idle");
  }
}

window.addEventListener(
  "keydown",
  e => {
    if (!recordingShortcut) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape") return finishShortcut(null);
    if (e.key === "Backspace" || e.key === "Delete") return finishShortcut("");
    if (e.key === "Shift") {
      recordingShortcut.shift = !e.ctrlKey && !e.altKey && !e.metaKey;
      return;
    }
    recordingShortcut.shift = false;
    if (["Control", "Alt", "Meta"].includes(e.key)) return;
    if (e.metaKey || (!e.ctrlKey && !e.altKey && !/^F([1-9]|1[0-2])$/.test(e.key))) {
      setNotice("请使用 Ctrl/Alt 组合键、F1–F12，或单按 Shift", "error");
      return;
    }
    const base = e.key === " " ? "Space" : e.key === "Enter" ? "Enter" : e.key.toUpperCase();
    const parts = [e.ctrlKey && "Ctrl", e.altKey && "Alt", e.shiftKey && "Shift", base].filter(Boolean);
    finishShortcut(parts.join("+"));
  },
  true
);

window.addEventListener(
  "keyup",
  e => {
    if (recordingShortcut && e.key === "Shift" && recordingShortcut.shift) {
      e.preventDefault();
      finishShortcut("Shift");
    }
  },
  true
);

window.addEventListener("blur", () => {
  if (recordingShortcut) finishShortcut(null);
});

// ================= BOOT =================
if (invoke) {
  invoke("get_settings")
    .then(settings => {
      apply(settings);
      api("personalization-stats")
        .then(stats => {
          updateEngineStatus(true);
          renderHabitStats(stats);
        })
        .catch(() => updateEngineStatus(false));
    })
    .catch(e => {
      setNotice(String(e), "error");
      updateEngineStatus(false);
    });
} else {
  apply({
    schema: "double_pinyin_flypy",
    opencc_mode: "simplified",
    frecency_half_life_sec: 604800,
    candidate_font_px: 16,
    theme: "system",
    clipboard_ring_capacity: 50,
    pure_offline: false,
    linux_english_mode: true,
    shortcut_actions: [
      { id: "toggle_language", label: "切换中英文" },
      { id: "settings", label: "打开设置" },
      { id: "clipboard", label: "打开剪贴板" },
      { id: "lookup", label: "打开查词" },
      { id: "handwriting", label: "打开手写" },
    ],
    shortcuts: [
      { action: "toggle_language", key: "Shift", text: "" },
      { action: "settings", key: "", text: "setting" },
      { action: "clipboard", key: "Ctrl+Shift+V", text: "" },
      { action: "lookup", key: "", text: "" },
      { action: "handwriting", key: "", text: "" },
    ],
  });
  updateEngineStatus(false);
}
