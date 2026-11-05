import { getCurrentWindow } from "@tauri-apps/api/window";

// The popup is borderless, so it has no title bar to drag. Its chrome — the
// sidebar background, the search bar, the preview header and the shortcut bar
// — moves the window instead, through the same native window drag a title
// bar uses (smooth, and macOS handles the mouse until it is released).
const DRAG_ZONES = "#sidebar, #search-row, #preview-header, #toolbar";
// Anything the user clicks, types in or selects never starts a drag.
const INTERACTIVE =
  "input, textarea, select, button, a, label, summary, [role], [tabindex], [contenteditable], .sidebar-item, .clip-item, .snippet-item, .selectable";

export function isDragStart(target: Element, offsetX: number, offsetY: number): boolean {
  if (target.closest(INTERACTIVE)) return false;
  const zone = target.closest(DRAG_ZONES);
  if (!zone) return false;
  // A press on the zone's own scrollbar scrolls instead.
  return !(target === zone && (offsetX >= zone.clientWidth || offsetY >= zone.clientHeight));
}

/** Lets the popup be moved by dragging its chrome. Returns the cleanup. */
export function installPopupDrag(root: HTMLElement): () => void {
  const onMouseDown = (e: MouseEvent) => {
    // Single plain left presses only: modifier clicks keep their meaning.
    if (e.button !== 0 || e.detail !== 1 || e.shiftKey || e.metaKey || e.ctrlKey || e.altKey) return;
    if (!(e.target instanceof Element) || !isDragStart(e.target, e.offsetX, e.offsetY)) return;
    // Keeps the search field focused and no text cursor appears.
    e.preventDefault();
    getCurrentWindow().startDragging().catch(() => {});
  };
  root.addEventListener("mousedown", onMouseDown);
  return () => root.removeEventListener("mousedown", onMouseDown);
}
