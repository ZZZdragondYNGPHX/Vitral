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
let modelsState = null;
let modelsLoaded = false;
let modelsPollTimer = null;
let activeDownloads = new Set();
let confirmDeleteModel = { id: null, timer: null };

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
  if (out) {
    out.value =
      input.name === "candidate_font_px"
        ? `${val} px`
        : input.name === "prediction_delay_ms"
          ? `${val} ms`
          : `${val} 条`;
  }
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
  const modelsOffline = $("#models-offline-hint");
  if (modelsOffline) modelsOffline.hidden = !offline;
  const badge = $("#offline-badge");
  badge.textContent = offline ? "网络已阻断" : "协同就绪";
  badge.className = `badge ${offline ? "is-warn" : "is-ok"}`;

  all(".online").forEach(el => {
    el.disabled = busy || offline;
  });
  if (modelsLoaded) renderModels();
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

  const fuzzyGroup = $("#fuzzy-pairs-group");
  if (fuzzyGroup) {
    const enabled = Boolean(settings.fuzzy_pinyin);
    fuzzyGroup.hidden = !enabled;
    const selected = new Set(Array.isArray(settings.fuzzy_pinyin_pairs) ? settings.fuzzy_pinyin_pairs : []);
    for (const box of all("[data-fuzzy-pair]")) {
      box.checked = selected.has(box.dataset.fuzzyPair);
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
  if (tabId === "cloud" && invoke) {
    api("sync-status").then(renderSyncHealth).catch(() => {});
  }
  if (tabId === "models") {
    loadModels(true);
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

all("[data-fuzzy-pair]").forEach(box => {
  box.onchange = () => markDirty();
});
const fuzzyMaster = $("#fuzzy-pinyin-toggle");
if (fuzzyMaster) {
  fuzzyMaster.addEventListener("change", () => {
    const g = $("#fuzzy-pairs-group");
    if (g) g.hidden = !fuzzyMaster.checked;
  });
}

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
    patch.fuzzy_pinyin_pairs = all("[data-fuzzy-pair]")
      .filter(box => box.checked)
      .map(box => box.dataset.fuzzyPair);
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

function tokenizeClipboardText(text) {
  const bounded = String(text || "").slice(0, 1500);
  const re = /https?:\/\/[^\s]+|[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}|[A-Za-z0-9_./:-]+|[\u4E00-\u9FFF]+|[^\s\u4E00-\u9FFFA-Za-z0-9]+/g;
  const out = [];
  for (const m of bounded.matchAll(re)) {
    if (out.length >= 48) break;
    const tok = m[0].trim();
    if (!tok) continue;
    if (/^[\u4E00-\u9FFF]+$/.test(tok) && tok.length > 4) {
      let i = 0;
      while (i < tok.length && out.length < 48) {
        const rem = tok.length - i;
        const step = rem <= 4 ? rem : rem === 5 ? 2 : 3;
        out.push(tok.slice(i, i + step));
        i += step;
      }
    } else {
      out.push(tok);
    }
  }
  return out;
}

function createClipRow(item) {
  const row = document.createElement("li");
  row.className = `clip-item${item.pinned ? " is-pinned" : ""}`;

  const body = document.createElement("div");
  body.className = "clip-body";

  const text = document.createElement("div");
  text.className = "clip-text";
  text.textContent = item.content.length > 420 ? `${item.content.slice(0, 420)}…` : item.content;
  text.title = item.content.length > 420 ? `${item.content.slice(0, 420)}…（共 ${item.content.length} 字符）` : item.content;

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
      const rawCode = code.value.trim();
      if (rawCode && !/^[A-Za-z]{1,16}$/.test(rawCode)) {
        setNotice("简码请使用 1–16 位英文字母，不含数字、符号或空格", "error");
        code.focus();
        return;
      }
      const shortcutCode = rawCode.toLowerCase();
      code.value = shortcutCode;
      const r = await api("clipboard-pin", {
        id: item.id,
        shortcut_code: shortcutCode || null,
      });
      clipboardEntries = r.ring || [];
      renderClipboard();
      setNotice(item.pinned ? "已更新简码" : "已置顶收藏", "ok");
    });

  const splitBtn = document.createElement("button");
  splitBtn.type = "button";
  splitBtn.className = "btn-mini";
  splitBtn.textContent = "分词";
  let tokenDrawer = null;
  splitBtn.onclick = () => {
    if (tokenDrawer) {
      tokenDrawer.remove();
      tokenDrawer = null;
      splitBtn.textContent = "分词";
      return;
    }
    const tokens = tokenizeClipboardText(item.content);
    if (!tokens.length) return;
    tokenDrawer = document.createElement("div");
    tokenDrawer.style.cssText = "display:flex;flex-wrap:wrap;gap:6px;margin-top:8px;align-items:center;";
    const picked = new Set();
    const copyPickedBtn = document.createElement("button");
    copyPickedBtn.type = "button";
    copyPickedBtn.className = "btn-mini";
    copyPickedBtn.textContent = "复制选中 (0)";
    copyPickedBtn.disabled = true;
    copyPickedBtn.onclick = async () => {
      const joined = tokens.filter((_, i) => picked.has(i)).join("");
      if (!joined) return;
      try {
        await navigator.clipboard.writeText(joined);
        setNotice(`已复制 ${joined.length} 字符片段`, "ok");
      } catch {
        setNotice("无法写入系统剪贴板", "error");
      }
    };
    tokens.forEach((tok, idx) => {
      const chip = document.createElement("button");
      chip.type = "button";
      chip.className = "btn-mini";
      chip.textContent = tok;
      chip.onclick = () => {
        if (picked.has(idx)) {
          picked.delete(idx);
          chip.style.fontWeight = "";
          chip.style.outline = "";
        } else {
          picked.add(idx);
          chip.style.fontWeight = "600";
          chip.style.outline = "1.5px solid var(--accent, #0ea5e9)";
        }
        copyPickedBtn.textContent = `复制选中 (${picked.size})`;
        copyPickedBtn.disabled = picked.size === 0;
      };
      tokenDrawer.append(chip);
    });
    tokenDrawer.append(copyPickedBtn);
    body.append(tokenDrawer);
    splitBtn.textContent = "收起";
  };

  actions.append(copyBtn, splitBtn, code, pinBtn);

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
      if (cmd === "sync-status") {
        renderSyncHealth(data);
        setNotice("已刷新跨端同步链路状态", "ok");
        return;
      }
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
          const [usage, health] = await Promise.all([api("storage"), api("sync-status")]);
          renderUsage(usage);
          renderSyncHealth(health);
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

function renderSyncHealth(status) {
  const el = $("#sync-health");
  if (!el || !status) return;
  const fmtTs = ms => (ms > 0 ? new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" }) : "尚未同步");
  const lanText = status.pure_offline
    ? "纯离线已阻断"
    : status.last_lan_error
      ? `异常：${status.last_lan_error}`
      : `${status.lan_peer_count || 0} 台节点 · ${fmtTs(status.last_lan_sync_ms)}`;
  const cloudText = status.pure_offline
    ? "纯离线已阻断"
    : !status.cloud_configured
      ? "未配置 WebDAV"
      : status.last_cloud_error
        ? `异常：${status.last_cloud_error}`
        : `正常 · ${fmtTs(status.last_cloud_sync_ms)}`;
  const cells = [
    ["局域网通道", lanText],
    ["私有云通道", cloudText],
    ["本地词库快照", `${status.backup_count || 0} 份滚动备份`],
  ];
  el.replaceChildren();
  for (const [k, v] of cells) {
    const div = document.createElement("div");
    const dt = document.createElement("dt");
    dt.textContent = k;
    const dd = document.createElement("dd");
    dd.textContent = v;
    div.append(dt, dd);
    el.append(div);
  }
  el.hidden = false;
}

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

// ================= MODEL CENTER =================
const CAPABILITY_LABELS = {
  grammar: "语法增强",
  handwriting: "手写识别",
  reranker: "语义重排",
  voice: "离线语音",
  local_llm: "本地智能",
};

const RAM_TIER_LABELS = {
  low: "轻量档位",
  standard: "标准档位",
  high: "高性能档位",
  unknown: "内存未知",
};

function formatRam(mb) {
  const n = Number(mb) || 0;
  if (n <= 0) return "未知";
  return n >= 1024 ? `${(n / 1024).toFixed(n % 1024 === 0 ? 0 : 1)} GB` : `${n} MB`;
}

function modelTotalBytes(model) {
  return (model.files || []).reduce((sum, f) => sum + (Number(f.size) || 0), 0);
}

function stopModelsPolling() {
  if (modelsPollTimer) {
    clearInterval(modelsPollTimer);
    modelsPollTimer = null;
  }
}

function syncModelsPolling(state) {
  const models = state?.models || [];
  const nextDownloading = new Set();
  for (const m of models) {
    const status = m.download?.status;
    if (status === "downloading") {
      nextDownloading.add(m.id);
    } else if (activeDownloads.has(m.id)) {
      if (status === "completed") {
        setNotice(`模型「${m.name}」已下载并热加载启用`, "ok");
      } else if (status === "failed") {
        setNotice(`模型「${m.name}」下载失败：${m.download?.error || "未知错误"}`, "error");
      } else if (status === "cancelled") {
        setNotice(`已取消下载「${m.name}」`, "idle");
      }
    }
  }
  activeDownloads = nextDownloading;

  if (activeDownloads.size > 0 && invoke) {
    if (!modelsPollTimer) {
      modelsPollTimer = setInterval(() => {
        api("models-state")
          .then(next => {
            modelsState = next;
            modelsLoaded = true;
            renderModels();
            syncModelsPolling(next);
          })
          .catch(() => stopModelsPolling());
      }, 600);
    }
  } else {
    stopModelsPolling();
  }
}

const MODEL_GROUP_DEFS = [
  { id: "input", title: "输入与语言增强", subtitle: "万象长句语法解码与上下文语义重排", caps: ["grammar", "reranker"] },
  { id: "multimodal", title: "多模态交互", subtitle: "离线手写认字与本地语音转写", caps: ["handwriting", "voice"] },
  { id: "ai", title: "本地大模型与智能预测", subtitle: "端侧语境预测、用户画像蒸馏与离线 AI 工具", caps: ["local_llm"] },
];
const MODEL_CAP_DEFS = {
  grammar: { title: "中文语法模型", subtitle: "提升全拼与双拼长句切分及语境排序" },
  reranker: { title: "语义重排模型", subtitle: "连续手写模糊字位重排与语义增强" },
  handwriting: { title: "离线手写识别", subtitle: "单字查音与连续手写联句（含生僻字索引）" },
  voice: { title: "离线语音转写", subtitle: "按硬件档位选择轻快均衡或高精多语方言" },
  local_llm: { title: "本地 Qwen3 推理", subtitle: "按内存档位选择低 / 中 / 高精端侧预测与 AI" },
};
let modelNav = { group: null, cap: null };

function modelTierLabel(m) {
  const rec = Number(m.recommended_ram_mb) || 0;
  if (rec <= 2048) return "低配 · 轻量档";
  if (rec <= 6144) return "中配 · 均衡档";
  return "高配 · 旗舰档";
}

function renderModels() {
  const list = $("#models-list");
  const empty = $("#models-empty");
  const ramBadge = $("#ram-tier-badge");
  const storeSub = $("#models-store-sub");
  if (!list || !empty) return;

  if (!modelsState) {
    list.replaceChildren();
    empty.hidden = false;
    empty.textContent = invoke ? "尚未读取到模型状态，请确认输入服务已运行后点击「刷新状态」" : "界面预览模式";
    return;
  }

  const tier = modelsState.ram_tier || "unknown";
  const ramText = modelsState.total_ram_mb > 0 ? formatRam(modelsState.total_ram_mb) : "";
  ramBadge.textContent = ramText ? `${RAM_TIER_LABELS[tier] || tier} · ${ramText}` : (RAM_TIER_LABELS[tier] || tier);
  ramBadge.className = `badge ${tier === "low" ? "is-warn" : tier === "high" ? "is-ok" : "is-accent"}`;

  if (modelsState.store_root) {
    storeSub.textContent = `存储目录：${modelsState.store_root} · 下载完成或切换开关后自动热加载生效`;
  }

  const offline = $("[name=pure_offline]").checked;
  const models = modelsState.models || [];
  list.replaceChildren();
  empty.hidden = models.length > 0;
  if (!models.length) {
    empty.textContent = "模型目录为空";
    return;
  }

  const knownCaps = new Set(MODEL_GROUP_DEFS.flatMap(g => g.caps));
  const extraCaps = [...new Set(models.map(m => m.capability).filter(c => c && !knownCaps.has(c)))];
  const groups = extraCaps.length
    ? [...MODEL_GROUP_DEFS, { id: "other", title: "扩展模型", subtitle: "自定义目录中的扩展能力模型", caps: extraCaps }]
    : MODEL_GROUP_DEFS;

  if (modelNav.group) {
    const curGroup = groups.find(g => g.id === modelNav.group);
    const navBar = document.createElement("div");
    navBar.className = "model-card";
    navBar.style.display = "flex";
    navBar.style.justifyContent = "space-between";
    navBar.style.alignItems = "center";
    navBar.style.padding = "10px 14px";

    const crumbs = document.createElement("div");
    crumbs.style.display = "flex";
    crumbs.style.gap = "6px";
    crumbs.style.alignItems = "center";
    const rootLink = document.createElement("button");
    rootLink.type = "button";
    rootLink.className = "btn-ghost";
    rootLink.textContent = "模型目录";
    rootLink.onclick = () => { modelNav = { group: null, cap: null }; renderModels(); };
    crumbs.append(rootLink);
    if (curGroup) {
      const sep1 = document.createElement("span");
      sep1.textContent = "›";
      const grpLink = document.createElement("button");
      grpLink.type = "button";
      grpLink.className = "btn-ghost";
      grpLink.textContent = curGroup.title;
      grpLink.onclick = () => { modelNav = { group: curGroup.id, cap: null }; renderModels(); };
      crumbs.append(sep1, grpLink);
    }
    if (modelNav.cap) {
      const sep2 = document.createElement("span");
      sep2.textContent = "›";
      const capSpan = document.createElement("strong");
      capSpan.textContent = MODEL_CAP_DEFS[modelNav.cap]?.title || CAPABILITY_LABELS[modelNav.cap] || modelNav.cap;
      crumbs.append(sep2, capSpan);
    }
    const backBtn = document.createElement("button");
    backBtn.type = "button";
    backBtn.className = "btn-ghost";
    backBtn.textContent = "← 返回上级";
    backBtn.onclick = () => {
      if (modelNav.cap) modelNav = { group: modelNav.group, cap: null };
      else modelNav = { group: null, cap: null };
      renderModels();
    };
    navBar.append(crumbs, backBtn);
    list.append(navBar);
  }

  // Level 1: Primary Category Directories
  if (!modelNav.group) {
    for (const grp of groups) {
      const grpModels = models.filter(m => grp.caps.includes(m.capability));
      if (!grpModels.length) continue;
      const activeCnt = grpModels.filter(m => m.active).length;
      const installedCnt = grpModels.filter(m => m.installed || m.bundled).length;
      const card = document.createElement("article");
      card.className = `model-card${activeCnt > 0 ? " is-active" : ""}`;
      card.style.cursor = "pointer";
      card.onclick = () => {
        const availCaps = grp.caps.filter(c => grpModels.some(m => m.capability === c));
        modelNav = { group: grp.id, cap: availCaps.length === 1 ? availCaps[0] : null };
        renderModels();
      };
      const head = document.createElement("div");
      head.className = "model-head";
      const tg = document.createElement("div");
      tg.className = "model-title-group";
      const tl = document.createElement("div");
      tl.className = "model-title-line";
      const t = document.createElement("span");
      t.className = "model-title";
      t.textContent = grp.title;
      const b = document.createElement("span");
      b.className = activeCnt > 0 ? "badge is-ok" : "badge";
      b.textContent = activeCnt > 0 ? `已启用 ${activeCnt} 项` : `共 ${grpModels.length} 个模型`;
      tl.append(t, b);
      if (installedCnt > 0) {
        const ib = document.createElement("span");
        ib.className = "badge is-accent";
        ib.textContent = `已安装 ${installedCnt}`;
        tl.append(ib);
      }
      const d = document.createElement("p");
      d.className = "model-desc";
      d.textContent = grp.subtitle;
      tg.append(tl, d);
      const arrow = document.createElement("span");
      arrow.className = "badge";
      arrow.textContent = "进入 ›";
      head.append(tg, arrow);
      card.append(head);
      list.append(card);
    }
    return;
  }

  // Level 2: Sub-feature Directories
  if (!modelNav.cap) {
    const curGroup = groups.find(g => g.id === modelNav.group);
    const subCaps = (curGroup?.caps || []).filter(c => models.some(m => m.capability === c));
    for (const cap of subCaps) {
      const capModels = models.filter(m => m.capability === cap).sort((a, b) => (a.recommended_ram_mb || 0) - (b.recommended_ram_mb || 0));
      const def = MODEL_CAP_DEFS[cap] || { title: CAPABILITY_LABELS[cap] || cap, subtitle: "" };
      const activeCnt = capModels.filter(m => m.active).length;
      const tiers = [...new Set(capModels.map(modelTierLabel))].join(" / ");
      const card = document.createElement("article");
      card.className = `model-card${activeCnt > 0 ? " is-active" : ""}`;
      card.style.cursor = "pointer";
      card.onclick = () => { modelNav = { group: modelNav.group, cap }; renderModels(); };
      const head = document.createElement("div");
      head.className = "model-head";
      const tg = document.createElement("div");
      tg.className = "model-title-group";
      const tl = document.createElement("div");
      tl.className = "model-title-line";
      const t = document.createElement("span");
      t.className = "model-title";
      t.textContent = def.title;
      const tb = document.createElement("span");
      tb.className = "badge is-accent";
      tb.textContent = tiers;
      const sb = document.createElement("span");
      sb.className = activeCnt > 0 ? "badge is-ok" : "badge";
      sb.textContent = activeCnt > 0 ? "已启用" : "未启用";
      tl.append(t, tb, sb);
      const d = document.createElement("p");
      d.className = "model-desc";
      d.textContent = def.subtitle;
      tg.append(tl, d);
      const arrow = document.createElement("span");
      arrow.className = "badge";
      arrow.textContent = "查看档位 ›";
      head.append(tg, arrow);
      card.append(head);
      list.append(card);
    }
    return;
  }

  // Level 3: Tiered Models (Low / Mid / High)
  if (modelNav.cap === "local_llm") {
    const builtin = document.createElement("article");
    builtin.className = "model-card is-active";
    builtin.innerHTML = `<div class="model-head"><div class="model-title-group"><div class="model-title-line"><span class="model-title">低配 · 内置算法预测引擎</span><span class="badge is-accent">低配 · 零体积</span><span class="badge is-ok">全机型推荐</span></div><p class="model-desc">无需下载大模型，基于本地词典前缀续写与个人习惯记忆实现零延迟预测</p><div class="model-meta">体积 0 B · 纯本地即时生效</div></div><span class="badge is-ok">内置已启用</span></div>`;
    list.append(builtin);
  }

  const visibleModels = models
    .filter(m => m.capability === modelNav.cap)
    .sort((a, b) => (a.recommended_ram_mb || 0) - (b.recommended_ram_mb || 0));

  for (const m of visibleModels) {
    const card = document.createElement("article");
    card.className = `model-card${m.active ? " is-active" : ""}`;
    const dl = m.download;
    const isDownloading = dl?.status === "downloading";
    const totalBytes = modelTotalBytes(m);

    const head = document.createElement("div");
    head.className = "model-head";

    const titleGroup = document.createElement("div");
    titleGroup.className = "model-title-group";

    const titleLine = document.createElement("div");
    titleLine.className = "model-title-line";
    const title = document.createElement("span");
    title.className = "model-title";
    title.textContent = m.name;
    titleLine.append(title);

    const tierTag = document.createElement("span");
    tierTag.className = "badge is-accent";
    tierTag.textContent = modelTierLabel(m);
    titleLine.append(tierTag);

    const fitTag = document.createElement("span");
    if (m.fit === "recommended") {
      fitTag.className = "badge is-ok";
      fitTag.textContent = "内存推荐";
    } else if (m.fit === "compatible") {
      fitTag.className = "badge";
      fitTag.textContent = "内存兼容";
    } else {
      fitTag.className = "badge is-warn";
      fitTag.textContent = `超出推荐内存（最低 ${formatRam(m.min_ram_mb)}）`;
    }
    titleLine.append(fitTag);

    const desc = document.createElement("p");
    desc.className = "model-desc";
    desc.textContent = m.description;

    const meta = document.createElement("div");
    meta.className = "model-meta";
    meta.textContent = [
      `版本 ${m.version}`,
      `体积 ${formatBytes(totalBytes)}`,
      `内存要求 ${formatRam(m.min_ram_mb)} / 推荐 ${formatRam(m.recommended_ram_mb)}`,
      m.license,
    ].filter(Boolean).join(" · ");

    titleGroup.append(titleLine, desc, meta);

    const statusBadge = document.createElement("span");
    if (isDownloading) {
      statusBadge.className = "badge is-accent";
      statusBadge.textContent = "下载中";
    } else if (m.active) {
      statusBadge.className = "badge is-ok";
      statusBadge.textContent = m.installed ? "已下载 · 已启用" : "本地内置 · 已启用";
    } else if (m.installed || m.bundled) {
      statusBadge.className = "badge";
      statusBadge.textContent = m.installed ? "已下载 · 未启用" : "本地内置 · 未启用";
    } else {
      statusBadge.className = "badge";
      statusBadge.textContent = "未下载";
    }

    head.append(titleGroup, statusBadge);
    card.append(head);

    if (isDownloading) {
      const box = document.createElement("div");
      box.className = "model-progress";
      const dlTotal = Number(dl.total_bytes) || totalBytes || 1;
      const dlDone = Math.min(Number(dl.downloaded_bytes) || 0, dlTotal);
      const pct = Math.min(100, Math.round((dlDone / dlTotal) * 100));

      const top = document.createElement("div");
      top.className = "model-progress-top";
      const fileLabel = document.createElement("span");
      fileLabel.textContent = dl.current_file ? `正在下载 ${dl.current_file}` : "正在校验与下载…";
      const bytesLabel = document.createElement("span");
      bytesLabel.textContent = `${formatBytes(dlDone)} / ${formatBytes(dlTotal)} (${pct}%)`;
      top.append(fileLabel, bytesLabel);

      const bar = document.createElement("div");
      bar.className = "model-bar";
      const fill = document.createElement("div");
      fill.className = "model-bar-fill";
      fill.style.width = `${pct}%`;
      bar.append(fill);

      box.append(top, bar);
      card.append(box);
    } else if (dl?.status === "failed" && dl.error) {
      const err = document.createElement("div");
      err.className = "model-error";
      err.textContent = `上次下载失败：${dl.error}`;
      card.append(err);
    }

    const foot = document.createElement("div");
    foot.className = "model-foot";

    const toggleWrap = document.createElement("label");
    toggleWrap.className = "model-toggle";
    const toggle = document.createElement("input");
    toggle.type = "checkbox";
    toggle.className = "switch";
    toggle.checked = Boolean(m.enabled && (m.installed || m.bundled));
    toggle.disabled = busy || isDownloading || (!m.installed && !m.bundled);
    toggle.setAttribute("aria-label", `启用${m.name}`);
    toggle.onchange = () => {
      const nextEnabled = toggle.checked;
      const replacedPeer = nextEnabled
        ? ((modelsState?.models || []).find(
            (x) => x.capability === m.capability && x.id !== m.id && x.active
          )?.name || "")
        : "";
      run(async () => {
        modelsState = await api("models-set-enabled", { id: m.id, enabled: nextEnabled });
        renderModels();
        syncModelsPolling(modelsState);
        setNotice(
          nextEnabled
            ? (replacedPeer
                ? `已切换为「${m.name}」，已自动关闭「${replacedPeer}」并释放缓存`
                : `已启用「${m.name}」并完成热加载`)
            : `已停用「${m.name}」并释放缓存`,
          "ok"
        );
      });
    };
    const toggleText = document.createElement("span");
    toggleText.textContent = m.installed || m.bundled ? (m.enabled ? "功能已启用" : "已暂停使用") : "下载后可启用";
    toggleWrap.append(toggle, toggleText);

    const actions = document.createElement("div");
    actions.className = "model-actions";

    if (isDownloading) {
      const cancelBtn = document.createElement("button");
      cancelBtn.type = "button";
      cancelBtn.className = "btn-danger-ghost";
      cancelBtn.textContent = "取消下载";
      cancelBtn.onclick = () =>
        run(async () => {
          modelsState = await api("models-cancel", { id: m.id });
          renderModels();
          syncModelsPolling(modelsState);
          setNotice(`已取消下载「${m.name}」`, "idle");
        });
      actions.append(cancelBtn);
    } else if (!m.installed) {
      const dlBtn = document.createElement("button");
      dlBtn.type = "button";
      dlBtn.className = "online btn-accent-soft";
      dlBtn.disabled = busy || offline;
      dlBtn.textContent =
        dl?.status === "failed"
          ? "重试下载"
          : m.fit === "exceeds_ram"
            ? "仍要下载"
            : m.bundled
              ? "下载到用户目录"
              : "下载并启用";
      dlBtn.onclick = () =>
        run(async () => {
          modelsState = await api("models-download", { id: m.id });
          renderModels();
          syncModelsPolling(modelsState);
          setNotice(`开始下载「${m.name}」…`, "busy");
        });
      actions.append(dlBtn);
    }

    if (m.installed && !isDownloading) {
      const delBtn = document.createElement("button");
      delBtn.type = "button";
      const confirming = confirmDeleteModel.id === m.id;
      delBtn.className = "btn-danger-ghost";
      delBtn.disabled = busy;
      delBtn.textContent = confirming ? "确认删除文件" : "删除模型";
      delBtn.onclick = () => {
        if (confirmDeleteModel.id !== m.id) {
          clearTimeout(confirmDeleteModel.timer);
          confirmDeleteModel = {
            id: m.id,
            timer: setTimeout(() => {
              confirmDeleteModel = { id: null, timer: null };
              renderModels();
            }, 3500),
          };
          renderModels();
          return;
        }
        clearTimeout(confirmDeleteModel.timer);
        confirmDeleteModel = { id: null, timer: null };
        run(async () => {
          modelsState = await api("models-delete", { id: m.id });
          renderModels();
          syncModelsPolling(modelsState);
          setNotice(`已删除「${m.name}」并释放空间`, "ok");
        });
      };
      actions.append(delBtn);
    }

    foot.append(toggleWrap, actions);
    card.append(foot);
    list.append(card);
  }
}

async function loadModels(silent = false) {
  if (!invoke) {
    modelsState = {
      total_ram_mb: 16384,
      ram_tier: "high",
      store_root: "~/.local/share/vitral/model_store",
      models: [
        {
          id: "wanxiang-grammar",
          capability: "grammar",
          name: "万象中文语法模型",
          description: "提升全拼与双拼长句解码及语境排序准确度",
          version: "LTS",
          license: "CC-BY-4.0",
          min_ram_mb: 2048,
          recommended_ram_mb: 4096,
          files: [{ size: 390786092 }],
          fit: "recommended",
          installed: false,
          bundled: false,
          enabled: false,
          active: false,
          download: null,
        },
      ],
    };
    modelsLoaded = true;
    renderModels();
    return;
  }
  await run(
    async () => {
      modelsState = await api("models-state");
      modelsLoaded = true;
      renderModels();
      syncModelsPolling(modelsState);
      if (!silent) setNotice("已刷新模型状态", "ok");
    },
    { silent }
  );
}

$("#models-refresh")?.addEventListener("click", () => loadModels(false));
$("#models-update-catalog")?.addEventListener("click", () =>
  run(async () => {
    const customUrl = $("#model-catalog-url")?.value.trim() || "";
    const payload = customUrl ? { url: customUrl } : {};
    modelsState = await api("models-refresh-catalog", payload);
    modelsLoaded = true;
    renderModels();
    syncModelsPolling(modelsState);
    setNotice("模型目录已更新", "ok");
  })
);

// ================= LOCAL USER PROFILE =================
function splitTags(raw) {
  return String(raw || "")
    .split(/[,，、;；]/)
    .map(s => s.trim())
    .filter(Boolean);
}

function renderProfile(state) {
  const p = state?.profile;
  const badge = $("#profile-source-badge");
  const meta = $("#profile-meta");
  if (!badge || !meta) return;
  if (!p) return;

  const distilled = p.distilled_at_ms > 0 || p.revision > 0;
  badge.textContent = !distilled
    ? "尚未蒸馏"
    : p.source === "local_qwen"
      ? "本地 Qwen 蒸馏"
      : "本地统计画像";
  badge.className = `badge ${distilled ? (p.source === "local_qwen" ? "is-ok" : "is-accent") : ""}`;

  const bits = [`采纳 ${p.accepted_count || 0} 次`, `忽略 ${p.dismissed_count || 0} 次`];
  if (p.distilled_at_ms) bits.push(`更新于 ${formatTime(p.distilled_at_ms)}`);
  if (state?.needs_distill) bits.push("有新习惯可蒸馏");
  meta.textContent = bits.join(" · ");

  $("#profile-summary").value = p.summary || "";
  $("#profile-domains").value = (p.domains || []).join(", ");
  $("#profile-entities").value = (p.entities || []).join(", ");
  $("#profile-traits").value = (p.style_traits || []).join(", ");
}

$("#btn-profile-distill")?.addEventListener("click", () =>
  run(async () => {
    const state = await api("profile-distill");
    renderProfile(state);
    setNotice(
      state?.profile?.source === "local_qwen"
        ? "已完成本地 Qwen 画像蒸馏"
        : "已完成本地统计画像蒸馏",
      "ok"
    );
  })
);

$("#btn-profile-save")?.addEventListener("click", () =>
  run(async () => {
    const state = await api("profile-update", {
      summary: $("#profile-summary").value.trim(),
      domains: splitTags($("#profile-domains").value),
      entities: splitTags($("#profile-entities").value),
      style_traits: splitTags($("#profile-traits").value),
    });
    renderProfile(state);
    setNotice("个人画像已保存并即时生效", "ok");
  })
);

$("#btn-profile-clear")?.addEventListener("click", () =>
  run(async () => {
    const state = await api("profile-clear");
    renderProfile(state);
    setNotice("个人画像已清空", "ok");
  })
);

// ================= BOOT =================
if (invoke) {
  invoke("get_settings")
    .then(settings => {
      apply(settings);
      api("personalization-stats")
        .then(stats => {
          updateEngineStatus(true);
          renderHabitStats(stats);
          api("profile-state").then(renderProfile).catch(() => {});
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
