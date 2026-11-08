import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Parameter } from "./Snippets";

export type Preview = { text: string | null; errors: Record<string, string> };

/** Live, debounced backend preview of a template with the given values.
 * The backend (`snippet_preview`) is the single source of truth for
 * validation and rendering, so this never re-implements either. */
export function usePreview(content: string, parameters: Parameter[], values: Record<string, string>, suffix: string) {
  const [preview, setPreview] = useState<Preview | null>(null);
  const [templateError, setTemplateError] = useState<string | null>(null);
  const seq = useRef(0);
  const key = JSON.stringify([content, parameters, values, suffix]);
  useEffect(() => {
    const id = ++seq.current;
    const t = window.setTimeout(() => {
      invoke<Preview>("snippet_preview", { content, parameters, values, suffix })
        .then((p) => { if (id === seq.current) { setPreview(p); setTemplateError(null); } })
        .catch((e) => { if (id === seq.current) { setPreview(null); setTemplateError(String(e)); } });
    }, 120);
    return () => window.clearTimeout(t);
  }, [key]);
  return { preview, templateError };
}

/** One labeled input per parameter, typed by its kind, with its own error
 * underneath. `showErrors` stays false until the user has tried to submit or
 * touched a field, so an empty form doesn't open covered in red. */
export function ParamFields(props: {
  parameters: Parameter[];
  values: Record<string, string>;
  errors: Record<string, string>;
  showErrors: boolean;
  onChange: (name: string, value: string) => void;
  autoFocusFirst?: boolean;
}) {
  const { parameters, values, errors, showErrors, onChange, autoFocusFirst } = props;
  return (
    <div className="param-fields">
      {parameters.map((p, i) => {
        const id = `param-${p.name}`;
        const error = showErrors ? errors[p.name] : undefined;
        const value = values[p.name] ?? "";
        const common = {
          id,
          "aria-invalid": error ? true : undefined,
          "aria-describedby": error ? `${id}-error` : undefined,
          autoFocus: autoFocusFirst && i === 0,
          "data-param": p.name,
        } as const;
        let input: React.ReactNode;
        if (p.kind === "dropdown") {
          input = (
            <select {...common} value={value} onChange={(e) => onChange(p.name, e.target.value)}>
              <option value="">{p.default ? `Default: ${p.default}` : "Choose…"}</option>
              {p.options.map((o) => <option key={o} value={o}>{o}</option>)}
            </select>
          );
        } else if (p.kind === "date" && !p.format) {
          input = <input {...common} type="date" value={value} onChange={(e) => onChange(p.name, e.target.value)} />;
        } else {
          input = (
            <input
              {...common}
              type="text"
              inputMode={p.kind === "integer" ? "numeric" : p.kind === "url" ? "url" : undefined}
              spellCheck={false}
              placeholder={p.default ?? (p.kind === "date" ? p.format ?? "" : p.kind === "url" ? "https://" : "")}
              value={value}
              onChange={(e) => onChange(p.name, e.target.value)}
            />
          );
        }
        return (
          <label key={p.name} className="param-field" htmlFor={id}>
            <span>
              {p.name}
              <small>{p.kind}{p.required ? "" : " · optional"}</small>
            </span>
            {input}
            {error && <small id={`${id}-error`} className="param-error">{error}</small>}
          </label>
        );
      })}
    </div>
  );
}
