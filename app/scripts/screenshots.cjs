// Renders README screenshots from the built UI (npm run screenshots) with
// sample data in an in-page mock of the Tauri bridge — never real history.
// Output: docs/screenshots/*.png
const http = require("http");
const fs = require("fs");
const path = require("path");
const { chromium } = require("playwright");

const DIST = path.join(__dirname, "..", "dist");
const OUT = path.join(__dirname, "..", "..", "docs", "screenshots");
fs.mkdirSync(OUT, { recursive: true });

const server = http.createServer((req, res) => {
  const url = decodeURIComponent(req.url.split("?")[0]);
  const p = path.join(DIST, url === "/" ? "index.html" : url);
  fs.readFile(p, (err, data) => {
    if (err) { res.writeHead(404); res.end(); return; }
    const type = p.endsWith(".js") ? "text/javascript" : p.endsWith(".css") ? "text/css" : p.endsWith(".svg") ? "image/svg+xml" : "text/html";
    res.writeHead(200, { "Content-Type": type });
    res.end(data);
  });
});

// Runs inside the page before the app loads.
function mockTauri([label, theme]) {
  try { localStorage.setItem("clipflow-theme", theme); } catch {}
  const calls = [];
  window.__calls = calls;
  let cb = 0;
  const min = 60_000, hour = 60 * min, day = 24 * hour;
  const now = Date.now();

  // A believable app screenshot, drawn on a canvas.
  const screenshot = () => {
    const c = document.createElement("canvas");
    c.width = 1200; c.height = 760;
    const g = c.getContext("2d");
    g.fillStyle = "#f6f7f9"; g.fillRect(0, 0, 1200, 760);
    g.fillStyle = "#1f2937"; g.fillRect(0, 0, 220, 760);
    g.fillStyle = "#cbd5e1"; g.font = "22px -apple-system, sans-serif";
    ["Dashboard", "Customers", "Sales history", "Contracts", "Invoices"].forEach((t, i) => {
      if (i === 2) { g.fillStyle = "#3b82f6"; g.fillRect(12, 112 + i * 56, 196, 44); g.fillStyle = "#fff"; }
      else g.fillStyle = "#cbd5e1";
      g.fillText(t, 32, 142 + i * 56);
    });
    g.fillStyle = "#111827"; g.font = "bold 34px -apple-system, sans-serif"; g.fillText("Sales history · Q3 2026", 260, 80);
    g.fillStyle = "#fff"; g.fillRect(260, 120, 900, 560);
    g.strokeStyle = "#e5e7eb"; g.lineWidth = 2; g.strokeRect(260, 120, 900, 560);
    g.font = "bold 20px -apple-system, sans-serif"; g.fillStyle = "#374151";
    ["Invoice", "Customer", "Amount", "Status"].forEach((h, i) => g.fillText(h, 290 + i * 220, 165));
    g.font = "20px -apple-system, sans-serif";
    [["INV-2026-114", "Globex Ltd", "$42,000", "Paid"], ["INV-2026-115", "Acme Corp", "$12,480", "Overdue"], ["INV-2026-116", "Initech", "$9,800", "Paid"], ["INV-2026-117", "Northwind", "$3,150", "Draft"]]
      .forEach((row, r) => row.forEach((cell, i) => { g.fillStyle = cell === "Overdue" ? "#dc2626" : "#111827"; g.fillText(cell, 290 + i * 220, 225 + r * 60); }));
    return c.toDataURL("image/png");
  };
  const shot = screenshot();
  const thumb = (color, glyph) => {
    const c = document.createElement("canvas"); c.width = 80; c.height = 80;
    const g = c.getContext("2d"); g.fillStyle = color; g.fillRect(0, 0, 80, 80);
    g.fillStyle = "#fff"; g.font = "36px sans-serif"; g.fillText(glyph, 20, 54);
    return c.toDataURL("image/png");
  };

  const base = { source_app: "", last_used_at: null, use_count: 0, favorite: false, pinned: false, edited: false, ocr_status: 0, ocr_text: null, ocr_lines: null, ocr_error: null };
  const code = `export async function fetchInvoices(customerId: string) {
  const res = await fetch(\`/api/customers/\${customerId}/invoices\`);
  if (!res.ok) {
    throw new Error(\`Request failed: \${res.status}\`);
  }
  const invoices: Invoice[] = await res.json();
  return invoices
    .filter((invoice) => invoice.status !== "draft")
    .sort((a, b) => b.issuedAt - a.issuedAt);
}

export function totalOutstanding(invoices: Invoice[]) {
  return invoices
    .filter((invoice) => invoice.status === "overdue")
    .reduce((sum, invoice) => sum + invoice.amount, 0);
}
`.repeat(4);
  const items = [
    { ...base, id: "shot", kind: "screenshot", content: "Screenshot 2026-11-07 at 10.24.png", preview: "1200x760", created_at: now - 2 * min, ocr_status: 1,
      ocr_text: "Dashboard\nCustomers\nSales history\nContracts\nInvoices\nSales history · Q3 2026\nINV-2026-115 Acme Corp $12,480 Overdue",
      ocr_lines: JSON.stringify([{ text: "Sales history", x: 0.01, y: 0.295, w: 0.163, h: 0.058 }, { text: "Sales history · Q3 2026", x: 0.212, y: 0.06, w: 0.36, h: 0.06 }]) },
    { ...base, id: "meet", kind: "link", content: "https://calendar.example.com/event/quarterly-review", preview: "https://calendar.example.com/event/quarterly-review", created_at: now - 9 * min },
    { ...base, id: "addr", kind: "text", content: "Thanks for the quick turnaround! I've attached the signed contract — let me know if you need anything else before Friday.", preview: "Thanks for the quick turnaround!", created_at: now - 25 * min, favorite: true },
    { ...base, id: "zip", kind: "file", content: JSON.stringify(["/Users/sam/Downloads/Final Python.zip"]), preview: "Final Python.zip", created_at: now - 48 * min },
    { ...base, id: "code", kind: "code", content: code, preview: "export async function fetchInvoices", created_at: now - 2 * hour, pinned: true },
    { ...base, id: "docs", kind: "file", content: JSON.stringify(["/Users/sam/Documents/Q3 Sales Report.xlsx", "/Users/sam/Documents/Contract.pdf"]), preview: "Q3 Sales Report.xlsx, Contract.pdf", created_at: now - 5 * hour },
    { ...base, id: "kh", kind: "text", content: "សួស្តី! ខ្ញុំនឹងផ្ញើឯកសារនៅថ្ងៃស្អែក។", preview: "សួស្តី! ខ្ញុំនឹងផ្ញើឯកសារនៅថ្ងៃស្អែក។", created_at: now - day },
    { ...base, id: "cmd", kind: "text", content: "git log --oneline --graph --decorate -20", preview: "git log --oneline --graph --decorate -20", created_at: now - 2 * day },
  ];
  const snip = { category: null, enabled: true, favorite: false, created_at: now - 30 * day, updated_at: now - 3 * day, last_used_at: null, suffix: "exact", parameters: "[]" };
  const snippets = [
    { ...snip, id: "s1", name: "Email signature", keyword: "sig", content: "Best regards,\nSam Lee\nProduct Manager · Example Inc.", content_type: "text", use_count: 42, suffix: "newline" },
    { ...snip, id: "s2", name: "Address", keyword: "addr", content: "123 Example Street, Suite 400\nSan Francisco, CA 94105", content_type: "text", use_count: 17 },
    { ...snip, id: "s3", name: "Company logo", keyword: "logo", content: "logo.png", content_type: "image", use_count: 9 },
    { ...snip, id: "s4", name: "Meeting link", keyword: "meet", content: "https://meet.example.com/sam-lee", content_type: "text", use_count: 31, suffix: "space" },
    { ...snip, id: "s5", name: "Kill Port", keyword: "killport", content: "lsof -tiTCP:{{port}} -sTCP:LISTEN | xargs kill", content_type: "text", use_count: 12,
      parameters: JSON.stringify([{ name: "port", kind: "integer", min: 1, max: 65535, required: true, default: null }]),
      variants: JSON.stringify({ windows: "Get-NetTCPConnection -LocalPort {{port}} -State Listen | ForEach-Object { Stop-Process -Id $_.OwningProcess }" }) },
  ];

  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label }, currentWebview: { label } },
    transformCallback: (fn) => { const id = ++cb; window["__cb" + id] = fn; return id; },
    convertFileSrc: (p) => p,
    invoke: async (cmd, args) => {
      calls.push({ cmd, args });
      switch (cmd) {
        case "get_history": return items;
        case "get_items": return items.filter((i) => args.ids.includes(i.id));
        case "get_image_data_url": return shot;
        case "get_thumbnail_data_url": return shot;
        case "get_editor_targets": return ["shot"];
        case "ocr_status": return { supported: true, enabled: true, language: "auto", pending: 0, updating: 0, done: 1, failed: 0 };
        case "file_entries": {
          const sizes = { zip: 18_400_000, xlsx: 248_000, pdf: 1_260_000 };
          return JSON.parse(items.find((i) => i.id === args.id).content).map((p) => {
            const ext = p.split(".").pop();
            return { name: p.split("/").pop(), path: p, url: "file://" + p.split("/").map(encodeURIComponent).join("/"), exists: true, accessible: true, is_dir: false, is_symlink: false, size: sizes[ext] ?? 1000, modified: now - 3 * day, extension: ext };
          });
        }
        case "history_retention_get": return { retention: { days: 30, include_favorites: false, include_pinned: false }, eligible: 0 };
        case "history_retention_preview": return 0;
        case "monitoring_status": return { paused: false, until: null };
        case "snippets_list": return snippets;
        case "snippet_image_preview": return thumb("#2563eb", "◆");
        case "snippet_settings_get": return { enabled: true, prefix: ";", trigger_mode: "space", escape_enabled: true, case_sensitive: true, default_pause_minutes: 15 };
        case "snippets_status": return { enabled: true, paused: false, paused_until: null, listener: "active", blocked_by: null, platform: "macos" };
        case "snippet_prompt_get": return window.__prompt ?? null;
        case "snippet_preview": {
          const errors = {}, vals = {};
          for (const p of args.parameters) {
            const raw = args.values[p.name] || p.default || "";
            if (!raw) { if (p.required) errors[p.name] = "Required"; else vals[p.name] = ""; continue; }
            if (p.kind === "integer" && !/^\s*-?\d+\s*$/.test(raw)) { errors[p.name] = `'${raw}' is not a whole number`; continue; }
            vals[p.name] = raw.trim();
          }
          if (Object.keys(errors).length) return { text: null, errors };
          return { text: args.content.replace(/\{\{\s*(\w+)\s*\}\}/g, (_, n) => vals[n]), errors };
        }
        default: return null;
      }
    },
  };
}

(async () => {
  await new Promise((r) => server.listen(0, r));
  const url = `http://127.0.0.1:${server.address().port}/`;
  const browser = await chromium.launch();
  const save = (page, name) => page.screenshot({ path: path.join(OUT, name) }).then(() => console.log("saved", name));

  for (const theme of ["light", "dark"]) {
    const page = await browser.newPage({ viewport: { width: 860, height: 580 }, deviceScaleFactor: 2, colorScheme: theme });
    await page.addInitScript(mockTauri, ["history", theme]);
    await page.goto(url);
    await page.waitForSelector(".clip-item");
    await page.waitForTimeout(300);
    const pick = (text) => page.locator(".clip-item", { hasText: text }).first().click().then(() => page.waitForTimeout(250));

    if (theme === "light") {
      // Search finds text inside a screenshot (OCR).
      await page.fill("#search-input", "sales history");
      await page.waitForTimeout(300);
      await pick("Screenshot 2026-11-07");
      await save(page, "search-text-in-images.png");
      await page.fill("#search-input", "");

      await pick("Final Python.zip");
      await page.waitForSelector(".file-card .file-row");
      await save(page, "files.png");

      await pick("export async function");
      await page.click('button:has-text("Expand")');
      await page.waitForSelector(".text-viewer");
      await page.keyboard.press("Meta+f");
      await page.keyboard.type("invoice");
      await page.waitForTimeout(200);
      await save(page, "text-viewer.png");
      await page.keyboard.press("Escape");
      await page.keyboard.press("Escape");

      await page.click('button[aria-label="Settings"]');
      await page.waitForSelector(".history-settings");
      await page.waitForTimeout(150);
      await save(page, "history-settings.png");
      await page.keyboard.press("Escape");

      await page.click('button[aria-label="Snippets"]');
      await page.waitForSelector(".snippet-item");
      await page.locator(".snippet-item", { hasText: ";sig" }).click();
      await page.waitForTimeout(250);
      await save(page, "snippets.png");

      // A snippet with a {{port}} value, filled in from Clipet itself.
      await page.locator(".snippet-item", { hasText: ";killport" }).click();
      await page.keyboard.press("Enter");
      await page.waitForSelector(".snippet-test");
      await page.keyboard.type("5173");
      await page.waitForTimeout(350);
      await save(page, "snippet-fill-in.png");
    } else {
      await pick("Thanks for the quick");
      await save(page, "history-dark.png");
    }
    await page.close();
  }

  // The small window that asks for a missing value after `;killport` + Tab
  // in another app. Captured on its own, with transparent corners.
  {
    const popup = await browser.newPage({ viewport: { width: 440, height: 420 }, deviceScaleFactor: 2, colorScheme: "dark" });
    await popup.addInitScript((p) => { window.__prompt = p; }, {
      token: 1, name: "Kill Port", trigger: ";killport", content: "lsof -tiTCP:{{port}} -sTCP:LISTEN | xargs kill", suffix: "exact",
      parameters: [{ name: "port", kind: "integer", min: 1, max: 65535, required: true, default: null }], prefill: {},
    });
    await popup.addInitScript(mockTauri, ["prompt", "dark"]);
    await popup.goto(url);
    await popup.waitForSelector(".prompt-card .param-field");
    await popup.fill('[data-param="port"]', "8989");
    await popup.waitForTimeout(350);
    await popup.locator(".prompt-card").screenshot({ path: path.join(OUT, "snippet-popup.png"), omitBackground: true });
    console.log("saved snippet-popup.png");
    await popup.close();
  }

  // Annotation editor with a couple of marks.
  const editor = await browser.newPage({ viewport: { width: 1100, height: 720 }, deviceScaleFactor: 2 });
  await editor.addInitScript(mockTauri, ["editor", "light"]);
  await editor.goto(url);
  await editor.waitForFunction(() => document.getElementById("editor-canvas")?.width === 1200);
  const canvas = editor.locator("#editor-canvas");
  const at = async (x, y) => {
    const r = await canvas.boundingBox();
    const s = r.width / 1200;
    return { x: r.x + x * s, y: r.y + y * s };
  };
  const drag = async (from, to) => {
    const f = await at(...from), t = await at(...to);
    await editor.mouse.move(f.x, f.y); await editor.mouse.down();
    await editor.mouse.move(t.x, t.y, { steps: 8 }); await editor.mouse.up();
    await editor.waitForTimeout(80);
  };
  await editor.click('button[title="Rectangle"]');
  await drag([272, 255], [1150, 300]);
  await editor.click('button[title="Arrow"]');
  await drag([1110, 620], [1050, 312]);
  await editor.keyboard.press("Escape");
  await editor.waitForTimeout(150);
  await save(editor, "annotation-editor.png");

  await browser.close();
  server.close();
})();
