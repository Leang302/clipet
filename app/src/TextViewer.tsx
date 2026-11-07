import { useEffect, useMemo, useRef, useState } from "react";

/** Text longer than this gets a short preview and an Expand button. */
export const LONG_TEXT_CHARS = 1200;
export const LONG_TEXT_LINES = 18;
/** The compact preview never renders more than this many characters. */
export const PREVIEW_CHARS = 4000;
/** Highlighting stops after this many matches; counting doesn't. */
const MAX_MARKS = 2000;

export function isLongText(text: string): boolean {
  if (text.length > LONG_TEXT_CHARS) return true;
  let lines = 1;
  for (let i = 0; i < text.length; i++) if (text.charCodeAt(i) === 10 && ++lines > LONG_TEXT_LINES) return true;
  return false;
}

export function countLines(text: string): number {
  let lines = 1;
  for (let i = 0; i < text.length; i++) if (text.charCodeAt(i) === 10) lines++;
  return lines;
}

/** Case-insensitive match offsets of `query` in `text`. */
export function findMatches(text: string, query: string): number[] {
  if (!query) return [];
  const hay = text.toLowerCase();
  const needle = query.toLowerCase();
  const out: number[] = [];
  for (let i = hay.indexOf(needle); i !== -1; i = hay.indexOf(needle, i + needle.length)) out.push(i);
  return out;
}

// Scroll positions per item, kept for the session so reopening an item
// returns to where it was read.
const scrollMemory = new Map<string, number>();

export function TextViewer(props: {
  id: string;
  text: string;
  mono: boolean;
  onClose: () => void;
  onCopy: (text: string) => void;
  onSave: (text: string) => Promise<void>;
}) {
  const { id, text, mono, onClose, onCopy, onSave } = props;
  const [query, setQuery] = useState("");
  const [current, setCurrent] = useState(0);
  const [wrap, setWrap] = useState(!mono);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(text);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const bodyRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  const matches = useMemo(() => findMatches(text, query), [text, query]);
  useEffect(() => setCurrent(0), [query]);

  // Restore the remembered position once, then remember as the user scrolls.
  useEffect(() => {
    const body = bodyRef.current;
    if (body) body.scrollTop = scrollMemory.get(id) ?? 0;
  }, [id]);

  // Bring the current match into view.
  useEffect(() => {
    if (!matches.length) return;
    bodyRef.current?.querySelector("mark.current")?.scrollIntoView({ block: "center" });
  }, [current, matches]);

  const step = (delta: number) => {
    if (matches.length) setCurrent((c) => (c + delta + matches.length) % matches.length);
  };

  async function save() {
    setSaving(true);
    setError(null);
    try {
      await onSave(draft);
      setEditing(false);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  }

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      const mod = e.metaKey || e.ctrlKey;
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopImmediatePropagation();
        if (editing) { setEditing(false); setDraft(text); setError(null); }
        else if (query && document.activeElement === searchRef.current) setQuery("");
        else onClose();
      } else if (mod && e.key.toLowerCase() === "f" && !editing) {
        e.preventDefault();
        searchRef.current?.focus();
        searchRef.current?.select();
      } else if (mod && e.key.toLowerCase() === "s" && editing) {
        e.preventDefault();
        save();
      } else if (e.key === "Enter" && document.activeElement === searchRef.current) {
        e.preventDefault();
        step(e.shiftKey ? -1 : 1);
      } else if (mod && e.key.toLowerCase() === "g") {
        e.preventDefault();
        step(e.shiftKey ? -1 : 1);
      }
    }
    // Capture phase: the popup's own shortcuts (Enter = paste, Esc = hide)
    // must not fire while reading.
    document.addEventListener("keydown", onKey, true);
    return () => document.removeEventListener("keydown", onKey, true);
  });

  const body = useMemo(() => {
    if (!matches.length) return text;
    const parts: React.ReactNode[] = [];
    let last = 0;
    matches.slice(0, MAX_MARKS).forEach((start, i) => {
      parts.push(text.slice(last, start));
      parts.push(<mark key={i} className={i === current ? "current" : ""}>{text.slice(start, start + query.length)}</mark>);
      last = start + query.length;
    });
    parts.push(text.slice(last));
    return parts;
  }, [text, matches, current, query]);

  const shown = editing ? draft : text;
  return (
    <div className="text-viewer" role="dialog" aria-modal="true" aria-label="Full text">
      <div className="text-viewer-bar">
        {!editing && (
          <div className="text-viewer-search">
            <input
              ref={searchRef}
              value={query}
              placeholder="Find in text (⌘F)"
              aria-label="Find in text"
              onChange={(e) => setQuery(e.target.value)}
            />
            <span className="match-count" aria-live="polite">
              {query ? (matches.length ? `${current + 1} / ${matches.length}` : "No matches") : ""}
            </span>
            <button className="icon-btn" disabled={!matches.length} title="Previous match (⇧Enter)" aria-label="Previous match" onClick={() => step(-1)}>↑</button>
            <button className="icon-btn" disabled={!matches.length} title="Next match (Enter)" aria-label="Next match" onClick={() => step(1)}>↓</button>
          </div>
        )}
        <span className="spacer" />
        {!editing && (
          <label className="text-viewer-wrap">
            <input type="checkbox" checked={wrap} onChange={(e) => setWrap(e.target.checked)} /> Wrap lines
          </label>
        )}
        {editing ? (
          <>
            <button className="icon-btn" onClick={() => { setEditing(false); setDraft(text); setError(null); }}>Cancel</button>
            <button className="icon-btn primary" disabled={saving || draft === text} onClick={save}>{saving ? "Saving…" : "Save (⌘S)"}</button>
          </>
        ) : (
          <>
            <button className="icon-btn" onClick={() => onCopy(text)}>Copy All</button>
            <button className="icon-btn" onClick={() => { setDraft(text); setEditing(true); }}>Edit</button>
            <button className="icon-btn" title="Close (Esc)" onClick={onClose}>Done</button>
          </>
        )}
      </div>
      {error && <div className="text-viewer-error" role="alert">{error}</div>}
      {editing ? (
        <textarea
          className={"text-viewer-body" + (mono ? " mono" : "")}
          value={draft}
          autoFocus
          spellCheck={false}
          onChange={(e) => setDraft(e.target.value)}
        />
      ) : (
        <div
          ref={bodyRef}
          className={"text-viewer-body selectable" + (mono ? " mono" : "") + (wrap ? "" : " nowrap")}
          tabIndex={0}
          onScroll={(e) => scrollMemory.set(id, e.currentTarget.scrollTop)}
        >
          {body}
        </div>
      )}
      <div className="text-viewer-foot">
        {shown.length.toLocaleString()} characters · {countLines(shown).toLocaleString()} lines
        {matches.length > MAX_MARKS && ` · highlighting the first ${MAX_MARKS.toLocaleString()} matches`}
      </div>
    </div>
  );
}
