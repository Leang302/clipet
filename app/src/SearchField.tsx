import type { RefObject } from "react";
import { Search, X } from "lucide-react";

/** The list search box: leading icon, a clear button once there's a query,
 * and the same `#search-input` id the keyboard handling relies on. */
export function SearchField(props: {
  inputRef: RefObject<HTMLInputElement | null>;
  value: string;
  onChange: (value: string) => void;
  placeholder: string;
}) {
  const { inputRef, value, onChange, placeholder } = props;
  return (
    <div className="search-field">
      <Search size={14} strokeWidth={2} className="search-field-icon" aria-hidden="true" />
      <input
        ref={inputRef}
        id="search-input"
        type="text"
        spellCheck={false}
        aria-label={placeholder}
        placeholder={placeholder}
        value={value}
        onChange={(e) => onChange(e.target.value)}
      />
      {value && (
        <button
          type="button"
          className="search-clear"
          aria-label="Clear search"
          title="Clear search (Esc)"
          onClick={() => { onChange(""); inputRef.current?.focus(); }}
        >
          <X size={12} strokeWidth={2.2} aria-hidden="true" />
        </button>
      )}
    </div>
  );
}
