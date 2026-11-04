import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { applyTheme, loadTheme, saveTheme, THEME_STORAGE_KEY, type ThemeChoice } from "./theme";
import "./clipflow.css";

type ClipItem = {
  id: string;
  kind: string; // "text" | "link" | "image" | "screenshot" | "file" | "code"
  content: string;
  preview: string;
  source_app: string;
  created_at: number;
  last_used_at: number | null;
  use_count: number;
  favorite: boolean;
  pinned: boolean;
  edited: boolean;
};

type PasteRecovery = {
  copied: boolean;
  ids: string[];
  message: string;
};

const MANUAL_PASTE_READY_PREFIX = "CLIPET_MANUAL_PASTE_READY:";
const AUTO_PASTE_BLOCKED_PREFIX = "CLIPET_AUTO_PASTE_BLOCKED:";

const TYPE_META: Record<string, { icon: string; label: string }> = {
  text: { icon: "📝", label: "Text" },
  link: { icon: "🔗", label: "Link" },
  image: { icon: "🖼️", label: "Image" },
  screenshot: { icon: "🖥️", label: "Screenshot" },
  file: { icon: "📄", label: "File" },
  code: { icon: "⌨️", label: "Code" },
};

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

function timeAgo(ts: number): string {
  const s = Math.floor((Date.now() - ts) / 1000);
  if (s < 10) return "just now";
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

function previewLabel(item: ClipItem): string {
  if (item.kind === "link") return item.content;
  return item.content.length > 70 ? item.content.slice(0, 70) : item.content;
}

export default function App() {
  const [items, setItems] = useState<ClipItem[]>([]);
  const [category, setCategory] = useState("all");
  const [query, setQuery] = useState("");
  const [sort, setSort] = useState<"newest" | "oldest" | "frequent" | "favorites">("newest");
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [activeId, setActiveId] = useState<string | null>(null);
  const [anchorId, setAnchorId] = useState<string | null>(null);
  const [imageUrls, setImageUrls] = useState<Record<string, string>>({});
  const [toastMsg, setToastMsg] = useState<string | null>(null);
  const [isPasting, setIsPasting] = useState(false);
  const [theme, setTheme] = useState<ThemeChoice>(() => loadTheme());
  const [confirmClear, setConfirmClear] = useState<{ toDelete: number; favCount: number } | null>(null);
  const [pasteRecovery, setPasteRecovery] = useState<PasteRecovery | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const toastTimer = useRef<number | undefined>(undefined);
  // State updates are asynchronous, so use a ref as the immediate lock that
  // prevents Enter repeats, double-clicks and buttons from starting a second
  // native paste while the first transfer is still running.
  const pasteInFlightRef = useRef(false);

  useEffect(() => {
    applyTheme(theme);
  }, [theme]);

  // Picks up a theme change made from the editor window or a future settings
  // window, since each Tauri window is a separate document with its own copy
  // of this component's state (`storage` fires in OTHER windows, not the one
  // that wrote the value).
  useEffect(() => {
    function onStorage(e: StorageEvent) {
      if (e.key === THEME_STORAGE_KEY) setTheme(loadTheme());
    }
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, []);

  function toast(msg: string, ms = 2400) {
    setToastMsg(msg);
    window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToastMsg(null), ms);
  }

  function handlePasteFailure(error: unknown, ids: string[]) {
    const message = String(error);
    if (message.startsWith(MANUAL_PASTE_READY_PREFIX)) {
      setPasteRecovery({
        copied: true,
        ids,
        message: message.slice(MANUAL_PASTE_READY_PREFIX.length),
      });
      return;
    }
    if (message.startsWith(AUTO_PASTE_BLOCKED_PREFIX)) {
      setPasteRecovery({
        copied: false,
        ids,
        message: message.slice(AUTO_PASTE_BLOCKED_PREFIX.length),
      });
      return;
    }
    toast(message, 9000);
  }

  async function refresh() {
    const data = await invoke<ClipItem[]>("get_history");
    setItems(data);
  }

  useEffect(() => {
    refresh();
    searchRef.current?.focus();

    const unlistenNew = listen("clipflow://new-item", () => refresh());
    const unlistenOpen = listen("clipflow://history-opened", () => {
      refresh();
      searchRef.current?.focus();
    });
    // An annotation was saved for this item — its file on disk changed, so the
    // data URL we already cached for it is stale and must be dropped.
    const unlistenUpdated = listen<string>("clipflow://item-updated", (event) => {
      const id = event.payload;
      setImageUrls((prev) => {
        const next = { ...prev };
        delete next[id];
        return next;
      });
      refresh();
    });
    return () => {
      unlistenNew.then((f) => f());
      unlistenOpen.then((f) => f());
      unlistenUpdated.then((f) => f());
    };
  }, []);

  // Lazily resolve image/screenshot thumbnails to data URLs as they appear.
  useEffect(() => {
    const toFetch = items.filter((i) => (i.kind === "image" || i.kind === "screenshot") && !imageUrls[i.id]);
    if (!toFetch.length) return;
    toFetch.forEach((i) => {
      invoke<string>("get_image_data_url", { filename: i.content })
        .then((url) => setImageUrls((prev) => ({ ...prev, [i.id]: url })))
        .catch(() => {});
    });
  }, [items]);

  const visible = useMemo(() => {
    let list = items.slice();
    if (category === "favorites") list = list.filter((i) => i.favorite);
    else if (category !== "all") list = list.filter((i) => i.kind === category);
    if (query.trim()) {
      const q = query.toLowerCase();
      list = list.filter((i) => i.content.toLowerCase().includes(q) || i.source_app.toLowerCase().includes(q));
    }
    switch (sort) {
      case "oldest": list.sort((a, b) => a.created_at - b.created_at); break;
      case "frequent": list.sort((a, b) => b.use_count - a.use_count); break;
      case "favorites": list.sort((a, b) => Number(b.favorite) - Number(a.favorite) || b.created_at - a.created_at); break;
      default: list.sort((a, b) => b.created_at - a.created_at);
    }
    return list;
  }, [items, category, query, sort]);

  const visibleIds = visible.map((i) => i.id);
  useEffect(() => {
    if (!activeId || !visibleIds.includes(activeId)) {
      setActiveId(visibleIds[0] ?? null);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [visibleIds.join(",")]);

  function byId(id: string | null) {
    return items.find((i) => i.id === id) ?? null;
  }

  function onItemClick(e: React.MouseEvent, item: ClipItem) {
    if (e.shiftKey && anchorId) {
      const a = visibleIds.indexOf(anchorId), b = visibleIds.indexOf(item.id);
      const [lo, hi] = a < b ? [a, b] : [b, a];
      setSelectedIds(new Set(visibleIds.slice(lo, hi + 1)));
      setActiveId(item.id);
    } else if (e.metaKey || e.ctrlKey) {
      setSelectedIds((prev) => {
        const next = new Set(prev);
        next.has(item.id) ? next.delete(item.id) : next.add(item.id);
        return next;
      });
      setActiveId(item.id);
      setAnchorId(item.id);
    } else {
      setSelectedIds(new Set([item.id]));
      setActiveId(item.id);
      setAnchorId(item.id);
    }
  }

  async function pasteOne(id: string) {
    if (pasteInFlightRef.current) return;
    pasteInFlightRef.current = true;
    setIsPasting(true);
    try {
      await invoke("paste_item", { id });
      toast("Pasted");
    } catch (e) {
      handlePasteFailure(e, [id]);
    } finally {
      pasteInFlightRef.current = false;
      setIsPasting(false);
    }
  }

  async function pasteMany(ids: string[]) {
    if (!ids.length || pasteInFlightRef.current) return;
    if (ids.length === 1) return pasteOne(ids[0]);
    pasteInFlightRef.current = true;
    setIsPasting(true);
    try {
      await invoke("paste_items_sequential", { ids });
      toast(`Pasted ${ids.length} items`);
    } catch (e) {
      handlePasteFailure(e, ids);
    } finally {
      pasteInFlightRef.current = false;
      setIsPasting(false);
    }
  }

  async function openEditor(ids: string[]) {
    if (pasteInFlightRef.current) return;
    try {
      await invoke("start_annotation", { ids });
    } catch (e) {
      toast(`Couldn't open editor: ${e}`);
    }
  }

  async function openAccessibilitySettings() {
    try {
      await invoke("open_accessibility_settings");
    } catch (e) {
      toast(`${e}`, 9000);
    }
  }

  function retryPaste() {
    if (!pasteRecovery) return;
    const ids = pasteRecovery.ids;
    setPasteRecovery(null);
    if (ids.length === 1) void pasteOne(ids[0]);
    else void pasteMany(ids);
  }

  async function toggleFavorite(item: ClipItem) {
    await invoke("toggle_favorite", { id: item.id, favorite: !item.favorite });
    refresh();
  }
  async function togglePinned(item: ClipItem) {
    await invoke("toggle_pinned", { id: item.id, pinned: !item.pinned });
    refresh();
  }
  async function removeItem(item: ClipItem) {
    await invoke("delete_item", { id: item.id });
    refresh();
  }

  function requestClearNonFavorites() {
    const favCount = items.filter((i) => i.favorite).length;
    const toDelete = items.length - favCount;
    if (!toDelete) {
      toast("Nothing to clear — everything left is a favorite");
      return;
    }
    // window.confirm() is unreliable inside Tauri's webview (it can silently
    // no-op instead of showing a dialog) — use a real in-app confirmation
    // instead of trusting the browser's native one.
    setConfirmClear({ toDelete, favCount });
  }

  async function confirmClearNow() {
    setConfirmClear(null);
    await invoke("clear_non_favorites");
    setSelectedIds(new Set());
    refresh();
  }

  function onKeyDown(e: KeyboardEvent) {
    const searchFocused = document.activeElement === searchRef.current;

    if (e.key === "Escape") {
      e.preventDefault();
      invoke("hide_history");
      return;
    }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!visibleIds.length) return;
      let idx = Math.max(0, activeId ? visibleIds.indexOf(activeId) : 0);
      idx = e.key === "ArrowDown" ? Math.min(visibleIds.length - 1, idx + 1) : Math.max(0, idx - 1);
      const newId = visibleIds[idx];
      if (e.shiftKey) {
        const anchor = anchorId ?? activeId ?? newId;
        const a = visibleIds.indexOf(anchor), b = idx;
        const [lo, hi] = a < b ? [a, b] : [b, a];
        setSelectedIds(new Set(visibleIds.slice(lo, hi + 1)));
        setAnchorId(anchor);
      } else {
        setSelectedIds(new Set([newId]));
        setAnchorId(newId);
      }
      setActiveId(newId);
      return;
    }
    if (!searchFocused && (e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "a") {
      // Only hijack ⌘/Ctrl+A for "select all items" when focus isn't in the
      // search box — otherwise it should do its normal job of selecting the
      // text inside the field, like any other text input.
      e.preventDefault();
      setSelectedIds(new Set(visibleIds));
      return;
    }
    if (e.key === "Enter") {
      e.preventDefault();
      if (e.repeat || pasteInFlightRef.current) return;
      // Enter pastes whatever is selected — one item or many — no Shift needed.
      const ordered = visibleIds.filter((id) => selectedIds.has(id));
      pasteMany(ordered.length ? ordered : activeId ? [activeId] : []);
      return;
    }
    if (!searchFocused && e.key.length === 1 && !e.metaKey && !e.ctrlKey && !e.altKey) {
      searchRef.current?.focus();
    }
  }

  // A document-level listener (rather than a React onKeyDown on the container)
  // because clicking a non-focusable list item moves focus to <body>, and a
  // keydown dispatched there would never bubble down into a child div's handler.
  useEffect(() => {
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [visibleIds.join(","), activeId, anchorId, selectedIds]);

  // Order selections by their position in the current (sorted/filtered) list,
  // not by Set insertion order — a Shift-range selects in list order already,
  // but a Ctrl/Cmd-click can add items out of order, which otherwise leaks
  // into preview stacking order, "Edit Selected" batch order, and multi-paste
  // order. Spec: "Default to history-display order for batch operations...
  // Never silently reorder screenshots during transfer."
  const orderedSelectedIds = visibleIds.filter((id) => selectedIds.has(id));
  const selectedItems = orderedSelectedIds.map(byId).filter((i): i is ClipItem => !!i);
  const previewList = selectedItems.length ? selectedItems : activeId ? [byId(activeId)!].filter(Boolean) : [];

  return (
    <div id="clip-popup">
      <div className="popup-body">
        <div id="sidebar">
          {CATEGORIES.map((c) => {
            const count = c.key === "all" ? items.length
              : c.key === "favorites" ? items.filter((i) => i.favorite).length
              : items.filter((i) => i.kind === c.key).length;
            return (
              <div
                key={c.key}
                className={"sidebar-item" + (category === c.key ? " active" : "")}
                onClick={() => setCategory(c.key)}
              >
                <span className="icon">{c.icon}</span>
                <span>{c.label}</span>
                <span className="count">{count}</span>
              </div>
            );
          })}
          <button
            className="icon-btn"
            style={{ width: "100%", marginTop: 10 }}
            title="Deletes everything except favorites"
            disabled={isPasting}
            onClick={requestClearNonFavorites}
          >
            🗑 Clear
          </button>
          <select
            value={theme}
            onChange={(e) => { const t = e.target.value as ThemeChoice; setTheme(t); saveTheme(t); }}
            style={{ width: "100%", marginTop: 8, fontSize: 12, padding: "5px 6px", borderRadius: 6, border: "1px solid var(--border-strong)", background: "var(--bg-panel)", color: "var(--text-main)" }}
          >
            <option value="system">Theme: System</option>
            <option value="light">Theme: Light</option>
            <option value="dark">Theme: Dark</option>
          </select>
        </div>

        <div id="list-panel">
          <div id="search-row">
            <input
              ref={searchRef}
              id="search-input"
              placeholder="Search clipboard history…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
            <select id="sort-select" value={sort} onChange={(e) => setSort(e.target.value as typeof sort)}>
              <option value="newest">Newest</option>
              <option value="oldest">Oldest</option>
              <option value="frequent">Most used</option>
              <option value="favorites">Favorites first</option>
            </select>
          </div>
          <div id="item-list">
            {!visible.length && (
              <div id="empty-list">No items match {query ? "your search" : "this filter"}.</div>
            )}
            {visible.map((item) => {
              const meta = TYPE_META[item.kind] ?? { icon: "📋", label: item.kind };
              const thumb = imageUrls[item.id];
              return (
                <div
                  key={item.id}
                  className={"clip-item" + (selectedIds.has(item.id) ? " selected" : "") + (activeId === item.id ? " active" : "")}
                  onClick={(e) => onItemClick(e, item)}
                  onDoubleClick={() => pasteOne(item.id)}
                >
                  <div className="clip-thumb">{thumb ? <img src={thumb} alt="" /> : <span>{meta.icon}</span>}</div>
                  <div className="clip-meta">
                    <div className="clip-title">{previewLabel(item)}</div>
                    <div className="clip-sub">
                      {item.pinned && <span>📌</span>}
                      {item.favorite && <span>★</span>}
                      <span>{meta.label}</span>
                      <span>·</span>
                      <span>{timeAgo(item.created_at)}</span>
                      {item.edited && <span>· edited</span>}
                    </div>
                  </div>
                </div>
              );
            })}
          </div>
        </div>

        <div id="preview-panel">
          <div id="preview-header">
            <span>{previewList.length > 1 ? "Selected items" : previewList[0] ? (TYPE_META[previewList[0].kind]?.label ?? previewList[0].kind) : "No selection"}</span>
            {previewList.length > 1 && <span id="preview-count-badge">{previewList.length} selected</span>}
          </div>
          <div id="preview-body">
            {!previewList.length && <div id="preview-empty">Select an item to preview it here.</div>}
            {previewList.length > 1 && previewList.filter((i) => i.kind === "image" || i.kind === "screenshot").length > 1 && (
              <button
                className="icon-btn"
                style={{ marginBottom: 10, background: "var(--accent)", color: "#fff", borderColor: "var(--accent)" }}
                disabled={isPasting}
                onClick={() => openEditor(previewList.filter((i) => i.kind === "image" || i.kind === "screenshot").map((i) => i.id))}
              >
                Edit Selected ({previewList.filter((i) => i.kind === "image" || i.kind === "screenshot").length})
              </button>
            )}
            {previewList.map((item) => (
              <div className="preview-card" key={item.id}>
                {(item.kind === "image" || item.kind === "screenshot") && (
                  imageUrls[item.id]
                    ? <img src={imageUrls[item.id]} alt="" />
                    : <div className="card-caption">Loading image…</div>
                )}
                {item.kind === "text" && <div className="preview-text">{item.content}</div>}
                {item.kind === "code" && <div className="preview-text" style={{ background: "#1b1f24", color: "#d6e2ff" }}>{item.content}</div>}
                {item.kind === "link" && (
                  <>
                    <div className="preview-link-domain">{(() => { try { return new URL(item.content).hostname; } catch { return ""; } })()}</div>
                    <div className="preview-link-url">{item.content}</div>
                  </>
                )}
                {item.kind === "file" && <div>{item.content}</div>}
                {item.edited && <div className="card-caption">edited</div>}
                <div style={{ display: "flex", gap: 6, marginTop: 8, flexWrap: "wrap" }}>
                  {(item.kind === "image" || item.kind === "screenshot") && (
                    <button className="icon-btn" disabled={isPasting} onClick={() => openEditor([item.id])}>✎ Edit</button>
                  )}
                  <button className="icon-btn" onClick={() => toggleFavorite(item)}>{item.favorite ? "★ Favorited" : "☆ Favorite"}</button>
                  <button className="icon-btn" onClick={() => togglePinned(item)}>{item.pinned ? "📌 Pinned" : "Pin"}</button>
                  <button className="icon-btn" disabled={isPasting} onClick={() => pasteOne(item.id)}>
                    {isPasting ? "Pasting…" : "Paste"}
                  </button>
                  <button className="icon-btn danger" disabled={isPasting} onClick={() => removeItem(item)}>Delete</button>
                </div>
              </div>
            ))}
          </div>
        </div>
      </div>

      <div id="toolbar">
        <span><span className="hint-key">↑↓</span>Navigate</span>
        <span><span className="hint-key">Shift</span>Range</span>
        <span><span className="hint-key">⌘/Ctrl</span>Toggle</span>
        <span><span className="hint-key">Enter</span>Paste selection</span>
        <span><span className="hint-key">Esc</span>Close</span>
        <span className="spacer"></span>
        <span className="truncate">Copies, then sends <span className="dest">⌘/Ctrl+V</span></span>
      </div>

      <div id="toast" className={toastMsg ? "show" : ""}>{toastMsg}</div>

      {confirmClear && (
        <div
          style={{
            position: "fixed", inset: 0, background: "rgba(0,0,0,0.35)",
            display: "flex", alignItems: "center", justifyContent: "center", zIndex: 1000,
          }}
          onMouseDown={(e) => { if (e.target === e.currentTarget) setConfirmClear(null); }}
        >
          <div style={{ background: "var(--bg-panel)", border: "1px solid var(--border-strong)", borderRadius: 10, padding: 18, width: 300, boxShadow: "0 20px 50px rgba(0,0,0,0.3)" }}>
            <div style={{ fontSize: 14, fontWeight: 600, marginBottom: 6 }}>Clear clipboard history?</div>
            <div style={{ fontSize: 12.5, color: "var(--text-muted)", marginBottom: 16 }}>
              This deletes {confirmClear.toDelete} item(s). Favorites ({confirmClear.favCount}) will be kept.
            </div>
            <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
              <button className="icon-btn" onClick={() => setConfirmClear(null)}>Cancel</button>
              <button className="icon-btn danger" disabled={isPasting} onClick={confirmClearNow}>Clear</button>
            </div>
          </div>
        </div>
      )}

      {pasteRecovery && (
        <div className="modal-backdrop" onMouseDown={(e) => { if (e.target === e.currentTarget) setPasteRecovery(null); }}>
          <div className="recovery-dialog" role="dialog" aria-modal="true" aria-labelledby="paste-recovery-title">
            <div id="paste-recovery-title" className="recovery-title">macOS blocked automatic paste</div>
            {pasteRecovery.copied && (
              <div className="recovery-copied">
                Your selection is already copied. To use it now, close Clipet, click the destination, and press ⌘V.
              </div>
            )}
            {!pasteRecovery.copied && (
              <div className="recovery-warning">
                This selection contains multiple clipboard payloads, so it could not be copied safely as one manual paste.
              </div>
            )}
            <div className="recovery-message">{pasteRecovery.message}</div>
            <div className="recovery-actions">
              <button className="icon-btn" onClick={() => setPasteRecovery(null)}>Not now</button>
              <button className="icon-btn" onClick={openAccessibilitySettings}>Open Accessibility</button>
              <button className="icon-btn primary" onClick={retryPaste}>Try Again</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
