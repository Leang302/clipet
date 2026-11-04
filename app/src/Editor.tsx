import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { applyTheme, loadTheme, THEME_STORAGE_KEY } from "./theme";
import "./editor.css";

type ClipItem = {
  id: string;
  kind: string;
  content: string;
  preview: string;
  source_app: string;
  created_at: number;
};

type ShapeKind = "rect" | "ellipse" | "line" | "arrow";
type Tool = "select" | "pen" | "highlighter" | ShapeKind | "text" | "blur" | "pixelate" | "crop" | "eraser";

const TOOLS: { key: Tool; icon: string; label: string }[] = [
  { key: "select", icon: "↖", label: "Select / move" },
  { key: "pen", icon: "✏️", label: "Pen" },
  { key: "highlighter", icon: "🖊️", label: "Highlighter" },
  { key: "arrow", icon: "➚", label: "Arrow" },
  { key: "rect", icon: "▭", label: "Rectangle" },
  { key: "ellipse", icon: "◯", label: "Ellipse" },
  { key: "line", icon: "／", label: "Line" },
  { key: "text", icon: "T", label: "Text" },
  { key: "blur", icon: "◍", label: "Blur" },
  { key: "pixelate", icon: "▦", label: "Pixelate" },
  { key: "crop", icon: "⛶", label: "Crop" },
  { key: "eraser", icon: "⌫", label: "Eraser" },
];

const SHAPE_KINDS: Tool[] = ["rect", "ellipse", "line", "arrow"];
// Tools under which clicking an existing text/shape selects and drags it
// instead of drawing. Freehand/raster tools always draw.
const OBJECT_TOOLS: Tool[] = ["select", "text", ...SHAPE_KINDS];

// Text and shapes stay live objects (not pixels) until the image is saved, so
// they can be moved, resized and restyled after placement. Text is drawn with
// textBaseline "top", so (x, y) is the top-left of its box. Shapes are defined
// by their two drag points: opposite corners for rect/ellipse, ends for lines.
type TextObj = { kind: "text"; id: string; x: number; y: number; text: string; color: string; size: number };
type ShapeObj = { kind: ShapeKind; id: string; x1: number; y1: number; x2: number; y2: number; color: string; width: number };
type Obj = TextObj | ShapeObj;
type HitPart = "body" | "resize" | "p1" | "p2";

type ImgState = {
  baseHistory: string[]; // data URLs, index 0 = original load
  annoHistory: (string | null)[]; // raster layer (pen/highlighter/eraser) per step
  objsHistory: Obj[][]; // live text/shape objects per step
  idx: number;
  w: number;
  h: number;
  annoCanvas: HTMLCanvasElement;
};

const textFont = (size: number) => `600 ${size}px -apple-system, "Segoe UI", sans-serif`;
const newId = () => Math.random().toString(36).slice(2, 10);

function drawShape(ctx: CanvasRenderingContext2D, s: Omit<ShapeObj, "id">) {
  ctx.save();
  ctx.strokeStyle = s.color;
  ctx.fillStyle = s.color;
  ctx.lineWidth = s.width;
  ctx.lineJoin = "round";
  ctx.lineCap = "round";
  const { x1, y1, x2, y2 } = s;
  if (s.kind === "line" || s.kind === "arrow") {
    ctx.beginPath(); ctx.moveTo(x1, y1); ctx.lineTo(x2, y2); ctx.stroke();
    if (s.kind === "arrow") {
      const angle = Math.atan2(y2 - y1, x2 - x1);
      const headLen = 10 + s.width * 1.5;
      ctx.beginPath();
      ctx.moveTo(x2, y2);
      ctx.lineTo(x2 - headLen * Math.cos(angle - Math.PI / 6), y2 - headLen * Math.sin(angle - Math.PI / 6));
      ctx.lineTo(x2 - headLen * Math.cos(angle + Math.PI / 6), y2 - headLen * Math.sin(angle + Math.PI / 6));
      ctx.closePath(); ctx.fill();
    }
  } else if (s.kind === "rect") {
    ctx.strokeRect(Math.min(x1, x2), Math.min(y1, y2), Math.abs(x2 - x1), Math.abs(y2 - y1));
  } else {
    ctx.beginPath();
    ctx.ellipse((x1 + x2) / 2, (y1 + y2) / 2, Math.abs(x2 - x1) / 2, Math.abs(y2 - y1) / 2, 0, 0, Math.PI * 2);
    ctx.stroke();
  }
  ctx.restore();
}

function drawObjects(ctx: CanvasRenderingContext2D, objs: Obj[]) {
  for (const o of objs) {
    if (o.kind === "text") {
      ctx.save();
      ctx.textBaseline = "top";
      ctx.font = textFont(o.size);
      ctx.fillStyle = o.color;
      ctx.fillText(o.text, o.x, o.y);
      ctx.restore();
    } else {
      drawShape(ctx, o);
    }
  }
}

function objBox(ctx: CanvasRenderingContext2D, o: Obj) {
  if (o.kind === "text") {
    ctx.save();
    ctx.font = textFont(o.size);
    const w = ctx.measureText(o.text).width;
    ctx.restore();
    return { x: o.x, y: o.y, w, h: o.size * 1.2 };
  }
  return { x: Math.min(o.x1, o.x2), y: Math.min(o.y1, o.y2), w: Math.abs(o.x2 - o.x1), h: Math.abs(o.y2 - o.y1) };
}

function distToSegment(p: Point, ax: number, ay: number, bx: number, by: number) {
  const dx = bx - ax, dy = by - ay;
  const len2 = dx * dx + dy * dy;
  const t = len2 ? Math.max(0, Math.min(1, ((p.x - ax) * dx + (p.y - ay) * dy) / len2)) : 0;
  return Math.hypot(p.x - (ax + t * dx), p.y - (ay + t * dy));
}

function moveObj(o: Obj, dx: number, dy: number): Obj {
  return o.kind === "text"
    ? { ...o, x: o.x + dx, y: o.y + dy }
    : { ...o, x1: o.x1 + dx, y1: o.y1 + dy, x2: o.x2 + dx, y2: o.y2 + dy };
}

function loadImage(src: string): Promise<HTMLImageElement> {
  return new Promise((resolve) => {
    const img = new Image();
    img.onload = () => resolve(img);
    img.src = src;
  });
}

type Point = { x: number; y: number };
// baseImg/annoImg are the already-decoded committed layers, captured at
// pointerdown so every pointermove can repaint synchronously (no awaits —
// an await between clearing and redrawing the canvas is what flickered).
type DragInfo = {
  id: string;
  tool: Tool;
  startX: number; startY: number; lastX: number; lastY: number;
  points: Point[];
  baseImg: HTMLImageElement;
  annoImg: HTMLImageElement | null;
};

type ObjDrag = {
  id: string; // image id
  objId: string;
  mode: HitPart;
  start: Point;
  orig: Obj;
  working: Obj[];
  moved: boolean;
  baseImg: HTMLImageElement;
  annoImg: HTMLImageElement | null;
};

export default function Editor() {
  const [targets, setTargets] = useState<string[]>([]);
  const [items, setItems] = useState<Record<string, ClipItem>>({});
  const [index, setIndex] = useState(0);
  const [tool, setTool] = useState<Tool>("pen");
  const [color, setColor] = useState("#ff3b30");
  const [thickness, setThickness] = useState(4);
  const [fontSize, setFontSize] = useState(18);
  const [tick, setTick] = useState(0); // bumped to force re-render after imperative canvas/history mutations
  const [toastMsg, setToastMsg] = useState<string | null>(null);
  // window.prompt()/window.confirm() are unreliable inside Tauri's webview, so
  // the text tool and the "discard unsaved edits?" check use real in-app
  // modals instead (see the matching note in App.tsx's clear-history flow).
  const [textEntry, setTextEntry] = useState<{ id: string; p: Point; editTextId?: string } | null>(null);
  const [textValue, setTextValue] = useState("");
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  const [selectedId, setSelectedId] = useState<string | null>(null);

  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const imgStates = useRef<Record<string, ImgState>>({});
  const dragRef = useRef<DragInfo | null>(null);
  const objDragRef = useRef<ObjDrag | null>(null);
  // Consecutive color/size/width tweaks to the same object collapse into one
  // undo step (a color picker fires a change event on every pixel of movement).
  const lastStyleEditRef = useRef<string | null>(null);
  const toastTimer = useRef<number | undefined>(undefined);
  const imgCache = useRef(new Map<string, HTMLImageElement>());

  async function getImg(src: string): Promise<HTMLImageElement> {
    const cached = imgCache.current.get(src);
    if (cached) return cached;
    const img = await loadImage(src);
    imgCache.current.set(src, img);
    return img;
  }

  function toast(msg: string) {
    setToastMsg(msg);
    window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToastMsg(null), 2400);
  }

  const currentId = targets[index];

  async function loadSession() {
    const ids = await invoke<string[]>("get_editor_targets");
    if (!ids.length) return;
    imgStates.current = {}; // fresh start every time the editor opens
    imgCache.current.clear();
    setSelectedId(null);
    const fetchedItems = await invoke<ClipItem[]>("get_items", { ids });
    const byId: Record<string, ClipItem> = {};
    fetchedItems.forEach((i) => (byId[i.id] = i));
    setItems(byId);
    setTargets(ids);
    setIndex(0);

    for (const item of fetchedItems) {
      const dataUrl = await invoke<string>("get_image_data_url", { filename: item.content });
      imgStates.current[item.id] = {
        baseHistory: [dataUrl],
        annoHistory: [null],
        objsHistory: [[]],
        idx: 0,
        w: 0,
        h: 0,
        annoCanvas: document.createElement("canvas"),
      };
    }
    setTick((t) => t + 1);
  }

  useEffect(() => {
    loadSession();
    const unlisten = listen("clipflow://editor-start", () => loadSession());
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // This window is a separate document from the main popup, so it needs its
  // own copy of the manual theme preference — read it on open, and pick up
  // live changes made from the main window while this one stays open.
  useEffect(() => {
    applyTheme(loadTheme());
    function onStorage(e: StorageEvent) {
      if (e.key === THEME_STORAGE_KEY) applyTheme(loadTheme());
    }
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, []);

  const renderCanvas = useCallback(async () => {
    if (!currentId) return;
    const st = imgStates.current[currentId];
    const canvas = canvasRef.current;
    if (!st || !canvas) return;

    // Decode everything BEFORE touching the visible canvas, so the previous
    // frame stays on screen until the new one can be drawn in one go.
    const base = await getImg(st.baseHistory[st.idx]);
    const annoSrc = st.annoHistory[st.idx];
    const anno = annoSrc ? await getImg(annoSrc) : null;
    if (dragRef.current || objDragRef.current) return; // a drag started meanwhile; it owns the canvas
    if (!st.w) {
      st.w = base.naturalWidth;
      st.h = base.naturalHeight;
    }
    // Assigning width/height wipes a canvas even when unchanged — only do it on a real resize.
    if (canvas.width !== st.w || canvas.height !== st.h) {
      canvas.width = st.w;
      canvas.height = st.h;
    }
    paintScene(st, base, anno, st.objsHistory[st.idx], selectedId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [currentId, selectedId]);

  /** Canvas pixels per on-screen pixel (the canvas is CSS-scaled to fit). */
  function canvasScale() {
    const c = canvasRef.current!;
    const r = c.getBoundingClientRect();
    return r.width ? c.width / r.width : 1;
  }

  /** Handle centers for the selected object (canvas coordinates). */
  function handlesFor(ctx: CanvasRenderingContext2D, o: Obj): { part: HitPart; x: number; y: number }[] {
    if (o.kind === "text") {
      const pad = 4 * canvasScale();
      const b = objBox(ctx, o);
      return [{ part: "resize", x: b.x + b.w + pad, y: b.y + b.h + pad }];
    }
    return [{ part: "p1", x: o.x1, y: o.y1 }, { part: "p2", x: o.x2, y: o.y2 }];
  }

  /** Draws base + raster layer + objects (+ selection UI) in one synchronous pass. */
  function paintScene(st: ImgState, base: HTMLImageElement, anno: CanvasImageSource | null, objs: Obj[], selId: string | null) {
    const ctx = canvasRef.current!.getContext("2d")!;
    ctx.clearRect(0, 0, st.w, st.h);
    ctx.drawImage(base, 0, 0, st.w, st.h);
    if (anno) ctx.drawImage(anno, 0, 0, st.w, st.h);
    drawObjects(ctx, objs);
    const sel = selId ? objs.find((o) => o.id === selId) : undefined;
    if (!sel) return;
    const s = canvasScale();
    ctx.save();
    ctx.lineWidth = 1.5 * s;
    ctx.strokeStyle = "#3b6fff";
    if (sel.kind !== "line" && sel.kind !== "arrow") {
      const pad = 4 * s;
      const b = objBox(ctx, sel);
      ctx.setLineDash([6 * s, 4 * s]);
      ctx.strokeRect(b.x - pad, b.y - pad, b.w + pad * 2, b.h + pad * 2);
      ctx.setLineDash([]);
    }
    const hs = 12 * s;
    ctx.fillStyle = "#fff";
    for (const h of handlesFor(ctx, sel)) {
      ctx.beginPath();
      if (sel.kind === "text") ctx.rect(h.x - hs / 2, h.y - hs / 2, hs, hs);
      else ctx.arc(h.x, h.y, hs / 2, 0, Math.PI * 2);
      ctx.fill();
      ctx.stroke();
    }
    ctx.restore();
  }

  function currentObjs(id: string): Obj[] {
    const st = imgStates.current[id];
    return st ? st.objsHistory[st.idx] : [];
  }

  /** What's under p: a handle of the selected object, an object's body, or nothing. */
  function hitObject(id: string, p: Point): { o: Obj; part: HitPart } | null {
    const ctx = canvasRef.current!.getContext("2d")!;
    const s = canvasScale();
    const tol = 6 * s; // generous grab margin around thin strokes, in screen pixels
    const objs = currentObjs(id);
    const sel = objs.find((o) => o.id === selectedId);
    if (sel) {
      for (const h of handlesFor(ctx, sel)) {
        if (Math.hypot(p.x - h.x, p.y - h.y) <= 10 * s) return { o: sel, part: h.part };
      }
    }
    for (let i = objs.length - 1; i >= 0; i--) {
      const o = objs[i];
      const isSel = o.id === selectedId;
      if (o.kind === "text") {
        const b = objBox(ctx, o);
        const pad = 4 * s;
        if (p.x >= b.x - pad && p.x <= b.x + b.w + pad && p.y >= b.y - pad && p.y <= b.y + b.h + pad) return { o, part: "body" };
        continue;
      }
      const reach = o.width / 2 + tol;
      if (o.kind === "line" || o.kind === "arrow") {
        if (distToSegment(p, o.x1, o.y1, o.x2, o.y2) <= reach) return { o, part: "body" };
      } else if (o.kind === "rect") {
        const b = objBox(ctx, o);
        const inOuter = p.x >= b.x - reach && p.x <= b.x + b.w + reach && p.y >= b.y - reach && p.y <= b.y + b.h + reach;
        const inInner = p.x > b.x + reach && p.x < b.x + b.w - reach && p.y > b.y + reach && p.y < b.y + b.h - reach;
        // Unselected outlines are grabbed by their stroke (so you can still draw
        // inside them); once selected, anywhere inside drags it.
        if (inOuter && (!inInner || isSel)) return { o, part: "body" };
      } else {
        const cx = (o.x1 + o.x2) / 2, cy = (o.y1 + o.y2) / 2;
        const rx = Math.max(Math.abs(o.x2 - o.x1) / 2, 1), ry = Math.max(Math.abs(o.y2 - o.y1) / 2, 1);
        const d = Math.hypot((p.x - cx) / rx, (p.y - cy) / ry);
        if (Math.abs(d - 1) * Math.min(rx, ry) <= reach || (isSel && d <= 1)) return { o, part: "body" };
      }
    }
    return null;
  }

  useEffect(() => {
    renderCanvas();
  }, [renderCanvas, tick]);

  function isDirty(id: string): boolean {
    const st = imgStates.current[id];
    return !!st && st.idx !== 0;
  }

  function pushHistory(id: string, newBase: string | undefined, newAnno: string | null | undefined, newObjs?: Obj[]) {
    const st = imgStates.current[id];
    st.baseHistory = st.baseHistory.slice(0, st.idx + 1);
    st.annoHistory = st.annoHistory.slice(0, st.idx + 1);
    st.objsHistory = st.objsHistory.slice(0, st.idx + 1);
    st.baseHistory.push(newBase !== undefined ? newBase : st.baseHistory[st.idx]);
    st.annoHistory.push(newAnno !== undefined ? newAnno : st.annoHistory[st.idx]);
    st.objsHistory.push(newObjs ?? st.objsHistory[st.idx]);
    st.idx++;
    lastStyleEditRef.current = null;
  }

  async function flattenToDataUrl(id: string, includeObjs = true): Promise<string> {
    const st = imgStates.current[id];
    const c = document.createElement("canvas");
    c.width = st.w;
    c.height = st.h;
    const ctx = c.getContext("2d")!;
    const base = await getImg(st.baseHistory[st.idx]);
    ctx.drawImage(base, 0, 0);
    const annoSrc = st.annoHistory[st.idx];
    if (annoSrc) {
      const anno = await getImg(annoSrc);
      ctx.drawImage(anno, 0, 0);
    }
    // Objects get baked into pixels only on final save; blur/pixelate/crop
    // pass false so text and shapes stay live and editable on top.
    if (includeObjs) drawObjects(ctx, st.objsHistory[st.idx]);
    return c.toDataURL("image/png");
  }

  /** Scratch raster layer, reset to the committed raster annotations. Synchronous. */
  function resetAnnoLayer(st: ImgState, annoImg: HTMLImageElement | null) {
    const c = st.annoCanvas;
    if (c.width !== st.w || c.height !== st.h) {
      c.width = st.w;
      c.height = st.h;
    }
    const ctx = c.getContext("2d")!;
    ctx.clearRect(0, 0, st.w, st.h);
    if (annoImg) ctx.drawImage(annoImg, 0, 0);
    return ctx;
  }

  /** Repaints the in-progress drawing drag in a single synchronous pass — no blank frame. */
  function paintDrag(d: DragInfo) {
    const st = imgStates.current[d.id];
    const actx = resetAnnoLayer(st, d.annoImg);
    if (d.tool === "pen" || d.tool === "highlighter" || d.tool === "eraser") {
      actx.lineJoin = "round"; actx.lineCap = "round";
      actx.lineWidth = thickness * (d.tool === "highlighter" ? 2.2 : 1);
      actx.globalAlpha = d.tool === "highlighter" ? 0.35 : 1;
      actx.globalCompositeOperation = d.tool === "eraser" ? "destination-out" : "source-over";
      actx.strokeStyle = color;
      actx.beginPath();
      d.points.forEach((pt, i) => (i === 0 ? actx.moveTo(pt.x, pt.y) : actx.lineTo(pt.x, pt.y)));
      if (d.points.length === 1) actx.lineTo(d.points[0].x + 0.01, d.points[0].y); // a click still leaves a dot
      actx.stroke();
      actx.globalAlpha = 1;
      actx.globalCompositeOperation = "source-over";
    } else if (SHAPE_KINDS.includes(d.tool)) {
      drawShape(actx, { kind: d.tool as ShapeKind, x1: d.startX, y1: d.startY, x2: d.lastX, y2: d.lastY, color, width: thickness });
    } else {
      // blur / pixelate / crop: dashed region preview
      actx.save();
      actx.strokeStyle = "rgba(59,111,255,0.9)";
      actx.lineWidth = 1.5 * canvasScale();
      actx.setLineDash([6, 4]);
      actx.strokeRect(Math.min(d.startX, d.lastX), Math.min(d.startY, d.lastY), Math.abs(d.lastX - d.startX), Math.abs(d.lastY - d.startY));
      actx.restore();
    }
    paintScene(st, d.baseImg, st.annoCanvas, st.objsHistory[st.idx], null);
  }

  function canvasPoint(e: { clientX: number; clientY: number }): Point {
    const canvas = canvasRef.current!;
    const r = canvas.getBoundingClientRect();
    const sx = canvas.width / r.width;
    const sy = canvas.height / r.height;
    return { x: (e.clientX - r.left) * sx, y: (e.clientY - r.top) * sy };
  }

  function syncSidebarTo(o: Obj) {
    setColor(o.color);
    if (o.kind === "text") setFontSize(o.size);
    else setThickness(o.width);
  }

  async function committedLayers(id: string) {
    const st = imgStates.current[id];
    // Already decoded by renderCanvas, so these resolve from the cache.
    const baseImg = await getImg(st.baseHistory[st.idx]);
    const annoSrc = st.annoHistory[st.idx];
    const annoImg = annoSrc ? await getImg(annoSrc) : null;
    return { baseImg, annoImg };
  }

  async function onPointerDown(e: React.PointerEvent) {
    if (!currentId) return;
    const id = currentId;
    const p = canvasPoint(e);

    if (OBJECT_TOOLS.includes(tool)) {
      const hit = hitObject(id, p);
      if (hit) {
        setSelectedId(hit.o.id);
        lastStyleEditRef.current = null;
        syncSidebarTo(hit.o);
        canvasRef.current?.setPointerCapture(e.pointerId);
        const { baseImg, annoImg } = await committedLayers(id);
        objDragRef.current = {
          id, objId: hit.o.id, mode: hit.part, start: p, orig: hit.o,
          working: currentObjs(id).map((o) => ({ ...o })), moved: false, baseImg, annoImg,
        };
        return;
      }
      if (tool === "select") {
        setSelectedId(null);
        return;
      }
      if (tool === "text") {
        // Empty space: first click just deselects; with nothing selected it adds new text.
        if (selectedId) setSelectedId(null);
        else {
          setTextValue("");
          setTextEntry({ id, p });
        }
        return;
      }
      setSelectedId(null); // shape tool on empty space → start drawing a new shape
    }

    canvasRef.current?.setPointerCapture(e.pointerId);
    const { baseImg, annoImg } = await committedLayers(id);
    dragRef.current = { id, tool, startX: p.x, startY: p.y, lastX: p.x, lastY: p.y, points: [p], baseImg, annoImg };
    if (tool === "pen" || tool === "highlighter" || tool === "eraser") paintDrag(dragRef.current);
  }

  function onPointerMove(e: React.PointerEvent) {
    const od = objDragRef.current;
    if (od) {
      const p = canvasPoint(e);
      const dx = p.x - od.start.x, dy = p.y - od.start.y;
      if (Math.abs(dx) + Math.abs(dy) > 1) od.moved = true;
      const i = od.working.findIndex((w) => w.id === od.objId);
      const orig = od.orig;
      if (od.mode === "body") {
        od.working[i] = moveObj(orig, dx, dy);
      } else if (od.mode === "resize" && orig.kind === "text") {
        // Scale font size with the handle's vertical travel relative to the text's height.
        const origH = orig.size * 1.2;
        const size = Math.round(Math.min(400, Math.max(8, orig.size * ((origH + dy) / origH))));
        od.working[i] = { ...orig, size };
        setFontSize(size);
      } else if (orig.kind !== "text") {
        od.working[i] = od.mode === "p1"
          ? { ...orig, x1: orig.x1 + dx, y1: orig.y1 + dy }
          : { ...orig, x2: orig.x2 + dx, y2: orig.y2 + dy };
      }
      paintScene(imgStates.current[od.id], od.baseImg, od.annoImg, od.working, od.objId);
      return;
    }

    if (OBJECT_TOOLS.includes(tool) && currentId && !dragRef.current) {
      const hit = hitObject(currentId, canvasPoint(e));
      const idle = tool === "select" ? "default" : tool === "text" ? "text" : "crosshair";
      canvasRef.current!.style.cursor = !hit ? idle
        : hit.part === "body" ? "move"
        : hit.part === "resize" ? "nwse-resize" : "grab";
    }

    const drag = dragRef.current;
    if (!drag) return;
    const p = canvasPoint(e);
    drag.points.push(p);
    drag.lastX = p.x;
    drag.lastY = p.y;
    paintDrag(drag);
  }

  async function applyRegionEffect(id: string, rect: { x: number; y: number; w: number; h: number }, effect: "blur" | "pixelate") {
    const st = imgStates.current[id];
    const flat = await flattenToDataUrl(id, false);
    const img = await loadImage(flat);
    const c = document.createElement("canvas");
    c.width = st.w; c.height = st.h;
    const ctx = c.getContext("2d")!;
    ctx.drawImage(img, 0, 0);

    const rx = Math.round(rect.x), ry = Math.round(rect.y), rw = Math.round(rect.w), rh = Math.round(rect.h);
    if (effect === "blur") {
      ctx.save();
      ctx.beginPath(); ctx.rect(rx, ry, rw, rh); ctx.clip();
      ctx.filter = "blur(10px)";
      ctx.drawImage(img, 0, 0);
      ctx.restore();
    } else {
      const factor = 0.08;
      const tmp = document.createElement("canvas");
      tmp.width = Math.max(1, Math.round(rw * factor));
      tmp.height = Math.max(1, Math.round(rh * factor));
      const tctx = tmp.getContext("2d")!;
      tctx.drawImage(img, rx, ry, rw, rh, 0, 0, tmp.width, tmp.height);
      ctx.imageSmoothingEnabled = false;
      ctx.save();
      ctx.beginPath(); ctx.rect(rx, ry, rw, rh); ctx.clip();
      ctx.drawImage(tmp, 0, 0, tmp.width, tmp.height, rx, ry, rw, rh);
      ctx.restore();
    }
    pushHistory(id, c.toDataURL(), null);
  }

  async function applyCrop(id: string, rect: { x: number; y: number; w: number; h: number }) {
    const st = imgStates.current[id];
    const flat = await flattenToDataUrl(id, false);
    const img = await loadImage(flat);
    const rx = Math.round(rect.x), ry = Math.round(rect.y), rw = Math.round(rect.w), rh = Math.round(rect.h);
    const c = document.createElement("canvas");
    c.width = rw; c.height = rh;
    c.getContext("2d")!.drawImage(img, rx, ry, rw, rh, 0, 0, rw, rh);
    st.w = rw; st.h = rh;
    // Objects stay live — just shift them into the cropped coordinate space.
    const shifted = st.objsHistory[st.idx].map((o) => moveObj(o, -rx, -ry));
    pushHistory(id, c.toDataURL(), null, shifted);
  }

  async function onPointerUp(e: React.PointerEvent) {
    const od = objDragRef.current;
    if (od) {
      objDragRef.current = null;
      if (od.moved) pushHistory(od.id, undefined, undefined, od.working);
      setTick((t) => t + 1);
      return;
    }

    const drag = dragRef.current;
    if (!drag) return;
    dragRef.current = null;
    const { id } = drag;
    const p = canvasPoint(e);
    drag.points.push(p);
    drag.lastX = p.x;
    drag.lastY = p.y;
    const rect = {
      x: Math.min(drag.startX, p.x), y: Math.min(drag.startY, p.y),
      w: Math.abs(p.x - drag.startX), h: Math.abs(p.y - drag.startY),
    };
    const st = imgStates.current[id];

    if (drag.tool === "pen" || drag.tool === "highlighter" || drag.tool === "eraser") {
      // Commit exactly what's on screen: paint the final frame, then snapshot the scratch layer.
      paintDrag(drag);
      const annoUrl = st.annoCanvas.toDataURL();
      imgCache.current.set(annoUrl, await loadImage(annoUrl));
      pushHistory(id, undefined, annoUrl);
    } else if (SHAPE_KINDS.includes(drag.tool)) {
      if (Math.hypot(rect.w, rect.h) >= 3) {
        const shape: ShapeObj = {
          kind: drag.tool as ShapeKind, id: newId(),
          x1: drag.startX, y1: drag.startY, x2: p.x, y2: p.y, color, width: thickness,
        };
        pushHistory(id, undefined, undefined, [...currentObjs(id), shape]);
        setSelectedId(shape.id); // new shape starts selected, ready to adjust
      }
    } else if (drag.tool === "blur" || drag.tool === "pixelate") {
      if (rect.w > 4 && rect.h > 4) await applyRegionEffect(id, rect, drag.tool);
    } else if (drag.tool === "crop") {
      if (rect.w > 10 && rect.h > 10) await applyCrop(id, rect);
    }

    setTick((t) => t + 1);
  }

  function undo() {
    if (!currentId) return;
    const st = imgStates.current[currentId];
    lastStyleEditRef.current = null;
    if (st.idx > 0) { st.idx--; setTick((t) => t + 1); }
  }
  function redo() {
    if (!currentId) return;
    const st = imgStates.current[currentId];
    lastStyleEditRef.current = null;
    if (st.idx < st.baseHistory.length - 1) { st.idx++; setTick((t) => t + 1); }
  }

  /** Restyles the selected object. Consecutive tweaks to one object are a single undo step. */
  function styleSelected(patch: { color?: string; size?: number; width?: number }) {
    if (!currentId || !selectedId) return;
    const st = imgStates.current[currentId];
    const next = st.objsHistory[st.idx].map((o): Obj => {
      if (o.id !== selectedId) return o;
      if (o.kind === "text") return { ...o, color: patch.color ?? o.color, size: patch.size ?? o.size };
      return { ...o, color: patch.color ?? o.color, width: patch.width ?? o.width };
    });
    if (lastStyleEditRef.current === selectedId && st.idx > 0) {
      st.objsHistory[st.idx] = next; // continue the same style-edit step
    } else {
      pushHistory(currentId, undefined, undefined, next);
      lastStyleEditRef.current = selectedId;
    }
    setTick((t) => t + 1);
  }

  function deleteSelected() {
    if (!currentId || !selectedId) return;
    pushHistory(currentId, undefined, undefined, currentObjs(currentId).filter((o) => o.id !== selectedId));
    setSelectedId(null);
    setTick((t) => t + 1);
  }

  function changeTool(next: Tool) {
    setTool(next);
    if (!OBJECT_TOOLS.includes(next)) setSelectedId(null);
    if (canvasRef.current) canvasRef.current.style.cursor = next === "select" ? "default" : next === "text" ? "text" : "crosshair";
  }

  function onCanvasDoubleClick(e: React.MouseEvent) {
    if (!OBJECT_TOOLS.includes(tool) || !currentId) return;
    const hit = hitObject(currentId, canvasPoint(e));
    if (!hit || hit.o.kind !== "text") return;
    setSelectedId(hit.o.id);
    setTextValue(hit.o.text);
    setTextEntry({ id: currentId, p: { x: hit.o.x, y: hit.o.y }, editTextId: hit.o.id });
  }

  useEffect(() => {
    setSelectedId(null); // selection never carries across images in a batch
  }, [index]);

  async function handleDone() {
    let savedCount = 0;
    for (const id of targets) {
      if (isDirty(id)) {
        const dataUrl = await flattenToDataUrl(id);
        try {
          await invoke("save_annotated_image", { id, dataUrl });
          savedCount++;
        } catch (e) {
          toast(`Failed to save: ${e}`);
          return;
        }
      }
    }
    if (savedCount === 1 && targets.length === 1) {
      await invoke("copy_item_to_clipboard", { id: targets[0] }).catch(() => {});
    }
    await invoke("close_editor");
    imgStates.current = {};
  }

  async function discardAndClose() {
    setConfirmDiscard(false);
    await invoke("close_editor");
    imgStates.current = {};
  }

  function handleCancel() {
    const anyDirty = targets.some(isDirty);
    if (anyDirty) {
      setConfirmDiscard(true);
      return;
    }
    discardAndClose();
  }

  // The OS close button (red traffic light) is routed here from Rust; a ref
  // keeps the once-registered listener calling the latest handleCancel.
  const handleCancelRef = useRef(handleCancel);
  handleCancelRef.current = handleCancel;
  useEffect(() => {
    const unlisten = listen("clipflow://editor-close-requested", () => handleCancelRef.current());
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  function commitText() {
    if (!textEntry) return;
    const { id, p, editTextId } = textEntry;
    const txt = textValue.trim();
    setTextEntry(null);
    const objs = currentObjs(id);
    if (editTextId) {
      // Editing an existing text; clearing it out deletes it.
      const next = txt
        ? objs.map((o) => (o.id === editTextId && o.kind === "text" ? { ...o, text: txt } : o))
        : objs.filter((o) => o.id !== editTextId);
      pushHistory(id, undefined, undefined, next);
      if (!txt) setSelectedId(null);
    } else {
      if (!txt) return;
      // Place it so the click point is the vertical middle of the first line.
      const t: TextObj = { kind: "text", id: newId(), x: p.x, y: p.y - fontSize * 0.6, text: txt, color, size: fontSize };
      pushHistory(id, undefined, undefined, [...objs, t]);
      setSelectedId(t.id);
    }
    setTick((t) => t + 1);
  }

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      // The text-entry and discard-confirm modals have their own Enter/Escape
      // handling (via onKeyDown on their own input/buttons) — don't let this
      // document-level listener also fire save/cancel underneath them.
      if (textEntry || confirmDiscard) return;
      const mod = e.metaKey || e.ctrlKey;
      const key = e.key.toLowerCase();
      // Only real text fields own Delete/Backspace. Focus commonly sits on the
      // color picker or a slider right after restyling, and those must not
      // swallow "delete the selected object".
      const el = e.target as HTMLElement;
      const inField = el?.tagName === "TEXTAREA" || (el?.tagName === "INPUT" && (el as HTMLInputElement).type === "text");
      if (mod && key === "z" && !e.shiftKey) { e.preventDefault(); undo(); }
      else if (mod && (key === "y" || (key === "z" && e.shiftKey))) { e.preventDefault(); redo(); }
      else if ((e.key === "Delete" || e.key === "Backspace") && selectedId && !inField) { e.preventDefault(); deleteSelected(); }
      else if (e.key === "Escape") {
        e.preventDefault();
        if (selectedId) setSelectedId(null); // first Esc just deselects
        else handleCancel();
      }
      else if (e.key === "Enter") { e.preventDefault(); handleDone(); }
    }
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [targets, tick, textEntry, confirmDiscard, selectedId, currentId]);

  const currentItem = currentId ? items[currentId] : undefined;
  const currentSt = currentId ? imgStates.current[currentId] : undefined;
  const selectedObj = currentId && selectedId ? currentObjs(currentId).find((o) => o.id === selectedId) : undefined;
  const selTag = (kind: "any" | "text" | "shape") =>
    !selectedObj ? "" : kind === "any" || (kind === "text") === (selectedObj.kind === "text") ? " (selected)" : "";

  return (
    <div id="editor-shell">
      <div id="editor-topbar">
        <span className="title">
          {targets.length > 1 ? `Annotate (${index + 1} of ${targets.length})` : "Annotate"}
          {currentItem ? ` — ${currentItem.content}` : ""}
        </span>
        <span className="spacer"></span>
        <button className="ghost" onClick={handleCancel}>Cancel</button>
        <button className="primary" onClick={handleDone}>Done</button>
      </div>
      <div id="editor-main">
        <div id="editor-tools">
          {TOOLS.map((t) => (
            <button
              key={t.key}
              className={"tool-btn" + (tool === t.key ? " active" : "")}
              title={t.label}
              onClick={() => changeTool(t.key)}
            >
              {t.icon}
            </button>
          ))}
        </div>
        <div id="editor-canvas-wrap">
          <canvas
            id="editor-canvas"
            ref={canvasRef}
            onPointerDown={onPointerDown}
            onPointerMove={onPointerMove}
            onPointerUp={onPointerUp}
            onDoubleClick={onCanvasDoubleClick}
          />
        </div>
        <div id="editor-sidebar">
          <label>Color{selTag("any")}</label>
          <input
            type="color"
            value={color}
            onChange={(e) => { setColor(e.target.value); styleSelected({ color: e.target.value }); }}
          />
          <label>Thickness {thickness}px{selTag("shape")}</label>
          <input
            type="range" min={1} max={30} value={thickness}
            onChange={(e) => { const w = Number(e.target.value); setThickness(w); styleSelected({ width: w }); }}
          />
          <label>Font size {fontSize}px{selTag("text")}</label>
          <input
            type="range" min={8} max={200} value={fontSize}
            onChange={(e) => { const s = Number(e.target.value); setFontSize(s); styleSelected({ size: s }); }}
          />
          {selectedObj && (
            <button className="ghost" style={{ marginTop: 12, color: "#e5484d", borderColor: "#e5484d" }} onClick={deleteSelected}>
              Delete selected
            </button>
          )}
          {OBJECT_TOOLS.includes(tool) && (
            <p style={{ marginTop: 8, fontSize: 11, lineHeight: 1.5 }}>
              Click a shape or text to select · drag to move · drag the handles to reshape · double-click text to edit · <b>Delete</b> removes
            </p>
          )}
          <label style={{ marginTop: 16 }}>History</label>
          <button className="ghost" disabled={!currentSt || currentSt.idx === 0} onClick={undo}>Undo</button>
          <button className="ghost" disabled={!currentSt || currentSt.idx >= currentSt.baseHistory.length - 1} onClick={redo}>Redo</button>
          <p style={{ marginTop: 16, fontSize: 11, lineHeight: 1.5 }}>
            <b>Enter</b> save &amp; close · <b>Esc</b> cancel<br />
            <b>⌘/Ctrl+Z</b> undo · <b>⌘/Ctrl+Y</b> redo
          </p>
        </div>
      </div>
      {targets.length > 1 && (
        <div id="editor-batch-strip">
          {targets.map((id, i) => {
            const st = imgStates.current[id];
            return (
              <div
                key={id}
                className={"batch-thumb" + (i === index ? " current" : "")}
                onClick={() => setIndex(i)}
              >
                {st && <img src={st.baseHistory[st.idx]} alt="" />}
                {isDirty(id) && <div className="dirty-dot" />}
              </div>
            );
          })}
          <div id="editor-batch-nav">
            <button onClick={() => setIndex((i) => Math.max(0, i - 1))} disabled={index === 0}>‹ Prev</button>
            <span>{index + 1} / {targets.length}</span>
            <button onClick={() => setIndex((i) => Math.min(targets.length - 1, i + 1))} disabled={index === targets.length - 1}>Next ›</button>
          </div>
        </div>
      )}
      <div id="editor-toast" className={toastMsg ? "show" : ""}>{toastMsg}</div>

      {textEntry && (
        <div
          style={{ position: "fixed", inset: 0, background: "rgba(0,0,0,0.35)", display: "flex", alignItems: "center", justifyContent: "center", zIndex: 1000 }}
          onMouseDown={(e) => { if (e.target === e.currentTarget) setTextEntry(null); }}
        >
          <div style={{ background: "var(--bg-panel)", border: "1px solid var(--border-strong)", borderRadius: 10, padding: 16, width: 280, boxShadow: "0 20px 50px rgba(0,0,0,0.3)" }}>
            <div style={{ fontSize: 13, fontWeight: 600, marginBottom: 8, color: "var(--text-main)" }}>{textEntry.editTextId ? "Edit text" : "Add text"}</div>
            <input
              autoFocus
              value={textValue}
              onChange={(e) => setTextValue(e.target.value)}
              onKeyDown={(e) => {
                e.stopPropagation();
                if (e.key === "Enter") { e.preventDefault(); commitText(); }
                else if (e.key === "Escape") { e.preventDefault(); setTextEntry(null); }
              }}
              style={{ width: "100%", padding: "7px 9px", borderRadius: 6, border: "1px solid var(--border-strong)", background: "var(--bg-panel-alt)", color: "var(--text-main)", fontSize: 13, marginBottom: 12 }}
            />
            <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
              <button className="ghost" onClick={() => setTextEntry(null)}>Cancel</button>
              <button className="primary" onClick={commitText}>{textEntry.editTextId ? "Save" : "Add"}</button>
            </div>
          </div>
        </div>
      )}

      {confirmDiscard && (
        <div
          style={{ position: "fixed", inset: 0, background: "rgba(0,0,0,0.35)", display: "flex", alignItems: "center", justifyContent: "center", zIndex: 1000 }}
          onMouseDown={(e) => { if (e.target === e.currentTarget) setConfirmDiscard(false); }}
        >
          <div style={{ background: "var(--bg-panel)", border: "1px solid var(--border-strong)", borderRadius: 10, padding: 18, width: 300, boxShadow: "0 20px 50px rgba(0,0,0,0.3)" }}>
            <div style={{ fontSize: 14, fontWeight: 600, marginBottom: 6, color: "var(--text-main)" }}>Discard unsaved edits?</div>
            <div style={{ fontSize: 12.5, color: "var(--text-muted)", marginBottom: 16 }}>
              Any changes since you opened the editor will be lost.
            </div>
            <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
              <button className="ghost" onClick={() => setConfirmDiscard(false)}>Keep editing</button>
              <button className="primary" style={{ background: "var(--warn)" }} onClick={discardAndClose}>Discard</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
