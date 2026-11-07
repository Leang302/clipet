import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/** One file or folder a file entry points to, looked up live (see `file_entries`). */
export type FileEntry = {
  name: string;
  path: string;
  url: string | null;
  exists: boolean;
  accessible: boolean;
  is_dir: boolean;
  is_symlink: boolean;
  size: number | null;
  modified: number | null;
  extension: string;
};

const IS_MAC = navigator.userAgent.includes("Mac");
export const REVEAL_LABEL = IS_MAC ? "Reveal in Finder" : "Show in Explorer";

const ICONS: [string, string[]][] = [
  ["📦", ["zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "dmg", "iso"]],
  ["📊", ["xlsx", "xls", "csv", "numbers", "ods"]],
  ["📝", ["doc", "docx", "pages", "odt", "rtf", "txt", "md"]],
  ["📕", ["pdf"]],
  ["📽️", ["ppt", "pptx", "key", "odp"]],
  ["🖼️", ["png", "jpg", "jpeg", "gif", "webp", "heic", "bmp", "tiff", "svg"]],
  ["🎞️", ["mp4", "mov", "mkv", "avi", "webm", "m4v"]],
  ["🎵", ["mp3", "wav", "aac", "flac", "m4a", "ogg"]],
  ["💻", ["js", "ts", "tsx", "jsx", "py", "rs", "go", "java", "c", "cpp", "h", "cs", "rb", "php", "swift", "kt", "html", "css", "json", "yml", "yaml", "sh", "sql"]],
];

export function fileIcon(extension: string, isDir = false): string {
  if (isDir) return "📁";
  return ICONS.find(([, exts]) => exts.includes(extension.toLowerCase()))?.[0] ?? "📄";
}

/** Icon for a file history row from its stored paths (no disk access). */
export function fileItemIcon(content: string): string {
  try {
    const paths: string[] = JSON.parse(content);
    if (paths.length > 1) return "🗂️";
    const name = paths[0]?.split(/[\\/]/).pop() ?? "";
    const dot = name.lastIndexOf(".");
    return dot > 0 ? fileIcon(name.slice(dot + 1)) : "📁";
  } catch {
    return "📄";
  }
}

function formatSize(bytes: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let n = bytes;
  let u = 0;
  while (n >= 1024 && u < units.length - 1) { n /= 1024; u++; }
  return `${u === 0 ? n : n.toFixed(n < 10 ? 1 : 0)} ${units[u]}`;
}

function describe(f: FileEntry): string {
  if (!f.exists) return "Not found";
  const parts = [f.is_dir ? "Folder" : f.extension ? f.extension.toUpperCase() + " file" : "File"];
  if (f.size !== null) parts.push(formatSize(f.size));
  if (f.modified) parts.push("modified " + new Date(f.modified).toLocaleDateString());
  if (f.is_symlink) parts.push("alias/link");
  if (!f.accessible) parts.push("no permission to read");
  return parts.join(" · ");
}

export function FileCard(props: {
  id: string;
  isPasting: boolean;
  onPaste: () => void;
  toast: (msg: string, ms?: number) => void;
}) {
  const { id, isPasting, onPaste, toast } = props;
  const [entries, setEntries] = useState<FileEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = () => invoke<FileEntry[]>("file_entries", { id }).then(setEntries).catch((e) => setError(String(e)));
  useEffect(() => {
    setEntries(null);
    setError(null);
    load();
    // Files move while the popup is closed; look again each time it is shown.
    window.addEventListener("focus", load);
    return () => window.removeEventListener("focus", load);
  }, [id]);

  if (error) return <div className="file-card"><div className="card-caption error">{error}</div></div>;
  if (!entries) return <div className="file-card"><div className="card-caption">Checking files…</div></div>;

  const missing = entries.filter((f) => !f.exists);
  const many = entries.length > 1;
  const act = (cmd: string, args: Record<string, unknown>, done?: string) =>
    invoke(cmd, { id, ...args }).then(() => done && toast(done)).catch((e) => toast(String(e), 6000));

  return (
    <div className="file-card">
      <div className="file-list" role="list">
        {entries.map((f, index) => (
          <div className={"file-row" + (f.exists ? "" : " missing")} role="listitem" key={index}>
            <span className="file-icon" aria-hidden>{fileIcon(f.extension, f.is_dir)}</span>
            <div className="file-info">
              <div className="file-name selectable">{f.name}</div>
              <div className="file-path selectable" title={f.path}>{f.path}</div>
              <div className="file-meta">{describe(f)}</div>
            </div>
            {many && f.exists && (
              <div className="file-row-actions">
                <button className="icon-btn" title={REVEAL_LABEL} aria-label={`${REVEAL_LABEL}: ${f.name}`} onClick={() => act("open_file_entry", { index, reveal: true })}>⌕</button>
                <button className="icon-btn" title="Open" aria-label={`Open ${f.name}`} onClick={() => act("open_file_entry", { index, reveal: false })}>↗</button>
              </div>
            )}
          </div>
        ))}
      </div>
      {missing.length > 0 && (
        <div className="file-warning" role="status">
          {missing.length === entries.length && !many
            ? "This file isn't at its saved location any more — it may have been moved, renamed or deleted, or its drive is disconnected."
            : `${missing.length} of ${entries.length} items aren't at their saved location any more.`}
          {" "}Paste File needs every file to be there; you can still copy the path.
        </div>
      )}
      <div className="file-actions">
        <button
          className="icon-btn primary"
          disabled={isPasting || missing.length > 0}
          title="Paste the actual file into apps that accept files (Finder, mail, chat…)"
          onClick={onPaste}
        >
          {isPasting ? "Pasting…" : many ? `Paste ${entries.length} Files` : "Paste File"}
        </button>
        <button className="icon-btn" title="Copy the file system path as text" onClick={() => act("copy_file_paths", { asUrl: false }, many ? "Paths copied" : "Path copied")}>Copy Path</button>
        <button className="icon-btn" title="Copy a file:// link (spaces and other characters encoded)" onClick={() => act("copy_file_paths", { asUrl: true }, many ? "File links copied" : "File link copied")}>Copy File Link</button>
        {!many && entries[0]?.exists && (
          <>
            <button className="icon-btn" onClick={() => act("open_file_entry", { index: 0, reveal: true })}>{REVEAL_LABEL}</button>
            <button className="icon-btn" onClick={() => act("open_file_entry", { index: 0, reveal: false })}>Open</button>
          </>
        )}
      </div>
    </div>
  );
}
