import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { InputStatus } from "./DeviceSync";
import { EMERGENCY_KEYS } from "./DeviceSync";

/** The small always-on-top banner (window "connect"): a device asking for
 *  control, or an input-sharing session in either direction. It never takes
 *  keyboard focus, so typing keeps going where it was. */
export default function ConnectBanner() {
  const [status, setStatus] = useState<InputStatus | null>(null);

  useEffect(() => {
    const load = () => invoke<InputStatus>("input_status").then(setStatus).catch(() => {});
    load();
    const unlisten = listen("clipet://connect-input", load);
    return () => { unlisten.then((f) => f()); };
  }, []);

  if (!status) return null;
  const act = (command: string, args?: Record<string, unknown>) => invoke(command, args).catch(() => {});
  const mode = status.mode;

  let text: React.ReactNode = null;
  let actions: React.ReactNode = null;
  if (status.request) {
    text = <><strong>{status.request}</strong> wants to control this computer</>;
    actions = (
      <>
        <button className="icon-btn" onClick={() => act("input_answer", { approve: false })}>Deny</button>
        <button className="icon-btn primary" onClick={() => act("input_answer", { approve: true })}>Allow</button>
      </>
    );
  } else if (mode.state === "controlled-by") {
    text = <><strong>{mode.peer}</strong> is controlling this computer</>;
    actions = <button className="icon-btn danger" onClick={() => act("input_stop")}>Stop</button>;
  } else if (mode.state === "controlling") {
    text = <>Controlling <strong>{mode.peer}</strong> · {EMERGENCY_KEYS} to stop</>;
    actions = <button className="icon-btn" onClick={() => act("input_stop")}>Stop</button>;
  } else if (mode.state === "connecting") {
    text = <>Connecting to <strong>{mode.peer}</strong>…</>;
    actions = <button className="icon-btn" onClick={() => act("input_stop")}>Cancel</button>;
  } else {
    return null;
  }
  return (
    <div className="connect-banner" role="status">
      <span className="connect-banner-dot" aria-hidden="true" />
      <span className="connect-banner-text">{text}</span>
      <span className="connect-banner-actions">{actions}</span>
    </div>
  );
}
