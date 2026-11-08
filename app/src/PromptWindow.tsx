import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import type { Parameter } from "./Snippets";
import { ParamFields, usePreview } from "./ParamForm";
import "./clipflow.css";

type PromptInfo = {
  token: number;
  name: string;
  trigger: string;
  content: string;
  suffix: string;
  parameters: Parameter[];
  prefill: Record<string, string>;
};

const WIDTH = 440;

/** The small window that asks for a parameterized snippet's missing values
 * after `;keyword` + Tab in another app (spec Part D2). The typed shortcut is
 * left as-is until Insert; Esc puts you back in that app with nothing changed. */
export default function PromptWindow() {
  const [info, setInfo] = useState<PromptInfo | null>(null);
  const [values, setValues] = useState<Record<string, string>>({});
  const [triedSubmit, setTriedSubmit] = useState(false);
  const [busy, setBusy] = useState(false);
  /** Insertion was refused or failed; the request is over. */
  const [failure, setFailure] = useState<string | null>(null);
  /** A problem with the values; the request is still open to fix it. */
  const [inlineError, setInlineError] = useState<string | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const focusedFor = useRef<number | null>(null);

  async function load() {
    const next = await invoke<PromptInfo | null>("snippet_prompt_get");
    setInfo(next);
    setValues(next?.prefill ?? {});
    setTriedSubmit(false);
    setBusy(false);
    setFailure(null);
    setInlineError(null);
  }

  useEffect(() => {
    load();
    const unlisten = listen("clipet://prompt-open", () => load());
    return () => { unlisten.then((f) => f()); };
  }, []);

  const { preview, templateError } = usePreview(info?.content ?? "", info?.parameters ?? [], values, info?.suffix ?? "exact");
  const errors = preview?.errors ?? {};
  const valid = !!preview && preview.text !== null;
  const showErrors = triedSubmit || Object.values(values).some((v) => v !== "");

  const focusParam = (name: string) =>
    rootRef.current?.querySelector<HTMLElement>(`[data-param="${CSS.escape(name)}"]`)?.focus();

  // Once per request: focus the first field that still needs attention.
  useEffect(() => {
    if (!info || !preview || focusedFor.current === info.token) return;
    focusedFor.current = info.token;
    const first = info.parameters.find((p) => preview.errors[p.name]) ?? info.parameters[0];
    if (first) focusParam(first.name);
  }, [info?.token, preview]);

  // Fit the window to its content, so one field stays compact.
  useLayoutEffect(() => {
    const el = rootRef.current;
    if (!el) return;
    const h = Math.min(640, Math.ceil(el.getBoundingClientRect().height) + 2);
    getCurrentWindow().setSize(new LogicalSize(WIDTH, h)).catch(() => {});
  });

  async function submit() {
    if (!info || busy || failure) return;
    if (!valid) {
      setTriedSubmit(true);
      const first = info.parameters.find((p) => errors[p.name]);
      if (first) focusParam(first.name);
      return;
    }
    setBusy(true);
    try {
      await invoke("snippet_prompt_submit", { token: info.token, values });
    } catch (e) {
      // Still our request → the values were rejected; fix them here.
      // Otherwise the insertion itself was refused and the request is over.
      const still = await invoke<PromptInfo | null>("snippet_prompt_get").catch(() => null);
      if (still?.token === info.token) {
        setTriedSubmit(true);
        setInlineError(String(e));
      } else {
        setFailure(String(e));
      }
    } finally {
      setBusy(false);
    }
  }

  function cancel() {
    if (!info) return;
    invoke("snippet_prompt_cancel", { token: info.token, returnFocus: !failure }).catch(() => {});
  }

  if (!info) {
    return <div ref={rootRef} className="prompt-card"><div className="prompt-empty">No snippet is waiting for values.</div></div>;
  }

  return (
    <div
      ref={rootRef}
      className="prompt-card"
      role="dialog"
      aria-labelledby="prompt-title"
      onKeyDown={(e) => {
        if (e.key === "Escape") { e.preventDefault(); cancel(); }
        if (e.key === "Enter" && !e.repeat && !e.metaKey && !e.ctrlKey && (e.target as HTMLElement).tagName !== "BUTTON") {
          e.preventDefault();
          submit();
        }
      }}
    >
      <div className="prompt-header">
        <span id="prompt-title">{info.name}</span>
        <span className="snippet-trigger">{info.trigger}</span>
      </div>

      {failure ? (
        <div className="prompt-failure" role="alert">
          <p>{failure}</p>
          <p className="prompt-hint">Your typed shortcut was left as it was.</p>
          <div className="snippet-actions">
            <button className="icon-btn primary" autoFocus onClick={cancel}>Close</button>
          </div>
        </div>
      ) : (
        <>
          <ParamFields
            parameters={info.parameters}
            values={values}
            errors={errors}
            showErrors={showErrors}
            onChange={(name, value) => { setInlineError(null); setValues((s) => ({ ...s, [name]: value })); }}
          />
          <div className="snippet-test-output" aria-live="polite">
            {templateError
              ? <span className="snippet-error">{templateError}</span>
              : valid
                ? <pre>{preview!.text}</pre>
                : <span className="snippet-test-placeholder">The result appears here once every value is valid.</span>}
          </div>
          {inlineError && <div className="snippet-error" role="alert">{inlineError}</div>}
          <div className="prompt-footer">
            <span className="prompt-hint">Pastes text only — nothing is run.</span>
            <div className="snippet-actions">
              <button className="icon-btn" onClick={cancel}>Cancel <span className="hint">esc</span></button>
              <button className="icon-btn primary" disabled={busy} onClick={submit}>
                {busy ? "Inserting…" : "Insert"} <span className="hint">↵</span>
              </button>
            </div>
          </div>
        </>
      )}
    </div>
  );
}
