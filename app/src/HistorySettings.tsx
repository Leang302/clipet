import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { ThemeChoice } from "./theme";
import type { OcrLanguage, OcrStatus } from "./App";

export type Retention = { days: number; include_favorites: boolean; include_pinned: boolean };
type RetentionStatus = { retention: Retention; eligible: number };
type MonitoringStatus = { paused: boolean; until: number | null };

const PRESETS = [0, 1, 7, 30, 90];

function periodLabel(days: number): string {
  if (days === 0) return "Forever";
  return days === 1 ? "1 day" : `${days} days`;
}

/** History retention, recording pause and full clear, in one dialog. */
export function HistorySettings(props: {
  totalItems: number;
  onClose: () => void;
  toast: (msg: string, ms?: number) => void;
  ocrStatus: OcrStatus | null;
  onSetOcrEnabled: (enabled: boolean) => void;
  onSetOcrLanguage: (language: OcrLanguage) => void;
  theme: ThemeChoice;
  onSetTheme: (theme: ThemeChoice) => void;
}) {
  const { totalItems, onClose, toast, ocrStatus, onSetOcrEnabled, onSetOcrLanguage, theme, onSetTheme } = props;
  const [saved, setSaved] = useState<RetentionStatus | null>(null);
  const [draft, setDraft] = useState<Retention | null>(null);
  const [custom, setCustom] = useState(false);
  const [preview, setPreview] = useState<number | null>(null);
  const [monitor, setMonitor] = useState<MonitoringStatus | null>(null);
  const [confirmClearAll, setConfirmClearAll] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    invoke<RetentionStatus>("history_retention_get").then((s) => {
      setSaved(s);
      setDraft(s.retention);
      setCustom(!PRESETS.includes(s.retention.days));
    });
    invoke<MonitoringStatus>("monitoring_status").then(setMonitor);
  }, []);

  // Live "would delete N items" for the setting being edited.
  useEffect(() => {
    if (!draft) return;
    invoke<number>("history_retention_preview", { retention: draft }).then(setPreview).catch(() => setPreview(null));
  }, [draft?.days, draft?.include_favorites, draft?.include_pinned]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); e.stopImmediatePropagation(); confirmClearAll ? setConfirmClearAll(false) : onClose(); }
    };
    document.addEventListener("keydown", onKey, true);
    return () => document.removeEventListener("keydown", onKey, true);
  });

  if (!saved || !draft) return null;
  const changed = JSON.stringify(draft) !== JSON.stringify(saved.retention);

  async function save() {
    if (!draft) return;
    setBusy(true);
    try {
      const removed = await invoke<number>("history_retention_set", { retention: draft });
      const status = await invoke<RetentionStatus>("history_retention_get");
      setSaved(status);
      setDraft(status.retention);
      toast(removed ? `Saved · deleted ${removed} expired item${removed === 1 ? "" : "s"}` : "Saved");
    } catch (e) {
      toast(String(e), 6000);
    } finally {
      setBusy(false);
    }
  }

  async function clearExpired() {
    setBusy(true);
    try {
      const removed = await invoke<number>("history_clear_expired");
      setSaved(await invoke<RetentionStatus>("history_retention_get"));
      toast(`Deleted ${removed} expired item${removed === 1 ? "" : "s"}`);
    } catch (e) {
      toast(String(e), 6000);
    } finally {
      setBusy(false);
    }
  }

  async function clearAll() {
    setConfirmClearAll(false);
    await invoke("clear_history");
    toast("History cleared");
  }

  const pause = async (minutes: number | null) => setMonitor(await invoke<MonitoringStatus>("pause_monitoring", { minutes }));
  const resume = async () => { await invoke("set_paused", { paused: false }); setMonitor(await invoke<MonitoringStatus>("monitoring_status")); };

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div className="recovery-dialog history-settings" role="dialog" aria-modal="true" aria-labelledby="history-settings-title" onMouseDown={(e) => e.stopPropagation()}>
        <div className="recovery-title" id="history-settings-title">Settings</div>

        <section>
          <div className="hs-row">
            <span>Appearance</span>
            <select aria-label="Theme" value={theme} onChange={(e) => onSetTheme(e.target.value as ThemeChoice)}>
              <option value="system">System</option>
              <option value="light">Light</option>
              <option value="dark">Dark</option>
            </select>
          </div>
          {ocrStatus?.supported && (
            <>
              <label className="hs-check">
                <input type="checkbox" checked={ocrStatus.enabled} onChange={(e) => onSetOcrEnabled(e.target.checked)} />
                Search text in images and screenshots
              </label>
              {ocrStatus.enabled && (
                <label className="hs-row">
                  <span>Recognize</span>
                  <select aria-label="OCR language" value={ocrStatus.language} onChange={(e) => onSetOcrLanguage(e.target.value as OcrLanguage)}>
                    <option value="korean-english">Korean + English</option>
                    <option value="auto">Automatic</option>
                    <option value="english">English</option>
                    <option value="korean">Korean</option>
                  </select>
                </label>
              )}
              {ocrStatus.enabled && ocrStatus.pending > 0 && (
                <p className="hs-note" role="status">Reading {ocrStatus.pending} image{ocrStatus.pending === 1 ? "" : "s"}…</p>
              )}
              {ocrStatus.enabled && !ocrStatus.pending && ocrStatus.updating > 0 && (
                <p className="hs-note" role="status">Improving {ocrStatus.updating} image{ocrStatus.updating === 1 ? "" : "s"}…</p>
              )}
            </>
          )}
        </section>

        <section>
          <label className="hs-row">
            <span>Keep history</span>
            <select
              aria-label="Keep history"
              value={custom ? "custom" : String(draft.days)}
              onChange={(e) => {
                if (e.target.value === "custom") { setCustom(true); setDraft({ ...draft, days: draft.days || 14 }); }
                else { setCustom(false); setDraft({ ...draft, days: Number(e.target.value) }); }
              }}
            >
              {PRESETS.map((d) => <option key={d} value={d}>{d === 0 ? "Forever (no automatic cleanup)" : `For ${periodLabel(d)}`}</option>)}
              <option value="custom">Custom…</option>
            </select>
          </label>
          {custom && (
            <label className="hs-row">
              <span>Days</span>
              <input
                type="number"
                min={1}
                max={36500}
                value={draft.days}
                aria-label="Number of days to keep history"
                onChange={(e) => setDraft({ ...draft, days: Math.max(1, Math.min(36500, Math.floor(Number(e.target.value) || 1))) })}
              />
            </label>
          )}
          {draft.days > 0 && (
            <>
              <label className="hs-check"><input type="checkbox" checked={draft.include_favorites} onChange={(e) => setDraft({ ...draft, include_favorites: e.target.checked })} /> Also delete old favorites</label>
              <label className="hs-check"><input type="checkbox" checked={draft.include_pinned} onChange={(e) => setDraft({ ...draft, include_pinned: e.target.checked })} /> Also delete old pinned items</label>
            </>
          )}
          <p className="hs-note">
            {draft.days === 0
              ? "Nothing is deleted automatically."
              : `Items copied more than ${periodLabel(draft.days)} ago are deleted at startup and every hour. ${draft.include_favorites || draft.include_pinned ? "" : "Favorites and pinned items are kept. "}Snippets and the original files behind file items are never deleted.`}
          </p>
          {changed && preview !== null && draft.days > 0 && (
            <p className={"hs-note" + (preview > 0 ? " warn" : "")} role="status">
              {preview > 0 ? `Saving deletes ${preview} item${preview === 1 ? "" : "s"} right away.` : "Nothing is old enough to delete yet."}
            </p>
          )}
          {!changed && saved.eligible > 0 && (
            <p className="hs-note" role="status">{saved.eligible} item{saved.eligible === 1 ? " is" : "s are"} past the limit.</p>
          )}
          <div className="recovery-actions">
            {!changed && saved.retention.days > 0 && saved.eligible > 0 && (
              <button className="icon-btn" disabled={busy} onClick={clearExpired}>Clear Expired Now</button>
            )}
            <button className={"icon-btn " + (preview && preview > 0 ? "danger" : "primary")} disabled={!changed || busy} onClick={save}>
              {changed && preview && preview > 0 ? `Save and delete ${preview}` : "Save"}
            </button>
          </div>
        </section>

        <section>
          <div className="hs-row">
            <span>Recording</span>
            <span className="hs-status" role="status">
              {!monitor ? "" : !monitor.paused ? "On" : monitor.until ? `Paused until ${new Date(monitor.until).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}` : "Paused until you resume"}
            </span>
          </div>
          <p className="hs-note">Pause while you copy passwords or other private text. Copies that password managers mark as private are never recorded.</p>
          <div className="recovery-actions">
            {monitor?.paused
              ? <button className="icon-btn primary" onClick={resume}>Resume Recording</button>
              : <>
                  <button className="icon-btn" onClick={() => pause(15)}>Pause 15 min</button>
                  <button className="icon-btn" onClick={() => pause(60)}>Pause 1 hour</button>
                  <button className="icon-btn" onClick={() => pause(null)}>Pause until resumed</button>
                </>}
          </div>
        </section>

        <section>
          {confirmClearAll ? (
            <>
              <p className="hs-note warn" role="alert">Delete all {totalItems} items, including favorites and pinned items? Snippets are kept. This can't be undone.</p>
              <div className="recovery-actions">
                <button className="icon-btn" autoFocus onClick={() => setConfirmClearAll(false)}>Cancel</button>
                <button className="icon-btn danger" onClick={clearAll}>Delete Everything</button>
              </div>
            </>
          ) : (
            <div className="recovery-actions">
              <button className="icon-btn danger" disabled={!totalItems} onClick={() => setConfirmClearAll(true)}>Clear All History…</button>
            </div>
          )}
        </section>

        <div className="recovery-actions">
          <button className="icon-btn" onClick={onClose}>Done</button>
        </div>
      </div>
    </div>
  );
}
