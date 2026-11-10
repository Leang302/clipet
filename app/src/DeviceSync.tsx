import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type SyncSettings = {
  enabled: boolean;
  paused: boolean;
  sync_text: boolean;
  sync_images: boolean;
  sync_pins: boolean;
  max_text_kb: number;
  max_image_mb: number;
};

type PeerStatus = {
  id: string;
  short_id: string;
  name: string;
  online: boolean;
  added_at: number;
  last_seen: number | null;
  last_sync: number | null;
  last_error: string | null;
};

type Pairing =
  | { state: "idle" }
  | { state: "hosting"; invite: string; expires_at: number }
  | { state: "confirm"; name: string; code: string }
  | { state: "joining"; host_name: string; code: string | null }
  | { state: "done"; name: string }
  | { state: "failed"; reason: string };

export type SyncStatus = {
  settings: SyncSettings;
  device_name: string;
  key_storage: "keychain" | "file" | null;
  running: boolean;
  error: string | null;
  node: { device_id: string; short_id: string; relay_connected: boolean; peers: PeerStatus[]; pairing: Pairing } | null;
};

const TEXT_SIZES = [64, 256, 1024, 2048];
const IMAGE_SIZES = [2, 5, 10, 25];

function ago(ms: number | null, now: number): string {
  if (!ms) return "never";
  const s = Math.max(0, Math.round((now - ms) / 1000));
  if (s < 45) return "just now";
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return new Date(ms).toLocaleDateString();
}

function sizeLabel(kb: number): string {
  return kb >= 1024 ? `${kb / 1024} MB` : `${kb} KB`;
}

/** One-line summary for the Settings page row. */
export function syncSummary(status: SyncStatus | null): string {
  if (!status) return "";
  if (!status.settings.enabled) return "Off";
  if (status.settings.paused) return "Paused";
  if (status.error) return "Not running";
  const peers = status.node?.peers ?? [];
  if (!peers.length) return "On · no devices yet";
  const online = peers.filter((p) => p.online).length;
  return `On · ${online} of ${peers.length} online`;
}

export function useSyncStatus(): [SyncStatus | null, (s: SyncStatus) => void] {
  const [status, setStatus] = useState<SyncStatus | null>(null);
  useEffect(() => {
    const load = () => invoke<SyncStatus>("sync_status").then(setStatus).catch(() => {});
    load();
    const unlisten = listen("clipet://sync-changed", load);
    return () => { unlisten.then((f) => f()); };
  }, []);
  return [status, setStatus];
}

/** Settings → Device Sync. */
export function DeviceSync(props: { onBack: () => void; toast: (msg: string, ms?: number) => void }) {
  const { onBack, toast } = props;
  const [status, setStatus] = useSyncStatus();
  const [now, setNow] = useState(Date.now());
  const [joinOpen, setJoinOpen] = useState(false);
  const [inviteText, setInviteText] = useState("");
  const [renaming, setRenaming] = useState<string | null>(null);
  const [nameDraft, setNameDraft] = useState("");
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);

  async function run(command: string, args?: Record<string, unknown>, done?: string) {
    setBusy(true);
    try {
      setStatus(await invoke<SyncStatus>(command, args));
      if (done) toast(done);
      return true;
    } catch (e) {
      toast(String(e), 6000);
      return false;
    } finally {
      setBusy(false);
    }
  }

  if (!status) return null;
  const { settings, node } = status;
  const pairing: Pairing = node?.pairing ?? { state: "idle" };
  const peers = node?.peers ?? [];
  const saveOptions = (patch: Partial<SyncSettings>) => run("sync_save_settings", { settings: { ...settings, ...patch } });

  function startRename(id: string, current: string) {
    setRenaming(id);
    setNameDraft(current);
  }

  async function finishRename() {
    if (renaming === null) return;
    const ok = renaming === "this"
      ? await run("sync_rename_this_device", { name: nameDraft })
      : await run("sync_rename_device", { id: renaming, name: nameDraft });
    if (ok) setRenaming(null);
  }

  async function join() {
    if (await run("sync_join", { invite: inviteText })) {
      setJoinOpen(false);
      setInviteText("");
    }
  }

  async function copyInvite() {
    try {
      await invoke("sync_copy_invite");
      toast("Invitation copied. It isn't saved in your history.");
    } catch (e) {
      toast(String(e), 6000);
    }
  }

  const renameField = (
    <span className="ds-rename">
      <input
        autoFocus
        aria-label="Device name"
        value={nameDraft}
        maxLength={64}
        onChange={(e) => setNameDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") { e.preventDefault(); finishRename(); }
          if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); setRenaming(null); }
        }}
      />
      <button className="icon-btn primary" disabled={busy || !nameDraft.trim()} onClick={finishRename}>Save</button>
    </span>
  );

  return (
    <>
      <div className="recovery-title ds-title">
        <button className="icon-btn" onClick={onBack} aria-label="Back to Settings">‹ Back</button>
        <span id="history-settings-title">Device Sync</span>
      </div>

      <section>
        <label className="hs-check">
          <input
            type="checkbox"
            checked={settings.enabled}
            disabled={busy}
            onChange={(e) => run("sync_set_enabled", { enabled: e.target.checked }, e.target.checked ? "Sync is on" : "Sync is off")}
          />
          Sync clipboard history with my other devices
        </label>
        {!settings.enabled && (
          <p className="hs-note">
            Pair your computers once and new copies appear in Clipet on each of them. No account and no Clipet server:
            devices connect directly, or through an encrypted relay when they can't. Turning sync on also offers your
            existing history (filtered by the options below) to the devices you pair.
          </p>
        )}
        {settings.enabled && status.error && <p className="hs-note warn" role="alert">{status.error}</p>}
      </section>

      {settings.enabled && (
        <>
          <section>
            <div className="hs-row">
              <span>This device</span>
              {renaming === "this" ? renameField : (
                <span className="ds-inline">
                  <strong>{status.device_name}</strong>
                  <button className="icon-btn" onClick={() => startRename("this", status.device_name)}>Rename</button>
                </span>
              )}
            </div>
            {node && (
              <p className="hs-note">
                ID <code title={node.device_id}>{node.short_id}</code> ·{" "}
                {settings.paused
                  ? "Sync paused"
                  : node.relay_connected
                    ? "Reachable from other networks"
                    : "Connecting to the relay… devices on this network can still sync"}
              </p>
            )}
            {status.key_storage === "file" && (
              <p className="hs-note warn">
                The system keychain wasn't available, so this device's sync key is stored in Clipet's data folder.
              </p>
            )}
          </section>

          <section>
            <div className="hs-row"><span>Devices</span></div>
            {peers.length === 0 && <p className="hs-note">No devices yet. Use Add Device here, then Join on the other computer.</p>}
            <ul className="ds-peers">
              {peers.map((p) => (
                <li key={p.id}>
                  <div className="ds-peer-main">
                    <span className={"ds-dot" + (p.online ? " on" : "")} aria-hidden="true" />
                    {renaming === p.id ? renameField : <strong title={`ID ${p.short_id}`}>{p.name}</strong>}
                    <span className="hs-status">
                      {settings.paused ? "Paused" : p.online ? "Online" : "Offline"} · synced {ago(p.last_sync, now)}
                    </span>
                  </div>
                  {!p.online && p.last_error && !settings.paused && <p className="hs-note">{p.last_error}</p>}
                  {confirmRemove === p.id ? (
                    <div className="ds-confirm">
                      <p className="hs-note warn" role="alert">
                        Stop syncing with {p.name}? Items it already received stay on it. Remove this device there too.
                      </p>
                      <div className="recovery-actions">
                        <button className="icon-btn" autoFocus onClick={() => setConfirmRemove(null)}>Cancel</button>
                        <button
                          className="icon-btn danger"
                          disabled={busy}
                          onClick={async () => { if (await run("sync_remove_device", { id: p.id }, `Removed ${p.name}`)) setConfirmRemove(null); }}
                        >
                          Remove
                        </button>
                      </div>
                    </div>
                  ) : renaming !== p.id && (
                    <div className="ds-peer-actions">
                      <button className="icon-btn" onClick={() => startRename(p.id, p.name)}>Rename</button>
                      <button className="icon-btn" onClick={() => setConfirmRemove(p.id)}>Remove…</button>
                    </div>
                  )}
                </li>
              ))}
            </ul>

            <PairingPanel
              pairing={pairing}
              now={now}
              busy={busy}
              onCopy={copyInvite}
              onDecide={(approve) => run("sync_decide_pairing", { approve })}
              onClose={() => run("sync_cancel_pairing")}
            />

            {joinOpen && pairing.state === "idle" && (
              <div className="ds-panel">
                <p className="hs-note">Paste the invitation shown on your other device:</p>
                <textarea
                  className="ds-invite"
                  autoFocus
                  rows={3}
                  aria-label="Invitation"
                  value={inviteText}
                  placeholder="clipet-invite:…"
                  onChange={(e) => setInviteText(e.target.value)}
                />
                <div className="recovery-actions">
                  <button className="icon-btn" onClick={() => { setJoinOpen(false); setInviteText(""); }}>Cancel</button>
                  <button className="icon-btn primary" disabled={busy || !inviteText.trim()} onClick={join}>Join</button>
                </div>
              </div>
            )}

            {pairing.state === "idle" && !joinOpen && (
              <div className="recovery-actions">
                <button className="icon-btn primary" disabled={busy || !status.running} onClick={() => run("sync_create_invite")}>Add Device</button>
                <button className="icon-btn" disabled={!status.running} onClick={() => setJoinOpen(true)}>Join a Device…</button>
                {peers.length > 0 && (
                  <>
                    <button className="icon-btn" disabled={busy || settings.paused} onClick={() => run("sync_now", undefined, "Syncing…")}>Sync Now</button>
                    <button className="icon-btn" disabled={busy} onClick={() => run("sync_set_paused", { paused: !settings.paused })}>
                      {settings.paused ? "Resume Sync" : "Pause Sync"}
                    </button>
                  </>
                )}
              </div>
            )}
          </section>

          <section>
            <div className="hs-row"><span>What to sync</span></div>
            <label className="hs-check">
              <input type="checkbox" checked={settings.sync_text} disabled={busy} onChange={(e) => saveOptions({ sync_text: e.target.checked })} />
              Text and links
            </label>
            <label className="hs-check">
              <input type="checkbox" checked={settings.sync_images} disabled={busy} onChange={(e) => saveOptions({ sync_images: e.target.checked })} />
              Images and screenshots
            </label>
            <label className="hs-check">
              <input type="checkbox" checked={settings.sync_pins} disabled={busy} onChange={(e) => saveOptions({ sync_pins: e.target.checked })} />
              Pinned state
            </label>
            <label className="hs-row">
              <span>Largest text item</span>
              <select value={settings.max_text_kb} disabled={busy} onChange={(e) => saveOptions({ max_text_kb: Number(e.target.value) })}>
                {TEXT_SIZES.map((kb) => <option key={kb} value={kb}>{sizeLabel(kb)}</option>)}
              </select>
            </label>
            {settings.sync_images && (
              <label className="hs-row">
                <span>Largest image</span>
                <select value={settings.max_image_mb} disabled={busy} onChange={(e) => saveOptions({ max_image_mb: Number(e.target.value) })}>
                  {IMAGE_SIZES.map((mb) => <option key={mb} value={mb}>{mb} MB</option>)}
                </select>
              </label>
            )}
            <p className="hs-note">
              Synced items appear in Clipet's history on your other devices; their clipboards are never changed.
              Copied files, favorites, Clear and automatic cleanup stay on each device. Copies marked private by
              password managers are never recorded, so they never sync.
            </p>
          </section>
        </>
      )}

      <section>
        <p className="hs-note">
          Copies are end-to-end encrypted between your devices. When a direct connection isn't possible they pass
          through public relays run by n0 (the makers of iroh), which can't read them. Very restrictive networks can
          block both; sync then catches up later. Removing a device stops future sync, but can't delete what it
          already received.
        </p>
      </section>
    </>
  );
}

function PairingPanel(props: {
  pairing: Pairing;
  now: number;
  busy: boolean;
  onCopy: () => void;
  onDecide: (approve: boolean) => void;
  onClose: () => void;
}) {
  const { pairing, now, busy, onCopy, onDecide, onClose } = props;
  switch (pairing.state) {
    case "idle":
      return null;
    case "hosting": {
      const left = Math.max(0, Math.round((pairing.expires_at - now) / 1000));
      return (
        <div className="ds-panel" role="status">
          <p className="hs-note">
            On the other computer, open Clipet → Settings → Device Sync → <strong>Join a Device</strong> and paste this
            invitation. It works once and expires in {Math.floor(left / 60)}:{String(left % 60).padStart(2, "0")}.
          </p>
          <textarea className="ds-invite" readOnly rows={3} aria-label="Invitation" value={pairing.invite} onFocus={(e) => e.target.select()} />
          <div className="recovery-actions">
            <button className="icon-btn" onClick={onClose}>Cancel</button>
            <button className="icon-btn primary" onClick={onCopy}>Copy Invitation</button>
          </div>
        </div>
      );
    }
    case "confirm":
      return (
        <div className="ds-panel" role="alert">
          <p className="hs-note"><strong>{pairing.name}</strong> wants to sync with this device.</p>
          <p className="ds-code" aria-label="Verification code">{pairing.code}</p>
          <p className="hs-note">Approve only if the other computer shows the same code.</p>
          <div className="recovery-actions">
            <button className="icon-btn" disabled={busy} onClick={() => onDecide(false)}>Decline</button>
            <button className="icon-btn primary" disabled={busy} onClick={() => onDecide(true)}>Approve</button>
          </div>
        </div>
      );
    case "joining":
      return (
        <div className="ds-panel" role="status">
          {pairing.code ? (
            <>
              <p className="hs-note">Check that <strong>{pairing.host_name}</strong> shows this code, then approve there:</p>
              <p className="ds-code" aria-label="Verification code">{pairing.code}</p>
            </>
          ) : (
            <p className="hs-note">Connecting to <strong>{pairing.host_name}</strong>…</p>
          )}
          <div className="recovery-actions">
            <button className="icon-btn" onClick={onClose}>Cancel</button>
          </div>
        </div>
      );
    case "done":
      return (
        <div className="ds-panel" role="status">
          <p className="hs-note">Paired with <strong>{pairing.name}</strong>. History syncs automatically from now on.</p>
          <div className="recovery-actions"><button className="icon-btn primary" onClick={onClose}>OK</button></div>
        </div>
      );
    case "failed":
      return (
        <div className="ds-panel" role="alert">
          <p className="hs-note warn">{pairing.reason}</p>
          <div className="recovery-actions"><button className="icon-btn" onClick={onClose}>OK</button></div>
        </div>
      );
  }
}
