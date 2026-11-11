import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type SyncSettings = {
  enabled: boolean;
  clipboard: boolean;
  paused: boolean;
  sync_text: boolean;
  sync_images: boolean;
  sync_pins: boolean;
  max_text_kb: number;
  max_image_mb: number;
};

export type Side = "left" | "right" | "top" | "bottom";

type PeerStatus = {
  id: string;
  short_id: string;
  name: string;
  online: boolean;
  added_at: number;
  last_seen: number | null;
  last_sync: number | null;
  last_error: string | null;
  allow_clipboard: boolean;
  allow_control: boolean;
  side: Side | null;
};

type Pairing =
  | { state: "idle" }
  | { state: "hosting"; invite: string; code: string; expires_at: number }
  | { state: "confirm"; name: string; code: string | null }
  | { state: "joining"; host_name: string; code: string | null; needs_confirm: boolean }
  | { state: "done"; name: string }
  | { state: "failed"; reason: string };

export type InputSettings = {
  enabled: boolean;
  mouse: boolean;
  keyboard: boolean;
  edge_switch: boolean;
  hotkey_switch: boolean;
  edge_delay_ms: number;
  require_confirmation: boolean;
  swap_ctrl_cmd: boolean;
  invert_scroll: boolean;
  allow_relayed: boolean;
};

export type InputMode =
  | { state: "idle" }
  | { state: "connecting"; peer: string }
  | { state: "controlling"; peer: string }
  | { state: "controlled-by"; peer: string };

export type InputStatus = {
  settings: InputSettings;
  permission: { granted: boolean; hint: string };
  supported: boolean;
  mode: InputMode;
  request: string | null;
  last_error: string | null;
};

export type SyncStatus = {
  settings: SyncSettings;
  device_name: string;
  key_storage: "keychain" | "file" | null;
  running: boolean;
  error: string | null;
  node: {
    device_id: string;
    short_id: string;
    relay_connected: boolean;
    peers: PeerStatus[];
    pairing: Pairing;
    nearby: { id: string; short_id: string; name: string }[];
  } | null;
  input: InputStatus;
};

const TEXT_SIZES = [64, 256, 1024, 2048];
const IMAGE_SIZES = [2, 5, 10, 25];
const EDGE_DELAYS = [0, 150, 300, 600];
const SIDE_LABELS: Record<Side, string> = { left: "Left", right: "Right", top: "Above", bottom: "Below" };
const isMac = /Mac/.test(navigator.platform);
export const EMERGENCY_KEYS = isMac ? "Control+Option+Shift+Esc" : "Ctrl+Alt+Shift+Esc";

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
  if (status.error) return "Not running";
  const mode = status.input.mode;
  if (mode.state === "controlling") return `Controlling ${mode.peer}`;
  if (mode.state === "controlled-by") return `Controlled by ${mode.peer}`;
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
    const a = listen("clipet://sync-changed", load);
    const b = listen("clipet://connect-input", load);
    return () => { a.then((f) => f()); b.then((f) => f()); };
  }, []);
  return [status, setStatus];
}

/** Settings → Clipet Connect. */
export function DeviceSync(props: { onBack: () => void; toast: (msg: string, ms?: number) => void }) {
  const { onBack, toast } = props;
  const [status, setStatus] = useSyncStatus();
  const [now, setNow] = useState(Date.now());
  const [joinOpen, setJoinOpen] = useState(false);
  const [inviteText, setInviteText] = useState("");
  const [nearbyPick, setNearbyPick] = useState<string | null>(null);
  const [codeText, setCodeText] = useState("");
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
  const { settings, node, input } = status;
  const pairing: Pairing = node?.pairing ?? { state: "idle" };
  const peers = node?.peers ?? [];
  const saveOptions = (patch: Partial<SyncSettings>) => run("sync_save_settings", { settings: { ...settings, ...patch } });
  const saveInput = (patch: Partial<InputSettings>) => run("input_save_settings", { settings: { ...input.settings, ...patch } });
  const setDevice = (id: string, patch: { allow_clipboard?: boolean; allow_control?: boolean; side?: Side | null }) =>
    run("connect_set_device", { id, ...patch });
  const sharing = input.mode.state === "controlling" || input.mode.state === "controlled-by";

  async function finishRename() {
    if (renaming === null) return;
    const ok = renaming === "this"
      ? await run("sync_rename_this_device", { name: nameDraft })
      : await run("sync_rename_device", { id: renaming, name: nameDraft });
    if (ok) setRenaming(null);
  }

  function closeJoin() {
    setJoinOpen(false);
    setInviteText("");
    setCodeText("");
    setNearbyPick(null);
  }

  async function join() {
    const ok = nearbyPick
      ? await run("sync_join_nearby", { id: nearbyPick, code: codeText })
      : await run("sync_join", { invite: inviteText });
    if (ok) closeJoin();
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
        <span id="history-settings-title">Clipet Connect</span>
      </div>

      <section>
        <label className="hs-check">
          <input
            type="checkbox"
            checked={settings.enabled}
            disabled={busy}
            onChange={(e) => run("sync_set_enabled", { enabled: e.target.checked }, e.target.checked ? "Clipet Connect is on" : "Clipet Connect is off")}
          />
          Connect this computer with my other devices
        </label>
        {!settings.enabled && (
          <p className="hs-note">
            Pair your computers once to share clipboard history and, if you want, one mouse and keyboard. No account and
            no Clipet server: devices connect directly, or through an encrypted relay when they can't.
          </p>
        )}
        {settings.enabled && status.error && <p className="hs-note warn" role="alert">{status.error}</p>}
      </section>

      {settings.enabled && (
        <>
          {sharing && (
            <section className="ds-live" role="status">
              <div className="hs-row">
                <span>
                  {input.mode.state === "controlling"
                    ? <>Your keyboard and mouse are controlling <strong>{input.mode.peer}</strong></>
                    : <><strong>{(input.mode as { peer: string }).peer}</strong> is controlling this computer</>}
                </span>
                <button className="icon-btn danger" onClick={() => run("input_stop")}>Stop Sharing</button>
              </div>
              <p className="hs-note">Emergency release anytime: {EMERGENCY_KEYS}.</p>
            </section>
          )}

          <section>
            <div className="hs-row">
              <span>This device</span>
              {renaming === "this" ? renameField : (
                <span className="ds-inline">
                  <strong>{status.device_name}</strong>
                  <button className="icon-btn" onClick={() => { setRenaming("this"); setNameDraft(status.device_name); }}>Rename</button>
                </span>
              )}
            </div>
            {node && (
              <p className="hs-note">
                ID <code title={node.device_id}>{node.short_id}</code> ·{" "}
                {node.relay_connected ? "Reachable from other networks" : "Connecting to the relay… devices on this network can still connect"}
              </p>
            )}
            {status.key_storage === "file" && (
              <p className="hs-note warn">The system keychain wasn't available, so this device's key is stored in Clipet's data folder.</p>
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
                      {p.online ? "Online" : "Offline"}
                      {settings.clipboard && p.allow_clipboard && <> · synced {ago(p.last_sync, now)}</>}
                    </span>
                  </div>
                  {!p.online && p.last_error && <p className="hs-note">{p.last_error}</p>}
                  <div className="ds-grants">
                    <label className="hs-check">
                      <input type="checkbox" checked={p.allow_clipboard} disabled={busy} onChange={(e) => setDevice(p.id, { allow_clipboard: e.target.checked })} />
                      Sync clipboard
                    </label>
                    <label className="hs-check">
                      <input type="checkbox" checked={p.allow_control} disabled={busy} onChange={(e) => setDevice(p.id, { allow_control: e.target.checked })} />
                      Can control this computer
                    </label>
                  </div>
                  {confirmRemove === p.id ? (
                    <div className="ds-confirm">
                      <p className="hs-note warn" role="alert">
                        Stop connecting with {p.name}? Clipboard items it already received stay on it. Remove this device there too.
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
                      {input.settings.enabled && p.online && input.mode.state === "idle" && (
                        <button className="icon-btn primary" onClick={() => run("input_switch_to", { id: p.id })}>Control</button>
                      )}
                      <button className="icon-btn" onClick={() => { setRenaming(p.id); setNameDraft(p.name); }}>Rename</button>
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
                <p className="hs-note">On the other computer, click <strong>Add Device</strong>. Then:</p>
                {(node?.nearby.length ?? 0) > 0 ? (
                  <>
                    <div className="ds-nearby" role="radiogroup" aria-label="Nearby devices">
                      {node!.nearby.map((d) => (
                        <button
                          key={d.id}
                          role="radio"
                          aria-checked={nearbyPick === d.id}
                          className={"icon-btn" + (nearbyPick === d.id ? " primary" : "")}
                          onClick={() => setNearbyPick(d.id)}
                          title={`ID ${d.short_id}`}
                        >
                          {d.name}
                        </button>
                      ))}
                    </div>
                    {nearbyPick && (
                      <label className="hs-row">
                        <span>Code shown there</span>
                        <input
                          autoFocus
                          inputMode="numeric"
                          aria-label="Pairing code"
                          placeholder="123 456"
                          value={codeText}
                          maxLength={7}
                          onChange={(e) => setCodeText(e.target.value)}
                          onKeyDown={(e) => { if (e.key === "Enter" && codeText.replace(/\D/g, "").length === 6) join(); }}
                        />
                      </label>
                    )}
                  </>
                ) : (
                  <p className="hs-note">No device with Add Device open was found on this network.</p>
                )}
                {!nearbyPick && (
                  <>
                    <p className="hs-note">On another network? Paste the invitation instead:</p>
                    <textarea className="ds-invite" rows={3} aria-label="Invitation" value={inviteText} placeholder="clipet-invite:…" onChange={(e) => setInviteText(e.target.value)} />
                  </>
                )}
                <div className="recovery-actions">
                  <button className="icon-btn" onClick={closeJoin}>Cancel</button>
                  <button
                    className="icon-btn primary"
                    disabled={busy || (nearbyPick ? codeText.replace(/\D/g, "").length !== 6 : !inviteText.trim())}
                    onClick={join}
                  >
                    Join
                  </button>
                </div>
              </div>
            )}

            {pairing.state === "idle" && !joinOpen && (
              <div className="recovery-actions">
                <button className="icon-btn primary" disabled={busy || !status.running} onClick={() => run("sync_create_invite")}>Add Device</button>
                <button className="icon-btn" disabled={!status.running} onClick={() => setJoinOpen(true)}>Join a Device…</button>
                {peers.length > 0 && settings.clipboard && (
                  <button className="icon-btn" disabled={busy || settings.paused} onClick={() => run("sync_now", undefined, "Syncing…")}>Sync Now</button>
                )}
              </div>
            )}
          </section>

          <section>
            <label className="hs-check ds-heading">
              <input type="checkbox" checked={input.settings.enabled} disabled={busy || !input.supported} onChange={(e) => saveInput({ enabled: e.target.checked })} />
              Share mouse and keyboard
            </label>
            {!input.supported && <p className="hs-note">Not available on this system.</p>}
            {input.supported && input.permission.hint && (
              <p className={"hs-note" + (input.permission.granted ? "" : " warn")} role="alert">{input.permission.hint}</p>
            )}
            {input.last_error && input.mode.state === "idle" && <p className="hs-note warn" role="status">{input.last_error}</p>}
            {!input.settings.enabled && (
              <p className="hs-note">
                Move the pointer off the edge of this screen to use another computer with this keyboard and mouse. Each
                computer decides who may control it ("Can control this computer" above). Keystrokes are never stored.
              </p>
            )}
            {input.settings.enabled && (
              <>
                <Arrangement thisName={status.device_name} peers={peers} disabled={busy} onPlace={(id, side) => setDevice(id, { side })} />
                <label className="hs-check"><input type="checkbox" checked={input.settings.mouse} onChange={(e) => saveInput({ mouse: e.target.checked })} /> Mouse</label>
                <label className="hs-check"><input type="checkbox" checked={input.settings.keyboard} onChange={(e) => saveInput({ keyboard: e.target.checked })} /> Keyboard</label>
                <label className="hs-check"><input type="checkbox" checked={input.settings.edge_switch} onChange={(e) => saveInput({ edge_switch: e.target.checked })} /> Switch at the screen edge</label>
                {input.settings.edge_switch && (
                  <label className="hs-row">
                    <span>Edge delay</span>
                    <select value={input.settings.edge_delay_ms} onChange={(e) => saveInput({ edge_delay_ms: Number(e.target.value) })}>
                      {EDGE_DELAYS.map((ms) => <option key={ms} value={ms}>{ms === 0 ? "None" : `${ms} ms`}</option>)}
                    </select>
                  </label>
                )}
                <label className="hs-check">
                  <input type="checkbox" checked={input.settings.hotkey_switch} onChange={(e) => saveInput({ hotkey_switch: e.target.checked })} />
                  Switch with {isMac ? "Control+Option+Shift" : "Ctrl+Alt+Shift"}+arrow key
                </label>
                <label className="hs-check">
                  <input type="checkbox" checked={input.settings.require_confirmation} onChange={(e) => saveInput({ require_confirmation: e.target.checked })} />
                  Ask on this computer before another device takes control
                </label>
                <label className="hs-check">
                  <input type="checkbox" checked={input.settings.swap_ctrl_cmd} onChange={(e) => saveInput({ swap_ctrl_cmd: e.target.checked })} />
                  {isMac ? "⌘ acts as Ctrl on a PC" : "Ctrl acts as ⌘ on a Mac"} (so copy and paste shortcuts just work)
                </label>
                <label className="hs-check">
                  <input type="checkbox" checked={input.settings.invert_scroll} onChange={(e) => saveInput({ invert_scroll: e.target.checked })} />
                  Reverse scrolling from other devices
                </label>
                <label className="hs-check">
                  <input type="checkbox" checked={input.settings.allow_relayed} onChange={(e) => saveInput({ allow_relayed: e.target.checked })} />
                  Allow over the internet when there's no direct connection (slower)
                </label>
                <p className="hs-note">Emergency release, always: {EMERGENCY_KEYS}.</p>
              </>
            )}
          </section>

          <section>
            <label className="hs-check ds-heading">
              <input type="checkbox" checked={settings.clipboard} disabled={busy} onChange={(e) => saveOptions({ clipboard: e.target.checked })} />
              Sync clipboard history
            </label>
            {settings.clipboard && (
              <>
                <label className="hs-check"><input type="checkbox" checked={settings.sync_text} disabled={busy} onChange={(e) => saveOptions({ sync_text: e.target.checked })} /> Text and links</label>
                <label className="hs-check"><input type="checkbox" checked={settings.sync_images} disabled={busy} onChange={(e) => saveOptions({ sync_images: e.target.checked })} /> Images and screenshots</label>
                <label className="hs-check"><input type="checkbox" checked={settings.sync_pins} disabled={busy} onChange={(e) => saveOptions({ sync_pins: e.target.checked })} /> Pinned state</label>
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
                <div className="recovery-actions">
                  <button className="icon-btn" disabled={busy} onClick={() => run("sync_set_paused", { paused: !settings.paused })}>
                    {settings.paused ? "Resume Sync" : "Pause Sync"}
                  </button>
                </div>
                <p className="hs-note">
                  Synced items appear in Clipet's history on your other devices; their clipboards are never changed.
                  Copied files, favorites, Clear and automatic cleanup stay on each device. Copies marked private by
                  password managers are never recorded, so they never sync.
                </p>
              </>
            )}
          </section>
        </>
      )}

      <section>
        <p className="hs-note">
          Everything between your devices is end-to-end encrypted. When a direct connection isn't possible, clipboard
          sync passes through public relays run by n0 (the makers of iroh), which can't read it. Removing a device stops
          future connections, but can't delete what it already received.
        </p>
      </section>
    </>
  );
}

/** Drag a device next to this one to say where its screen is. */
function Arrangement(props: { thisName: string; peers: PeerStatus[]; disabled: boolean; onPlace: (id: string, side: Side | null) => void }) {
  const { thisName, peers, disabled, onPlace } = props;
  const box = useRef<HTMLDivElement>(null);
  const [drag, setDrag] = useState<{ id: string; x: number; y: number } | null>(null);

  function sideAt(clientX: number, clientY: number): Side | null {
    const r = box.current?.getBoundingClientRect();
    if (!r) return null;
    const dx = clientX - (r.left + r.width / 2);
    const dy = clientY - (r.top + r.height / 2);
    if (clientY > r.bottom - 6 || (Math.abs(dx) < 40 && Math.abs(dy) < 25)) return null;
    return Math.abs(dx) / r.width > Math.abs(dy) / r.height ? (dx > 0 ? "right" : "left") : (dy > 0 ? "bottom" : "top");
  }

  const placed = peers.filter((p) => p.side);
  const unplaced = peers.filter((p) => !p.side);
  const dragging = drag ? peers.find((p) => p.id === drag.id) : null;

  return (
    <div className="ds-arrange">
      <p className="hs-note">Screen arrangement — drag a device to the side where its screen is:</p>
      <div
        className="ds-desk"
        ref={box}
        onPointerMove={(e) => drag && setDrag({ ...drag, x: e.clientX, y: e.clientY })}
        onPointerUp={(e) => {
          if (!drag) return;
          onPlace(drag.id, sideAt(e.clientX, e.clientY));
          setDrag(null);
        }}
        onPointerLeave={() => setDrag(null)}
      >
        <div className="ds-screen ds-self">{thisName}<span>This device</span></div>
        {placed.map((p) => (
          <div
            key={p.id}
            className={`ds-screen ds-${p.side}` + (drag?.id === p.id ? " ds-dragging" : "")}
            onPointerDown={(e) => { if (!disabled) { e.currentTarget.setPointerCapture?.(e.pointerId); setDrag({ id: p.id, x: e.clientX, y: e.clientY }); } }}
          >
            {p.name}
          </div>
        ))}
        {dragging && drag && box.current && (
          <div className="ds-screen ds-ghost" style={{ left: drag.x - box.current.getBoundingClientRect().left - 50, top: drag.y - box.current.getBoundingClientRect().top - 20 }}>
            {dragging.name}
          </div>
        )}
      </div>
      {unplaced.length > 0 && (
        <div className="ds-tray">
          {unplaced.map((p) => (
            <div key={p.id} className="ds-screen ds-unplaced" onPointerDown={(e) => { if (!disabled) setDrag({ id: p.id, x: e.clientX, y: e.clientY }); }}>
              {p.name}
            </div>
          ))}
        </div>
      )}
      {peers.map((p) => (
        <label key={p.id} className="hs-row">
          <span>{p.name}</span>
          <select aria-label={`Where ${p.name}'s screen is`} value={p.side ?? ""} disabled={disabled} onChange={(e) => onPlace(p.id, (e.target.value || null) as Side | null)}>
            <option value="">Not placed</option>
            {(Object.keys(SIDE_LABELS) as Side[]).map((s) => <option key={s} value={s}>{SIDE_LABELS[s]}</option>)}
          </select>
        </label>
      ))}
    </div>
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
            On the other computer, open Clipet → Settings → Clipet Connect → <strong>Join a Device</strong>. On the same
            network, pick this device and type:
          </p>
          <p className="ds-code" aria-label="Pairing code">{pairing.code}</p>
          <p className="hs-note">
            On another network, send it this invitation instead. Either works once; this closes in{" "}
            {Math.floor(left / 60)}:{String(left % 60).padStart(2, "0")}.
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
          <p className="hs-note"><strong>{pairing.name}</strong> wants to connect with this device.</p>
          {pairing.code ? (
            <>
              <p className="ds-code" aria-label="Verification code">{pairing.code}</p>
              <p className="hs-note">Approve only if the other computer shows the same code.</p>
            </>
          ) : (
            <p className="hs-note">It entered the right code. Approve only if that's your device.</p>
          )}
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
              <p className="hs-note">Check that <strong>{pairing.host_name}</strong> shows this code:</p>
              <p className="ds-code" aria-label="Verification code">{pairing.code}</p>
            </>
          ) : (
            <p className="hs-note">Waiting for <strong>{pairing.host_name}</strong> to approve…</p>
          )}
          <div className="recovery-actions">
            {pairing.needs_confirm ? (
              <>
                <button className="icon-btn" disabled={busy} onClick={() => onDecide(false)}>Codes differ</button>
                <button className="icon-btn primary" disabled={busy} onClick={() => onDecide(true)}>Codes match</button>
              </>
            ) : (
              <button className="icon-btn" onClick={onClose}>Cancel</button>
            )}
          </div>
        </div>
      );
    case "done":
      return (
        <div className="ds-panel" role="status">
          <p className="hs-note">Paired with <strong>{pairing.name}</strong>. Turn on what you want to share with it above.</p>
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
