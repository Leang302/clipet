import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type Snippet = {
  id: string;
  name: string;
  keyword: string;
  content: string;
  content_type: string;
  category: string | null;
  enabled: boolean;
  favorite: boolean;
  use_count: number;
  created_at: number;
  updated_at: number;
  last_used_at: number | null;
};

type SnippetSettings = {
  enabled: boolean;
  prefix: string;
  trigger_mode: "space" | "tab" | "both";
  escape_enabled: boolean;
  case_sensitive: boolean;
  default_pause_minutes: number;
};

type SnippetStatus = {
  enabled: boolean;
  paused: boolean;
  paused_until: number | null;
  listener: "starting" | "active" | "needs-permission" | "unsupported";
};

type Draft = {
  id: string | null;
  name: string;
  keyword: string;
  content: string;
  content_type: "text" | "image";
  /** A history image picked for this snippet; null keeps the current image. */
  image_item: string | null;
  /** Preview of the picked (or current) image. */
  imageUrl: string | null;
  enabled: boolean;
};

/** Content handed over from a history item's "Create Snippet…" action. */
export type SnippetSeed = { content: string; imageItem?: string; imageUrl?: string; nonce: number };

const isImageSnippet = (s: Snippet) => s.content_type === "image";

const PAUSE_CHOICES = [5, 15, 30, 60];

function clock(ms: number) {
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

export function SnippetsView(props: {
  seed: SnippetSeed | null;
  isPasting: boolean;
  onPaste: (id: string) => void;
  toast: (msg: string, ms?: number) => void;
  onOpenAccessibility: () => void;
  /** Image items in history, newest first, offered when picking a snippet image. */
  historyImages: string[];
}) {
  const { seed, isPasting, onPaste, toast, onOpenAccessibility, historyImages } = props;
  const [snippets, setSnippets] = useState<Snippet[]>([]);
  const [settings, setSettings] = useState<SnippetSettings | null>(null);
  const [status, setStatus] = useState<SnippetStatus | null>(null);
  const [query, setQuery] = useState("");
  const [activeId, setActiveId] = useState<string | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  // Image snippet previews, keyed by id and version so an edit refetches.
  const [previews, setPreviews] = useState<Record<string, string>>({});
  const [historyThumbs, setHistoryThumbs] = useState<Record<string, string>>({});
  const searchRef = useRef<HTMLInputElement>(null);
  const keywordRef = useRef<HTMLInputElement>(null);

  const prefix = settings?.prefix ?? ";";

  async function refresh() {
    setSnippets(await invoke<Snippet[]>("snippets_list"));
  }

  useEffect(() => {
    refresh();
    invoke<SnippetSettings>("snippet_settings_get").then(setSettings);
    invoke<SnippetStatus>("snippets_status").then(setStatus);
    const unlistenChanged = listen("clipet://snippets-changed", () => refresh());
    const unlistenStatus = listen<SnippetStatus>("clipet://snippets-status", (e) => setStatus(e.payload));
    searchRef.current?.focus();
    return () => {
      unlistenChanged.then((f) => f());
      unlistenStatus.then((f) => f());
    };
  }, []);

  // "Create Snippet…" from history opens a prefilled editor.
  useEffect(() => {
    if (!seed) return;
    setShowSettings(false);
    setError(null);
    setDraft({
      id: null, name: "", keyword: "", content: seed.content, enabled: true,
      content_type: seed.imageItem ? "image" : "text",
      image_item: seed.imageItem ?? null,
      imageUrl: seed.imageUrl ?? null,
    });
  }, [seed?.nonce]);

  const previewKey = (s: Snippet) => `${s.id}:${s.updated_at}`;
  useEffect(() => {
    for (const s of snippets) {
      const key = previewKey(s);
      if (!isImageSnippet(s) || previews[key] !== undefined) continue;
      setPreviews((p) => ({ ...p, [key]: "" }));
      invoke<string>("snippet_image_preview", { id: s.id })
        .then((url) => setPreviews((p) => ({ ...p, [key]: url })))
        .catch(() => {});
    }
  }, [snippets]);

  // History thumbnails, loaded only while choosing an image.
  const choosingImage = draft?.content_type === "image";
  useEffect(() => {
    if (!choosingImage) return;
    for (const id of historyImages) {
      if (historyThumbs[id] !== undefined) continue;
      setHistoryThumbs((t) => ({ ...t, [id]: "" }));
      invoke<string>("get_thumbnail_data_url", { id })
        .then((url) => setHistoryThumbs((t) => ({ ...t, [id]: url })))
        .catch(() => {});
    }
  }, [choosingImage, historyImages.join(",")]);

  useEffect(() => {
    if (draft && draft.id === null) keywordRef.current?.focus();
  }, [draft?.id, seed?.nonce]);

  const visible = useMemo(() => {
    const q = query.trim().toLowerCase().replace(new RegExp(`^\\${prefix}`), "");
    if (!q) return snippets;
    return snippets.filter((s) =>
      s.keyword.toLowerCase().includes(q) || s.name.toLowerCase().includes(q) || (!isImageSnippet(s) && s.content.toLowerCase().includes(q)),
    );
  }, [snippets, query, prefix]);

  useEffect(() => {
    if (!activeId || !visible.some((s) => s.id === activeId)) setActiveId(visible[0]?.id ?? null);
  }, [visible.map((s) => s.id).join(",")]);

  const active = snippets.find((s) => s.id === activeId) ?? null;

  // Live clash warning while typing a shortcut (the backend enforces it too).
  const draftKeyword = draft ? draft.keyword.trim().replace(new RegExp(`^\\${prefix}`), "") : "";
  const clash = draft && draftKeyword
    ? snippets.find((s) => s.id !== draft.id && s.keyword.toLowerCase() === draftKeyword.toLowerCase())
    : undefined;

  function startNew() {
    setShowSettings(false);
    setError(null);
    setDraft({ id: null, name: "", keyword: "", content: "", content_type: "text", image_item: null, imageUrl: null, enabled: true });
  }

  function startEdit(s: Snippet) {
    setShowSettings(false);
    setError(null);
    const image = isImageSnippet(s);
    setDraft({
      id: s.id, name: s.name, keyword: s.keyword, enabled: s.enabled,
      content: image ? "" : s.content,
      content_type: image ? "image" : "text",
      image_item: null,
      imageUrl: image ? previews[previewKey(s)] || null : null,
    });
  }

  async function save() {
    if (!draft) return;
    try {
      const { id, name, keyword, content, content_type, image_item, enabled } = draft;
      const saved = await invoke<Snippet>("snippet_save", {
        input: { id, name, keyword, content, content_type, image_item, enabled },
      });
      setDraft(null);
      setError(null);
      await refresh();
      setActiveId(saved.id);
      toast(`Saved — type ${prefix}${saved.keyword} then ${settings?.trigger_mode === "tab" ? "Tab" : "Space"}`);
    } catch (e) {
      setError(String(e));
    }
  }

  async function remove(id: string) {
    setConfirmDelete(null);
    await invoke("snippet_delete", { id });
    refresh();
  }

  async function duplicate(id: string) {
    try {
      const copy = await invoke<Snippet>("snippet_duplicate", { id });
      await refresh();
      setActiveId(copy.id);
      toast("Duplicated (disabled until you edit it)");
    } catch (e) {
      toast(String(e), 6000);
    }
  }

  async function setEnabled(s: Snippet, enabled: boolean) {
    await invoke("snippet_set_enabled", { id: s.id, enabled });
    refresh();
  }

  async function copy(id: string) {
    try {
      await invoke("copy_snippet", { id });
      toast("Copied");
    } catch (e) {
      toast(String(e), 6000);
    }
  }

  async function updateSettings(patch: Partial<SnippetSettings>) {
    if (!settings) return;
    try {
      setSettings(await invoke<SnippetSettings>("snippet_settings_set", { settings: { ...settings, ...patch } }));
    } catch (e) {
      toast(String(e), 6000);
    }
  }

  function onKeyDown(e: KeyboardEvent) {
    const editing = !!draft;
    if (e.key === "Escape") {
      e.preventDefault();
      if (confirmDelete) setConfirmDelete(null);
      else if (editing) { setDraft(null); setError(null); }
      else if (showSettings) setShowSettings(false);
      else invoke("hide_history");
      return;
    }
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter" && editing) {
      e.preventDefault();
      save();
      return;
    }
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "n") {
      e.preventDefault();
      startNew();
      return;
    }
    if (editing || showSettings) return;
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!visible.length) return;
      const idx = Math.max(0, visible.findIndex((s) => s.id === activeId));
      const next = e.key === "ArrowDown" ? Math.min(visible.length - 1, idx + 1) : Math.max(0, idx - 1);
      setActiveId(visible[next].id);
      return;
    }
    if (e.key === "Enter" && active) {
      e.preventDefault();
      if (!e.repeat) onPaste(active.id);
      return;
    }
    if (document.activeElement !== searchRef.current && e.key.length === 1 && !e.metaKey && !e.ctrlKey && !e.altKey) {
      searchRef.current?.focus();
    }
  }

  useEffect(() => {
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  });

  const triggerLabel = settings?.trigger_mode === "tab" ? "Tab" : settings?.trigger_mode === "both" ? "Space or Tab" : "Space";

  return (
    <>
      <div id="list-panel">
        <div id="search-row">
          <input
            ref={searchRef}
            id="search-input"
            placeholder="Search snippets…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
          <button className="icon-btn primary" onClick={startNew} title="New snippet (⌘N)">+ New</button>
        </div>
        <StatusBar status={status} settings={settings} onOpenAccessibility={onOpenAccessibility} />
        <div id="item-list">
          {!visible.length && (
            <div id="empty-list">
              {query ? "No snippets match your search." : "No snippets yet. Right-click a copied item and choose Create Snippet, or click + New."}
            </div>
          )}
          {visible.map((s) => (
            <div
              key={s.id}
              className={"clip-item snippet-item" + (activeId === s.id ? " active selected" : "") + (s.enabled ? "" : " disabled")}
              onClick={() => { setActiveId(s.id); if (draft && draft.id !== s.id) setDraft(null); }}
              onDoubleClick={() => onPaste(s.id)}
            >
              {isImageSnippet(s) && (
                <div className="clip-thumb">{previews[previewKey(s)] ? <img src={previews[previewKey(s)]} alt="" draggable={false} /> : <span>🖼️</span>}</div>
              )}
              <div className="clip-meta">
                <div className="clip-title">
                  <span className="snippet-trigger">{prefix}{s.keyword}</span>
                  <span className="snippet-name">{s.name}</span>
                </div>
                <div className="clip-sub">
                  <span className="truncate">{isImageSnippet(s) ? "Image" : s.content.replace(/\s+/g, " ").slice(0, 60)}</span>
                </div>
                <div className="clip-sub">
                  {!s.enabled && <span>Disabled ·</span>}
                  <span>Used {s.use_count}×</span>
                </div>
              </div>
            </div>
          ))}
        </div>
      </div>

      <div id="preview-panel">
        <div id="preview-header">
          <span>{showSettings ? "Snippet settings" : draft ? (draft.id ? "Edit snippet" : "New snippet") : active ? active.name : "Snippets"}</span>
          <button className="icon-btn" onClick={() => { setDraft(null); setShowSettings((v) => !v); }}>
            {showSettings ? "Done" : "⚙ Settings"}
          </button>
        </div>
        <div id="preview-body">
          {showSettings && settings && <SettingsForm settings={settings} onChange={updateSettings} />}

          {!showSettings && draft && (
            <div className="snippet-form">
              <label>
                <span>Shortcut</span>
                <div className="snippet-keyword-field">
                  <span className="snippet-prefix">{prefix}</span>
                  <input
                    ref={keywordRef}
                    value={draft.keyword}
                    placeholder="name"
                    spellCheck={false}
                    onChange={(e) => setDraft({ ...draft, keyword: e.target.value })}
                  />
                </div>
                {clash
                  ? <small className="snippet-error">{prefix}{clash.keyword} is already used by “{clash.name}”.</small>
                  : <small>Type {prefix}{draftKeyword || "name"} then {triggerLabel} anywhere to insert it.</small>}
              </label>
              <label>
                <span>Name</span>
                <input value={draft.name} placeholder="Optional — defaults to the shortcut" onChange={(e) => setDraft({ ...draft, name: e.target.value })} />
              </label>
              <div className="snippet-type" role="radiogroup" aria-label="Snippet type">
                {(["text", "image"] as const).map((t) => (
                  <button
                    key={t}
                    role="radio"
                    aria-checked={draft.content_type === t}
                    className={draft.content_type === t ? "on" : ""}
                    onClick={() => setDraft({ ...draft, content_type: t })}
                  >
                    {t === "text" ? "Text" : "Image"}
                  </button>
                ))}
              </div>
              {draft.content_type === "text" ? (
                <label>
                  <span>Content</span>
                  <textarea
                    value={draft.content}
                    rows={8}
                    spellCheck={false}
                    onChange={(e) => setDraft({ ...draft, content: e.target.value })}
                  />
                </label>
              ) : (
                <div className="snippet-image-field">
                  {draft.imageUrl
                    ? <img className="snippet-image-preview" src={draft.imageUrl} alt="" draggable={false} />
                    : <div className="snippet-image-empty">Choose an image below.</div>}
                  <small>Pastes into apps that accept images, like chat, mail and notes. To use a new image, copy it first and it will appear here.</small>
                  {historyImages.length > 0 ? (
                    <div className="snippet-image-grid">
                      {historyImages.map((id) => (
                        <button
                          key={id}
                          className={"snippet-image-choice" + (draft.image_item === id ? " on" : "")}
                          title="Use this image"
                          onClick={() => setDraft({ ...draft, image_item: id, imageUrl: historyThumbs[id] || null })}
                        >
                          {historyThumbs[id] ? <img src={historyThumbs[id]} alt="" draggable={false} /> : <span>🖼️</span>}
                        </button>
                      ))}
                    </div>
                  ) : (
                    <small>No images in your history yet.</small>
                  )}
                </div>
              )}
              <label className="snippet-check">
                <input type="checkbox" checked={draft.enabled} onChange={(e) => setDraft({ ...draft, enabled: e.target.checked })} />
                <span>Enabled</span>
              </label>
              {error && <div className="snippet-error">{error}</div>}
              <div className="snippet-actions">
                <button className="icon-btn" onClick={() => { setDraft(null); setError(null); }}>Cancel</button>
                <button className="icon-btn primary" disabled={!!clash} onClick={save}>Save <span className="hint">⌘↵</span></button>
              </div>
            </div>
          )}

          {!showSettings && !draft && active && (
            <div className="preview-card">
              <div className="snippet-detail-trigger">
                <span className="snippet-trigger">{prefix}{active.keyword}</span>
                {!active.enabled && <span className="snippet-badge">Disabled</span>}
              </div>
              {isImageSnippet(active)
                ? (previews[previewKey(active)]
                    ? <img className="snippet-image-preview" src={previews[previewKey(active)]} alt="" draggable={false} />
                    : <div className="snippet-image-empty">Loading image…</div>)
                : <div className="preview-text selectable">{active.content}</div>}
              <div className="card-caption">
                Used {active.use_count}× · updated {new Date(active.updated_at).toLocaleDateString()}
              </div>
              <div className="snippet-actions wrap">
                <button className="icon-btn primary" disabled={isPasting} onClick={() => onPaste(active.id)}>{isPasting ? "Pasting…" : "Paste"}</button>
                <button className="icon-btn" onClick={() => copy(active.id)}>Copy</button>
                <button className="icon-btn" onClick={() => startEdit(active)}>Edit</button>
                <button className="icon-btn" onClick={() => duplicate(active.id)}>Duplicate</button>
                <button className="icon-btn" onClick={() => setEnabled(active, !active.enabled)}>{active.enabled ? "Disable" : "Enable"}</button>
                {confirmDelete === active.id
                  ? <button className="icon-btn danger" onClick={() => remove(active.id)}>Confirm delete</button>
                  : <button className="icon-btn danger" onClick={() => setConfirmDelete(active.id)}>Delete</button>}
              </div>
            </div>
          )}

          {!showSettings && !draft && !active && (
            <div id="preview-empty">Snippets turn a short shortcut into saved text or an image anywhere you type.</div>
          )}
        </div>
      </div>
    </>
  );
}

function StatusBar(props: { status: SnippetStatus | null; settings: SnippetSettings | null; onOpenAccessibility: () => void }) {
  const { status, settings, onOpenAccessibility } = props;
  if (!status || !settings) return null;
  const pauseDefault = settings.default_pause_minutes;

  let text: string;
  let tone = "ok";
  let action: React.ReactNode = null;
  if (status.listener === "unsupported") {
    text = "Expansion isn't available on this OS yet";
    tone = "warn";
  } else if (status.listener === "needs-permission") {
    text = "Needs Accessibility permission to expand";
    tone = "warn";
    action = <button className="icon-btn" onClick={onOpenAccessibility}>Open</button>;
  } else if (!status.enabled) {
    text = "Text expansion is off";
    tone = "muted";
  } else if (status.paused) {
    text = status.paused_until ? `Paused until ${clock(status.paused_until)}` : "Paused until you resume";
    tone = "muted";
    action = <button className="icon-btn" onClick={() => invoke("snippets_resume")}>Resume</button>;
  } else {
    text = "Expanding as you type";
    action = (
      <select
        className="snippet-pause-select"
        value=""
        onChange={(e) => {
          const v = e.target.value;
          if (v) invoke("snippets_pause", { minutes: v === "indefinite" ? null : Number(v) });
        }}
      >
        <option value="">Pause…</option>
        {PAUSE_CHOICES.map((m) => (
          <option key={m} value={m}>{m < 60 ? `${m} min` : "1 hour"}{m === pauseDefault ? " (default)" : ""}</option>
        ))}
        <option value="indefinite">Until I resume</option>
      </select>
    );
  }
  return (
    <div className={`snippet-status ${tone}`}>
      <span className="dot" />
      <span className="truncate">{text}</span>
      {action}
    </div>
  );
}

function SettingsForm(props: { settings: SnippetSettings; onChange: (patch: Partial<SnippetSettings>) => void }) {
  const { settings, onChange } = props;
  const [prefixInput, setPrefixInput] = useState(settings.prefix);
  useEffect(() => setPrefixInput(settings.prefix), [settings.prefix]);

  return (
    <div className="snippet-form">
      <label className="snippet-check">
        <input type="checkbox" checked={settings.enabled} onChange={(e) => onChange({ enabled: e.target.checked })} />
        <span>Enable Smart Snippets</span>
      </label>
      <label>
        <span>Expand when I press</span>
        <select value={settings.trigger_mode} onChange={(e) => onChange({ trigger_mode: e.target.value as SnippetSettings["trigger_mode"] })}>
          <option value="space">Space (adds a space after)</option>
          <option value="tab">Tab (nothing added)</option>
          <option value="both">Space or Tab</option>
        </select>
      </label>
      <label>
        <span>Prefix</span>
        <input
          className="snippet-prefix-input"
          value={prefixInput}
          maxLength={1}
          onChange={(e) => setPrefixInput(e.target.value)}
          onBlur={() => { if (prefixInput !== settings.prefix) onChange({ prefix: prefixInput }); }}
        />
      </label>
      <label className="snippet-check">
        <input type="checkbox" checked={settings.escape_enabled} onChange={(e) => onChange({ escape_enabled: e.target.checked })} />
        <span>Type {settings.prefix}{settings.prefix}name to get {settings.prefix}name without expanding</span>
      </label>
      <label className="snippet-check">
        <input type="checkbox" checked={settings.case_sensitive} onChange={(e) => onChange({ case_sensitive: e.target.checked })} />
        <span>Case-sensitive shortcuts</span>
      </label>
      <label>
        <span>Pause duration</span>
        <select value={settings.default_pause_minutes} onChange={(e) => onChange({ default_pause_minutes: Number(e.target.value) })}>
          {PAUSE_CHOICES.map((m) => <option key={m} value={m}>{m < 60 ? `${m} minutes` : "1 hour"}</option>)}
        </select>
      </label>
    </div>
  );
}
