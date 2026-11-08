import { useRef, useState, type ReactElement } from "react";
import { createPortal } from "react-dom";

const SHOW_DELAY_MS = 300;

/** A label shown beside its trigger on hover (after a short delay) or
 * keyboard focus — never on click, never blocking it (spec Part B1).
 * The bubble is rendered into <body> at fixed coordinates: inside the
 * sidebar it would be clipped, because the sidebar scrolls. */
export function Tooltip(props: { label: string; shortcut?: string; children: ReactElement }) {
  const { label, shortcut, children } = props;
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);
  const wrapRef = useRef<HTMLSpanElement>(null);
  const timer = useRef<number | null>(null);

  const show = () => {
    if (timer.current) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      const el = wrapRef.current;
      const r = el?.getBoundingClientRect();
      if (!el || !r) return;
      // Beside the rail's edge when inside the sidebar, else beside the trigger.
      const edge = el.closest("#sidebar")?.getBoundingClientRect().right ?? r.right;
      setPos({ left: edge + 6, top: r.top + r.height / 2 });
    }, SHOW_DELAY_MS);
  };
  const hide = () => {
    if (timer.current) window.clearTimeout(timer.current);
    timer.current = null;
    setPos(null);
  };

  return (
    <span ref={wrapRef} className="tooltip-wrap" onMouseEnter={show} onMouseLeave={hide} onFocus={show} onBlur={hide} onMouseDown={hide}>
      {children}
      {pos &&
        createPortal(
          <span className="tooltip-bubble" role="tooltip" style={{ left: pos.left, top: pos.top }}>
            {label}
            {shortcut && <kbd>{shortcut}</kbd>}
          </span>,
          document.body,
        )}
    </span>
  );
}
