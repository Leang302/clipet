"use strict";

/* ============================== Helpers ============================== */

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));
const uid = () => Math.random().toString(36).slice(2, 10);
const now = () => Date.now();

function timeAgo(ts) {
  const s = Math.floor((now() - ts) / 1000);
  if (s < 10) return "just now";
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.floor(h / 24);
  return `${d}d ago`;
}

function loadImage(src) {
  return new Promise((resolve) => {
    const img = new Image();
    img.onload = () => resolve(img);
    img.src = src;
  });
}

const IMG_W = 760, IMG_H = 480;

// Generates a placeholder raster (photo / screenshot) entirely offscreen — no network assets.
function makePlaceholder({ kind, label, seed = 0, sub = "" }) {
  const c = document.createElement("canvas");
  c.width = IMG_W; c.height = IMG_H;
  const ctx = c.getContext("2d");
  const hues = [210, 265, 150, 25, 340, 185];
  const hue = hues[seed % hues.length];
  const grad = ctx.createLinearGradient(0, 0, IMG_W, IMG_H);
  grad.addColorStop(0, `hsl(${hue} 70% 88%)`);
  grad.addColorStop(1, `hsl(${(hue + 40) % 360} 70% 70%)`);
  ctx.fillStyle = grad;
  ctx.fillRect(0, 0, IMG_W, IMG_H);

  if (kind === "screenshot") {
    // Fake UI chrome so it reads as a "screenshot" rather than a photo.
    ctx.fillStyle = "rgba(255,255,255,0.9)";
    ctx.fillRect(0, 0, IMG_W, 34);
    ["#ff5f57", "#febc2e", "#28c840"].forEach((col, i) => {
      ctx.fillStyle = col;
      ctx.beginPath(); ctx.arc(20 + i * 18, 17, 6, 0, Math.PI * 2); ctx.fill();
    });
    ctx.fillStyle = "rgba(255,255,255,0.55)";
    for (let i = 0; i < 4; i++) ctx.fillRect(30, 60 + i * 60, IMG_W - 60, 40);
  } else {
    ctx.fillStyle = "rgba(255,255,255,0.35)";
    ctx.beginPath(); ctx.arc(IMG_W * 0.3, IMG_H * 0.4, 80, 0, Math.PI * 2); ctx.fill();
    ctx.beginPath(); ctx.moveTo(0, IMG_H);
    ctx.lineTo(IMG_W * 0.3, IMG_H * 0.6); ctx.lineTo(IMG_W * 0.55, IMG_H * 0.8);
    ctx.lineTo(IMG_W * 0.8, IMG_H * 0.55); ctx.lineTo(IMG_W, IMG_H * 0.75); ctx.lineTo(IMG_W, IMG_H);
    ctx.closePath(); ctx.fill();
  }

  ctx.fillStyle = "rgba(20,22,27,0.82)";
  ctx.font = "600 26px -apple-system, sans-serif";
  ctx.fillText(label, 24, IMG_H - 48);
  if (sub) {
    ctx.font = "13px -apple-system, sans-serif";
    ctx.fillStyle = "rgba(20,22,27,0.6)";
    ctx.fillText(sub, 24, IMG_H - 24);
  }
  return c.toDataURL("image/png");
}

function blankTransparent(w, h) {
  const c = document.createElement("canvas");
  c.width = w; c.height = h;
  return c;
}

/* ============================== Sample data ============================== */

const TYPE_META = {
  text: { icon: "📝", label: "Text" },
  richtext: { icon: "🖋️", label: "Rich Text" },
  code: { icon: "⌨️", label: "Code" },
  link: { icon: "🔗", label: "Link" },
  file: { icon: "📄", label: "File" },
  image: { icon: "🖼️", label: "Image" },
  screenshot: { icon: "🖥️", label: "Screenshot" },
};

const todayAt = (h, m = 0) => {
  const d = new Date();
  d.setHours(h, m, 0, 0);
  return d.getTime();
};

let items = [];

function seedData() {
  const T = now();
  items = [
    { id: uid(), type: "text", content: "Q3 planning notes — follow up with design on the onboarding flow by EOD Friday.", createdAt: T - 2 * 60000, lastUsedAt: T - 2 * 60000, useCount: 1, favorite: false, pinned: true, tags: ["work"], sourceApp: "Notes" },
    { id: uid(), type: "link", content: "https://docs.example.com/api/rate-limits", domain: "docs.example.com", createdAt: T - 6 * 60000, lastUsedAt: null, useCount: 0, favorite: false, pinned: false, tags: [], sourceApp: "Browser" },
    { id: uid(), type: "code", content: "function debounce(fn, wait) {\n  let t;\n  return (...a) => {\n    clearTimeout(t);\n    t = setTimeout(() => fn(...a), wait);\n  };\n}", createdAt: T - 40 * 60000, lastUsedAt: T - 30 * 60000, useCount: 4, favorite: true, pinned: false, tags: ["snippets"], sourceApp: "VS Code" },
    { id: uid(), type: "file", content: "Q3-budget-final.xlsx", ext: "xlsx", createdAt: T - 90 * 60000, lastUsedAt: null, useCount: 0, favorite: false, pinned: false, tags: [], sourceApp: "Finder" },
    { id: uid(), type: "text", content: "leangk49@gmail.com", createdAt: T - 120 * 60000, lastUsedAt: T - 100 * 60000, useCount: 2, favorite: false, pinned: false, tags: [], sourceApp: "Mail" },
  ];

  // The spec's delayed-annotation example: A at 9am, B at 11am, C at 3pm today.
  const shots = [
    { label: "Screenshot A", time: todayAt(9, 2), sub: "9:02 AM · 1920×1080" },
    { label: "Screenshot B", time: todayAt(11, 14), sub: "11:14 AM · 1920×1080" },
    { label: "Screenshot C", time: todayAt(15, 37), sub: "3:37 PM · 1920×1080" },
  ];
  shots.forEach((s, i) => {
    const data = makePlaceholder({ kind: "screenshot", label: s.label, seed: i, sub: s.sub });
    items.push({
      id: uid(), type: "screenshot", content: s.label, thumb: data, original: data, edited: false,
      resolution: "1920×1080", createdAt: s.time, lastUsedAt: null, useCount: 0,
      favorite: false, pinned: false, tags: [], sourceApp: "Desktop",
    });
  });

  const photoLabels = ["Dashboard mock", "Logo v3", "Team offsite"];
  photoLabels.forEach((label, i) => {
    const data = makePlaceholder({ kind: "image", label, seed: i + 3, sub: "PNG · 1200×800" });
    items.push({
      id: uid(), type: "image", content: label, thumb: data, original: data, edited: false,
      resolution: "1200×800", createdAt: T - (200 + i * 50) * 60000, lastUsedAt: null, useCount: 0,
      favorite: i === 1, pinned: false, tags: [], sourceApp: "Figma",
    });
  });

  items.sort((a, b) => b.createdAt - a.createdAt);
}

/* ============================== App state ============================== */

const state = {
  popupOpen: false,
  category: "all",
  query: "",
  sort: "newest",
  selectedIds: new Set(),
  activeId: null,
  anchorId: null,
  trayMenuOpen: false,
  paused: false,
  editor: { open: false, targets: [], index: 0, images: {}, batch: false },
  settings: JSON.parse(localStorage.getItem("clipflow-settings") || "null") || {
    startAtLogin: true, backgroundMonitoring: true, theme: "light",
    notifEnabled: true, notifDuration: 5, notifPosition: "bottom-right",
    autoClipboardCopy: true, autoLocalSave: false, imageFormat: "PNG",
    maxHistoryItems: 500, duplicateHandling: "move-to-top", excludedApps: "1Password, Bitwarden",
  },
  settingsTab: "General",
  activeDestApp: "Notes",
};

function visibleItems() {
  let list = items.slice();
  if (state.category !== "all") {
    if (state.category === "favorites") list = list.filter((i) => i.favorite);
    else list = list.filter((i) => i.type === state.category);
  }
  if (state.query.trim()) {
    const q = state.query.toLowerCase();
    list = list.filter((i) => (i.content || "").toLowerCase().includes(q) || (i.sourceApp || "").toLowerCase().includes(q));
  }
  switch (state.sort) {
    case "oldest": list.sort((a, b) => a.createdAt - b.createdAt); break;
    case "frequent": list.sort((a, b) => b.useCount - a.useCount); break;
    case "favorites": list.sort((a, b) => (b.favorite - a.favorite) || (b.createdAt - a.createdAt)); break;
    default: list.sort((a, b) => b.createdAt - a.createdAt);
  }
  return list;
}

function byId(id) { return items.find((i) => i.id === id); }

/* ============================== Toast ============================== */

let toastTimer = null;
function toast(msg) {
  const el = $("#toast");
  el.textContent = msg;
  el.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove("show"), 2200);
}

/* ============================== Sidebar ============================== */

const CATEGORIES = [
  { key: "all", icon: "🗂️", label: "All History" },
  { key: "favorites", icon: "⭐", label: "Favorites" },
  { key: "text", icon: "📝", label: "Text" },
  { key: "image", icon: "🖼️", label: "Images" },
  { key: "screenshot", icon: "🖥️", label: "Screenshots" },
  { key: "link", icon: "🔗", label: "Links" },
  { key: "file", icon: "📄", label: "Files" },
  { key: "code", icon: "⌨️", label: "Code" },
];

function renderSidebar() {
  const sb = $("#sidebar");
  sb.innerHTML = "";
  CATEGORIES.forEach((c) => {
    const count = c.key === "all" ? items.length
      : c.key === "favorites" ? items.filter((i) => i.favorite).length
      : items.filter((i) => i.type === c.key).length;
    const el = document.createElement("div");
    el.className = "sidebar-item" + (state.category === c.key ? " active" : "");
    el.innerHTML = `<span class="icon">${c.icon}</span><span>${c.label}</span><span class="count">${count}</span>`;
    el.onclick = () => { state.category = c.key; renderSidebar(); renderList(); };
    sb.appendChild(el);
  });

  const apps = [...new Set(items.map((i) => i.sourceApp).filter(Boolean))].slice(0, 4);
  if (apps.length) {
    const lbl = document.createElement("div");
    lbl.className = "sidebar-section-label";
    lbl.textContent = "Recent sources";
    sb.appendChild(lbl);
    apps.forEach((app) => {
      const el = document.createElement("div");
      el.className = "sidebar-item";
      el.innerHTML = `<span class="icon">•</span><span>${app}</span>`;
      el.onclick = () => { state.query = app; $("#search-input").value = app; renderList(); };
      sb.appendChild(el);
    });
  }
}

/* ============================== List ============================== */

function itemPreviewLabel(item) {
  switch (item.type) {
    case "text": return item.content.slice(0, 60);
    case "code": return item.content.split("\n")[0].slice(0, 60);
    case "link": return item.domain || item.content;
    case "file": return item.content;
    case "image": return item.content;
    case "screenshot": return item.content;
    default: return item.content;
  }
}

function renderList() {
  const list = visibleItems();
  const wrap = $("#item-list");
  wrap.innerHTML = "";

  if (!list.length) {
    wrap.innerHTML = `<div id="empty-list">No items match ${state.query ? "your search" : "this filter"}.</div>`;
    return;
  }

  if (!state.activeId || !list.some((i) => i.id === state.activeId)) {
    state.activeId = list[0].id;
  }

  list.forEach((item) => {
    const meta = TYPE_META[item.type];
    const el = document.createElement("div");
    el.className = "clip-item" + (state.selectedIds.has(item.id) ? " selected" : "") + (state.activeId === item.id ? " active" : "");
    el.dataset.id = item.id;

    let thumbHtml = `<span>${meta.icon}</span>`;
    if (item.thumb) thumbHtml = `<img src="${item.thumb}" alt="" />`;

    el.innerHTML = `
      <div class="clip-thumb">${thumbHtml}</div>
      <div class="clip-meta">
        <div class="clip-title">${escapeHtml(itemPreviewLabel(item))}</div>
        <div class="clip-sub">
          ${item.pinned ? '<span class="clip-pin">📌</span>' : ""}
          ${item.favorite ? '<span class="clip-fav">★</span>' : ""}
          <span>${meta.label}</span>
          <span>·</span>
          <span>${timeAgo(item.createdAt)}</span>
          ${item.resolution ? `<span>· ${item.resolution}</span>` : ""}
          ${item.edited ? `<span>· edited</span>` : ""}
        </div>
      </div>`;

    el.addEventListener("click", (e) => onItemClick(e, item, list));
    el.addEventListener("dblclick", () => pasteItems([item.id]));
    wrap.appendChild(el);
  });

  const activeEl = wrap.querySelector(".clip-item.active");
  if (activeEl) activeEl.scrollIntoView({ block: "nearest" });

  renderPreview();
  renderToolbar();
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function onItemClick(e, item, list) {
  const ids = list.map((i) => i.id);
  if (e.shiftKey && state.anchorId) {
    const a = ids.indexOf(state.anchorId), b = ids.indexOf(item.id);
    const [lo, hi] = a < b ? [a, b] : [b, a];
    state.selectedIds = new Set(ids.slice(lo, hi + 1));
    state.activeId = item.id;
  } else if (e.metaKey || e.ctrlKey) {
    if (state.selectedIds.has(item.id)) state.selectedIds.delete(item.id);
    else state.selectedIds.add(item.id);
    state.activeId = item.id;
    state.anchorId = item.id;
  } else {
    state.selectedIds = new Set([item.id]);
    state.activeId = item.id;
    state.anchorId = item.id;
  }
  renderList();
}

/* ============================== Preview panel ============================== */

function renderPreview() {
  const body = $("#preview-body");
  const headerLabel = $("#preview-header-label");
  const badge = $("#preview-count-badge");
  const selected = [...state.selectedIds].map(byId).filter(Boolean);
  const list = selected.length ? selected : (byId(state.activeId) ? [byId(state.activeId)] : []);

  body.innerHTML = "";
  if (!list.length) {
    headerLabel.textContent = "No selection";
    badge.style.display = "none";
    body.innerHTML = `<div id="preview-empty">Select an item to preview it here.</div>`;
    return;
  }

  if (list.length > 1) {
    headerLabel.textContent = "Selected items";
    badge.style.display = "inline-block";
    badge.textContent = `${list.length} selected`;
  } else {
    headerLabel.textContent = TYPE_META[list[0].type].label;
    badge.style.display = "none";
  }

  const editableCount = list.filter((i) => i.type === "image" || i.type === "screenshot").length;
  if (list.length > 1 && editableCount > 1) {
    const btn = document.createElement("button");
    btn.textContent = `Edit Selected (${editableCount})`;
    btn.className = "card-edit-btn";
    btn.style.position = "static";
    btn.style.marginBottom = "10px";
    btn.style.background = "var(--accent)";
    btn.onclick = () => openEditor(list.filter((i) => i.type === "image" || i.type === "screenshot").map((i) => i.id), true);
    body.appendChild(btn);
  }

  list.forEach((item) => body.appendChild(renderPreviewCard(item)));
}

function renderPreviewCard(item) {
  const card = document.createElement("div");
  card.className = "preview-card";

  if (item.type === "image" || item.type === "screenshot") {
    card.innerHTML = `<img src="${item.thumb}" alt="" /><div class="card-caption">${escapeHtml(item.content)} · ${item.resolution || ""}${item.edited ? " · edited" : ""}</div>`;
    const btn = document.createElement("button");
    btn.className = "card-edit-btn";
    btn.textContent = "✎ Edit";
    btn.onclick = (e) => { e.stopPropagation(); openEditor([item.id], false); };
    card.appendChild(btn);
  } else if (item.type === "text" || item.type === "richtext") {
    card.innerHTML = `<div class="preview-text">${escapeHtml(item.content)}</div>`;
  } else if (item.type === "code") {
    card.innerHTML = `<div class="preview-text" style="background:#1b1f24;color:#d6e2ff;">${escapeHtml(item.content)}</div>`;
  } else if (item.type === "link") {
    card.innerHTML = `<div class="preview-link-domain">${escapeHtml(item.domain || "")}</div><div class="preview-link-url">${escapeHtml(item.content)}</div>`;
  } else if (item.type === "file") {
    card.innerHTML = `<div class="preview-file-row"><span style="font-size:22px;">📄</span><span>${escapeHtml(item.content)}</span></div>`;
  }
  return card;
}

/* ============================== Toolbar ============================== */

function renderToolbar() {
  const dest = $("#toolbar-dest");
  dest.innerHTML = state.selectedIds.size > 1
    ? `Paste all to <span class="dest">${state.activeDestApp}</span>`
    : `Paste to <span class="dest">${state.activeDestApp}</span>`;
}

/* ============================== Paste ============================== */

function pasteItems(ids) {
  const list = ids.map(byId).filter(Boolean);
  if (!list.length) return;
  list.forEach((i) => { i.useCount++; i.lastUsedAt = now(); });
  if (list.length === 1) toast(`Pasted "${itemPreviewLabel(list[0]).slice(0, 40)}" to ${state.activeDestApp}`);
  else toast(`Pasted ${list.length} items to ${state.activeDestApp}`);
  closePopup();
}

/* ============================== Popup open/close ============================== */

function openPopup() {
  state.popupOpen = true;
  $("#popup-overlay").classList.add("open");
  positionPopup();
  renderSidebar();
  renderList();
  $("#search-input").value = state.query;
  setTimeout(() => $("#search-input").focus(), 0);
}

function closePopup() {
  state.popupOpen = false;
  $("#popup-overlay").classList.remove("open");
}

function positionPopup() {
  const popup = $("#clip-popup");
  const dw = window.innerWidth, dh = window.innerHeight;
  const w = 860, h = 520;
  let left = Math.min(Math.max(dw / 2 - w / 2, 12), dw - w - 12);
  let top = Math.min(Math.max(dh / 2 - h / 2, 60), dh - h - 12);
  popup.style.left = `${left}px`;
  popup.style.top = `${top}px`;
}

/* ============================== Keyboard ============================== */

document.addEventListener("keydown", (e) => {
  const mod = e.metaKey || e.ctrlKey;

  if (mod && e.shiftKey && e.key.toLowerCase() === "v") {
    e.preventDefault();
    state.popupOpen ? closePopup() : openPopup();
    return;
  }

  if (state.editor.open) {
    if (e.key === "Escape") { e.preventDefault(); cancelEditor(); }
    return;
  }

  if (!state.popupOpen) return;

  if ($("#settings-overlay").classList.contains("open")) {
    if (e.key === "Escape") closeSettings();
    return;
  }

  const active = document.activeElement;
  const searchFocused = active && active.id === "search-input";

  if (e.key === "Escape") { e.preventDefault(); closePopup(); return; }

  const list = visibleItems();
  const ids = list.map((i) => i.id);

  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    if (!ids.length) return;
    let idx = Math.max(0, ids.indexOf(state.activeId));
    idx = e.key === "ArrowDown" ? Math.min(ids.length - 1, idx + 1) : Math.max(0, idx - 1);
    const newId = ids[idx];
    if (e.shiftKey) {
      if (!state.anchorId) state.anchorId = state.activeId;
      const a = ids.indexOf(state.anchorId), b = idx;
      const [lo, hi] = a < b ? [a, b] : [b, a];
      state.selectedIds = new Set(ids.slice(lo, hi + 1));
    } else {
      state.selectedIds = new Set([newId]);
      state.anchorId = newId;
    }
    state.activeId = newId;
    renderList();
    return;
  }

  if (mod && e.key.toLowerCase() === "a") {
    e.preventDefault();
    state.selectedIds = new Set(ids);
    renderList();
    return;
  }

  if (e.key === "Enter") {
    e.preventDefault();
    if (e.shiftKey) {
      const targets = state.selectedIds.size ? [...state.selectedIds] : (state.activeId ? [state.activeId] : []);
      pasteItems(targets);
    } else {
      pasteItems([state.activeId]);
    }
    return;
  }

  if (!searchFocused && e.key.length === 1 && !mod && !e.altKey) {
    $("#search-input").focus();
    $("#search-input").value += e.key;
    state.query = $("#search-input").value;
    renderList();
  }
});

$("#search-input").addEventListener("input", (e) => { state.query = e.target.value; renderList(); });
$("#sort-select").addEventListener("change", (e) => { state.sort = e.target.value; renderList(); });

$("#popup-overlay").addEventListener("mousedown", (e) => {
  if (e.target.id === "popup-overlay") closePopup();
});

/* ============================== Tray ============================== */

$("#tray-icon").addEventListener("click", () => {
  state.trayMenuOpen = !state.trayMenuOpen;
  $("#tray-menu").classList.toggle("open", state.trayMenuOpen);
  $("#tray-storage-val").textContent = `${(items.length * 0.4).toFixed(1)} MB`;
});
document.addEventListener("click", (e) => {
  if (!e.target.closest("#tray-menu") && !e.target.closest("#tray-icon")) {
    state.trayMenuOpen = false;
    $("#tray-menu").classList.remove("open");
  }
});
$("#tray-open-history").onclick = () => { state.trayMenuOpen = false; $("#tray-menu").classList.remove("open"); openPopup(); };
$("#tray-capture").onclick = () => { state.trayMenuOpen = false; $("#tray-menu").classList.remove("open"); captureScreenshot("full"); };
$("#tray-settings").onclick = () => { state.trayMenuOpen = false; $("#tray-menu").classList.remove("open"); openSettings(); };
$("#tray-quit").onclick = () => { toast("ClipFlow would quit now (prototype — no-op)"); state.trayMenuOpen = false; $("#tray-menu").classList.remove("open"); };
$("#tray-pause").onclick = () => {
  state.paused = !state.paused;
  $("#tray-pause-label").textContent = state.paused ? "Resume Monitoring" : "Pause Monitoring";
  $("#tray-icon").classList.toggle("paused", state.paused);
  toast(state.paused ? "Clipboard monitoring paused" : "Clipboard monitoring resumed");
  state.trayMenuOpen = false; $("#tray-menu").classList.remove("open");
};

/* ============================== Simulated clipboard events ============================== */

function addItem(item) {
  item.id = item.id || uid();
  item.createdAt = now();
  item.lastUsedAt = null;
  item.useCount = 0;
  item.favorite = false;
  item.pinned = false;
  item.tags = [];
  items.unshift(item);
  if (state.popupOpen) { renderSidebar(); renderList(); }
}

$("#sim-copy-text").onclick = () => {
  if (state.paused) return toast("Monitoring is paused");
  addItem({ type: "text", content: "Ship the v2 pricing page by Friday — confirm with growth team first.", sourceApp: "Notes" });
  toast("Copied text");
};
$("#sim-copy-code").onclick = () => {
  if (state.paused) return toast("Monitoring is paused");
  addItem({ type: "code", content: "const result = items.filter(Boolean).map(toDTO);", sourceApp: "VS Code" });
  toast("Copied code");
};
$("#sim-copy-link").onclick = () => {
  if (state.paused) return toast("Monitoring is paused");
  addItem({ type: "link", content: "https://example.com/changelog/2026-11", domain: "example.com", sourceApp: "Browser" });
  toast("Copied link");
};
$("#sim-copy-file").onclick = () => {
  if (state.paused) return toast("Monitoring is paused");
  addItem({ type: "file", content: `export-${Math.floor(Math.random() * 900 + 100)}.csv`, sourceApp: "Finder" });
  toast("Copied file reference");
};

let shotSeed = 10;
$("#sim-shot-region").onclick = () => captureScreenshot("region");
$("#sim-shot-full").onclick = () => captureScreenshot("full");

function captureScreenshot(mode) {
  if (state.paused) { toast("Monitoring is paused — screenshot still captured, but not auto-saved"); }
  shotSeed++;
  const label = mode === "full" ? "Full screen" : "Region capture";
  const time = new Date();
  const data = makePlaceholder({ kind: "screenshot", label, seed: shotSeed, sub: time.toLocaleTimeString() + " · 1920×1080" });
  const item = {
    id: uid(), type: "screenshot", content: label, thumb: data, original: data, edited: false,
    resolution: "1920×1080", createdAt: now(), lastUsedAt: null, useCount: 0,
    favorite: false, pinned: false, tags: [], sourceApp: "Desktop",
  };
  items.unshift(item);
  if (state.popupOpen) { renderSidebar(); renderList(); }
  showScreenshotNotif(item);
}

/* ============================== Screenshot floating notification ============================== */

function showScreenshotNotif(item) {
  if (!state.settings.notifEnabled) return;
  const container = $("#screenshot-notifs");
  const el = document.createElement("div");
  el.className = "shot-notif";
  el.innerHTML = `
    <img class="shot-thumb" src="${item.thumb}" alt="" />
    <div class="shot-bar"></div>
    <div class="shot-actions">
      <button data-act="copy">Copy</button>
      <button data-act="save">Save</button>
      <button data-act="dismiss">Dismiss</button>
    </div>`;
  container.appendChild(el);

  const bar = el.querySelector(".shot-bar");
  const durationMs = state.settings.notifDuration * 1000;
  let remaining = durationMs;
  let last = performance.now();
  let raf;
  let paused = false;

  function tick(t) {
    if (!paused) remaining -= (t - last);
    last = t;
    const pct = Math.max(0, remaining / durationMs);
    bar.style.transform = `scaleX(${pct})`;
    if (remaining <= 0) { el.remove(); return; }
    raf = requestAnimationFrame(tick);
  }
  raf = requestAnimationFrame(tick);

  el.addEventListener("mouseenter", () => { paused = true; });
  el.addEventListener("mouseleave", () => { paused = false; last = performance.now(); });

  el.addEventListener("click", (e) => {
    const act = e.target.dataset.act;
    if (act === "copy") { toast("Screenshot copied to clipboard"); return; }
    if (act === "save") { toast("Saved to ~/Pictures/ClipFlow/" + item.content.replace(/\s+/g, "-").toLowerCase() + ".png"); return; }
    if (act === "dismiss") { cancelAnimationFrame(raf); el.remove(); return; }
    // click on the thumbnail itself
    cancelAnimationFrame(raf); el.remove();
    openEditor([item.id], false);
  });
}

/* ============================== Annotation editor ============================== */

const TOOLS = [
  { key: "pen", icon: "✏️", label: "Pen" },
  { key: "highlighter", icon: "🖊️", label: "Highlighter" },
  { key: "arrow", icon: "➚", label: "Arrow" },
  { key: "rect", icon: "▭", label: "Rectangle" },
  { key: "ellipse", icon: "◯", label: "Ellipse" },
  { key: "line", icon: "／", label: "Line" },
  { key: "text", icon: "T", label: "Text" },
  { key: "blur", icon: "◍", label: "Blur" },
  { key: "pixelate", icon: "▦", label: "Pixelate" },
  { key: "crop", icon: "⛶", label: "Crop" },
  { key: "eraser", icon: "⌫", label: "Eraser" },
];

let currentTool = "pen";

function renderToolButtons() {
  const wrap = $("#editor-tools");
  wrap.innerHTML = "";
  TOOLS.forEach((t) => {
    const b = document.createElement("button");
    b.className = "tool-btn" + (currentTool === t.key ? " active" : "");
    b.title = t.label;
    b.textContent = t.icon;
    b.onclick = () => { currentTool = t.key; renderToolButtons(); };
    wrap.appendChild(b);
  });
}

function imageState(id) {
  if (!state.editor.images[id]) {
    const item = byId(id);
    state.editor.images[id] = {
      baseHistory: [item.thumb],
      annoHistory: [null], // null anno = fully transparent
      idx: 0,
      w: IMG_W, h: IMG_H,
    };
  }
  return state.editor.images[id];
}

function currentImageId() { return state.editor.targets[state.editor.index]; }

async function renderEditorCanvas() {
  const id = currentImageId();
  const st = imageState(id);
  const canvas = $("#editor-canvas");
  canvas.width = st.w; canvas.height = st.h;
  const ctx = canvas.getContext("2d");
  ctx.clearRect(0, 0, st.w, st.h);
  const base = await loadImage(st.baseHistory[st.idx]);
  ctx.drawImage(base, 0, 0, st.w, st.h);
  const annoSrc = st.annoHistory[st.idx];
  if (annoSrc) {
    const anno = await loadImage(annoSrc);
    ctx.drawImage(anno, 0, 0, st.w, st.h);
  }
  $("#tool-undo").disabled = st.idx === 0;
  $("#tool-redo").disabled = st.idx === st.baseHistory.length - 1;
  renderBatchStrip();
  $("#editor-title").textContent = (state.editor.batch ? `Annotate (${state.editor.index + 1} of ${state.editor.targets.length}) — ` : "Annotate — ") + byId(id).content;
}

function pushHistory(id, newBaseSrc, newAnnoSrc) {
  const st = imageState(id);
  st.baseHistory = st.baseHistory.slice(0, st.idx + 1);
  st.annoHistory = st.annoHistory.slice(0, st.idx + 1);
  st.baseHistory.push(newBaseSrc !== undefined ? newBaseSrc : st.baseHistory[st.idx]);
  st.annoHistory.push(newAnnoSrc !== undefined ? newAnnoSrc : st.annoHistory[st.idx]);
  st.idx++;
}

function isDirty(id) { const st = imageState(id); return st.idx !== 0; }

function renderBatchStrip() {
  const strip = $("#editor-batch-strip");
  if (!state.editor.batch) { strip.style.display = "none"; return; }
  strip.style.display = "flex";
  strip.innerHTML = "";
  state.editor.targets.forEach((id, i) => {
    const item = byId(id);
    const st = imageState(id);
    const th = document.createElement("div");
    th.className = "batch-thumb" + (i === state.editor.index ? " current" : "");
    th.innerHTML = `<img src="${st.baseHistory[st.idx]}" alt="" />` + (isDirty(id) ? '<div class="dirty-dot"></div>' : "");
    th.onclick = () => { state.editor.index = i; renderEditorCanvas(); };
    strip.appendChild(th);
  });
  const nav = document.createElement("div");
  nav.id = "editor-batch-nav";
  nav.innerHTML = `<button id="batch-prev">‹ Prev</button><span>${state.editor.index + 1} / ${state.editor.targets.length}</span><button id="batch-next">Next ›</button>`;
  strip.appendChild(nav);
  $("#batch-prev").onclick = () => { if (state.editor.index > 0) { state.editor.index--; renderEditorCanvas(); } };
  $("#batch-next").onclick = () => { if (state.editor.index < state.editor.targets.length - 1) { state.editor.index++; renderEditorCanvas(); } };
}

function openEditor(ids, batch) {
  state.editor = { open: true, targets: ids, index: 0, images: {}, batch: !!batch };
  $("#editor-overlay").classList.add("open");
  renderToolButtons();
  renderEditorCanvas();
}

function closeEditorOverlay() {
  $("#editor-overlay").classList.remove("open");
  state.editor.open = false;
}

async function flattenToDataUrl(id) {
  const st = imageState(id);
  const c = document.createElement("canvas");
  c.width = st.w; c.height = st.h;
  const ctx = c.getContext("2d");
  const base = await loadImage(st.baseHistory[st.idx]);
  ctx.drawImage(base, 0, 0);
  const annoSrc = st.annoHistory[st.idx];
  if (annoSrc) { const anno = await loadImage(annoSrc); ctx.drawImage(anno, 0, 0); }
  return c.toDataURL("image/png");
}

$("#editor-cancel").onclick = cancelEditor;
function cancelEditor() {
  const anyDirty = state.editor.targets.some(isDirty);
  if (anyDirty && !confirm("Discard unsaved edits?")) return;
  closeEditorOverlay();
}

$("#editor-done").onclick = async () => {
  let savedCount = 0;
  for (const id of state.editor.targets) {
    if (isDirty(id)) {
      const item = byId(id);
      item.thumb = await flattenToDataUrl(id);
      item.edited = true;
      savedCount++;
    }
  }
  closeEditorOverlay();
  if (state.popupOpen) { renderList(); }
  if (savedCount === 0) { toast("No changes to save"); return; }
  if (!state.editor.batch && savedCount === 1) toast("Annotated image saved and copied to clipboard");
  else toast(`Saved ${savedCount} edited image${savedCount > 1 ? "s" : ""}`);
};

$("#tool-undo").onclick = () => { const id = currentImageId(); const st = imageState(id); if (st.idx > 0) { st.idx--; renderEditorCanvas(); } };
$("#tool-redo").onclick = () => { const id = currentImageId(); const st = imageState(id); if (st.idx < st.baseHistory.length - 1) { st.idx++; renderEditorCanvas(); } };

$("#tool-thickness").addEventListener("input", (e) => $("#thickness-val").textContent = e.target.value);
$("#tool-fontsize").addEventListener("input", (e) => $("#fontsize-val").textContent = e.target.value);

/* ---- Canvas drawing ---- */

let drag = null; // { startX, startY, snapshotAnno }

function canvasPoint(e) {
  const canvas = $("#editor-canvas");
  const r = canvas.getBoundingClientRect();
  const sx = canvas.width / r.width, sy = canvas.height / r.height;
  return { x: (e.clientX - r.left) * sx, y: (e.clientY - r.top) * sy };
}

function getAnnoCtx(id) {
  const st = imageState(id);
  if (!st._annoCanvas) { st._annoCanvas = document.createElement("canvas"); st._annoCanvas.width = st.w; st._annoCanvas.height = st.h; }
  const ctx = st._annoCanvas.getContext("2d");
  ctx.clearRect(0, 0, st.w, st.h);
  return { canvas: st._annoCanvas, ctx };
}

async function drawExistingAnno(ctx, id) {
  const st = imageState(id);
  const src = st.annoHistory[st.idx];
  if (src) { const img = await loadImage(src); ctx.drawImage(img, 0, 0); }
}

function color() { return $("#tool-color").value; }
function thickness() { return parseInt($("#tool-thickness").value, 10); }
function fontSize() { return parseInt($("#tool-fontsize").value, 10); }

$("#editor-canvas").addEventListener("pointerdown", async (e) => {
  const id = currentImageId();
  const p = canvasPoint(e);

  if (currentTool === "text") {
    const txt = prompt("Enter text:");
    if (txt) {
      const { canvas: annoCanvas, ctx } = getAnnoCtx(id);
      await drawExistingAnno(ctx, id);
      ctx.fillStyle = color();
      ctx.font = `600 ${fontSize()}px -apple-system, sans-serif`;
      ctx.fillText(txt, p.x, p.y);
      pushHistory(id, undefined, annoCanvas.toDataURL());
      renderEditorCanvas();
    }
    return;
  }

  drag = { tool: currentTool, startX: p.x, startY: p.y, lastX: p.x, lastY: p.y, points: [p] };

  if (currentTool === "pen" || currentTool === "highlighter" || currentTool === "eraser") {
    const { ctx } = getAnnoCtx(id);
    await drawExistingAnno(ctx, id);
  }
  $("#editor-canvas").setPointerCapture(e.pointerId);
});

$("#editor-canvas").addEventListener("pointermove", async (e) => {
  if (!drag) return;
  const id = currentImageId();
  const p = canvasPoint(e);
  const { canvas: annoCanvas, ctx } = getAnnoCtx(id);

  if (drag.tool === "pen" || drag.tool === "highlighter" || drag.tool === "eraser") {
    await drawExistingAnno(ctx, id);
    drag.points.push(p);
    ctx.lineJoin = "round"; ctx.lineCap = "round";
    ctx.lineWidth = thickness() * (drag.tool === "highlighter" ? 2.2 : 1);
    ctx.globalAlpha = drag.tool === "highlighter" ? 0.35 : 1;
    ctx.globalCompositeOperation = drag.tool === "eraser" ? "destination-out" : "source-over";
    ctx.strokeStyle = color();
    ctx.beginPath();
    drag.points.forEach((pt, i) => i === 0 ? ctx.moveTo(pt.x, pt.y) : ctx.lineTo(pt.x, pt.y));
    ctx.stroke();
    ctx.globalAlpha = 1; ctx.globalCompositeOperation = "source-over";
    drawLive(annoCanvas, id);
  } else if (["line", "rect", "ellipse", "arrow", "crop", "blur", "pixelate"].includes(drag.tool)) {
    await drawExistingAnno(ctx, id);
    drag.lastX = p.x; drag.lastY = p.y;
    drawShapePreview(ctx, drag);
    drawLive(annoCanvas, id);
  }
});

async function drawLive(annoCanvas, id) {
  const st = imageState(id);
  const canvas = $("#editor-canvas");
  const ctx = canvas.getContext("2d");
  ctx.clearRect(0, 0, st.w, st.h);
  const base = await loadImage(st.baseHistory[st.idx]);
  ctx.drawImage(base, 0, 0);
  ctx.drawImage(annoCanvas, 0, 0);
}

function drawShapePreview(ctx, d) {
  ctx.strokeStyle = color();
  ctx.fillStyle = color();
  ctx.lineWidth = thickness();
  ctx.lineJoin = "round"; ctx.lineCap = "round";
  const { startX: x0, startY: y0, lastX: x1, lastY: y1 } = d;
  if (d.tool === "line" || d.tool === "arrow") {
    ctx.beginPath(); ctx.moveTo(x0, y0); ctx.lineTo(x1, y1); ctx.stroke();
    if (d.tool === "arrow") {
      const angle = Math.atan2(y1 - y0, x1 - x0);
      const headLen = 10 + thickness();
      ctx.beginPath();
      ctx.moveTo(x1, y1);
      ctx.lineTo(x1 - headLen * Math.cos(angle - Math.PI / 6), y1 - headLen * Math.sin(angle - Math.PI / 6));
      ctx.lineTo(x1 - headLen * Math.cos(angle + Math.PI / 6), y1 - headLen * Math.sin(angle + Math.PI / 6));
      ctx.closePath(); ctx.fill();
    }
  } else if (d.tool === "rect" || d.tool === "crop") {
    ctx.strokeRect(Math.min(x0, x1), Math.min(y0, y1), Math.abs(x1 - x0), Math.abs(y1 - y0));
  } else if (d.tool === "ellipse") {
    ctx.beginPath();
    ctx.ellipse((x0 + x1) / 2, (y0 + y1) / 2, Math.abs(x1 - x0) / 2, Math.abs(y1 - y0) / 2, 0, 0, Math.PI * 2);
    ctx.stroke();
  } else if (d.tool === "blur" || d.tool === "pixelate") {
    ctx.save();
    ctx.strokeStyle = "rgba(59,111,255,0.9)";
    ctx.setLineDash([6, 4]);
    ctx.strokeRect(Math.min(x0, x1), Math.min(y0, y1), Math.abs(x1 - x0), Math.abs(y1 - y0));
    ctx.restore();
  }
}

$("#editor-canvas").addEventListener("pointerup", async (e) => {
  if (!drag) return;
  const id = currentImageId();
  const p = canvasPoint(e);
  const rect = { x: Math.min(drag.startX, p.x), y: Math.min(drag.startY, p.y), w: Math.abs(p.x - drag.startX), h: Math.abs(p.y - drag.startY) };

  if (drag.tool === "pen" || drag.tool === "highlighter" || drag.tool === "eraser") {
    const { canvas: annoCanvas } = getAnnoCtx(id);
    pushHistory(id, undefined, annoCanvas.toDataURL());
  } else if (drag.tool === "line" || drag.tool === "arrow" || drag.tool === "rect" || drag.tool === "ellipse") {
    const { canvas: annoCanvas, ctx } = getAnnoCtx(id);
    await drawExistingAnno(ctx, id);
    drawShapePreview(ctx, { ...drag, lastX: p.x, lastY: p.y });
    pushHistory(id, undefined, annoCanvas.toDataURL());
  } else if (drag.tool === "blur" || drag.tool === "pixelate") {
    if (rect.w > 4 && rect.h > 4) await applyRegionEffect(id, rect, drag.tool);
  } else if (drag.tool === "crop") {
    if (rect.w > 10 && rect.h > 10) await applyCrop(id, rect);
  }

  drag = null;
  renderEditorCanvas();
});

async function applyRegionEffect(id, rect, effect) {
  const st = imageState(id);
  const flat = await flattenToDataUrl(id);
  const img = await loadImage(flat);
  const c = document.createElement("canvas");
  c.width = st.w; c.height = st.h;
  const ctx = c.getContext("2d");
  ctx.drawImage(img, 0, 0);

  const rx = Math.round(rect.x), ry = Math.round(rect.y), rw = Math.round(rect.w), rh = Math.round(rect.h);
  if (effect === "blur") {
    ctx.save();
    ctx.beginPath(); ctx.rect(rx, ry, rw, rh); ctx.clip();
    ctx.filter = "blur(10px)";
    ctx.drawImage(img, 0, 0);
    ctx.restore();
  } else {
    const factor = 0.08;
    const tmp = document.createElement("canvas");
    tmp.width = Math.max(1, Math.round(rw * factor));
    tmp.height = Math.max(1, Math.round(rh * factor));
    const tctx = tmp.getContext("2d");
    tctx.imageSmoothingEnabled = true;
    tctx.drawImage(img, rx, ry, rw, rh, 0, 0, tmp.width, tmp.height);
    ctx.imageSmoothingEnabled = false;
    ctx.save();
    ctx.beginPath(); ctx.rect(rx, ry, rw, rh); ctx.clip();
    ctx.drawImage(tmp, 0, 0, tmp.width, tmp.height, rx, ry, rw, rh);
    ctx.restore();
  }
  pushHistory(id, c.toDataURL(), null);
}

async function applyCrop(id, rect) {
  const st = imageState(id);
  const flat = await flattenToDataUrl(id);
  const img = await loadImage(flat);
  const rx = Math.round(rect.x), ry = Math.round(rect.y), rw = Math.round(rect.w), rh = Math.round(rect.h);
  const c = document.createElement("canvas");
  c.width = rw; c.height = rh;
  c.getContext("2d").drawImage(img, rx, ry, rw, rh, 0, 0, rw, rh);
  st.w = rw; st.h = rh;
  pushHistory(id, c.toDataURL(), null);
}

/* ============================== Settings ============================== */

const SETTINGS_TABS = ["General", "Shortcuts", "Screenshot", "Clipboard", "Appearance"];

function saveSettings() { localStorage.setItem("clipflow-settings", JSON.stringify(state.settings)); }

function openSettings() {
  $("#settings-overlay").classList.add("open");
  renderSettingsTabs();
  renderSettingsContent();
}
function closeSettings() { $("#settings-overlay").classList.remove("open"); }
$("#settings-close").onclick = closeSettings;
$("#settings-overlay").addEventListener("mousedown", (e) => { if (e.target.id === "settings-overlay") closeSettings(); });

function renderSettingsTabs() {
  const wrap = $("#settings-tabs");
  wrap.innerHTML = "";
  SETTINGS_TABS.forEach((t) => {
    const el = document.createElement("div");
    el.className = "tab" + (state.settingsTab === t ? " active" : "");
    el.textContent = t;
    el.onclick = () => { state.settingsTab = t; renderSettingsTabs(); renderSettingsContent(); };
    wrap.appendChild(el);
  });
}

function switchRow(label, desc, key) {
  const row = document.createElement("div");
  row.className = "settings-row";
  row.innerHTML = `<div><div class="row-label">${label}</div><div class="row-desc">${desc}</div></div>`;
  const sw = document.createElement("div");
  sw.className = "switch" + (state.settings[key] ? " on" : "");
  sw.innerHTML = `<div class="knob"></div>`;
  sw.onclick = () => { state.settings[key] = !state.settings[key]; sw.classList.toggle("on"); saveSettings(); if (key === "theme") applyTheme(); };
  row.appendChild(sw);
  return row;
}

function selectRow(label, desc, key, options) {
  const row = document.createElement("div");
  row.className = "settings-row";
  row.innerHTML = `<div><div class="row-label">${label}</div><div class="row-desc">${desc}</div></div>`;
  const sel = document.createElement("select");
  options.forEach((o) => {
    const opt = document.createElement("option");
    opt.value = o; opt.textContent = o;
    if (state.settings[key] === o) opt.selected = true;
    sel.appendChild(opt);
  });
  sel.onchange = () => { state.settings[key] = sel.value; saveSettings(); };
  row.appendChild(sel);
  return row;
}

function textRow(label, desc, key) {
  const row = document.createElement("div");
  row.className = "settings-row";
  row.innerHTML = `<div><div class="row-label">${label}</div><div class="row-desc">${desc}</div></div>`;
  const inp = document.createElement("input");
  inp.type = "text"; inp.value = state.settings[key];
  inp.onchange = () => { state.settings[key] = inp.value; saveSettings(); };
  row.appendChild(inp);
  return row;
}

function shortcutRow(label, value) {
  const row = document.createElement("div");
  row.className = "settings-row";
  row.innerHTML = `<div class="row-label">${label}</div>`;
  const inp = document.createElement("input");
  inp.type = "text"; inp.value = value; inp.readOnly = true;
  inp.style.width = "140px"; inp.style.textAlign = "center";
  row.appendChild(inp);
  return row;
}

function renderSettingsContent() {
  const c = $("#settings-content");
  c.innerHTML = "";
  const h = document.createElement("h3");
  h.textContent = state.settingsTab;
  c.appendChild(h);

  if (state.settingsTab === "General") {
    c.appendChild(switchRow("Start at login", "Launch ClipFlow automatically when you sign in.", "startAtLogin"));
    c.appendChild(switchRow("Background monitoring", "Watch the clipboard while the app is running.", "backgroundMonitoring"));
  } else if (state.settingsTab === "Shortcuts") {
    c.appendChild(shortcutRow("Open clipboard history", navigator.platform.includes("Mac") ? "⌘⇧V" : "Ctrl+Shift+V"));
    c.appendChild(shortcutRow("Region screenshot", navigator.platform.includes("Mac") ? "⌘⇧4" : "Win+Shift+S"));
    c.appendChild(shortcutRow("Full screenshot", navigator.platform.includes("Mac") ? "⌘⇧3" : "PrtScn"));
    c.appendChild(shortcutRow("Window screenshot", navigator.platform.includes("Mac") ? "⌘⇧4, Space" : "Alt+PrtScn"));
    c.appendChild(shortcutRow("Paste as plain text", navigator.platform.includes("Mac") ? "⌘⇧V (hold)" : "Ctrl+Shift+Alt+V"));
  } else if (state.settingsTab === "Screenshot") {
    c.appendChild(switchRow("Floating notification", "Show a thumbnail preview after each capture.", "notifEnabled"));
    const dur = document.createElement("div");
    dur.className = "settings-row";
    dur.innerHTML = `<div class="row-label">Notification duration</div>`;
    const inp = document.createElement("input");
    inp.type = "number"; inp.min = 1; inp.max = 30; inp.value = state.settings.notifDuration; inp.style.width = "60px";
    inp.onchange = () => { state.settings.notifDuration = Math.max(1, parseInt(inp.value || "5", 10)); saveSettings(); };
    dur.appendChild(inp);
    c.appendChild(dur);
    c.appendChild(selectRow("Notification position", "Where the floating thumbnail appears.", "notifPosition", ["bottom-right", "bottom-left", "top-right", "top-left"]));
    c.appendChild(switchRow("Automatic clipboard copy", "Copy every capture to the system clipboard.", "autoClipboardCopy"));
    c.appendChild(switchRow("Automatic local save", "Also save a copy to disk on every capture.", "autoLocalSave"));
    c.appendChild(selectRow("Image format", "Format used when saving screenshots to disk.", "imageFormat", ["PNG", "JPEG"]));
  } else if (state.settingsTab === "Clipboard") {
    const row = document.createElement("div");
    row.className = "settings-row";
    row.innerHTML = `<div class="row-label">Maximum history items</div>`;
    const inp = document.createElement("input");
    inp.type = "number"; inp.value = state.settings.maxHistoryItems; inp.style.width = "80px";
    inp.onchange = () => { state.settings.maxHistoryItems = parseInt(inp.value || "500", 10); saveSettings(); };
    row.appendChild(inp);
    c.appendChild(row);
    c.appendChild(selectRow("Duplicate handling", "What happens when the same content is copied again.", "duplicateHandling", ["move-to-top", "ignore", "keep-both"]));
    c.appendChild(textRow("Excluded applications", "Comma-separated app names to never monitor.", "excludedApps"));
    const clearRow = document.createElement("div");
    clearRow.className = "settings-row";
    clearRow.innerHTML = `<div><div class="row-label">Clear history</div><div class="row-desc">Deletes all saved clipboard items. Cannot be undone.</div></div>`;
    const btn = document.createElement("button");
    btn.textContent = "Clear All"; btn.style.color = "var(--danger)";
    btn.onclick = () => { if (confirm("Clear all clipboard history?")) { items = []; renderSidebar(); renderList(); toast("History cleared"); } };
    clearRow.appendChild(btn);
    c.appendChild(clearRow);
  } else if (state.settingsTab === "Appearance") {
    c.appendChild(selectRow("Theme", "Light, dark, or follow the system.", "theme", ["light", "dark", "system"]));
  }
}

function applyTheme() {
  const t = state.settings.theme;
  const resolved = t === "system" ? (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light") : t;
  document.documentElement.setAttribute("data-theme", resolved);
}

/* ============================== Misc wiring ============================== */

$("#toggle-theme").onclick = () => {
  state.settings.theme = state.settings.theme === "dark" ? "light" : "dark";
  saveSettings();
  applyTheme();
};

$("#dismiss-onboard").onclick = () => { $("#onboard-banner").style.display = "none"; };

/* ============================== Init ============================== */

seedData();
applyTheme();
renderSidebar();
