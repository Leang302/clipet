import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { BUILTIN_PRESETS, type Preset } from "./snippetPresets";
import { ParamFields, usePreview } from "./ParamForm";
import { Image as ImageIcon, Settings2 } from "lucide-react";
import { SearchField } from "./SearchField";

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
  /** What follows the inserted text: nothing, one space, or a line break. */
  suffix: Suffix;
  /** JSON-encoded Parameter[] — parse with `parseParameters`. `"[]"` for a static snippet. */
  parameters: string;
  /** JSON object of per-OS templates — parse with `parseVariants`. `"{}"` for none. */
  variants?: string;
};

export type Platform = "macos" | "windows" | "linux";
const PLATFORMS: Platform[] = ["macos", "windows", "linux"];
const PLATFORM_LABELS: Record<Platform, string> = { macos: "macOS", windows: "Windows", linux: "Linux" };
type Variants = Partial<Record<Platform, string>>;

export function parseVariants(json: string | undefined): Variants {
  try {
    const v = JSON.parse(json ?? "{}");
    return v && typeof v === "object" && !Array.isArray(v) ? v : {};
  } catch {
    return {};
  }
}

/** Mirrors the backend's `resolve_template`: the OS's non-empty variant, else the default. */
function resolveTemplate(content: string, variantsJson: string | undefined, platform: Platform | undefined): string {
  const v = platform ? parseVariants(variantsJson)[platform] : undefined;
  return v ? v : content;
}

/** One `{{name}}` placeholder's type and validation, matching the Rust `Parameter`
 * shape exactly (flattened, internally tagged by `kind`) so it round-trips untouched. */
export type ParamKind =
  | { kind: "text"; min_length: number | null; max_length: number | null; pattern: string | null }
  | { kind: "integer"; min: number | null; max: number | null }
  | { kind: "url"; schemes: string[] | null }
  | { kind: "date"; format: string | null }
  | { kind: "dropdown"; options: string[] };

export type Parameter = { name: string; required: boolean; default: string | null } & ParamKind;

export function parseParameters(json: string): Parameter[] {
  try {
    const v = JSON.parse(json);
    return Array.isArray(v) ? v : [];
  } catch {
    return [];
  }
}

/** `{{name}}` placeholders in template order, deduped — a client-side mirror
 * of the Rust scan used only to drive the editor UI; the backend re-validates
 * on save, so this never needs to be perfectly exact. Escaped `\{{name}}` is
 * skipped. */
export function detectPlaceholders(content: string): string[] {
  const seen = new Set<string>();
  const order: string[] = [];
  const re = /(\\)?\{\{\s*([A-Za-z0-9_]+)\s*\}\}/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(content))) {
    if (m[1]) continue;
    if (!seen.has(m[2])) {
      seen.add(m[2]);
      order.push(m[2]);
    }
  }
  return order;
}

function defaultParameter(name: string): Parameter {
  return { name, kind: "text", min_length: null, max_length: null, pattern: null, required: true, default: null };
}

/** Rebuilds `current` to match `names` exactly: keeps existing config for a
 * name that's still present, adds a default Text config for a new one, drops
 * configs for names no longer in the template. */
function syncParameters(current: Parameter[], names: string[]): Parameter[] {
  const byName = new Map(current.map((p) => [p.name, p]));
  return names.map((name) => byName.get(name) ?? defaultParameter(name));
}

type Suffix = "exact" | "space" | "newline";
const SUFFIX_LABELS: Record<Suffix, string> = {
  exact: "Nothing — exact text",
  space: "One space",
  newline: "New line",
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
  /** App holding macOS secure input (a focused password field); typing is hidden from every app meanwhile. */
  blocked_by: string | null;
  /** Which per-OS template variant applies on this machine. */
  platform?: Platform;
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
  suffix: Suffix;
  enabled: boolean;
  parameters: Parameter[];
  variants: Variants;
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
  onPaste: (id: string, values?: Record<string, string>) => void;
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
  const [showPresets, setShowPresets] = useState(false);
  const [importingPreset, setImportingPreset] = useState<string | null>(null);
  const [kindFilter, setKindFilter] = useState<"all" | "static" | "params">("all");
  const [testingId, setTestingId] = useState<string | null>(null);
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
    setShowPresets(false);
    setError(null);
    setDraft({
      id: null, name: "", keyword: "", content: seed.content, enabled: true,
      content_type: seed.imageItem ? "image" : "text",
      image_item: seed.imageItem ?? null,
      suffix: "exact",
      imageUrl: seed.imageUrl ?? null,
      parameters: syncParameters([], detectPlaceholders(seed.content)),
      variants: {},
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

  // Keep `draft.parameters` in lockstep with the `{{placeholder}}`s actually
  // typed in the content, so the user never has to declare a name twice and
  // never silently loses a parameter's config by deleting then retyping it.
  const detectedParams = useMemo(
    () => (draft && draft.content_type === "text" ? detectPlaceholders(draft.content) : []),
    [draft?.content, draft?.content_type],
  );
  useEffect(() => {
    if (!draft || draft.content_type !== "text") return;
    const synced = syncParameters(draft.parameters, detectedParams);
    const changed = synced.length !== draft.parameters.length || synced.some((p, i) => p !== draft.parameters[i]);
    if (changed) setDraft((d) => (d ? { ...d, parameters: synced } : d));
  }, [detectedParams.join(","), draft?.content_type]);

  function updateParam(name: string, patch: Partial<Parameter>) {
    setDraft((d) => (d ? { ...d, parameters: d.parameters.map((p) => (p.name === name ? ({ ...p, ...patch } as Parameter) : p)) } : d));
  }

  function setParamKind(name: string, kind: ParamKind["kind"]) {
    const base = { name, required: true, default: null } as const;
    const byKind: Record<ParamKind["kind"], Parameter> = {
      text: { ...base, kind: "text", min_length: null, max_length: null, pattern: null },
      integer: { ...base, kind: "integer", min: null, max: null },
      url: { ...base, kind: "url", schemes: null },
      date: { ...base, kind: "date", format: null },
      dropdown: { ...base, kind: "dropdown", options: [] },
    };
    setDraft((d) => (d ? { ...d, parameters: d.parameters.map((p) => (p.name === name ? { ...byKind[kind], required: p.required, default: null } : p)) } : d));
  }

  const hasParamSnippets = useMemo(() => snippets.some((s) => parseParameters(s.parameters).length > 0), [snippets]);
  const visible = useMemo(() => {
    const q = query.trim().toLowerCase().replace(new RegExp(`^\\${prefix}`), "");
    const byKind = kindFilter === "all" ? snippets
      : snippets.filter((s) => (parseParameters(s.parameters).length > 0) === (kindFilter === "params"));
    if (!q) return byKind;
    return byKind.filter((s) =>
      s.keyword.toLowerCase().includes(q) || s.name.toLowerCase().includes(q) || (!isImageSnippet(s) && s.content.toLowerCase().includes(q)),
    );
  }, [snippets, query, prefix, kindFilter]);

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
    setShowPresets(false);
    setError(null);
    setDraft({ id: null, name: "", keyword: "", content: "", content_type: "text", image_item: null, imageUrl: null, suffix: "exact", enabled: true, parameters: [], variants: {} });
  }

  function startEdit(s: Snippet) {
    setShowSettings(false);
    setShowPresets(false);
    setError(null);
    const image = isImageSnippet(s);
    setDraft({
      id: s.id, name: s.name, keyword: s.keyword, enabled: s.enabled,
      content: image ? "" : s.content,
      content_type: image ? "image" : "text",
      image_item: null,
      imageUrl: image ? previews[previewKey(s)] || null : null,
      suffix: s.suffix ?? "exact",
      parameters: image ? [] : syncParameters(parseParameters(s.parameters), detectPlaceholders(s.content)),
      variants: image ? {} : parseVariants(s.variants),
    });
  }

  async function save() {
    if (!draft) return;
    try {
      const { id, name, keyword, content, content_type, image_item, suffix, enabled, parameters, variants } = draft;
      const text = content_type === "text";
      const saved = await invoke<Snippet>("snippet_save", {
        input: { id, name, keyword, content, content_type, image_item, suffix, enabled, parameters: text ? parameters : [], variants: text ? variants : {} },
      });
      setDraft(null);
      setError(null);
      await refresh();
      setActiveId(saved.id);
      const savedParams = parseParameters(saved.parameters);
      toast(
        savedParams.length > 0
          ? `Saved — type ${prefix}${saved.keyword} ${savedParams.map((p) => `<${p.name}>`).join(" ")} then Tab`
          : `Saved — type ${prefix}${saved.keyword} then ${settings?.trigger_mode === "tab" ? "Tab" : "Space"}`,
      );
    } catch (e) {
      setError(String(e));
    }
  }

  async function remove(id: string) {
    setConfirmDelete(null);
    await invoke("snippet_delete", { id });
    refresh();
  }

  async function importPreset(preset: Preset) {
    const taken = snippets.some((s) => s.keyword.toLowerCase() === preset.keyword.toLowerCase());
    if (taken) {
      toast(`${prefix}${preset.keyword} is already in use — edit or rename the existing snippet first.`, 6000);
      return;
    }
    setImportingPreset(preset.id);
    try {
      await invoke<Snippet>("snippet_save", {
        input: {
          id: null, name: preset.name, keyword: preset.keyword, content: preset.content,
          content_type: "text", image_item: null, suffix: preset.suffix, enabled: true,
          parameters: preset.parameters,
          variants: preset.variants ?? {},
        },
      });
      await refresh();
      toast(`Added ${prefix}${preset.keyword} — edit it anytime like any other snippet.`);
    } catch (e) {
      toast(String(e), 6000);
    } finally {
      setImportingPreset(null);
    }
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

  async function copy(id: string, values?: Record<string, string>) {
    try {
      await invoke("copy_snippet", { id, values: values ?? null });
      toast("Copied");
    } catch (e) {
      toast(String(e), 6000);
    }
  }

  const takesParams = (s: Snippet) => !isImageSnippet(s) && parseParameters(s.parameters).length > 0;

  /** A parameterized snippet can't be pasted until its values are known, so
   * Paste/Enter/double-click open its Test panel to ask for them instead. */
  function pasteOrAsk(s: Snippet) {
    if (takesParams(s)) {
      setActiveId(s.id);
      setTestingId(s.id);
    } else {
      onPaste(s.id);
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
      else if (showPresets) setShowPresets(false);
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
    if (editing || showSettings || showPresets) return;
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
      if (!e.repeat) pasteOrAsk(active);
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
          <SearchField inputRef={searchRef} value={query} onChange={setQuery} placeholder="Search snippets…" />
          <button className="icon-btn" onClick={() => { setDraft(null); setShowSettings(false); setShowPresets((v) => !v); }} title="Browse starter snippets">Presets</button>
          <button className="icon-btn primary" onClick={startNew} title="New snippet (⌘N)">+ New</button>
        </div>
        <StatusBar status={status} settings={settings} onOpenAccessibility={onOpenAccessibility} />
        {(hasParamSnippets || kindFilter !== "all") && (
          <div className="snippet-kind-filter" role="radiogroup" aria-label="Show snippets">
            {([["all", "All"], ["static", "Plain"], ["params", "With parameters"]] as const).map(([k, label]) => (
              <button key={k} role="radio" aria-checked={kindFilter === k} className={kindFilter === k ? "on" : ""} onClick={() => setKindFilter(k)}>
                {label}
              </button>
            ))}
          </div>
        )}
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
              onDoubleClick={() => pasteOrAsk(s)}
            >
              {isImageSnippet(s) && (
                <div className="clip-thumb">{previews[previewKey(s)] ? <img src={previews[previewKey(s)]} alt="" draggable={false} /> : <ImageIcon size={16} strokeWidth={1.75} aria-hidden="true" />}</div>
              )}
              <div className="clip-meta">
                <div className="clip-title">
                  <span className="snippet-trigger">{prefix}{s.keyword}</span>
                  <span className="snippet-name">{s.name}</span>
                  {parseParameters(s.parameters).length > 0 && (
                    <span className="snippet-param-badge" title="Takes parameters — complete with Tab">{"{ }"}</span>
                  )}
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
          <span>{showPresets ? "Starter snippets" : showSettings ? "Snippet settings" : draft ? (draft.id ? "Edit snippet" : "New snippet") : active ? active.name : "Snippets"}</span>
          <button className="icon-btn" onClick={() => { setDraft(null); setShowSettings((v) => !v); setShowPresets(false); }}>
            {showSettings ? "Done" : <><Settings2 size={13} strokeWidth={1.9} aria-hidden="true" />Settings</>}
          </button>
        </div>
        <div id="preview-body">
          {showPresets && (
            <PresetsBrowser
              prefix={prefix}
              existingKeywords={new Set(snippets.map((s) => s.keyword.toLowerCase()))}
              importingId={importingPreset}
              onImport={importPreset}
            />
          )}

          {showSettings && settings && (
            <>
              <SettingsForm settings={settings} onChange={updateSettings} />
              <BackupSection toast={toast} onImported={refresh} />
            </>
          )}

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
                {clash ? (
                  <small className="snippet-error">{prefix}{clash.keyword} is already used by “{clash.name}”.</small>
                ) : detectedParams.length > 0 ? (
                  <small>
                    Type {prefix}{draftKeyword || "name"} {detectedParams.map((n) => `<${n}>`).join(" ")} then Tab to fill in
                    values inline (quote multi-word ones: "like this"). Only Tab completes a parameterized shortcut, not Space.
                  </small>
                ) : (
                  <small>Type {prefix}{draftKeyword || "name"} then {triggerLabel} anywhere to insert it.</small>
                )}
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
                <>
                  <label>
                    <span>Content</span>
                    <textarea
                      value={draft.content}
                      rows={8}
                      spellCheck={false}
                      placeholder="Plain text, or add {{name}} placeholders to turn this into a parameterized snippet."
                      onChange={(e) => setDraft({ ...draft, content: e.target.value })}
                    />
                    <small>
                      Wrap a word in double braces, like <code>{"{{port}}"}</code>, to make it a fill-in parameter.
                    </small>
                  </label>
                  <PlatformVariantsEditor
                    variants={draft.variants}
                    placeholders={detectedParams}
                    onChange={(variants) => setDraft((d) => (d ? { ...d, variants } : d))}
                  />
                  {detectedParams.length > 0 && (
                    <>
                      <ParametersEditor parameters={draft.parameters} onChangeParam={updateParam} onChangeKind={setParamKind} />
                      <TestPanel
                        title={PLATFORMS.some((p) => draft.variants[p]) ? "Preview (default version)" : "Preview"}
                        content={draft.content}
                        parameters={draft.parameters}
                        suffix={draft.suffix}
                      />
                    </>
                  )}
                </>
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
                          {historyThumbs[id] ? <img src={historyThumbs[id]} alt="" draggable={false} /> : <ImageIcon size={16} strokeWidth={1.75} aria-hidden="true" />}
                        </button>
                      ))}
                    </div>
                  ) : (
                    <small>No images in your history yet.</small>
                  )}
                </div>
              )}
              {draft.content_type === "text" && (
                <label>
                  <span>After the text</span>
                  <select value={draft.suffix} onChange={(e) => setDraft({ ...draft, suffix: e.target.value as Suffix })}>
                    {(Object.keys(SUFFIX_LABELS) as Suffix[]).map((k) => <option key={k} value={k}>{SUFFIX_LABELS[k]}</option>)}
                  </select>
                  <small>The key you press to expand ({triggerLabel}) is never inserted. Spaces and line breaks you typed in the content are kept exactly.</small>
                </label>
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

          {!showSettings && !showPresets && !draft && active && (
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
              {!isImageSnippet(active) && PLATFORMS.filter((p) => parseVariants(active.variants)[p]).map((p) => (
                <div key={p} className="snippet-variant-view">
                  <span>
                    {PLATFORM_LABELS[p]} version
                    {status?.platform === p && <small> · used on this computer</small>}
                  </span>
                  <div className="preview-text selectable">{parseVariants(active.variants)[p]}</div>
                </div>
              ))}
              {!isImageSnippet(active) && parseParameters(active.parameters).length > 0 && (
                <div className="snippet-param-summary">
                  <span>Parameters:</span>
                  {parseParameters(active.parameters).map((p) => (
                    <span key={p.name} className="snippet-param-chip">
                      {p.name}: {p.kind}{p.required ? "" : " (optional)"}
                    </span>
                  ))}
                  <small>Only Tab completes this shortcut — Space never will.</small>
                </div>
              )}
              <div className="card-caption">
                Used {active.use_count}× · updated {new Date(active.updated_at).toLocaleDateString()}
              </div>
              {takesParams(active) && testingId === active.id && (
                <TestPanel
                  key={active.id + active.updated_at}
                  content={resolveTemplate(active.content, active.variants, status?.platform)}
                  parameters={parseParameters(active.parameters)}
                  suffix={active.suffix ?? "exact"}
                  autoFocus
                  busy={isPasting}
                  onPaste={(values) => onPaste(active.id, values)}
                  onCopy={(values) => copy(active.id, values)}
                  onClose={() => setTestingId(null)}
                />
              )}
              <div className="snippet-actions wrap">
                {takesParams(active) ? (
                  testingId !== active.id && (
                    <button className="icon-btn primary" onClick={() => setTestingId(active.id)}>Fill in &amp; paste…</button>
                  )
                ) : (
                  <>
                    <button className="icon-btn primary" disabled={isPasting} onClick={() => onPaste(active.id)}>{isPasting ? "Pasting…" : "Paste"}</button>
                    <button className="icon-btn" onClick={() => copy(active.id)}>Copy</button>
                  </>
                )}
                <button className="icon-btn" onClick={() => startEdit(active)}>Edit</button>
                <button className="icon-btn" onClick={() => duplicate(active.id)}>Duplicate</button>
                <button className="icon-btn" onClick={() => setEnabled(active, !active.enabled)}>{active.enabled ? "Disable" : "Enable"}</button>
                {confirmDelete === active.id
                  ? <button className="icon-btn danger" onClick={() => remove(active.id)}>Confirm delete</button>
                  : <button className="icon-btn danger" onClick={() => setConfirmDelete(active.id)}>Delete</button>}
              </div>
            </div>
          )}

          {!showSettings && !showPresets && !draft && !active && (
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
  } else if (status.blocked_by && status.enabled && !status.paused) {
    text = `Blocked by ${status.blocked_by}: a password field has hidden your typing`;
    tone = "warn";
    action = (
      <span
        className="snippet-status-help"
        tabIndex={0}
        role="note"
        title={`macOS hides keystrokes from all apps while ${status.blocked_by} has a password field focused, so no snippet can expand anywhere. Leave or close that field (if it stays stuck, quit and reopen ${status.blocked_by}). To insert a snippet into the password field itself, select it here and press Enter.`}
      >
        Why?
      </span>
    );
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

/** Fill in a parameterized snippet's values and see exactly what it would
 * insert, validated by the backend. With `onPaste`/`onCopy` it is also how a
 * parameterized snippet gets pasted from Clipet's own window; without them
 * (the editor) it is a preview only. Enter pastes, Esc closes; keys never
 * leak to the list's own shortcuts while typing here. */
function TestPanel(props: {
  content: string;
  parameters: Parameter[];
  suffix: Suffix;
  title?: string;
  autoFocus?: boolean;
  busy?: boolean;
  onPaste?: (values: Record<string, string>) => void;
  onCopy?: (values: Record<string, string>) => void;
  onClose?: () => void;
}) {
  const { content, parameters, suffix, title, autoFocus, busy, onPaste, onCopy, onClose } = props;
  const [values, setValues] = useState<Record<string, string>>({});
  const [triedSubmit, setTriedSubmit] = useState(false);
  const { preview, templateError } = usePreview(content, parameters, values, suffix);
  const rootRef = useRef<HTMLDivElement>(null);
  const valid = !!preview && preview.text !== null;
  const showErrors = triedSubmit || Object.values(values).some((v) => v !== "");

  function submit() {
    if (!onPaste) return;
    if (!valid) {
      setTriedSubmit(true);
      const first = parameters.find((p) => preview?.errors[p.name]);
      if (first) rootRef.current?.querySelector<HTMLElement>(`[data-param="${CSS.escape(first.name)}"]`)?.focus();
      return;
    }
    onPaste(values);
  }

  return (
    <div
      ref={rootRef}
      className="snippet-test"
      onKeyDown={(e) => {
        // ⌘↵ / ⌘N and friends stay app-wide.
        if (e.metaKey || e.ctrlKey) return;
        if (e.key === "Escape") {
          if (onClose) { e.stopPropagation(); e.preventDefault(); onClose(); }
          return;
        }
        // Typing here must not move the list selection or jump to search.
        e.stopPropagation();
        if (e.key === "Enter" && onPaste && !e.repeat && (e.target as HTMLElement).tagName !== "BUTTON") { e.preventDefault(); submit(); }
      }}
    >
      <span className="snippet-params-title">{title ?? "Fill in values"}</span>
      <ParamFields
        parameters={parameters}
        values={values}
        errors={preview?.errors ?? {}}
        showErrors={showErrors}
        autoFocusFirst={autoFocus}
        onChange={(name, value) => setValues((s) => ({ ...s, [name]: value }))}
      />
      <div className="snippet-test-output" aria-live="polite">
        {templateError
          ? <span className="snippet-error">{templateError}</span>
          : valid
            ? <pre className="selectable">{preview!.text}</pre>
            : <span className="snippet-test-placeholder">The result appears here once every value is valid.</span>}
      </div>
      {(onPaste || onCopy || onClose) && (
        <div className="snippet-actions">
          {onClose && <button className="icon-btn" onClick={onClose}>Cancel</button>}
          {onCopy && <button className="icon-btn" disabled={!valid} onClick={() => onCopy(values)}>Copy</button>}
          {onPaste && (
            <button className="icon-btn primary" disabled={busy} onClick={submit}>
              {busy ? "Pasting…" : "Paste"} <span className="hint">↵</span>
            </button>
          )}
        </div>
      )}
    </div>
  );
}

/** Browse-and-import list for the built-in starter library (Part F). Never
 * overwrites: a preset whose keyword is already taken shows "In use" and
 * can't be imported until the existing snippet is renamed or removed. */
function PresetsBrowser(props: {
  prefix: string;
  existingKeywords: Set<string>;
  importingId: string | null;
  onImport: (preset: Preset) => void;
}) {
  const { prefix, existingKeywords, importingId, onImport } = props;
  const categories = useMemo(() => {
    const byCategory = new Map<Preset["category"], Preset[]>();
    for (const p of BUILTIN_PRESETS) {
      if (!byCategory.has(p.category)) byCategory.set(p.category, []);
      byCategory.get(p.category)!.push(p);
    }
    return byCategory;
  }, []);

  return (
    <div className="snippet-presets">
      <p className="snippet-note">
        Optional starter snippets. Importing never changes or removes anything you already have.
      </p>
      {[...categories.entries()].map(([category, presets]) => (
        <div key={category} className="snippet-preset-group">
          <span className="snippet-preset-category">{category}</span>
          {presets.map((p) => {
            const inUse = existingKeywords.has(p.keyword.toLowerCase());
            return (
              <div key={p.id} className="snippet-preset-row">
                <div className="snippet-preset-row-head">
                  <span className="snippet-trigger">{prefix}{p.keyword}</span>
                  <span className="snippet-name">{p.name}</span>
                  <button
                    className="icon-btn primary"
                    disabled={inUse || importingId === p.id}
                    onClick={() => onImport(p)}
                  >
                    {inUse ? "In use" : importingId === p.id ? "Adding…" : "Add"}
                  </button>
                </div>
                <code className="snippet-preset-preview">{p.content}</code>
                {PLATFORMS.filter((os) => p.variants?.[os]).map((os) => (
                  <div key={os} className="snippet-variant-view">
                    <span>{PLATFORM_LABELS[os]} version</span>
                    <code className="snippet-preset-preview">{p.variants![os]}</code>
                  </div>
                ))}
                {p.parameters.length > 0 && (
                  <div className="snippet-param-summary">
                    {p.parameters.map((param) => (
                      <span key={param.name} className="snippet-param-chip">
                        {param.name}: {param.kind}{param.required ? "" : " (optional)"}
                      </span>
                    ))}
                  </div>
                )}
                {p.note && <small>{p.note}</small>}
              </div>
            );
          })}
        </div>
      ))}
    </div>
  );
}

type ImportReport = { imported: number; skipped: { keyword: string; reason: string }[] };

/** Export to a JSON file in Downloads; import one back. Import never
 * overwrites — anything skipped is listed with the reason. */
function BackupSection(props: { toast: (msg: string, ms?: number) => void; onImported: () => void }) {
  const { toast, onImported } = props;
  const fileRef = useRef<HTMLInputElement>(null);
  const [report, setReport] = useState<ImportReport | null>(null);
  const [busy, setBusy] = useState(false);

  async function exportAll() {
    setBusy(true);
    try {
      const r = await invoke<{ path: string; count: number; skipped_images: number }>("snippets_export");
      toast(
        `Exported ${r.count} snippet${r.count === 1 ? "" : "s"} to ${r.path}` +
          (r.skipped_images ? ` · ${r.skipped_images} image snippet${r.skipped_images === 1 ? "" : "s"} not included` : ""),
        8000,
      );
    } catch (e) {
      toast(String(e), 6000);
    } finally {
      setBusy(false);
    }
  }

  async function importFile(file: File) {
    setBusy(true);
    try {
      const r = await invoke<ImportReport>("snippets_import", { json: await file.text() });
      setReport(r);
      onImported();
    } catch (e) {
      setReport(null);
      toast(String(e), 6000);
    } finally {
      setBusy(false);
      if (fileRef.current) fileRef.current.value = "";
    }
  }

  return (
    <div className="snippet-form snippet-backup">
      <span className="snippet-params-title">Back up</span>
      <small>Export saves your text snippets (with parameters and per-OS versions) to a file in Downloads. Import adds snippets from such a file and never changes ones you already have.</small>
      <div className="snippet-actions wrap">
        <button className="icon-btn" disabled={busy} onClick={exportAll}>Export snippets…</button>
        <button className="icon-btn" disabled={busy} onClick={() => fileRef.current?.click()}>Import snippets…</button>
        <input
          ref={fileRef}
          type="file"
          accept=".json,application/json"
          hidden
          aria-label="Snippet file to import"
          onChange={(e) => { const f = e.target.files?.[0]; if (f) importFile(f); }}
        />
      </div>
      {report && (
        <div className="snippet-import-report" role="status">
          <span>Imported {report.imported} snippet{report.imported === 1 ? "" : "s"}{report.skipped.length ? `, skipped ${report.skipped.length}:` : "."}</span>
          {report.skipped.length > 0 && (
            <ul>
              {report.skipped.map((s, i) => <li key={i}><code>{s.keyword || "(no shortcut)"}</code> — {s.reason}</li>)}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}

/** Optional per-OS versions of the template (Part E), tucked behind a
 * disclosure so a plain snippet's form stays plain. */
function PlatformVariantsEditor(props: { variants: Variants; placeholders: string[]; onChange: (v: Variants) => void }) {
  const { variants, placeholders, onChange } = props;
  const hasAny = PLATFORMS.some((p) => variants[p] !== undefined);
  return (
    <details className="snippet-variants" open={hasAny}>
      <summary>Different version per OS <small>optional</small></summary>
      <small>The content above is used on any OS without its own version. Every version must use the same parameters.</small>
      {PLATFORMS.map((p) => {
        const on = variants[p] !== undefined;
        const text = variants[p] ?? "";
        let mismatch: string | null = null;
        if (on && text) {
          const used = detectPlaceholders(text);
          const missing = placeholders.filter((n) => !used.includes(n));
          const extra = used.filter((n) => !placeholders.includes(n));
          if (missing.length) mismatch = `Missing ${missing.map((n) => `{{${n}}}`).join(", ")}`;
          else if (extra.length) mismatch = `Uses ${extra.map((n) => `{{${n}}}`).join(", ")}, which the default version doesn't`;
        }
        return (
          <div key={p} className="snippet-variant">
            <label className="snippet-check">
              <input
                type="checkbox"
                checked={on}
                onChange={(e) => {
                  const next = { ...variants };
                  if (e.target.checked) next[p] = "";
                  else delete next[p];
                  onChange(next);
                }}
              />
              <span>{PLATFORM_LABELS[p]}</span>
            </label>
            {on && (
              <textarea
                rows={3}
                spellCheck={false}
                aria-label={`${PLATFORM_LABELS[p]} version`}
                aria-invalid={mismatch ? true : undefined}
                placeholder={`${PLATFORM_LABELS[p]} version…`}
                value={text}
                onChange={(e) => onChange({ ...variants, [p]: e.target.value })}
              />
            )}
            {mismatch && <small className="snippet-error">{mismatch}</small>}
          </div>
        );
      })}
    </details>
  );
}

const PARAM_KIND_LABELS: Record<ParamKind["kind"], string> = {
  text: "Text",
  integer: "Integer",
  url: "URL",
  date: "Date",
  dropdown: "Dropdown",
};

/** One row per detected `{{placeholder}}`, letting the user pick a type and
 * its validation without a separate "insert parameter" step — the template
 * itself is the only place a parameter is declared (Part C4). */
function ParametersEditor(props: {
  parameters: Parameter[];
  onChangeParam: (name: string, patch: Partial<Parameter>) => void;
  onChangeKind: (name: string, kind: ParamKind["kind"]) => void;
}) {
  const { parameters, onChangeParam, onChangeKind } = props;
  const csv = (v: string[] | null) => (v ?? []).join(", ");
  const parseCsv = (v: string) => {
    const items = v.split(",").map((s) => s.trim()).filter(Boolean);
    return items.length ? items : null;
  };

  return (
    <div className="snippet-params">
      <span className="snippet-params-title">Parameters</span>
      {parameters.map((p) => (
        <div key={p.name} className="snippet-param-row">
          <div className="snippet-param-row-head">
            <code className="snippet-param-name">{`{{${p.name}}}`}</code>
            <select value={p.kind} onChange={(e) => onChangeKind(p.name, e.target.value as ParamKind["kind"])}>
              {(Object.keys(PARAM_KIND_LABELS) as ParamKind["kind"][]).map((k) => (
                <option key={k} value={k}>{PARAM_KIND_LABELS[k]}</option>
              ))}
            </select>
            <label className="snippet-check inline">
              <input type="checkbox" checked={p.required} onChange={(e) => onChangeParam(p.name, { required: e.target.checked })} />
              <span>Required</span>
            </label>
          </div>
          <div className="snippet-param-row-fields">
            {!p.required && (
              <label className="snippet-param-field">
                <span>Default</span>
                <input value={p.default ?? ""} onChange={(e) => onChangeParam(p.name, { default: e.target.value || null })} />
              </label>
            )}
            {p.kind === "text" && (
              <>
                <label className="snippet-param-field narrow">
                  <span>Min length</span>
                  <input type="number" min={0} value={p.min_length ?? ""} onChange={(e) => onChangeParam(p.name, { min_length: e.target.value ? Number(e.target.value) : null } as Partial<Parameter>)} />
                </label>
                <label className="snippet-param-field narrow">
                  <span>Max length</span>
                  <input type="number" min={0} value={p.max_length ?? ""} onChange={(e) => onChangeParam(p.name, { max_length: e.target.value ? Number(e.target.value) : null } as Partial<Parameter>)} />
                </label>
                <label className="snippet-param-field">
                  <span>Pattern (regex, optional)</span>
                  <input value={p.pattern ?? ""} onChange={(e) => onChangeParam(p.name, { pattern: e.target.value || null } as Partial<Parameter>)} />
                </label>
              </>
            )}
            {p.kind === "integer" && (
              <>
                <label className="snippet-param-field narrow">
                  <span>Min</span>
                  <input type="number" value={p.min ?? ""} onChange={(e) => onChangeParam(p.name, { min: e.target.value ? Number(e.target.value) : null } as Partial<Parameter>)} />
                </label>
                <label className="snippet-param-field narrow">
                  <span>Max</span>
                  <input type="number" value={p.max ?? ""} onChange={(e) => onChangeParam(p.name, { max: e.target.value ? Number(e.target.value) : null } as Partial<Parameter>)} />
                </label>
              </>
            )}
            {p.kind === "url" && (
              <label className="snippet-param-field">
                <span>Allowed schemes (comma-separated, optional — e.g. https)</span>
                <input value={csv(p.schemes)} onChange={(e) => onChangeParam(p.name, { schemes: parseCsv(e.target.value) } as Partial<Parameter>)} />
              </label>
            )}
            {p.kind === "date" && (
              <label className="snippet-param-field">
                <span>Format (optional, default %Y-%m-%d)</span>
                <input value={p.format ?? ""} placeholder="%Y-%m-%d" onChange={(e) => onChangeParam(p.name, { format: e.target.value || null } as Partial<Parameter>)} />
              </label>
            )}
            {p.kind === "dropdown" && (
              <label className="snippet-param-field">
                <span>Options (comma-separated)</span>
                <input value={csv(p.options)} onChange={(e) => onChangeParam(p.name, { options: parseCsv(e.target.value) ?? [] } as Partial<Parameter>)} />
              </label>
            )}
          </div>
        </div>
      ))}
    </div>
  );
}

function SettingsForm(props: { settings: SnippetSettings; onChange: (patch: Partial<SnippetSettings>) => void }) {
  const { settings, onChange } = props;
  const [prefixInput, setPrefixInput] = useState(settings.prefix);
  useEffect(() => setPrefixInput(settings.prefix), [settings.prefix]);

  return (
    <div className="snippet-form">
      <p className="snippet-note">
        Password fields: typing a shortcut there never expands — macOS hides what you type in password fields from
        other apps, and Clipet doesn't work around that. To insert a snippet there, open Clipet (⌘⇧V), go to
        Snippets and press Enter; it pastes like ⌘V, which works wherever the field allows pasting.
      </p>
      <label className="snippet-check">
        <input type="checkbox" checked={settings.enabled} onChange={(e) => onChange({ enabled: e.target.checked })} />
        <span>Enable Smart Snippets</span>
      </label>
      <label>
        <span>Expand when I press</span>
        <select value={settings.trigger_mode} onChange={(e) => onChange({ trigger_mode: e.target.value as SnippetSettings["trigger_mode"] })}>
          <option value="space">Space</option>
          <option value="tab">Tab</option>
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
