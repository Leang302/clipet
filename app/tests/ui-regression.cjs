// Headless UI regression test for the popup and annotation editor: runs the
// built bundle (npm run test:ui) against an in-page mock of the Tauri bridge.
const http = require("http");
const fs = require("fs");
const path = require("path");
const { chromium } = require(process.env.PW_PATH || "playwright");

const DIST = process.argv[2];
const results = [];
function check(name, ok, detail = "") {
  results.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  — " + detail : ""}`);
}

const server = http.createServer((req, res) => {
  const p = path.join(DIST, decodeURIComponent(req.url.split("?")[0]) === "/" ? "index.html" : req.url.split("?")[0]);
  fs.readFile(p, (err, data) => {
    if (err) { res.writeHead(404); res.end(); return; }
    const type = p.endsWith(".js") ? "text/javascript" : p.endsWith(".css") ? "text/css" : "text/html";
    res.writeHead(200, { "Content-Type": type });
    res.end(data);
  });
});

// Runs inside the page before the app loads.
function mockTauri(label) {
  const calls = [];
  window.__calls = calls;
  let cb = 0;
  const img = (w, h, text) => {
    const c = document.createElement("canvas");
    c.width = w; c.height = h;
    const g = c.getContext("2d");
    g.fillStyle = "#fff"; g.fillRect(0, 0, w, h);
    g.fillStyle = "#222"; g.font = "28px sans-serif"; g.fillText(text || "", 20, 60);
    return c.toDataURL("image/png");
  };
  const base = { source_app: "", last_used_at: null, use_count: 0, favorite: false, pinned: false, edited: false, ocr_status: 0, ocr_text: null, ocr_lines: null };
  let items = [
    { ...base, id: "t1", kind: "text", content: "hello world", preview: "hello world", created_at: Date.now() - 1000 },
    { ...base, id: "t2", kind: "text", content: "second text", preview: "second text", created_at: Date.now() - 2000, pinned: true },
    { ...base, id: "img1", kind: "image", content: "img1.png", preview: "800x500", created_at: Date.now() - 3000,
      ocr_status: 1, ocr_text: "Exception in thread main\njava.lang.NullPointerException at Foo.java:42",
      ocr_lines: JSON.stringify([{ text: "Exception in thread main", x: 0.05, y: 0.1, w: 0.5, h: 0.06 }, { text: "java.lang.NullPointerException at Foo.java:42", x: 0.05, y: 0.2, w: 0.8, h: 0.06 }]) },
    { ...base, id: "t3", kind: "link", content: "https://example.com/a", preview: "https://example.com/a", created_at: Date.now() - 4000, favorite: true },
    { ...base, id: "img2", kind: "image", content: "img2.png", preview: "400x300", created_at: Date.now() - 5000, ocr_status: 2, ocr_error: "Vision could not read the image: unsupported format" },
    { ...base, id: "img3", kind: "image", content: "img3.png", preview: "2460x1318", created_at: Date.now() - 6000,
      ocr_status: 1, ocr_text: "약정보관리\n매출관리내역 기\n고객정보\nNull PointerException\nSCR-019",
      ocr_lines: JSON.stringify([{ text: "매출관리내역 기", x: 0.1, y: 0.1, w: 0.3, h: 0.05 }, { text: "고객정보", x: 0.1, y: 0.2, w: 0.2, h: 0.05 }]) },
    { ...base, id: "t4", kind: "text", content: "매출", preview: "매출", created_at: Date.now() - 7000 },
    { ...base, id: "f1", kind: "file", content: JSON.stringify(["/Users/me/Downloads/Final Python.zip"]), preview: "Final Python.zip", created_at: Date.now() - 8000 },
    { ...base, id: "f2", kind: "file", content: JSON.stringify(["/Users/me/a.pdf", "/Volumes/USB/gone.xlsx"]), preview: "a.pdf, gone.xlsx", created_at: Date.now() - 9000 },
    { ...base, id: "t5", kind: "text", content: Array.from({ length: 400 }, (_, i) => `line ${i} ${i % 50 === 0 ? "needle" : "hay"}`).join("\n"), preview: "line 0 needle", created_at: Date.now() - 10000 },
  ];
  const snip = { category: null, enabled: true, favorite: false, use_count: 0, created_at: 1, updated_at: 1, last_used_at: null };
  let snippets = [
    { ...snip, id: "s1", name: "Name", keyword: "name", content: "KimLeang", content_type: "text" },
    { ...snip, id: "s2", name: "Logo", keyword: "logo", content: "snip-abc.png", content_type: "image" },
  ];
  // Fires an app event at the page's listeners, like the backend would.
  window.__emit = (event, payload = null) => calls
    .filter((c) => c.cmd === "plugin:event|listen" && c.args.event === event)
    .forEach((c) => window["__cb" + c.args.handler]?.({ event, id: 0, payload }));
  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label }, currentWebview: { label } },
    transformCallback: (fn) => { const id = ++cb; window["__cb" + id] = fn; return id; },
    convertFileSrc: (p) => p,
    invoke: async (cmd, args) => {
      calls.push({ cmd, args });
      switch (cmd) {
        case "get_history": return items;
        case "get_items": return items.filter((i) => args.ids.includes(i.id));
        case "get_image_data_url": return img(800, 500, args.filename);
        case "get_thumbnail_data_url": return img(80, 50, "t");
        case "ocr_status": case "ocr_rescan": return { supported: true, enabled: true, language: "korean-english", pending: 1, updating: 0, done: 1, failed: 1 };
        case "ocr_set_enabled": return { supported: true, enabled: args.enabled, language: "korean-english", pending: 0, updating: 0, done: 1, failed: 1 };
        case "delete_item": items = items.filter((i) => i.id !== args.id); return null;
        case "get_editor_targets": return ["img1"];
        case "file_entries": {
          const entry = (path, exists) => ({ name: path.split("/").pop(), path, url: "file://" + path.split("/").map(encodeURIComponent).join("/"), exists, accessible: exists, is_dir: false, is_symlink: false, size: exists ? 2048 : null, modified: exists ? 1 : null, extension: path.split(".").pop() });
          return JSON.parse(items.find((i) => i.id === args.id).content).map((p) => entry(p, !p.includes("gone")));
        }
        case "history_retention_get": return { retention: { days: 0, include_favorites: false, include_pinned: false }, eligible: 0 };
        case "history_retention_preview": return args.retention.days === 0 ? 0 : 4;
        case "history_retention_set": return 4;
        case "monitoring_status": return { paused: false, until: null };
        case "pause_monitoring": return { paused: true, until: args.minutes ? Date.now() + args.minutes * 60000 : null };
        case "snippets_list": return snippets;
        case "snippet_image_preview": return img(200, 120, "logo");
        case "snippet_save": {
          const input = args.input;
          const saved = { ...snip, ...input, id: input.id || "s" + (snippets.length + 1), content: input.content_type === "image" ? "snip-new.png" : input.content, updated_at: Date.now() };
          snippets = [...snippets.filter((x) => x.id !== saved.id), saved];
          return saved;
        }
        case "snippet_settings_get": return { enabled: true, prefix: ";", trigger_mode: "space", escape_enabled: true, case_sensitive: true, default_pause_minutes: 15 };
        case "snippets_status": return { enabled: true, paused: false, paused_until: null, listener: "active" };
        default: return null; // plugin:event|listen etc.
      }
    },
  };
}

(async () => {
  await new Promise((r) => server.listen(0, r));
  const url = `http://127.0.0.1:${server.address().port}/`;
  const browser = await chromium.launch();
  const errors = [];

  // ---------------- Main popup ----------------
  {
    const page = await browser.newPage({ viewport: { width: 860, height: 520 } });
    page.on("pageerror", (e) => errors.push("popup: " + e.message));
    await page.addInitScript(mockTauri, "history");
    await page.goto(url);
    await page.waitForSelector(".clip-item");
    const sel = () => page.evaluate(() => window.getSelection().toString());
    const selectedIds = () => page.$$eval(".clip-item.selected .clip-title", (els) => els.map((e) => e.textContent));

    // Thumbnails: the list never loads full images for non-previewed items.
    await page.waitForTimeout(150);
    const fullLoads = await page.evaluate(() => window.__calls.filter((c) => c.cmd === "get_image_data_url").map((c) => c.args.filename));
    const thumbLoads = await page.evaluate(() => new Set(window.__calls.filter((c) => c.cmd === "get_thumbnail_data_url").map((c) => c.args.id)).size);
    check("list uses thumbnails; full images only for previewed items", thumbLoads === 3 && !fullLoads.includes("img2.png"), `thumbs=${thumbLoads} full=${JSON.stringify(fullLoads)}`);

    // Drag across the sidebar and the history list.
    const a = await page.locator(".sidebar-item").first().boundingBox();
    const b = await page.locator(".clip-item").nth(3).boundingBox();
    await page.mouse.move(a.x + 5, a.y + 5);
    await page.mouse.down();
    await page.mouse.move(b.x + b.width - 5, b.y + b.height - 5, { steps: 10 });
    await page.mouse.up();
    check("dragging across sidebar and list selects no text", (await sel()) === "", JSON.stringify(await sel()));

    // Click, then Shift+Click a range.
    await page.locator(".clip-item").nth(0).click();
    await page.locator(".clip-item").nth(2).click({ modifiers: ["Shift"] });
    check("Shift+Click selects a range of items", (await selectedIds()).length === 3, JSON.stringify(await selectedIds()));
    check("Shift+Click does not paint browser text selection", (await sel()) === "", JSON.stringify(await sel()));
    check("multi-selection shows check indicators", (await page.$$(".clip-item .clip-check")).length === 10);

    // Cmd+Click toggles one item off.
    await page.locator(".clip-item").nth(1).click({ modifiers: ["Meta"] });
    check("Cmd+Click toggles an item", (await selectedIds()).length === 2, JSON.stringify(await selectedIds()));

    // Escape collapses the multi-selection, popup stays open.
    await page.keyboard.press("Escape");
    check("Escape collapses a multi-selection first", (await selectedIds()).length <= 1 && !(await page.evaluate(() => window.__calls.some((c) => c.cmd === "hide_history"))));

    // Cmd+A with focus in the list selects all items, not page text.
    await page.locator(".clip-item").nth(0).click();
    await page.keyboard.press("Meta+a");
    check("Cmd+A in the list selects all items", (await selectedIds()).length === 10, JSON.stringify(await selectedIds()));
    check("Cmd+A in the list selects no page text", (await sel()) === "");

    // Cmd+A in the search box selects its text only.
    await page.click("#search-input");
    await page.keyboard.type("text");
    await page.keyboard.press("Meta+a");
    const inputSel = await page.evaluate(() => { const i = document.getElementById("search-input"); return [i.selectionStart, i.selectionEnd]; });
    check("Cmd+A in search selects the field's text", inputSel[0] === 0 && inputSel[1] === 4, JSON.stringify(inputSel));
    check("search filters items", (await page.$$(".clip-item")).length === 1);
    check("filtering drops hidden items from the selection", (await selectedIds()).length <= 2);

    // Escape clears search before closing.
    await page.keyboard.press("Escape");
    check("Escape clears the search first", (await page.inputValue("#search-input")) === "" && !(await page.evaluate(() => window.__calls.some((c) => c.cmd === "hide_history"))));

    // OCR search.
    await page.fill("#search-input", "NullPointer");
    await page.waitForTimeout(100);
    const titles = await page.$$eval(".clip-item .clip-title", (els) => els.map((e) => e.textContent));
    check("search finds a screenshot by text inside it", titles.includes("img1.png"), JSON.stringify(titles));
    check("OCR-only matches are labelled", (await page.$$(".ocr-chip")).length === titles.length);
    await page.waitForSelector(".ocr-image img");
    const hits = await page.$$(".ocr-hit");
    check("matched OCR line is highlighted in the preview", hits.length === 1);
    check("Copy text action is available", (await page.$$("text=Copy text")).length === 1);
    await page.fill("#search-input", "");


    // Failed OCR explains why and offers a re-scan.
    await page.locator(".clip-item", { hasText: "img2" }).click();
    await page.click(".ocr-section summary");
    check("failed OCR shows a meaningful error", (await page.$$("text=Vision could not read the image: unsupported format")).length === 1);
    check("failed OCR offers Re-scan", (await page.$$(".ocr-actions >> text=Re-scan")).length === 1);

    // Korean OCR search: full phrase and partial syllable runs.
    const results = async (q) => { await page.fill("#search-input", q); await page.waitForTimeout(60); return page.$$eval(".clip-item .clip-title", (els) => els.map((e) => e.textContent)); };
    for (const q of ["매출관리내역", "매출", "관리", "내역"]) {
      check(`Korean search "${q}" finds the screenshot`, (await results(q)).includes("img3.png"));
    }
    check("decomposed (NFD) Korean query still matches", (await results("매출관리내역".normalize("NFD"))).includes("img3.png"));
    check("English OCR search ignores case and OCR-inserted spaces", (await results("nullpointerexception")).includes("img3.png"));
    check("mixed Korean + English multi-word query", (await results("매출관리내역 SCR-019")).includes("img3.png"));
    const ranked = await results("매출");
    check("exact match ranks before substring matches", ranked[0] === "매출" && ranked.includes("img3.png"), JSON.stringify(ranked));
    check("no duplicate results", new Set(ranked).size === ranked.length);

    // The text panel opens itself when the match is inside the image.
    await results("고객정보");
    await page.locator(".clip-item", { hasText: "img3" }).click();
    await page.waitForSelector(".ocr-image img");
    check("text panel is expanded when search matched image text", (await page.$$("details.ocr-section[open]")).length === 1);
    check("all recognized lines are shown", (await page.locator(".ocr-text").textContent()).split("\n").length === 5);
    check("matching OCR line is highlighted", (await page.$$(".ocr-hit")).length === 1);
    await page.click(".ocr-actions >> text=Re-scan");
    const rescanned = await page.evaluate(() => window.__calls.filter((c) => c.cmd === "ocr_rescan").map((c) => c.args.ids));
    check("Re-scan queues the selected image", JSON.stringify(rescanned) === JSON.stringify([["img3"]]), JSON.stringify(rescanned));
    await page.fill("#search-input", "");

    // Delete two selected items: confirmation, then both deleted.
    await page.locator(".clip-item").nth(0).click();
    await page.locator(".clip-item").nth(1).click({ modifiers: ["Shift"] });
    await page.keyboard.press("Backspace");
    check("deleting several items asks first", (await page.$$("text=Delete 2 items?")).length === 1);
    await page.click(".recovery-actions .danger");
    await page.waitForTimeout(100);
    const deletes = await page.evaluate(() => window.__calls.filter((c) => c.cmd === "delete_item").length);
    check("confirmed delete removes both items", deletes === 2, `delete calls=${deletes}`);

    // Clear dialog mentions pinned items and snippets.
    await page.click("text=🗑 Clear");
    check("Clear keeps favorites, pinned items and snippets", (await page.$$("text=/Favorites and pinned items .* your snippets are kept/")).length === 1);
    await page.keyboard.press("Escape");

    // Dragging the popup: only its chrome moves the window.
    const startsDrag = (selector, init = {}) => page.evaluate(([selector, init]) => {
      const el = document.querySelector(selector);
      if (!el) return "missing " + selector;
      const before = window.__calls.filter((c) => c.cmd === "plugin:window|start_dragging").length;
      const r = el.getBoundingClientRect();
      el.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true, button: 0, detail: 1, clientX: r.left + 2, clientY: r.top + 2, ...init }));
      return window.__calls.filter((c) => c.cmd === "plugin:window|start_dragging").length > before;
    }, [selector, init]);
    await page.waitForTimeout(50);
    const dragCases = [
      ["#sidebar", true], ["#search-row", true], ["#preview-header", true], ["#toolbar .hint-key", true],
      [".sidebar-item", false], ["#search-input", false], [".clip-item", false], ["#sidebar select", false], ["#sidebar label", false],
    ];
    for (const [selector, expected] of dragCases) {
      const got = await startsDrag(selector);
      check(`pressing ${selector} ${expected ? "drags" : "does not drag"} the popup`, got === expected, String(got));
    }
    check("modifier presses never drag the popup", (await startsDrag("#sidebar", { shiftKey: true })) === false);
    check("double-clicks on the chrome don't drag or zoom", (await startsDrag("#sidebar", { detail: 2 })) === false);
    await page.locator(".clip-item").nth(0).click();
    check("items still select after drag attempts", (await selectedIds()).length === 1);

    // Reopening the popup starts on the first item, not where it was left.
    await page.locator(".clip-item").nth(2).click();
    await page.evaluate(() => window.__emit("clipflow://history-opened"));
    await page.waitForTimeout(150);
    const firstTitle = await page.locator(".clip-item").first().locator(".clip-title").textContent();
    const afterOpen = await selectedIds();
    check("opening the popup selects the first item", afterOpen.length === 1 && afterOpen[0] === firstTitle, JSON.stringify(afterOpen));

    // Preview text stays copyable on purpose.
    await page.locator(".clip-item", { hasText: "https://" }).click();
    await page.locator(".preview-link-url").dblclick();
    check("intentional content (preview text) remains selectable", (await sel()).length > 0, JSON.stringify(await sel()));
    await page.close();
  }

  // ---------------- Annotation editor ----------------
  {
    const page = await browser.newPage({ viewport: { width: 1200, height: 800 }, deviceScaleFactor: 2 });
    page.on("pageerror", (e) => errors.push("editor: " + e.message));
    await page.addInitScript(mockTauri, "editor");
    await page.goto(url);
    await page.waitForFunction(() => document.getElementById("editor-canvas")?.width === 800);
    const canvas = page.locator("#editor-canvas");
    const toScreen = async (x, y) => {
      const r = await canvas.boundingBox();
      const w = await page.evaluate(() => document.getElementById("editor-canvas").width);
      const s = r.width / w;
      return { x: r.x + x * s, y: r.y + y * s };
    };
    const pixel = (x, y) => page.evaluate(([x, y]) => Array.from(document.getElementById("editor-canvas").getContext("2d").getImageData(x, y, 1, 1).data), [x, y]);
    const isRed = (p) => p[0] > 200 && p[1] < 90 && p[2] < 90;
    const drag = async (from, to) => {
      const f = await toScreen(...from), t = await toScreen(...to);
      await page.mouse.move(f.x, f.y);
      await page.mouse.down();
      await page.mouse.move(t.x, t.y, { steps: 8 });
      await page.mouse.up();
      await page.waitForTimeout(80);
    };

    // Draw a rectangle.
    await page.click('button[title="Rectangle"]');
    await drag([200, 150], [400, 300]);
    check("rectangle is drawn", isRed(await pixel(300, 150)) && isRed(await pixel(200, 225)));
    check("new rectangle is selected and its properties shown", (await page.$$("text=Selected rect")).length === 1);

    // Resize from the top-right corner handle (c3 = x2,y1).
    await drag([400, 150], [500, 100]);
    check("top-right corner handle resizes the rectangle", isRed(await pixel(450, 100)) && isRed(await pixel(500, 200)) && !isRed(await pixel(400, 200)));

    // Thickness change applies only to the selected rectangle, and the tool default is untouched.
    const slider = page.locator('#editor-sidebar input[type="range"]');
    await slider.fill("20");
    await page.waitForTimeout(80);
    check("thickness applies to the selected object", isRed(await pixel(300, 108)));
    await page.keyboard.press("Escape");
    await page.waitForTimeout(80);
    const label = await page.locator("#editor-sidebar label", { hasText: "Thickness" }).textContent();
    check("deselecting shows tool defaults, not the object's values", label.includes("4px"), label);

    // Clicking empty space with Select deselects.
    await page.click('button[title="Select / move"]');
    await drag([300, 200], [300, 200]);
    check("clicking a box's inside (unselected) doesn't grab it", (await page.$$("text=Selected rect")).length === 0);
    await drag([300, 100], [300, 100]);
    check("clicking a shape's edge selects it", (await page.$$("text=Selected rect")).length === 1);
    await drag([700, 450], [700, 450]);
    check("clicking empty space deselects", (await page.$$("text=Selected rect")).length === 0);

    // Arrow with endpoint handles.
    await page.click('button[title="Arrow"]');
    await drag([100, 400], [300, 450]);
    await drag([300, 450], [350, 350]); // drag the end handle
    check("arrow endpoint handle moves the arrow tip", isRed(await pixel(225, 375)) && !isRed(await pixel(296, 449)), JSON.stringify([await pixel(225, 375), await pixel(296, 449)]));

    // Crop, then undo restores the original canvas size.
    await page.click('button[title="Crop"]');
    await drag([50, 50], [450, 350]);
    const cropped = await page.evaluate(() => document.getElementById("editor-canvas").width);
    await page.keyboard.press("Meta+z");
    await page.waitForFunction(() => document.getElementById("editor-canvas").width === 800, null, { timeout: 2000 }).catch(() => {});
    const restored = await page.evaluate(() => document.getElementById("editor-canvas").width);
    check("undoing a crop restores the original size", cropped === 400 && restored === 800, `cropped=${cropped} restored=${restored}`);

    // Zoom: 100% = one image pixel per device pixel; Fit returns.
    await page.click("#editor-zoom >> text=100%");
    await page.waitForTimeout(50);
    const cssW = (await canvas.boundingBox()).width;
    check("100% zoom shows actual pixels (HiDPI aware)", Math.abs(cssW - 400) < 1, `css width ${cssW}`);
    await page.keyboard.press("Meta+=");
    await page.waitForTimeout(50);
    check("Cmd+= zooms in", (await canvas.boundingBox()).width > cssW + 50);

    // Pointer accuracy while zoomed: draw a line and check pixels land at the right spot.
    await page.click('button[title="Line"]');
    await page.locator("#editor-canvas-wrap").evaluate((w) => { w.scrollLeft = 0; w.scrollTop = 0; });
    await drag([60, 60], [160, 60]);
    check("drawing stays pointer-accurate when zoomed", isRed(await pixel(110, 60)) && !isRed(await pixel(110, 75)), JSON.stringify(await pixel(110, 60)));
    await page.keyboard.press("Meta+0");

    // No browser text selection inside the editor.
    const s = await page.locator(".section-title").boundingBox();
    await page.mouse.move(s.x, s.y); await page.mouse.down(); await page.mouse.move(s.x + 150, s.y + 200, { steps: 5 }); await page.mouse.up();
    check("dragging over the editor sidebar selects no text", (await page.evaluate(() => window.getSelection().toString())) === "");
    await page.close();
  }

  // ---------------- Files, long text, image edit, history settings ----------------
  {
    const page = await browser.newPage({ viewport: { width: 860, height: 620 } });
    page.on("pageerror", (e) => errors.push("extras: " + e.message));
    await page.addInitScript(mockTauri, "history");
    await page.goto(url);
    await page.waitForSelector(".clip-item");
    const callsOf = (cmd) => page.evaluate((cmd) => window.__calls.filter((c) => c.cmd === cmd).map((c) => c.args), cmd);

    // A file entry: shown by name in Files, with explicit file actions.
    const fileRow = page.locator(".clip-item", { hasText: "Final Python.zip" });
    check("a file entry is listed by its name, not its stored paths", (await fileRow.count()) === 1 && !(await fileRow.textContent()).includes("["));
    check("file entries get a file-type icon", (await fileRow.locator(".clip-thumb").textContent()).includes("🗜️"));
    await fileRow.click();
    await page.waitForSelector(".file-card .file-row");
    for (const label of ["Paste File", "Copy Path", "Copy File Link", "Open"]) {
      check(`file entry offers "${label}"`, (await page.locator(".file-actions button", { hasText: new RegExp(`^${label}$`) }).count()) === 1);
    }
    check("file entry offers Reveal in Finder / Show in Explorer", (await page.locator(".file-actions button", { hasText: /Reveal in Finder|Show in Explorer/ }).count()) === 1);
    check("file entries have no plain Paste that could paste the name", (await page.locator(".preview-card button", { hasText: /^Paste$/ }).count()) === 0);
    await page.click(".file-actions >> text=Copy Path");
    await page.click(".file-actions >> text=Copy File Link");
    const copies = await callsOf("copy_file_paths");
    check("Copy Path and Copy File Link are separate actions", JSON.stringify(copies.map((c) => c.asUrl)) === "[false,true]", JSON.stringify(copies));
    await page.click(".file-actions >> text=Paste File");
    check("Paste File pastes the entry itself", (await callsOf("paste_item")).some((c) => c.id === "f1"));
    await page.click("#search-input");
    await page.keyboard.press("Escape");

    // Several files, one missing: clear warning, Paste File disabled.
    await page.locator(".clip-item", { hasText: "a.pdf, gone.xlsx" }).click();
    await page.waitForSelector(".file-warning");
    check("a missing file is flagged", (await page.locator(".file-row.missing").count()) === 1 && (await page.locator(".file-warning").textContent()).includes("1 of 2"));
    check("Paste File is disabled while a file is missing", await page.locator(".file-actions button.primary").isDisabled());

    // Long text: compact preview + full viewer with search.
    await page.locator(".clip-item", { hasText: "line 0 needle" }).click();
    check("long text gets a compact preview", (await page.locator(".preview-text.clamped").count()) === 1);
    check("long text shows its size", (await page.locator(".expand-row").textContent()).includes("400 lines"));
    await page.click("text=⤢ Expand");
    await page.waitForSelector(".text-viewer");
    check("the viewer gives the text the sidebar's width", await page.locator("#sidebar").isHidden());
    check("the history list stays visible while reading", await page.locator("#item-list").isVisible());
    await page.keyboard.press("Meta+f");
    await page.keyboard.type("needle");
    check("find in text counts matches", (await page.locator(".match-count").textContent()) === "1 / 8");
    await page.keyboard.press("Enter");
    check("Enter jumps to the next match (and does not paste)", (await page.locator(".match-count").textContent()) === "2 / 8" && (await callsOf("paste_item")).length === 1);
    check("matches are highlighted", (await page.locator(".text-viewer-body mark").count()) === 8 && (await page.locator(".text-viewer-body mark.current").count()) === 1);
    check("the viewer counts characters and lines", (await page.locator(".text-viewer-foot").textContent()).includes("400 lines"));
    await page.click(".text-viewer >> text=Edit");
    await page.locator("textarea.text-viewer-body").fill("edited text");
    await page.keyboard.press("Meta+s");
    await page.waitForTimeout(100);
    check("edited text is saved to the item", JSON.stringify(await callsOf("update_item_text")) === JSON.stringify([{ id: "t5", content: "edited text" }]));
    const hidesBefore = (await callsOf("hide_history")).length;
    await page.keyboard.press("Escape");
    await page.waitForTimeout(50);
    check("closing the viewer keeps the item selected", (await page.$$eval(".clip-item.selected .clip-title", (els) => els.map((e) => e.textContent))).join().startsWith("line 0 needle") && (await page.locator(".text-viewer").count()) === 0);
    check("Esc in the viewer doesn't hide the popup", (await callsOf("hide_history")).length === hidesBefore);

    // Image preview: hover reveals Edit, which opens the annotation editor.
    await page.locator(".clip-item", { hasText: "img1.png" }).click();
    await page.waitForSelector(".ocr-image img");
    const editBtn = page.locator(".image-edit-btn");
    check("the Edit button is hidden until hover", Number(await editBtn.evaluate((b) => getComputedStyle(b).opacity)) === 0);
    await page.hover(".ocr-image");
    await page.waitForTimeout(200);
    check("hovering the image reveals Edit", Number(await editBtn.evaluate((b) => getComputedStyle(b).opacity)) === 1);
    await editBtn.click();
    check("Edit opens the annotation editor for that image", JSON.stringify(await callsOf("start_annotation")) === JSON.stringify([{ ids: ["img1"] }]));

    // History settings: off by default, preview before deleting.
    await page.click("text=⚙ History Settings…");
    await page.waitForSelector(".history-settings");
    check("retention is off by default", (await page.locator('.history-settings select[aria-label="Keep history"]').inputValue()) === "0");
    await page.selectOption('.history-settings select[aria-label="Keep history"]', "7");
    await page.waitForTimeout(100);
    check("choosing a period previews what would be deleted", (await page.locator(".history-settings").textContent()).includes("Saving deletes 4 items right away"));
    check("favorites and pinned items are kept unless chosen", (await page.locator(".history-settings").textContent()).includes("Favorites and pinned items are kept"));
    await page.click(".history-settings >> text=Save and delete 4");
    await page.waitForTimeout(100);
    check("saving applies the chosen retention", JSON.stringify(await callsOf("history_retention_set")) === JSON.stringify([{ retention: { days: 7, include_favorites: false, include_pinned: false } }]));
    await page.click(".history-settings >> text=Pause 15 min");
    check("recording can be paused for a while", JSON.stringify(await callsOf("pause_monitoring")) === JSON.stringify([{ minutes: 15 }]) && (await page.locator(".history-settings").textContent()).includes("Paused until"));
    await page.click(".history-settings >> text=Clear All History…");
    check("Clear All History asks first", (await page.locator(".history-settings").textContent()).includes("including favorites and pinned"));
    await page.keyboard.press("Escape");
    await page.keyboard.press("Escape");
    check("Esc closes the settings dialog", (await page.locator(".history-settings").count()) === 0 && !(await callsOf("clear_history")).length);
    await page.close();
  }

  // ---------------- Image snippets ----------------
  {
    const page = await browser.newPage({ viewport: { width: 860, height: 620 } });
    page.on("pageerror", (e) => errors.push("snippets: " + e.message));
    await page.addInitScript(mockTauri, "history");
    await page.goto(url);
    await page.waitForSelector(".clip-item");
    const saves = () => page.evaluate(() => window.__calls.filter((c) => c.cmd === "snippet_save").map((c) => c.args.input));

    // From history: right-click an image, Create Snippet.
    await page.locator(".clip-item").nth(2).click({ button: "right" });
    await page.click("text=Create Snippet…");
    await page.waitForSelector(".snippet-form");
    check("an image from history opens an image snippet", await page.locator('.snippet-type [aria-checked="true"]').textContent() === "Image");
    check("the chosen history image is pre-selected", (await page.$$(".snippet-image-choice.on")).length === 1);
    await page.fill(".snippet-keyword-field input", "shot");
    await page.click("text=Save");
    await page.waitForTimeout(100);
    let last = (await saves()).pop();
    check("saving sends the history image, not text", last?.content_type === "image" && last?.image_item === "img1", JSON.stringify(last));

    // The list shows image snippets with a thumbnail and no file name.
    const logoRow = page.locator(".snippet-item", { hasText: ";logo" });
    check("image snippet rows show a thumbnail", (await logoRow.locator(".clip-thumb img").count()) === 1);
    check("image snippet rows say Image, not the file name", (await logoRow.textContent()).includes("Image") && !(await logoRow.textContent()).includes("snip-abc"));
    await logoRow.click();
    check("the detail view shows the image", (await page.$$("#preview-body .snippet-image-preview")).length === 1);

    // Editing keeps the current image unless a new one is picked.
    await page.click("#preview-body >> text=Edit");
    await page.fill(".snippet-form input >> nth=1", "Company logo");
    await page.click("text=Save");
    await page.waitForTimeout(100);
    last = (await saves()).pop();
    check("editing an image snippet keeps its image", last?.id === "s2" && last?.content_type === "image" && last?.image_item === null, JSON.stringify(last));

    // New snippet: switch to Image and pick from history.
    await page.click("text=+ New");
    await page.click('.snippet-type >> text=Image');
    await page.waitForTimeout(100);
    check("the image picker lists history images", (await page.$$(".snippet-image-choice")).length === 3);
    await page.fill(".snippet-keyword-field input", "pic");
    await page.locator(".snippet-image-choice").nth(2).click();
    check("picking an image previews it", (await page.$$(".snippet-image-field .snippet-image-preview")).length === 1);
    await page.click("text=Save");
    await page.waitForTimeout(100);
    last = (await saves()).pop();
    check("the picked image is saved", last?.image_item === "img3" && last?.keyword === "pic", JSON.stringify(last));

    // Text snippets choose what follows the text; exact is the default.
    await page.click("text=+ New");
    check("new snippets insert the exact text by default", (await page.locator(".snippet-form select").first().inputValue()) === "exact");
    await page.fill(".snippet-keyword-field input", "hello");
    await page.fill(".snippet-form textarea", "Hello World");
    await page.selectOption(".snippet-form select", "space");
    await page.click("text=Save");
    await page.waitForTimeout(100);
    last = (await saves()).pop();
    check("the whitespace setting is saved, content untouched", last?.suffix === "space" && last?.content === "Hello World", JSON.stringify(last));

    // Searching snippet content never matches an image's file name.
    await page.fill("#search-input", "snip-abc");
    check("search ignores image file names", (await page.$$(".snippet-item")).length === 0);
    await page.close();
  }

  check("no JavaScript errors", errors.length === 0, errors.join(" | "));
  await browser.close();
  server.close();
  const failed = results.filter((r) => !r.ok).length;
  console.log(`\n${results.length - failed}/${results.length} passed`);
  process.exit(failed ? 1 : 0);
})();
