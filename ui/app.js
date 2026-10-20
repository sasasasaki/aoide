const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const clone = (v) => JSON.parse(JSON.stringify(v));

let saved;   // 已生效的 { config, autostart }
let state;   // 界面上的 { config, autostart }
let models = [];
let defaultModelDir = "";
let downloading = null;
let systemLanguage = "zh";
let defaultPrompts = {};
let defaults = null; // 默认值 { config, autostart }

// === 恢复默认 ===
const getValue = (key) => (key === "autostart" ? state.autostart : state.config[key]);
const defaultValue = (key) => (key === "autostart" ? defaults.autostart : defaults.config[key]);
const isDefault = (key) => JSON.stringify(getValue(key)) === JSON.stringify(defaultValue(key));

/** 卡片里的控件对应哪些配置项 */
function cardKeys(card) {
  const keys = new Set();
  card.querySelectorAll("[data-key]").forEach((el) => keys.add(el.dataset.key));
  card.querySelectorAll("[data-keys]").forEach((el) => keys.add(el.dataset.keys));
  card.querySelectorAll("[data-prompt]").forEach((el) => keys.add(el.dataset.prompt));
  if (card.querySelector("#autostart")) keys.add("autostart");
  return [...keys];
}

function resetKeys(keys) {
  const { model_dir: dirBefore, ui_language: langBefore } = state.config;
  for (const key of keys) {
    if (key === "autostart") state.autostart = defaults.autostart;
    else state.config[key] = clone(defaults.config[key]);
  }
  render();
  if (state.config.ui_language !== langBefore) applyLanguage();
  if (state.config.model_dir !== dirBefore) refreshModels();
  scheduleSave();
}

/** 给每张有设置项的卡片加一个 ↺ 按钮 */
function setupResetButtons() {
  document.querySelectorAll(".card").forEach((card) => {
    const keys = cardKeys(card);
    const control = card.querySelector(".control");
    if (!keys.length || !control) return;
    const btn = document.createElement("button");
    btn.className = "btn icon-btn reset-btn";
    btn.innerHTML = "&#xE7A7;";
    btn.dataset.i18nTitle = "btn.resetItem";
    btn.title = t("btn.resetItem");
    btn.dataset.resetKeys = keys.join(",");
    btn.addEventListener("click", () => resetKeys(keys));
    control.append(btn);
  });
}

function updateResetButtons() {
  if (!defaults) return;
  document.querySelectorAll(".reset-btn").forEach((b) => {
    b.classList.toggle("is-default", b.dataset.resetKeys.split(",").every(isDefault));
  });
}

/** 界面语言设置（auto / zh / en）→ 实际语言 */
const resolveLang = (setting) => (setting === "zh" || setting === "en" ? setting : systemLanguage);

// === 快捷键显示 ===
const KEY_LABELS = {
  ctrl: "Ctrl", alt: "Alt", shift: "Shift", windows: "Win", esc: "Esc", space: "Space",
  enter: "Enter", tab: "Tab", backspace: "Backspace", delete: "Delete", insert: "Insert",
  home: "Home", end: "End", "page up": "PgUp", "page down": "PgDn", "caps lock": "Caps Lock",
  "print screen": "PrtSc", "scroll lock": "Scroll Lock", pause: "Pause", menu: "Menu",
  up: "↑", down: "↓", left: "←", right: "→",
};

function keyLabel(k) {
  if (KEY_LABELS[k]) return KEY_LABELS[k];
  return k.replace(/\b\w/g, (c) => c.toUpperCase()).replace("Windows", "Win");
}

function renderKeys(el, text) {
  el.replaceChildren(...text.split("+").filter(Boolean).map((k) => {
    const kbd = document.createElement("kbd");
    kbd.textContent = keyLabel(k.trim());
    return kbd;
  }));
}

// === 渲染 ===
function formatSize(mb) {
  return mb >= 1000 ? `${(mb / 1024).toFixed(1)} GB` : `${mb} MB`;
}

function renderModels() {
  const sel = $("model");
  sel.replaceChildren(...["sense_voice", "whisper"].map((engine) => {
    const group = document.createElement("optgroup");
    group.label = t(`engine.${engine}`);
    group.append(...models.filter((m) => m.engine === engine).map((m) => {
      const o = document.createElement("option");
      o.value = m.id;
      o.textContent = `${m.id}${m.downloaded ? "  ✓" : ""}`;
      return o;
    }));
    return group;
  }));
  sel.value = state.config.model;
  renderModelStatus();
}

function renderModelStatus() {
  const m = models.find((x) => x.id === state.config.model);
  if (!m) return;
  const parts = [t(m.downloaded ? "model.downloaded" : "model.notDownloaded"), formatSize(m.size_mb)];
  const note = m.note[LANG === "en" ? 1 : 0];
  if (note) parts.push(note);
  $("model-desc").textContent = parts.join(" · ");
  $("download-btn").hidden = m.downloaded || downloading !== null;

  // SenseVoice 固定在 CPU 上运行
  const cpuOnly = m.engine === "sense_voice";
  $("device-seg").classList.toggle("disabled", cpuOnly);
  $("device-seg").querySelectorAll("button").forEach((b) =>
    b.classList.toggle("on", b.dataset.value === (cpuOnly ? "cpu" : state.config.device)));
  $("device-desc").textContent = t(cpuOnly ? "device.cpuOnly" : "device.gpu");
}

function render() {
  const c = state.config;
  document.querySelectorAll("[data-keys]").forEach((el) => renderKeys(el, c[el.dataset.keys]));
  document.querySelectorAll("input[data-key], select[data-key]").forEach((el) => {
    if (el.type === "checkbox") el.checked = !!c[el.dataset.key];
    else el.value = c[el.dataset.key] ?? "";
  });
  document.querySelectorAll(".segmented[data-key]").forEach((seg) => {
    seg.querySelectorAll("button").forEach((b) => b.classList.toggle("on", b.dataset.value === c[seg.dataset.key]));
  });
  $("autostart").checked = state.autostart;
  document.querySelectorAll("textarea[data-prompt]").forEach((el) => {
    el.value = c[el.dataset.prompt] || defaultPrompts[el.dataset.prompt];
  });
  $("model-dir").placeholder = defaultModelDir;
  renderModels();
  updateResetButtons();
}

// === 自动保存：改动即生效 ===
// 开关、下拉框、快捷键等立即保存；文本框停止输入片刻后（或失去焦点时）保存
const TYPING_DELAY = 500;
let saveTimer = null;
let saving = Promise.resolve(true);

function scheduleSave(delay = 0) {
  updateResetButtons();
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => save(), delay);
}

/** 保存当前设置；校验失败时提示原因并返回 false（界面保留输入，便于修改） */
function save() {
  clearTimeout(saveTimer);
  saveTimer = null;
  saving = saving.then(async () => {
    if (JSON.stringify(state) === JSON.stringify(saved)) return true;
    const next = clone(state);
    try {
      await invoke("save_settings", { config: next.config, autostart: next.autostart });
      saved = next;
      return true;
    } catch (e) {
      toast(String(e), true);
      return false;
    }
  });
  return saving;
}

let toastTimer;
function toast(msg, isError = false) {
  const el = $("toast");
  el.textContent = msg;
  el.classList.toggle("error", isError);
  el.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove("show"), isError ? 5000 : 2500);
}

/** 切换界面语言：静态文字 + 动态生成的文字一起刷新（选择后立即预览，保存后托盘和通知也跟着换） */
function applyLanguage() {
  applyI18n(resolveLang(state.config.ui_language));
  renderModels();
  renderVocab();
  setUpdateStatus(updateStatus.key, updateStatus.vars, updateStatus.cls);
  updateResetButtons();
}

// === 分区导航 ===
function showPane(name) {
  if (!document.getElementById(`pane-${name}`)) name = "hotkeys";
  document.querySelectorAll(".pane").forEach((p) => (p.hidden = p.id !== `pane-${name}`));
  document.querySelectorAll(".nav-item").forEach((n) => n.classList.toggle("active", n.dataset.section === name));
  $("content").scrollTop = 0;
  try {
    localStorage.setItem("aoide.section", name);
  } catch {}
}

function initNav() {
  document.querySelectorAll(".nav-item").forEach((n) => n.addEventListener("click", () => showPane(n.dataset.section)));
  let last = "hotkeys";
  try {
    last = localStorage.getItem("aoide.section") || last;
  } catch {}
  showPane(last);
}

// === 词库 ===
let vocab = [];

async function loadVocab() {
  vocab = await invoke("get_vocab");
  renderVocab();
}

function el(tag, props = {}, ...children) {
  const e = Object.assign(document.createElement(tag), props);
  e.append(...children);
  return e;
}

function renderVocab() {
  const q = $("vocab-search").value.trim().toLowerCase();
  const match = (e) => !q || e.term.toLowerCase().includes(q) || e.variants.some((v) => v.text.toLowerCase().includes(q));
  const list = [...vocab].sort((a, b) => b.id - a.id).filter(match);
  $("vocab-count").textContent = vocab.length ? t("vocab.count", { n: vocab.length }) : "";
  $("vocab-empty").hidden = list.length > 0;
  $("vocab-empty").textContent = vocab.length ? t("vocab.noMatch") : t("vocab.empty");
  $("vocab-list").replaceChildren(...list.map((e) => {
    const chips = e.variants.length
      ? e.variants.map((v) => {
          const remove = el("button", { innerHTML: "&#xE711;", title: t("vocab.removeVariant") });
          remove.addEventListener("click", () => invoke("remove_vocab_variant", { id: e.id, text: v.text }).then(loadVocab));
          const times = v.count >= 2 ? el("span", { className: "times", textContent: " " + t("vocab.times", { n: v.count }) }) : "";
          return el("span", { className: "chip" }, v.text, times, remove);
        })
      : [el("span", { className: "desc", textContent: t("vocab.noVariants") })];
    const toggle = el("input", { type: "checkbox", checked: e.enabled });
    toggle.addEventListener("input", () => invoke("set_vocab_enabled", { id: e.id, enabled: toggle.checked }).then(loadVocab));
    const del = el("button", { className: "btn icon-btn", innerHTML: "&#xE74D;", title: t("vocab.delete") });
    del.addEventListener("click", () => invoke("remove_vocab", { id: e.id }).then(loadVocab));
    return el("div", { className: "card vocab-entry" + (e.enabled ? "" : " off") },
      el("span", { className: "ico", innerHTML: e.manual ? "&#xE70F;" : "&#xE945;" }),
      el("div", { className: "text" },
        el("div", { className: "title" }, e.term, el("span", { className: "badge", textContent: t(e.manual ? "vocab.manual" : "vocab.learned") })),
        el("div", { className: "chips" }, ...chips)),
      el("div", { className: "control" }, el("label", { className: "toggle", title: t("vocab.enable") }, toggle, el("span")), del));
  }));
}

async function addVocab() {
  const term = $("vocab-term").value.trim();
  const variants = $("vocab-variants").value.split(/[,，、;；]/).map((s) => s.trim()).filter(Boolean);
  try {
    await invoke("add_vocab", { term, variants });
    $("vocab-term").value = "";
    $("vocab-variants").value = "";
    $("vocab-term").focus();
    await loadVocab();
  } catch (e) {
    toast(String(e), true);
  }
}

// === 更新 ===
let update = null; // 已知的新版本 { version, notes, url }
let updateStatus = { key: "update.current" };

function setUpdateStatus(key, vars = {}, cls = "") {
  updateStatus = { key, vars, cls };
  const el = $("update-status");
  el.textContent = t(key, { version: $("about-version").textContent, ...vars });
  el.className = "desc " + cls;
}

function renderUpdate() {
  const busy = !$("update-progress").hidden;
  $("update-install").hidden = !update || busy;
  $("update-notes").hidden = !update;
  $("update-check").hidden = !!update || busy;
  if (update && !busy) setUpdateStatus("update.available", { version: update.version }, "available");
}

async function checkUpdate() {
  $("update-check").disabled = true;
  setUpdateStatus("update.checking");
  try {
    update = await invoke("check_update");
    if (!update) setUpdateStatus("update.upToDate");
  } catch (e) {
    setUpdateStatus("update.failed", { error: String(e) }, "error");
  } finally {
    $("update-check").disabled = false;
    renderUpdate();
  }
}

async function installUpdate() {
  $("update-progress").hidden = false;
  $("update-bar").style.width = "0";
  setUpdateStatus("update.downloading");
  renderUpdate();
  try {
    await invoke("install_update"); // 成功时安装程序会结束本进程
  } catch (e) {
    $("update-progress").hidden = true;
    setUpdateStatus("update.failed", { error: String(e) }, "error");
    renderUpdate();
  }
}

// === 交互 ===
let modelDirTimer;
async function refreshModels() {
  models = await invoke("list_models", { modelDir: state.config.model_dir });
  renderModels();
}

function bind() {
  const isTyping = (el) => el.tagName === "TEXTAREA" || (el.tagName === "INPUT" && el.type !== "checkbox");
  document.querySelectorAll("input[data-key], select[data-key]").forEach((el) => {
    if (isTyping(el)) el.addEventListener("change", () => save());
    el.addEventListener("input", () => {
      const key = el.dataset.key;
      if (el.type === "checkbox") state.config[key] = el.checked;
      else if (el.dataset.type === "int") state.config[key] = Math.max(0, parseInt(el.value, 10) || 0);
      else state.config[key] = el.value;
      if (key === "model") renderModelStatus();
      if (key === "ui_language") applyLanguage();
      if (key === "model_dir") {
        clearTimeout(modelDirTimer);
        modelDirTimer = setTimeout(refreshModels, 400);
      }
      scheduleSave(isTyping(el) ? TYPING_DELAY : 0);
    });
  });

  document.querySelectorAll(".segmented[data-key]").forEach((seg) => {
    seg.addEventListener("click", (e) => {
      const b = e.target.closest("button");
      if (!b) return;
      state.config[seg.dataset.key] = b.dataset.value;
      render();
      scheduleSave();
    });
  });

  // 提示词：与内置相同或清空时存为空字符串，表示跟随内置提示词
  document.querySelectorAll("textarea[data-prompt]").forEach((el) => {
    el.addEventListener("input", () => {
      const key = el.dataset.prompt;
      const v = el.value;
      state.config[key] = v.trim() === "" || v === defaultPrompts[key] ? "" : v;
      scheduleSave(TYPING_DELAY);
    });
    el.addEventListener("change", () => save());
  });

  $("update-check").addEventListener("click", checkUpdate);
  $("update-install").addEventListener("click", installUpdate);
  $("update-notes").addEventListener("click", () => update && invoke("open_url", { url: update.url }));
  listen("update-available", ({ payload }) => {
    update = payload;
    renderUpdate();
  });
  listen("update-progress", ({ payload }) => {
    const { done, total } = payload;
    if (total) {
      const pct = Math.floor((done / total) * 100);
      $("update-bar").style.width = `${pct}%`;
      setUpdateStatus("update.downloadingPct", { pct });
    }
  });
  listen("update-installing", () => {
    $("update-bar").style.width = "100%";
    setUpdateStatus("update.installing");
  });

  $("vocab-add-btn").addEventListener("click", addVocab);
  for (const id of ["vocab-term", "vocab-variants"]) {
    $(id).addEventListener("keydown", (e) => e.key === "Enter" && addVocab());
  }
  $("vocab-search").addEventListener("input", renderVocab);
  listen("vocab-changed", loadVocab);
  listen("translate-changed", ({ payload }) => {
    state.config.translate_to = payload;
    render();
  });

  $("autostart").addEventListener("input", (e) => {
    state.autostart = e.target.checked;
    scheduleSave();
  });

  document.querySelectorAll("[data-capture]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const key = btn.dataset.capture;
      document.querySelectorAll("[data-capture]").forEach((b) => (b.disabled = true));
      btn.classList.add("capturing");
      btn.textContent = t("btn.capturing");
      try {
        const combo = await invoke("capture_hotkey").catch((e) => {
          toast(String(e), true);
          return null;
        });
        if (combo) {
          // 冲突等原因保存失败时退回原来的快捷键，界面与实际生效的保持一致
          const before = state.config[key];
          state.config[key] = combo;
          render();
          if (!(await save())) {
            state.config[key] = before;
            render();
          }
        }
      } finally {
        btn.classList.remove("capturing");
        btn.textContent = t("btn.change");
        document.querySelectorAll("[data-capture]").forEach((b) => (b.disabled = false));
      }
    });
  });

  $("download-btn").addEventListener("click", async () => {
    try {
      await invoke("download_model", { id: state.config.model, modelDir: state.config.model_dir });
      downloading = state.config.model;
      $("download-row").hidden = false;
      $("download-text").textContent = t("download.running", { id: downloading });
      $("download-pct").textContent = "";
      $("download-bar").style.width = "0";
      renderModelStatus();
    } catch (e) {
      toast(String(e), true);
    }
  });

  $("cancel-btn").addEventListener("click", () => invoke("cancel_download"));

  $("browse-btn").addEventListener("click", async () => {
    const dir = await window.__TAURI__.dialog.open({ directory: true, defaultPath: state.config.model_dir || defaultModelDir });
    if (typeof dir === "string") {
      state.config.model_dir = dir;
      render();
      refreshModels();
      scheduleSave();
    }
  });

  $("open-models-btn").addEventListener("click", () => invoke("open_folder", { path: state.config.model_dir }));
  $("open-logs-btn").addEventListener("click", () => invoke("open_folder", { path: $("log-dir").textContent }));

  document.querySelectorAll("[data-url]").forEach((b) =>
    b.addEventListener("click", () => invoke("open_url", { url: b.dataset.url })));
  $("licenses-btn").addEventListener("click", () => invoke("open_licenses").catch((e) => toast(String(e), true)));

  $("reveal-btn").addEventListener("click", () => {
    const input = $("api-key");
    input.type = input.type === "password" ? "text" : "password";
  });

  // 关闭窗口前把还在等待的输入保存掉
  window.__TAURI__.window.getCurrentWindow().onCloseRequested(async () => {
    if (saveTimer !== null) await save();
    else await saving;
  });

  listen("download-progress", ({ payload }) => {
    const { done, total } = payload;
    const pct = total ? Math.floor((done / total) * 100) : 0;
    $("download-bar").style.width = `${pct}%`;
    $("download-pct").textContent = total
      ? `${formatSize(Math.round(done / 1048576))} / ${formatSize(Math.round(total / 1048576))} · ${pct}%`
      : formatSize(Math.round(done / 1048576));
  });

  listen("download-finished", async ({ payload }) => {
    downloading = null;
    $("download-row").hidden = true;
    if (payload.error) toast(t("download.failed", { id: payload.id, error: payload.error }), true);
    else toast(t("download.done", { id: payload.id }));
    await refreshModels();
  });
}

async function init() {
  const s = await invoke("get_settings");
  $("version").textContent = s.version;
  $("about-version").textContent = s.version;
  $("log-dir").textContent = s.log_dir;
  models = s.models;
  defaultModelDir = s.default_model_dir;
  systemLanguage = s.system_language;
  defaultPrompts = {
    polish_prompt: s.default_polish_prompt,
    revise_prompt: s.default_revise_prompt,
    insert_prompt: s.default_insert_prompt,
  };
  defaults = { config: s.defaults, autostart: false };
  state = { config: s.config, autostart: s.autostart };
  saved = clone(state);
  setupResetButtons();
  initNav();
  applyI18n(resolveLang(state.config.ui_language));
  render();
  bind();
  update = await invoke("get_update");
  setUpdateStatus("update.current");
  renderUpdate();
  await loadVocab();
}

init().catch((e) => toast(String(e), true));
