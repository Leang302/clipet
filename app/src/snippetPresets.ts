import type { Parameter } from "./Snippets";

/** A small, optional, hand-picked starter library (spec Part F). Importing
 * never overwrites an existing snippet — `keyword` clashes are skipped and
 * reported, matching "never overwrite user-defined snippets silently." */
export type Preset = {
  id: string;
  category: "Developer Tools" | "Work" | "Formatting";
  name: string;
  keyword: string;
  content: string;
  parameters: Parameter[];
  /** Per-OS versions; `content` is the fallback on any OS not listed. */
  variants?: Partial<Record<"macos" | "windows" | "linux", string>>;
  suffix: "exact" | "space" | "newline";
  /** Shown before import for anything that isn't purely inert text —
   * Part E: "make potentially destructive command presets understandable
   * before use." Single generic command, not an OS-specific variant (that
   * needs the per-snippet platform-variant storage this phase doesn't add). */
  note?: string;
};

const text = (name: string, required = true): Parameter => ({
  name, kind: "text", min_length: null, max_length: null, pattern: null, required, default: null,
});

const integer = (name: string, min: number | null = null, max: number | null = null): Parameter => ({
  name, kind: "integer", min, max, required: true, default: null,
});

const date = (name: string): Parameter => ({
  name, kind: "date", format: null, required: true, default: null,
});

export const BUILTIN_PRESETS: Preset[] = [
  {
    id: "killport", category: "Developer Tools", name: "Kill Port", keyword: "killport", suffix: "exact",
    content: "lsof -tiTCP:{{port}} -sTCP:LISTEN | xargs kill",
    variants: {
      linux: "lsof -tiTCP:{{port}} -sTCP:LISTEN | xargs -r kill",
      windows: "Get-NetTCPConnection -LocalPort {{port}} -State Listen -ErrorAction SilentlyContinue | Select-Object -ExpandProperty OwningProcess -Unique | ForEach-Object { Stop-Process -Id $_ }",
    },
    parameters: [integer("port", 1, 65535)],
    note: "Stops whatever is listening on that port — review the command before running it. Pastes only; Clipet never runs it. The macOS version was checked on macOS; the Linux (bash) and Windows (PowerShell) versions have not been tested yet.",
  },
  {
    id: "localhost", category: "Developer Tools", name: "Localhost URL", keyword: "localhost", suffix: "exact",
    content: "http://localhost:{{port}}",
    parameters: [integer("port", 1, 65535)],
  },
  {
    id: "dockerlogs", category: "Developer Tools", name: "Docker Logs", keyword: "dockerlogs", suffix: "exact",
    content: "docker logs -f {{container}}",
    parameters: [text("container")],
    note: "Generates a shell command — review it before running it.",
  },
  {
    id: "gitcommit", category: "Developer Tools", name: "Git Commit", keyword: "gitcommit", suffix: "exact",
    content: 'git commit -m "{{message}}"',
    parameters: [text("message")],
    note: "Generates a shell command — review it before running it.",
  },
  {
    id: "gitcheckout", category: "Developer Tools", name: "Git Checkout", keyword: "gitcheckout", suffix: "exact",
    content: "git checkout -b {{branch}}",
    parameters: [text("branch")],
    note: "Generates a shell command — review it before running it.",
  },
  {
    id: "ssh", category: "Developer Tools", name: "SSH Connection", keyword: "ssh", suffix: "exact",
    content: "ssh {{user}}@{{host}}",
    parameters: [text("user"), text("host")],
    note: "Generates a shell command — review it before running it.",
  },
  {
    id: "hello", category: "Work", name: "Email Greeting", keyword: "hello", suffix: "exact",
    content: "Hi {{name}},\n\n",
    parameters: [text("name")],
  },
  {
    id: "meeting", category: "Work", name: "Meeting Message", keyword: "meeting", suffix: "exact",
    content: 'Reminder: "{{title}}" starts at {{time}}.',
    parameters: [text("title"), text("time")],
  },
  {
    id: "followup", category: "Work", name: "Follow-up Message", keyword: "followup", suffix: "exact",
    content: "Hi {{name}}, just following up on this — let me know if you have any questions!",
    parameters: [text("name")],
  },
  {
    id: "status", category: "Work", name: "Status Update", keyword: "status", suffix: "exact",
    content: "Status update for {{project}}: {{update}}",
    parameters: [text("project"), text("update")],
  },
  {
    id: "url", category: "Formatting", name: "Reusable URL", keyword: "url", suffix: "exact",
    content: "https://{{domain}}/{{path}}",
    parameters: [text("domain"), text("path", false)],
  },
  {
    id: "today", category: "Formatting", name: "Date Template", keyword: "today", suffix: "exact",
    content: "{{date}}",
    parameters: [date("date")],
  },
  {
    id: "filepath", category: "Formatting", name: "File Path Template", keyword: "filepath", suffix: "exact",
    content: "/Users/{{username}}/Documents/{{filename}}",
    parameters: [text("username"), text("filename")],
  },
];
