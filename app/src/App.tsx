import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { applyTheme, loadTheme, saveTheme, THEME_STORAGE_KEY, type ThemeChoice } from "./theme";
import { SnippetsView, type SnippetSeed } from "./Snippets";
import { lineMatches, matchItem, type SearchMatch } from "./search";
import { installPopupDrag } from "./popupDrag";
import { FileCard, fileItemIcon } from "./FileCard";
import { TextViewer, isLongText, countLines, PREVIEW_CHARS } from "./TextViewer";
import { HistorySettings } from "./HistorySettings";
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
  ocr_status: number; // 0 pending, 1 done, 2 failed, 3 running
  ocr_text: string | null;
  ocr_lines: string | null; // JSON OcrLine[]
  ocr_error: string | null;
};

type OcrLine = { text: string; x: number; y: number; w: number; h: number };
type OcrLanguage = "auto" | "korean-english" | "english" | "korean";
type OcrStatus = {
  supported: boolean;
  enabled: boolean;
  language: OcrLanguage;
  pending: number;
  updating: number;
  done: number;
  failed: number;
};

const isImage = (i: ClipItem) => i.kind === "image" || i.kind === "screenshot";

function parseOcrLines(item: ClipItem): OcrLine[] {
  if (!item.ocr_lines) return [];
  try { return JSON.parse(item.ocr_lines) as OcrLine[]; } catch { return []; }
}

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

/** Text recognized in an image: status while pending, otherwise the text with a copy action. */
function OcrSection(props: {
  item: ClipItem;
  enabled: boolean;
  expanded: boolean;
  onCopy: (text: string) => void;
  onRescan: () => void;
}) {
  const { item, enabled, expanded, onCopy, onRescan } = props;
  const text = item.ocr_text?.trim() ? item.ocr_text : null;
  const lineCount = text ? text.split("\n").length : 0;
  const queued = item.ocr_status === 0 || item.ocr_status === 3;
  // `state` is the full message shown in the panel; `badge` the short form
  // next to the "Text in image" heading.
  let state: string | null = null;
  let badge: string | null = null;
  if (queued) {
    state = enabled ? (text ? "Re-scanning…" : "Reading text…") : "Text recognition is off.";
    badge = enabled ? (text ? "re-scanning" : "reading…") : "off";
  } else if (item.ocr_status === 2) {
    state = item.ocr_error || "Couldn't read text in this image.";
    badge = text ? "last scan failed" : "couldn't read";
  } else if (!text) {
    state = "No text detected in this image.";
    badge = "no text";
  }

  // Open by default when the search matched text inside this image, so the
  // match is visible without hunting for it.
  return (
    <details className="ocr-section" open={expanded}>
      <summary>
        Text in image
        {text && <span className="ocr-count"> · {lineCount} line{lineCount === 1 ? "" : "s"}</span>}
        {badge && <span className={"ocr-state" + (item.ocr_status === 2 ? " error" : "")}> · {badge}</span>}
      </summary>
      {text && <div className="ocr-text selectable">{text}</div>}
      {!text && state && <div className={"ocr-empty" + (item.ocr_status === 2 ? " error" : "")}>{state}</div>}
      {text && item.ocr_status === 2 && <div className="ocr-empty error">{state}</div>}
      <div className="ocr-actions">
        {text && <button className="icon-btn" onClick={() => onCopy(text)}>Copy text</button>}
        {enabled && <button className="icon-btn" disabled={queued} onClick={onRescan}>{queued ? "Scanning…" : "Re-scan"}</button>}
      </div>
    </details>
  );
}

function isTextual(item: ClipItem): boolean {
  return item.kind === "text" || item.kind === "link" || item.kind === "code";
}

function previewLabel(item: ClipItem): string {
  if (item.kind === "link") return item.content;
  // File entries store their paths as JSON; the preview holds their names.
  if (item.kind === "file") return item.preview;
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
  // The full-text viewer (its item id) and the history settings dialog.
  const [viewerId, setViewerId] = useState<string | null>(null);
  const [showHistorySettings, setShowHistorySettings] = useState(false);
  // Bumped each time the popup opens, after the list has reloaded.
  const [openCount, setOpenCount] = useState(0);
  // Small list thumbnails and full-size preview images are cached separately:
  // the list never decodes full screenshots, only the previewed items do.
  const [imageUrls, setImageUrls] = useState<Record<string, string>>({});
  const [thumbUrls, setThumbUrls] = useState<Record<string, string>>({});
  const [ocrStatus, setOcrStatus] = useState<OcrStatus | null>(null);
  const requestedThumbs = useRef(new Set<string>());
  const requestedImages = useRef(new Set<string>());
  const [confirmDelete, setConfirmDelete] = useState<string[] | null>(null);
  const [toastMsg, setToastMsg] = useState<string | null>(null);
  const [isPasting, setIsPasting] = useState(false);
  const [theme, setTheme] = useState<ThemeChoice>(() => loadTheme());
  const [confirmClear, setConfirmClear] = useState<{ toDelete: number; favCount: number } | null>(null);
  const [pasteRecovery, setPasteRecovery] = useState<PasteRecovery | null>(null);
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number; item: ClipItem } | null>(null);
  const [snippetSeed, setSnippetSeed] = useState<SnippetSeed | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const toastTimer = useRef<number | undefined>(undefined);
  // State updates are asynchronous, so use a ref as the immediate lock that
  // prevents Enter repeats, double-clicks and buttons from starting a second
  // native paste while the first transfer is still running.
  const pasteInFlightRef = useRef(false);

  useEffect(() => {
    applyTheme(theme);
  }, [theme]);

  useEffect(() => {
    const popup = document.getElementById("clip-popup");
    return popup ? installPopupDrag(popup) : undefined;
  }, []);

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
      refresh().then(() => setOpenCount((n) => n + 1));
      searchRef.current?.focus();
    });
    // An annotation was saved for this item — its file on disk changed, so the
    // data URL we already cached for it is stale and must be dropped.
    const unlistenUpdated = listen<string>("clipflow://item-updated", (event) => {
      const id = event.payload;
      const drop = (prev: Record<string, string>) => {
        const next = { ...prev };
        delete next[id];
        return next;
      };
      setImageUrls(drop);
      setThumbUrls(drop);
      refresh();
    });
    const unlistenOcr = listen("clipflow://ocr-updated", () => {
      refresh();
      refreshOcrStatus();
    });
    refreshOcrStatus();
    return () => {
      unlistenNew.then((f) => f());
      unlistenOpen.then((f) => f());
      unlistenUpdated.then((f) => f());
      unlistenOcr.then((f) => f());
    };
  }, []);

  async function refreshOcrStatus() {
    setOcrStatus(await invoke<OcrStatus>("ocr_status"));
  }

  async function setOcrEnabled(enabled: boolean) {
    setOcrStatus(await invoke<OcrStatus>("ocr_set_enabled", { enabled }));
  }

  async function setOcrLanguage(language: OcrLanguage) {
    setOcrStatus(await invoke<OcrStatus>("ocr_set_language", { language }));
  }

  async function rescan(ids: string[]) {
    setOcrStatus(await invoke<OcrStatus>("ocr_rescan", { ids }));
    refresh();
  }

  const searchResult = useMemo(() => {
    let list = items.slice();
    if (category === "favorites") list = list.filter((i) => i.favorite);
    else if (category !== "all") list = list.filter((i) => i.kind === category);
    // Text recognized inside images is searched alongside regular content.
    const matches = new Map<string, SearchMatch>();
    if (query.trim()) {
      list = list.filter((i) => {
        const m = matchItem(query, `${i.content}\n${i.source_app}`, i.ocr_text);
        if (m) matches.set(i.id, m);
        return !!m;
      });
    }
    switch (sort) {
      case "oldest": list.sort((a, b) => a.created_at - b.created_at); break;
      case "frequent": list.sort((a, b) => b.use_count - a.use_count); break;
      case "favorites": list.sort((a, b) => Number(b.favorite) - Number(a.favorite) || b.created_at - a.created_at); break;
      default: list.sort((a, b) => b.created_at - a.created_at);
    }
    // While searching, better matches come first (stable: the chosen sort
    // still orders items of equal quality).
    if (matches.size) list.sort((a, b) => matches.get(a.id)!.rank - matches.get(b.id)!.rank);
    return { list, matches };
  }, [items, category, query, sort]);
  const visible = searchResult.list;

  const visibleIds = visible.map((i) => i.id);
  // Every open starts on the newest item (the first one shown), not wherever
  // the previous visit left off.
  useEffect(() => {
    if (!openCount) return;
    const first = visibleIds[0] ?? null;
    setActiveId(first);
    setAnchorId(first);
    setSelectedIds(first ? new Set([first]) : new Set());
    document.getElementById("item-list")?.scrollTo(0, 0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [openCount]);
  useEffect(() => {
    if (!activeId || !visibleIds.includes(activeId)) {
      setActiveId(visibleIds[0] ?? null);
    }
    // Items hidden by a filter or search leave the selection, so Enter and
    // Delete only ever act on what's on screen.
    setSelectedIds((prev) => {
      const kept = [...prev].filter((id) => visibleIds.includes(id));
      return kept.length === prev.size ? prev : new Set(kept);
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [visibleIds.join(",")]);

  // Lazily load list thumbnails for visible images (each requested once).
  useEffect(() => {
    visible
      .filter((i) => isImage(i) && !thumbUrls[i.id] && !requestedThumbs.current.has(i.id))
      .forEach((i) => {
        requestedThumbs.current.add(i.id);
        invoke<string>("get_thumbnail_data_url", { id: i.id })
          .then((url) => setThumbUrls((prev) => ({ ...prev, [i.id]: url })))
          .catch(() => {})
          .finally(() => requestedThumbs.current.delete(i.id));
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [visibleIds.join(","), thumbUrls]);

  const queryMatchesOcrOnly = (item: ClipItem) => !!searchResult.matches.get(item.id)?.viaOcr;

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

  async function pasteSnippet(id: string) {
    if (pasteInFlightRef.current) return;
    pasteInFlightRef.current = true;
    setIsPasting(true);
    try {
      await invoke("paste_snippet", { id });
      toast("Pasted");
    } catch (e) {
      handlePasteFailure(e, []);
    } finally {
      pasteInFlightRef.current = false;
      setIsPasting(false);
    }
  }

  function createSnippetFrom(item: ClipItem) {
    setContextMenu(null);
    setSnippetSeed(isImage(item)
      ? { content: "", imageItem: item.id, imageUrl: thumbUrls[item.id], nonce: Date.now() }
      : { content: item.content, nonce: Date.now() });
    setCategory("snippets");
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

  async function copyText(text: string) {
    try {
      await invoke("copy_text", { text });
      toast("Text copied");
    } catch (e) {
      toast(String(e), 6000);
    }
  }

  async function removeItems(ids: string[]) {
    setConfirmDelete(null);
    for (const id of ids) await invoke("delete_item", { id });
    setSelectedIds(new Set());
    refresh();
  }

  function requestDeleteSelection() {
    const ids = orderedSelectedIds.length ? orderedSelectedIds : activeId ? [activeId] : [];
    if (!ids.length || pasteInFlightRef.current) return;
    if (ids.length === 1) void removeItems(ids);
    else setConfirmDelete(ids);
  }

  function requestClearNonFavorites() {
    // Favorites and pinned items are kept by Clear.
    const favCount = items.filter((i) => i.favorite || i.pinned).length;
    const toDelete = items.length - favCount;
    if (!toDelete) {
      toast("Nothing to clear — everything left is a favorite or pinned");
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
    // The Snippets view registers its own keyboard handling.
    if (category === "snippets") return;
    if (showHistorySettings) return;
    // While reading, the viewer owns the keyboard; ↑/↓ still move through
    // the list (and the viewer follows) unless typing in the viewer.
    if (viewerId) {
      const typing = document.activeElement instanceof HTMLInputElement || document.activeElement instanceof HTMLTextAreaElement;
      if (typing || (e.key !== "ArrowDown" && e.key !== "ArrowUp")) return;
    }
    if (contextMenu || confirmDelete || confirmClear) {
      if (e.key === "Escape") {
        e.preventDefault();
        setContextMenu(null);
        setConfirmDelete(null);
        setConfirmClear(null);
      }
      return;
    }
    const searchFocused = document.activeElement === searchRef.current;

    // Escape peels back one layer at a time: search text, then a multi-item
    // selection, then the popup itself.
    if (e.key === "Escape") {
      e.preventDefault();
      if (query) setQuery("");
      else if (selectedIds.size > 1) setSelectedIds(activeId ? new Set([activeId]) : new Set());
      else invoke("hide_history");
      return;
    }
    if (!searchFocused && (e.key === "Delete" || e.key === "Backspace")) {
      e.preventDefault();
      requestDeleteSelection();
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
  }, [visibleIds.join(","), activeId, anchorId, selectedIds, category, contextMenu, confirmDelete, confirmClear, query, viewerId, showHistorySettings]);

  // Order selections by their position in the current (sorted/filtered) list,
  // not by Set insertion order — a Shift-range selects in list order already,
  // but a Ctrl/Cmd-click can add items out of order, which otherwise leaks
  // into preview stacking order, "Edit Selected" batch order, and multi-paste
  // order. Spec: "Default to history-display order for batch operations...
  // Never silently reorder screenshots during transfer."
  const orderedSelectedIds = visibleIds.filter((id) => selectedIds.has(id));
  const selectedItems = orderedSelectedIds.map(byId).filter((i): i is ClipItem => !!i);
  // The viewer follows the active item through the list; anything that
  // isn't text closes it.
  const viewerItem = viewerId ? items.find((i) => i.id === viewerId && (i.kind === "text" || i.kind === "code")) ?? null : null;
  useEffect(() => {
    if (!viewerId || !activeId || activeId === viewerId) return;
    const next = items.find((i) => i.id === activeId);
    setViewerId(next && (next.kind === "text" || next.kind === "code") ? next.id : null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeId]);
  const previewList = selectedItems.length ? selectedItems : activeId ? [byId(activeId)!].filter(Boolean) : [];

  // Full-size images only for what the preview shows.
  useEffect(() => {
    previewList
      .filter((i) => isImage(i) && !imageUrls[i.id] && !requestedImages.current.has(i.id))
      .forEach((i) => {
        requestedImages.current.add(i.id);
        invoke<string>("get_image_data_url", { filename: i.content })
          .then((url) => setImageUrls((prev) => ({ ...prev, [i.id]: url })))
          .catch(() => {})
          .finally(() => requestedImages.current.delete(i.id));
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [previewList.map((i) => i.id).join(","), imageUrls]);

  const multi = selectedIds.size > 1;
  const q = query.trim().toLowerCase();

  return (
    <div id="clip-popup" className={viewerItem ? "reading" : undefined}>
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
          <div className="sidebar-divider" />
          <div
            className={"sidebar-item" + (category === "snippets" ? " active" : "")}
            onClick={() => setCategory("snippets")}
          >
            <span className="icon">✂️</span>
            <span>Snippets</span>
          </div>
          <button
            className="icon-btn"
            style={{ width: "100%", marginTop: 10 }}
            title="Deletes everything except favorites and pinned items"
            disabled={isPasting}
            onClick={requestClearNonFavorites}
          >
            🗑 Clear
          </button>
          <button
            className="icon-btn"
            style={{ width: "100%", marginTop: 6 }}
            title="Automatic cleanup, pausing recording, clearing everything"
            onClick={() => setShowHistorySettings(true)}
          >
            ⚙ History Settings…
          </button>
          {ocrStatus?.supported && (
            <label className="sidebar-setting" title="Reads text in screenshots and images on this device, so search can find them">
              <input type="checkbox" checked={ocrStatus.enabled} onChange={(e) => setOcrEnabled(e.target.checked)} />
              <span>
                Search text in images
                {ocrStatus.enabled && ocrStatus.pending > 0 && <small>Reading {ocrStatus.pending} image{ocrStatus.pending === 1 ? "" : "s"}…</small>}
                {ocrStatus.enabled && !ocrStatus.pending && ocrStatus.updating > 0 && <small>Improving {ocrStatus.updating} image{ocrStatus.updating === 1 ? "" : "s"}…</small>}
              </span>
            </label>
          )}
          {ocrStatus?.supported && ocrStatus.enabled && (
            <select
              className="sidebar-select"
              title="Languages to recognize in images"
              value={ocrStatus.language}
              onChange={(e) => setOcrLanguage(e.target.value as OcrLanguage)}
            >
              <option value="korean-english">Text: Korean + English</option>
              <option value="auto">Text: Automatic</option>
              <option value="english">Text: English</option>
              <option value="korean">Text: Korean</option>
            </select>
          )}
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

        {category === "snippets" && (
          <SnippetsView
            seed={snippetSeed}
            isPasting={isPasting}
            onPaste={pasteSnippet}
            toast={toast}
            onOpenAccessibility={openAccessibilitySettings}
            historyImages={items.filter(isImage).slice(0, 24).map((i) => i.id)}
          />
        )}

        {category !== "snippets" && <>
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
          <div
            id="item-list"
            tabIndex={-1}
            onMouseDown={(e) => {
              // Clicking empty space below the items clears a multi-selection.
              if (e.target === e.currentTarget && selectedIds.size) {
                setSelectedIds(activeId ? new Set([activeId]) : new Set());
              }
            }}
          >
            {!visible.length && (
              <div id="empty-list">No items match {query ? "your search" : "this filter"}.</div>
            )}
            {visible.map((item) => {
              const meta = TYPE_META[item.kind] ?? { icon: "📋", label: item.kind };
              const thumb = thumbUrls[item.id];
              const selected = selectedIds.has(item.id);
              return (
                <div
                  key={item.id}
                  className={"clip-item" + (selected ? " selected" : "") + (activeId === item.id ? " active" : "") + (multi ? "" : " single")}
                  aria-selected={selected}
                  // Shift/⌘-click would otherwise also extend the webview's
                  // native text selection across the whole popup.
                  onMouseDown={(e) => { if (e.shiftKey || e.metaKey || e.ctrlKey) e.preventDefault(); }}
                  onClick={(e) => onItemClick(e, item)}
                  onDoubleClick={() => pasteOne(item.id)}
                  onContextMenu={(e) => {
                    e.preventDefault();
                    setActiveId(item.id);
                    setContextMenu({ x: e.clientX, y: e.clientY, item });
                  }}
                >
                  <div className="clip-thumb">{thumb ? <img src={thumb} alt="" draggable={false} /> : <span>{item.kind === "file" ? fileItemIcon(item.content) : meta.icon}</span>}</div>
                  <div className="clip-meta">
                    <div className="clip-title">{previewLabel(item)}</div>
                    <div className="clip-sub">
                      {item.pinned && <span>📌</span>}
                      {item.favorite && <span>★</span>}
                      <span>{meta.label}</span>
                      <span>·</span>
                      <span>{timeAgo(item.created_at)}</span>
                      {item.edited && <span>· edited</span>}
                      {queryMatchesOcrOnly(item) && <span className="ocr-chip">text in image</span>}
                    </div>
                  </div>
                  {multi && <div className="clip-check">{selected ? "✓" : ""}</div>}
                </div>
              );
            })}
          </div>
        </div>

        <div id="preview-panel">
          {viewerItem && (
            <TextViewer
              key={viewerItem.id}
              id={viewerItem.id}
              text={viewerItem.content}
              mono={viewerItem.kind === "code"}
              onClose={() => setViewerId(null)}
              onCopy={copyText}
              onSave={async (content) => {
                await invoke("update_item_text", { id: viewerItem.id, content });
                await refresh();
                toast("Saved");
              }}
            />
          )}
          {!viewerItem && <>
          <div id="preview-header">
            <span>
              {previewList.length > 1
                ? "Selected items, in paste order"
                : previewList[0]
                  ? `${TYPE_META[previewList[0].kind]?.label ?? previewList[0].kind} · ${timeAgo(previewList[0].created_at)}`
                  : "No selection"}
            </span>
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
                {isImage(item) && (
                  imageUrls[item.id]
                    ? (
                      <div className="ocr-image">
                        <img src={imageUrls[item.id]} alt="" draggable={false} />
                        <button
                          className="image-edit-btn"
                          title="Edit / annotate this image"
                          aria-label="Edit image"
                          disabled={isPasting}
                          onClick={() => openEditor([item.id])}
                        >
                          ✎ Edit
                        </button>
                        {/* Highlight the lines of recognized text that match the search. */}
                        {q && parseOcrLines(item)
                          .filter((line) => lineMatches(query, line.text))
                          .map((line, i) => (
                            <div
                              key={i}
                              className="ocr-hit"
                              style={{ left: `${line.x * 100}%`, top: `${line.y * 100}%`, width: `${line.w * 100}%`, height: `${line.h * 100}%` }}
                            />
                          ))}
                      </div>
                    )
                    : <div className="card-caption">Loading image…</div>
                )}
                {isImage(item) && (
                  <OcrSection
                    // Remount per item so "expanded" follows the item shown.
                    key={`${item.id}-${queryMatchesOcrOnly(item)}`}
                    item={item}
                    enabled={!!ocrStatus?.enabled}
                    expanded={queryMatchesOcrOnly(item)}
                    onCopy={copyText}
                    onRescan={() => rescan([item.id])}
                  />
                )}
                {(item.kind === "text" || item.kind === "code") && (
                  <>
                    <div
                      className={"preview-text selectable" + (isLongText(item.content) ? " clamped" : "") + (item.kind === "code" ? " code" : "")}
                      style={item.kind === "code" ? { background: "#1b1f24", color: "#d6e2ff" } : undefined}
                    >
                      {item.content.length > PREVIEW_CHARS ? item.content.slice(0, PREVIEW_CHARS) : item.content}
                    </div>
                    {isLongText(item.content) && (
                      <div className="expand-row">
                        <span className="card-caption">{item.content.length.toLocaleString()} characters · {countLines(item.content).toLocaleString()} lines</span>
                        <button className="icon-btn" title="Read, search and edit the full text" onClick={() => setViewerId(item.id)}>⤢ Expand</button>
                      </div>
                    )}
                  </>
                )}
                {item.kind === "link" && (
                  <>
                    <div className="preview-link-domain">{(() => { try { return new URL(item.content).hostname; } catch { return ""; } })()}</div>
                    <div className="preview-link-url selectable">{item.content}</div>
                  </>
                )}
                {item.kind === "file" && (
                  <FileCard key={item.id} id={item.id} isPasting={isPasting} onPaste={() => pasteOne(item.id)} toast={toast} />
                )}
                {item.edited && <div className="card-caption">edited</div>}
                <div style={{ display: "flex", gap: 6, marginTop: 8, flexWrap: "wrap" }}>
                  {(item.kind === "image" || item.kind === "screenshot") && (
                    <button className="icon-btn" disabled={isPasting} onClick={() => openEditor([item.id])}>✎ Edit</button>
                  )}
                  <button className="icon-btn" onClick={() => toggleFavorite(item)}>{item.favorite ? "★ Favorited" : "☆ Favorite"}</button>
                  <button className="icon-btn" onClick={() => togglePinned(item)}>{item.pinned ? "📌 Pinned" : "Pin"}</button>
                  {(isTextual(item) || isImage(item)) && (
                    <button className="icon-btn" onClick={() => createSnippetFrom(item)}>✂️ Snippet</button>
                  )}
                  {item.kind !== "file" && (
                    <button className="icon-btn" disabled={isPasting} onClick={() => pasteOne(item.id)}>
                      {isPasting ? "Pasting…" : "Paste"}
                    </button>
                  )}
                  <button className="icon-btn danger" disabled={isPasting} onClick={() => removeItem(item)}>Delete</button>
                </div>
              </div>
            ))}
          </div>
          </>}
        </div>
        </>}
      </div>

      {contextMenu && (
        <div className="context-backdrop" onMouseDown={() => setContextMenu(null)} onContextMenu={(e) => { e.preventDefault(); setContextMenu(null); }}>
          <div
            className="context-menu"
            style={{ left: Math.min(contextMenu.x, window.innerWidth - 190), top: Math.min(contextMenu.y, window.innerHeight - 150) }}
            onMouseDown={(e) => e.stopPropagation()}
          >
            {(isTextual(contextMenu.item) || isImage(contextMenu.item)) && (
              <button onClick={() => createSnippetFrom(contextMenu.item)}>✂️ Create Snippet…</button>
            )}
            <button disabled={isPasting} onClick={() => { const id = contextMenu.item.id; setContextMenu(null); pasteOne(id); }}>Paste</button>
            {isImage(contextMenu.item) && ocrStatus?.enabled && (
              <button onClick={() => {
                // Re-scan the whole selection when the clicked image is part of it.
                const ids = selectedIds.has(contextMenu.item.id)
                  ? orderedSelectedIds.filter((id) => { const i = byId(id); return !!i && isImage(i); })
                  : [contextMenu.item.id];
                setContextMenu(null);
                rescan(ids);
              }}>Re-scan text</button>
            )}
            <button onClick={() => { const item = contextMenu.item; setContextMenu(null); toggleFavorite(item); }}>
              {contextMenu.item.favorite ? "Remove from Favorites" : "Add to Favorites"}
            </button>
            <button className="danger" disabled={isPasting} onClick={() => { const item = contextMenu.item; setContextMenu(null); removeItem(item); }}>Delete</button>
          </div>
        </div>
      )}

      {category === "snippets" ? (
        <div id="toolbar">
          <span><span className="hint-key">↑↓</span>Navigate</span>
          <span><span className="hint-key">Enter</span>Paste snippet</span>
          <span><span className="hint-key">⌘N</span>New</span>
          <span><span className="hint-key">Esc</span>Close</span>
          <span className="spacer"></span>
          <span className="truncate">Or just type the shortcut anywhere</span>
        </div>
      ) : (
      <div id="toolbar">
        <span><span className="hint-key">↑↓</span>Navigate</span>
        <span><span className="hint-key">Shift</span>Range</span>
        <span><span className="hint-key">⌘/Ctrl</span>Toggle</span>
        <span><span className="hint-key">Enter</span>Paste</span>
        <span><span className="hint-key">⌫</span>Delete</span>
        <span><span className="hint-key">Esc</span>{query ? "Clear search" : multi ? "Clear selection" : "Close"}</span>
        <span className="spacer"></span>
        <span className="truncate">Search also finds <span className="dest">text in images</span></span>
      </div>
      )}

      <div id="toast" className={toastMsg ? "show" : ""}>{toastMsg}</div>

      {showHistorySettings && (
        <HistorySettings totalItems={items.length} onClose={() => setShowHistorySettings(false)} toast={toast} />
      )}

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
              This deletes {confirmClear.toDelete} item(s). Favorites and pinned items ({confirmClear.favCount}) and your snippets are kept.
            </div>
            <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
              <button className="icon-btn" onClick={() => setConfirmClear(null)}>Cancel</button>
              <button className="icon-btn danger" disabled={isPasting} onClick={confirmClearNow}>Clear</button>
            </div>
          </div>
        </div>
      )}

      {confirmDelete && (
        <div className="modal-backdrop" onMouseDown={(e) => { if (e.target === e.currentTarget) setConfirmDelete(null); }}>
          <div className="recovery-dialog" role="dialog" aria-modal="true">
            <div className="recovery-title">Delete {confirmDelete.length} items?</div>
            <div className="recovery-message">They'll be removed from clipboard history. This can't be undone.</div>
            <div className="recovery-actions">
              <button className="icon-btn" autoFocus onClick={() => setConfirmDelete(null)}>Cancel</button>
              <button className="icon-btn danger" onClick={() => removeItems(confirmDelete)}>Delete</button>
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
