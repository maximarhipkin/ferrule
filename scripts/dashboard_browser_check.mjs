#!/usr/bin/env node
// The control room's browser check (docs/m37-control-room.md §6): drives
// a real Chromium over the DevTools protocol against `ferrule gateway` on
// a temp config and data dir, with the starter suite's mock model, a fake
// Telegram Bot API (for the /dashboard link) and a local MCP server that
// takes a key. It dismisses a notice, picks a fallback model, connects a
// service with a key, runs a console command and chats once, failing on
// any script error on the page. `--shots DIR` also saves the screenshots
// at 390 and 1280 px. Node 22+ (its built-in WebSocket), no packages; it
// never touches the owner's own config or data dir, and needs no key.
//
// `--shots-all DIR` (M47) saves every section at 390 and 1280 px, in the
// light and the dark theme, as JPEGs; `--measure` prints what a cold load
// of Home costs (bytes on the wire per file, first contentful paint, the
// API calls before it settles) as one JSON line.
//
//     node scripts/dashboard_browser_check.mjs --bin target/debug/ferrule [--chromium PATH] [--shots docs/assets/m37]
//         [--shots-all docs/assets/m47/after] [--shots-format webp] [--shots-quality 45] [--measure]
//
// `--shots-format webp` (M49) saves WebP instead of JPEG: a phone shot is
// scaled to 1.5x, a desktop one stays 1x, and a Hebrew pair (Home and
// Settings, light) is added.

import { spawn, spawnSync } from "node:child_process";
import { createServer, get as httpGet } from "node:http";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const OWNER = 42;
// Made up for the test: the local MCP server accepts only this.
const KEY = "browser-check-key-0000";
const PROXY_VARS = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"];

// NODE_USE_ENV_PROXY sends even loopback through a proxy unless NO_PROXY
// says otherwise, and it's read at startup: go again with it set.
if (process.env.NODE_USE_ENV_PROXY && !/127\.0\.0\.1/.test(process.env.NO_PROXY || process.env.no_proxy || "")) {
  const r = spawnSync(process.execPath, process.argv.slice(1), { stdio: "inherit", env: { ...process.env, NO_PROXY: "localhost,127.0.0.1,::1", no_proxy: "localhost,127.0.0.1,::1" } });
  process.exit(r.status ?? 1);
}

const argv = process.argv.slice(2);
const opt = (name) => { const i = argv.indexOf(name); return i >= 0 ? argv[i + 1] : null; };
const BIN = opt("--bin") && resolve(opt("--bin"));
const SHOTS = opt("--shots") && resolve(opt("--shots"));
const SHOTS_ALL = opt("--shots-all") && resolve(opt("--shots-all"));
const SHOTS_FORMAT = opt("--shots-format") || "jpeg";
const SHOTS_QUALITY = Number(opt("--shots-quality") || 70);
if (!["jpeg", "webp"].includes(SHOTS_FORMAT)) throw new Error("--shots-format is jpeg or webp");
const MEASURE = argv.includes("--measure");
const CHROMIUM = opt("--chromium") || ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable"].find((c) => spawnSync(c, ["--version"]).status === 0);
if (!BIN) { console.error("usage: dashboard_browser_check.mjs --bin <ferrule> [--chromium PATH] [--shots DIR]"); process.exit(2); }
if (!CHROMIUM) { console.error("no Chromium found: pass --chromium"); process.exit(2); }

const results = [];
function step(name, ok, detail) {
  results.push([name, ok]);
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? " — " + detail : ""}`);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(what, fn, secs = 20) {
  const end = Date.now() + secs * 1000;
  let last;
  while (Date.now() < end) {
    try { last = await fn(); if (last) return last; } catch (e) { last = e; }
    await sleep(150);
  }
  throw new Error(`timed out: ${what}${last instanceof Error ? " (" + last.message + ")" : ""}`);
}
function listen(handler) {
  return new Promise((ok) => { const s = createServer(handler); s.listen(0, "127.0.0.1", () => ok(s)); });
}
const bodyOf = (req) => new Promise((ok) => { const b = []; req.on("data", (c) => b.push(c)); req.on("end", () => ok(Buffer.concat(b).toString())); });
const json = (res, v, code = 200) => { const d = JSON.stringify(v); res.writeHead(code, { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(d) }); res.end(d); };

const tmp = mkdtempSync(join(tmpdir(), "ferrule-browser-check-"));
for (const d of ["work", "data", "home", "chrome"]) mkdirSync(join(tmp, d));
const procs = [];
const servers = [];
const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !PROXY_VARS.includes(k)));
env.NO_PROXY = "localhost,127.0.0.1,::1";

async function main() {
  // The mock model.
  const mock = spawn(process.platform === "win32" ? "python" : "python3", [join(ROOT, "evals/starter/mock/model.py"), "--port", "0"], { env, stdio: ["ignore", "pipe", "ignore"] });
  procs.push(mock);
  const modelUrl = await until("the mock model", () => new Promise((ok) => mock.stdout.once("data", (d) => ok((String(d).match(/(http:\/\/127\.0\.0\.1:\d+\/v1)/) || [])[1]))));

  // The fake Telegram: the owner asks for /dashboard, the link comes back.
  const updates = [];
  const sent = [];
  let nextUpdate = 0;
  const tg = await listen(async (req, res) => {
    const raw = await bodyOf(req);
    if (req.url.includes("/getUpdates")) {
      await sleep(updates.length ? 0 : 400);
      return json(res, { ok: true, result: updates.splice(0) });
    }
    try { sent.push(JSON.parse(raw || "{}")); } catch (_) { sent.push({}); }
    json(res, { ok: true, result: { message_id: 1000 + sent.length } });
  });
  servers.push(tg);

  // A local MCP server that answers only with the key.
  const mcp = await listen(async (req, res) => {
    const raw = await bodyOf(req);
    if (req.headers.authorization !== "Bearer " + KEY) return json(res, { error: "unauthorized" }, 401);
    if (req.method !== "POST") { res.writeHead(405); return res.end(); }
    const m = JSON.parse(raw || "{}");
    if (m.id === undefined) { res.writeHead(202); return res.end(); }
    const result = m.method === "initialize"
      ? { protocolVersion: "2025-06-18", capabilities: { tools: {} }, serverInfo: { name: "browser-check", version: "1" } }
      : m.method === "tools/list"
        ? { tools: [{ name: "lookup", description: "Look something up", inputSchema: { type: "object", properties: {} }, annotations: { readOnlyHint: true } }] }
        : {};
    json(res, { jsonrpc: "2.0", id: m.id, result });
  });
  servers.push(mcp);
  const port = (s) => s.address().port;

  writeFileSync(join(tmp, "ferrule.toml"), `default_provider = "mock"

[providers.mock]
base_url = "${modelUrl}"
api_key_env = "FERRULE_CHECK_KEY"
model = "mock"

[providers.spare]
base_url = "${modelUrl}"
api_key_env = "FERRULE_CHECK_KEY"
model = "mock-spare"

[gateway]
telegram_token_env = "FERRULE_CHECK_TG"
telegram_base_url = "http://127.0.0.1:${port(tg)}"
telegram_allowed_chats = [${OWNER}]

[trust]
owner_chat = ${OWNER}

[dashboard]
remote = "off"

[connections]
cloudflared = "off"

[[connections.custom]]
name = "localcheck"
title = "Local check"
url = "http://127.0.0.1:${port(mcp)}/mcp"
auth = "api_key"
header = "Authorization"
header_value = "Bearer {key}"
covers = "A local MCP server, for the browser check."
guide = ["Paste the key.", "Press Check and connect."]

[skills]
enabled = false

[sandbox]
mode = "off"

[egress]
private_allow = ["127.0.0.1:${port(mcp)}"]
`);
  Object.assign(env, {
    FERRULE_CONFIG: join(tmp, "ferrule.toml"),
    FERRULE_DATA_DIR: join(tmp, "data"),
    FERRULE_CHECK_KEY: "not-a-key",
    FERRULE_CHECK_TG: "CHECKTOKEN",
  });
  for (const v of ["HOME", "USERPROFILE", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "APPDATA", "LOCALAPPDATA"]) env[v] = join(tmp, "home");

  const gw = spawn(BIN, ["gateway"], { cwd: join(tmp, "work"), env, stdio: ["ignore", "ignore", "ignore"] });
  procs.push(gw);
  const marker = join(tmp, "data/gateway/dashboard.json");
  await until("the gateway's dashboard", () => existsSync(marker) && readFileSync(marker, "utf8").length > 0, 60);
  updates.push({ update_id: ++nextUpdate, message: { message_id: nextUpdate, chat: { id: OWNER, type: "private" }, from: { id: OWNER, username: "owner" }, text: "/dashboard", date: Math.floor(Date.now() / 1000) } });
  const link = await until("the /dashboard link", () => {
    const m = sent.map((s) => String(s.text || "").match(/Dashboard: (\S+)/)).find(Boolean);
    return m && m[1];
  }, 30);
  step("gateway and link", true, link.split("#")[0]);

  // Chromium, headless, on its own profile.
  const chrome = spawn(CHROMIUM, ["--headless=new", "--remote-debugging-port=0", "--user-data-dir=" + join(tmp, "chrome"),
    "--no-first-run", "--no-default-browser-check", "--disable-gpu", "--no-sandbox", "--no-proxy-server", "--hide-scrollbars", "about:blank"], { stdio: "ignore" });
  procs.push(chrome);
  const devtools = join(tmp, "chrome", "DevToolsActivePort");
  const [cdpPort] = (await until("Chromium's DevTools port", () => existsSync(devtools) && readFileSync(devtools, "utf8").trim().includes("\n") && readFileSync(devtools, "utf8").split("\n"), 30));
  const list = () => new Promise((ok, no) => httpGet(`http://127.0.0.1:${cdpPort}/json/list`, (r) => bodyOf(r).then((b) => ok(JSON.parse(b)))).on("error", no));
  const page = await until("Chromium's page", async () => (await list()).find((t) => t.type === "page"), 30);
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((ok, no) => { ws.onopen = ok; ws.onerror = no; });
  let id = 0;
  const waiting = new Map();
  const errors = [];
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && waiting.has(msg.id)) { const [ok, no] = waiting.get(msg.id); waiting.delete(msg.id); msg.error ? no(new Error(msg.error.message)) : ok(msg.result); }
    if (msg.method === "Runtime.exceptionThrown") errors.push(msg.params.exceptionDetails.exception?.description || msg.params.exceptionDetails.text);
    if (msg.method === "Log.entryAdded" && msg.params.entry.level === "error" && !/status of 4\d\d/.test(msg.params.entry.text)) errors.push(msg.params.entry.text);
  };
  const cdp = (method, params = {}) => new Promise((ok, no) => { const n = ++id; waiting.set(n, [ok, no]); ws.send(JSON.stringify({ id: n, method, params })); });
  const js = async (expr) => {
    const r = await cdp("Runtime.evaluate", { expression: `(async () => { ${expr} })()`, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
    return r.result.value;
  };
  const size = (w, h) => cdp("Emulation.setDeviceMetricsOverride", { width: w, height: h, deviceScaleFactor: w < 600 ? 2 : 1, mobile: w < 600 });
  await cdp("Runtime.enable");
  await cdp("Log.enable");
  await cdp("Page.enable");
  await size(1280, 900);
  await cdp("Page.navigate", { url: link });
  await until("the page to sign in", () => js(`return !document.getElementById("rail").hidden && !!document.querySelector("#main .card, #main .alert")`), 20);
  step("sign in", true, "the rail and Home are up");

  // In-page helpers: click a button by its text inside a selector.
  const click = (sel, label) => js(`
    const b = [...document.querySelectorAll(${JSON.stringify(sel)} + " button")].find((b) => b.textContent.trim() === ${JSON.stringify(label)} && !b.disabled);
    if (!b) return false; b.click(); return true;`);
  const get = (path) => js(`return await window.ferrule.api(${JSON.stringify(path)})`);
  const run = async (name, fn) => { try { step(name, true, await fn()); } catch (e) { step(name, false, e.message); } };

  // M47: the first-run checklist on Home follows the bot, opens a step in
  // place, and words like "fallback" explain themselves.
  await run("first-run checklist", async () => {
    await until("the checklist", () => js(`return document.querySelectorAll("#main .steps .step").length === 3`));
    const bar = await js(`const p = document.querySelector('#main [role="progressbar"]'); return p && [p.getAttribute("aria-valuenow"), p.getAttribute("aria-valuemax")]`);
    if (!bar || bar[1] !== "3" || Number(bar[0]) < 1) throw new Error("the progress bar says " + JSON.stringify(bar));
    if (SHOTS) {
      mkdirSync(SHOTS, { recursive: true });
      for (const [w, h] of [[390, 844], [1280, 860]]) {
        await size(w, h);
        await sleep(600);
        const r = await cdp("Page.captureScreenshot", { format: "png" });
        writeFileSync(join(SHOTS, `home-first-run-${w}.png`), Buffer.from(r.data, "base64"));
      }
      await size(1280, 900);
    }
    if (!(await click("#main .steps", "Say hello"))) throw new Error("no Say hello button");
    await until("the chat", () => js(`return !!document.querySelector(".composer textarea")`));
    await until("the answer to the greeting", () => js(`return document.querySelectorAll(".bubble:not(.you)").length > 0`), 45);
    await js(`window.ferrule.show("health")`);
    await until("the checklist to finish and go", () => js(`return !document.querySelector("#main .steps")`), 20);
    return "3 steps, " + bar[0] + " of 3 at first, gone once the bot answered";
  });

  await run("a step opens in place", async () => {
    // Pretend the model step is open, to see the provider form come up in it.
    await js(`
      const h = window.ferrule.sections.health;
      const steps = [{ id: "model", done: false, label: "Give your bot a brain", detail: "x" }, { id: "hello", done: false, label: "Say hello", detail: "y" }];
      const real = h.load;
      h.load = async function (health) { this.last = { h: health, approvals: [], setup: { done: false, steps } }; this.draw(); };
      window.__realLoad = real;
      await h.load((await window.ferrule.api("/api/health")));`);
    if (!(await click("#main .steps", "Set up"))) throw new Error("no Set up button");
    await until("a provider form in the step", () => js(`return document.querySelectorAll("#main .step-panel .card").length > 0`), 15).catch(async (e) => {
      throw new Error(e.message + "; the panel: " + (await js(`const p = document.querySelector("#main .step-panel"); return p ? p.outerHTML.slice(0, 300) : "none"`)));
    });
    const n = await js(`return document.querySelectorAll("#main .step-panel .card").length`);
    if (!(await click("#main .steps", "Close"))) throw new Error("no Close button");
    await js(`window.ferrule.sections.health.load = window.__realLoad; window.ferrule.show("health")`);
    return n + " provider cards drawn inside the step";
  });

  await run("a glossary word explains itself", async () => {
    await until("Details", () => js(`return [...document.querySelectorAll("#main details summary")].some((s) => /Details/.test(s.textContent))`));
    await js(`[...document.querySelectorAll("#main details summary")].find((s) => /Details/.test(s.textContent)).click()`);
    await until("the words", () => js(`return !!document.querySelector("#main button.tip")`));
    await js(`document.querySelector("#main button.tip").click()`);
    const shown = await js(`const b = document.querySelector("#main button.tip"); const d = document.getElementById(b.getAttribute("aria-controls")); return !d.hidden && d.textContent.length > 20 && b.getAttribute("aria-expanded") === "true"`);
    if (!shown) throw new Error("the definition didn't open");
    return "a tip opens its sentence";
  });

  // M47 part 4: the shell. The palette opens from the keyboard, finds a
  // page by a few letters, and focus lands on the page; the phone bar has
  // five items and its More sheet is a real dialog.
  const key = async (k, code, mods = 0) => {
    await cdp("Input.dispatchKeyEvent", { type: "keyDown", key: k, code, modifiers: mods, windowsVirtualKeyCode: { Enter: 13, Escape: 27 }[k] || k.toUpperCase().charCodeAt(0) });
    await cdp("Input.dispatchKeyEvent", { type: "keyUp", key: k, code, modifiers: mods });
  };
  await run("the palette finds a page", async () => {
    await js(`window.ferrule.show("health"); document.activeElement && document.activeElement.blur()`);
    await key("k", "KeyK", 2);
    await until("the palette", () => js(`return !!document.querySelector("dialog.pal[open] input[role=combobox]")`), 5);
    await cdp("Input.insertText", { text: "tas" });
    const first = await js(`const o = document.querySelector("dialog.pal li[role=option]"); const i = document.querySelector("dialog.pal input"); return o && [o.textContent, i.getAttribute("aria-activedescendant") === o.id, i.getAttribute("aria-expanded")]`);
    if (!first || !/Tasks/.test(first[0]) || !first[1]) throw new Error("the first result is " + JSON.stringify(first));
    await key("Enter", "Enter");
    await until("Tasks", () => js(`return location.hash === "#tasks" && !document.querySelector("dialog.pal")`), 8);
    const focus = await js(`return document.activeElement && document.activeElement.id`);
    if (focus !== "main") throw new Error("focus is on " + focus + ", not #main");
    await key("/", "Slash");
    const again = await js(`return !!document.querySelector("dialog.pal[open]")`);
    if (!again) throw new Error("/ did not open the palette");
    await key("Escape", "Escape");
    await until("it to close", () => js(`return !document.querySelector("dialog.pal")`), 5);
    return "Ctrl-K, tas, Enter → #tasks with focus in #main; / opens it and Esc closes it";
  });

  await run("the phone bar and its More sheet", async () => {
    await size(390, 844);
    await js(`window.ferrule.show("health")`);
    const n = await js(`return [...document.querySelectorAll("#tabs a")].map((a) => a.textContent.trim())`);
    if (n.length !== 5) throw new Error("the bar has " + n.join(","));
    const cur = await js(`return document.querySelectorAll('#tabs a[aria-current="page"]').length`);
    if (cur !== 1) throw new Error("aria-current on " + cur + " items");
    await js(`document.querySelector('#tabs a[href="#more"]').click()`);
    await until("the sheet", () => js(`return !document.getElementById("sheet").hidden && document.getElementById("sheet").contains(document.activeElement)`), 5).catch(async (e) => {
      throw new Error(e.message + ": " + (await js(`const sh = document.getElementById("sheet"); return JSON.stringify([sh.hidden, document.activeElement.outerHTML.slice(0, 80), sh.querySelectorAll("a").length])`)));
    });
    const groups = await js(`return [...document.querySelectorAll("#sheet .group")].map((g) => g.textContent)`);
    if (groups.length < 2) throw new Error("groups in the sheet: " + groups);
    await key("Escape", "Escape");
    await until("the sheet to close", () => js(`return document.getElementById("sheet").hidden`), 5);
    const wide = await js(`return document.documentElement.scrollWidth - document.documentElement.clientWidth`);
    await size(1280, 900);
    if (wide > 1) throw new Error("the page scrolls sideways by " + wide + " px");
    return n.join(" · ") + "; the sheet opens, closes on Esc; groups " + groups.join("/");
  });

  // A task card built from a made-up task (the fixture has none): the
  // schedule reads in words, the switch is a real switch, the raw line and
  // the destructive buttons sit behind Advanced.
  await run("a task card reads in words", async () => {
    await js(`window.ferrule.show("tasks")`);
    await until("Tasks", () => js(`return location.hash === "#tasks" && !!document.querySelector("#main .sec-head")`), 8);
    const r = await js(`
      const t = { id: "t1", name: "Morning summary", kind: "cron", schedule: "0 9 * * 1-5", timezone: "Asia/Jerusalem", enabled: true, builtin: false,
        next_run_at: Math.floor(Date.now() / 1000) + 3600, destination: "telegram", model: null,
        runs: [{ status: "failed", detail: "the model was down", started_at: Math.floor(Date.now() / 1000) - 60 }] };
      const card = window.ferrule.sections.tasks.item(t);
      document.getElementById("main").append(card);
      const sw = card.querySelector('button[role="switch"]');
      const adv = card.querySelector("details.advanced");
      return { text: card.textContent, on: sw && sw.getAttribute("aria-checked"), label: sw && sw.getAttribute("aria-label"),
        advOpen: adv && adv.open, del: [...(adv ? adv.querySelectorAll("button") : [])].some((b) => b.textContent === "Delete"),
        deleteOutside: [...card.querySelectorAll("button")].some((b) => b.textContent === "Delete" && !adv.contains(b)) };`);
    if (!/Weekdays at 09:00/.test(r.text)) throw new Error("no schedule in words: " + r.text);
    if (r.on !== "true" || !/Pause/.test(r.label)) throw new Error("the switch: " + JSON.stringify(r));
    if (r.advOpen || !r.del || r.deleteOutside) throw new Error("Advanced fold wrong: " + JSON.stringify(r));
    if (!/the model was down/.test(r.text)) throw new Error("a failed run isn't said");
    await js(`window.ferrule.show("health")`);
    return "Weekdays at 09:00, an on/off switch, Delete only under Advanced";
  });

  await run("dismiss a notice", async () => {
    const before = await get("/api/health");
    const target = (before.problems || []).find((p) => p.id && p.closable);
    if (!target) throw new Error("no closable notice: " + JSON.stringify(before.problems));
    await until("its × on the page", () => js(`return !!document.querySelector('[data-notice="${target.id}"] .x')`));
    await js(`document.querySelector('[data-notice="${target.id}"] .x').click()`);
    await until("it to leave the page", () => js(`return !document.querySelector('[data-notice="${target.id}"]')`));
    const after = await get("/api/health");
    if (!(after.hidden || []).some((p) => p.id === target.id)) throw new Error("not in the hidden list");
    await until("the hidden list on Home", () => js(`return [...document.querySelectorAll("details summary")].some((s) => /hidden/.test(s.textContent))`));
    return target.id + " hidden, listed with Show again";
  });

  await run("pick a fallback", async () => {
    await js(`window.ferrule.show("models")`);
    await until("the fallback picker", () => click("#main", "Add one"));
    await until("a new fallback row", () => js(`return !!document.querySelector("#main .fallback select")`));
    const ref = await js(`
      const s = document.querySelector("#main .fallback select");
      const o = [...s.options].find((o) => /spare/.test(o.value) && !o.disabled);
      if (!o) return null;
      s.value = o.value; s.dispatchEvent(new Event("change", { bubbles: true })); return o.value;`);
    if (!ref) throw new Error("the spare model isn't offered: " + JSON.stringify((await get("/api/models/choices")).models.map((m) => [m.reference, m.ready])));
    if (!(await click("#main", "Save the fallback list"))) throw new Error("no Save button");
    await until("the saved list", async () => ((await get("/api/models/choices")).fallback || []).includes(ref));
    return ref + " is the fallback";
  });

  await run("connect with a key", async () => {
    await js(`window.ferrule.show("connections", { tile: "localcheck" })`);
    await until("its tile, open", () => js(`return !!document.querySelector('#tile-localcheck details[open] input[name="key"]')`)).catch(async (e) => {
      throw new Error(e.message + "; tiles: " + JSON.stringify((await get("/api/connections")).tiles.map((t) => [t.tile, t.options.map((o) => o.name + ":" + o.auth + ":" + (o.fields || []).map((f) => f.name))])).slice(0, 600));
    });
    await js(`const i = document.querySelector('#tile-localcheck input[name="key"]'); i.focus(); i.value = ${JSON.stringify(KEY)}; i.dispatchEvent(new Event("input", { bubbles: true }));`);
    if (!(await click("#tile-localcheck", "Check and connect"))) throw new Error("no Check and connect button");
    await sleep(1500);
    const said = await js(`return document.getElementById("toast").innerText`);
    await until("the connection", async () => (await get("/api/connections")).connections.some((c) => c.name === "localcheck"), 30).catch(async (e) => {
      throw new Error(e.message + "; the page said: " + said);
    });
    const leaks = await js(`
      const typed = [...document.querySelectorAll("input")].some((i) => i.value === ${JSON.stringify(KEY)});
      const shown = document.body.innerText.includes(${JSON.stringify(KEY)});
      const said = JSON.stringify(await window.ferrule.api("/api/connections")).includes(${JSON.stringify(KEY)});
      return { typed, shown, said };`);
    if (leaks.typed || leaks.shown || leaks.said) throw new Error("the key is still visible: " + JSON.stringify(leaks));
    await until("its Test button", () => click("#main", "Test"));
    await until("the test's answer", () => js(`return !!document.querySelector("#main .said.ok")`), 30);
    return "connected; the key is gone from the page and the API; Test says it works";
  });

  await run("run a console command", async () => {
    await js(`window.ferrule.show("console")`);
    await until("the console", () => js(`return !!document.querySelector(".console-line input")`));
    await until("completion chips", () => js(`return document.querySelectorAll(".complete button").length > 3`));
    await js(`const i = document.querySelector(".console-line input"); i.value = "status"; i.dispatchEvent(new Event("input", { bubbles: true }));`);
    await click("#main", "Run");
    const head = await until("the command to finish", () => js(`const h = document.querySelector("#main pre.out"); return h && !h.hidden && /done|exit/.test(h.previousElementSibling.textContent) && h.previousElementSibling.textContent`), 30);
    const out = await js(`return document.querySelector("#main pre.out").textContent`);
    if (!out.trim()) throw new Error("no output: " + head);
    return head.trim();
  });

  await run("chat once", async () => {
    await js(`window.ferrule.show("chat")`);
    await until("the composer, enabled", () => js(`const t = document.querySelector(".composer textarea"); return t && !t.disabled`), 20);
    await js(`const t = document.querySelector(".composer textarea"); t.value = "hello from the browser check"; t.dispatchEvent(new Event("input", { bubbles: true }));`);
    await click(".composer", "Send");
    await until("an answer", () => js(`return document.querySelectorAll(".bubble:not(.you)").length > 0`), 45);
    return "the agent answered on the page";
  });

  await run("chat photo", async () => {
    // A picture drawn in the page, handed to the gallery input as a chosen file.
    await js(`window.ferrule.show("chat")`);
    await until("the composer", () => js(`return !!document.querySelector(".composer input[type=file]")`), 10);
    await js(`
      const c = document.createElement("canvas"); c.width = 320; c.height = 200;
      const g = c.getContext("2d"); g.fillStyle = "#2a7"; g.fillRect(0, 0, 320, 200); g.fillStyle = "#fff"; g.fillRect(40, 40, 120, 60);
      const blob = await new Promise((ok) => c.toBlob(ok, "image/png"));
      const dt = new DataTransfer(); dt.items.add(new File([blob], "check-photo.png", { type: "image/png" }));
      const input = document.querySelector(".composer input[type=file]");
      input.files = dt.files; input.dispatchEvent(new Event("change", { bubbles: true }));
    `);
    await until("the attached chip", () => js(`const a = document.querySelector(".composer .attached, .attached"); return a && !a.hidden && a.textContent.includes("check-photo")`), 10);
    await js(`const t = document.querySelector(".composer textarea"); t.value = "what colour is this?"; t.dispatchEvent(new Event("input", { bubbles: true }));`);
    await click(".composer", "Send");
    await until("the photo in the log", () => js(`return [...document.querySelectorAll(".photo-chip")].some((c) => c.textContent.includes(".jpg"))`), 30);
    await until("the chip cleared", () => js(`const a = document.querySelector(".attached"); return !a || a.hidden`), 10);
    await until("an answer to it", () => js(`return document.querySelectorAll(".bubble:not(.you)").length > 1`), 45);
    const img = await js(`return document.querySelectorAll("img").length`);
    if (img) throw new Error("the page drew an <img> for a photo: " + img);
    return "attached, sent as a JPEG, chip in the log, no <img>, answered";
  });

  await run("copy an answer", async () => {
    await js(`window.ferrule.show("chat")`);
    await until("a bot bubble with Copy", () => js(`return !!document.querySelector('.bubble:not(.you) button[aria-label="Copy the answer"]')`), 10);
    await js(`document.querySelector('.bubble:not(.you) button[aria-label="Copy the answer"]').click()`);
    return "the answer has a Copy button";
  });

  // M47 part 5c: a task from the picker, memory search, backup, the look
  // and the language.
  await run("a new task through the picker", async () => {
    await js(`window.ferrule.show("tasks")`);
    await until("New task", () => click("#main", "New task"), 10);
    await until("the dialog", () => js(`return !!document.querySelector("dialog.dlg.wide[open] input")`), 5);
    await js(`
      const d = document.querySelector("dialog.dlg.wide[open]");
      const set = (c, v) => { c.value = v; c.dispatchEvent(new Event("input", { bubbles: true })); };
      set(d.querySelector("input"), "Check task");
      set(d.querySelector("textarea"), "Say hello to the browser check.");`);
    const words = await until("the preview in words", () => js(`const p = document.querySelector("dialog.dlg.wide .preview .msg"); return p && /^Runs: weekdays at 09:00/.test(p.textContent) && p.textContent`), 10);
    const next = await js(`return document.querySelectorAll("dialog.dlg.wide .preview .next li").length`);
    if (next < 2) throw new Error("only " + next + " next runs are shown");
    await js(`document.querySelector('dialog.dlg.wide button[type="submit"]').click()`);
    await until("the dialog to close", () => js(`return !document.querySelector("dialog.dlg.wide")`), 10);
    await until("the task in the list", () => js(`return document.getElementById("main").textContent.includes("Check task")`), 15);
    const t = (await get("/api/tasks")).tasks.find((x) => x.name === "Check task");
    if (!t || t.schedule !== "0 9 * * 1-5") throw new Error("the task is " + JSON.stringify(t));
    return words.trim() + "; " + next + " next runs; added and listed";
  });

  await run("a bad schedule says why", async () => {
    await click("#main", "New task");
    await until("the dialog", () => js(`return !!document.querySelector("dialog.dlg.wide[open]")`), 5);
    await js(`
      const d = document.querySelector("dialog.dlg.wide[open]");
      const adv = d.querySelector("details.advanced"); adv.open = true;
      const raw = [...adv.querySelectorAll("input")].pop();
      raw.value = "61 25 * * *"; raw.dispatchEvent(new Event("input", { bubbles: true }));`);
    const said = await until("the reason", () => js(`const e = document.querySelector("dialog.dlg.wide .preview .err"); return e && e.textContent`), 10);
    await js(`document.querySelector("dialog.dlg.wide").close()`);
    return said.slice(0, 80);
  });

  await run("memory lists and searches", async () => {
    for (const text of ["Max likes his coffee black", "The office wifi is called Stanley"]) {
      const r = spawnSync(BIN, ["memory", "add", text], { env, cwd: join(tmp, "work") });
      if (r.status !== 0) throw new Error("memory add: " + r.stderr);
    }
    await js(`window.ferrule.show("memory")`);
    await until("both memories", () => js(`return document.querySelectorAll("#main .memory .item").length === 2`), 15);
    await js(`const i = document.querySelector('#main input[type="search"]'); i.focus(); i.value = "coffee"; i.dispatchEvent(new Event("input", { bubbles: true }));`);
    await until("one match", () => js(`const l = document.querySelectorAll("#main .memory .item"); return l.length === 1 && l[0].textContent.includes("coffee")`), 10);
    const before = (await get("/api/memory")).memories.length;
    if (before !== 2) throw new Error("the API lists " + before);
    return "2 listed, 'coffee' finds 1";
  });

  await run("settings: backup and dark", async () => {
    await js(`window.ferrule.show("settings")`);
    await until("Back up now", () => click("#main", "Back up now"), 10);
    await until("the backup in the list", () => js(`return !!document.querySelector('#main a[download]')`), 60);
    const href = await js(`return document.querySelector('#main a[download]').getAttribute("href")`);
    if (!/\/api\/backups\/download\?name=/.test(href)) throw new Error("the link is " + href);
    const got = await js(`const r = await fetch(${JSON.stringify("")} + document.querySelector('#main a[download]').getAttribute("href"), { credentials: "same-origin" }); const b = await r.arrayBuffer(); return [r.status, b.byteLength]`);
    if (got[0] !== 200 || got[1] < 100) throw new Error("the download answered " + got);
    await click("#main", "Dark");
    const th = await js(`return document.documentElement.getAttribute("data-theme")`);
    if (th !== "forge") throw new Error("the theme is " + th);
    await click("#main", "System");
    return "a backup made, listed and downloadable (" + got[1] + " bytes); Dark sets forge";
  });

  await run("Hebrew reads right to left", async () => {
    await js(`window.ferrule.show("settings")`);
    await js(`localStorage.setItem("ferrule-lang", "he"); location.reload()`);
    await sleep(500);
    await until("the page in Hebrew", () => js(`return document.documentElement.dir === "rtl" && !document.getElementById("rail").hidden && [...document.querySelectorAll("#rail a")].some((a) => a.textContent.includes("בית"))`), 20);
    const bad = await js(`
      const wide = document.documentElement.scrollWidth - document.documentElement.clientWidth;
      const cw = document.documentElement.clientWidth;
      const out = wide > 1 ? [...document.querySelectorAll("body *")].filter((e) => { const r = e.getBoundingClientRect(); return r.width > 0 && (r.left < -1 || r.right > cw + 1); }).slice(0, 6).map((e) => e.tagName + "." + e.className + "#" + e.id + "<" + (e.parentElement && e.parentElement.outerHTML.slice(0, 160)) + "> " + Math.round(e.getBoundingClientRect().left) + ".." + Math.round(e.getBoundingClientRect().right)) : [];
      return { wide, lang: document.documentElement.lang, out, nav: [...document.querySelectorAll("#rail a")].map((a) => a.textContent.trim()).join("|") };`);
    if (bad.lang !== "he" || bad.wide > 1) throw new Error(JSON.stringify(bad));
    if (SHOTS) {
      await size(390, 844); await js(`window.ferrule.show("tasks")`); await sleep(800);
      const r = await cdp("Page.captureScreenshot", { format: "png" });
      writeFileSync(join(SHOTS, "tasks-he-390.png"), Buffer.from(r.data, "base64"));
      await size(1280, 900);
    }
    await js(`localStorage.removeItem("ferrule-lang"); location.reload()`);
    await sleep(500);
    await until("the page in English again", () => js(`return document.documentElement.dir === "ltr" && !document.getElementById("rail").hidden`), 20);
    return "dir=rtl, nav in Hebrew, no sideways scroll; back to English";
  });

  if (SHOTS) {
    mkdirSync(SHOTS, { recursive: true });
    const shots = [];
    for (const [w, h] of [[390, 844], [1280, 860]]) {
      await size(w, h);
      for (const s of ["health", "connections", "models", "chat", "console"]) {
        await js(`window.ferrule.show(${JSON.stringify(s)})`);
        await sleep(1200);
        if (s === "console") {
          await js(`const i = document.querySelector(".console-line input"); i.value = "status"; i.dispatchEvent(new Event("input", { bubbles: true }));`);
          await click("#main", "Run");
          await until("status to finish", () => js(`const h = document.querySelector("#main pre.out"); return h && !h.hidden && /done|exit/.test(h.previousElementSibling.textContent)`), 30);
        }
        const r = await cdp("Page.captureScreenshot", { format: "png" });
        const file = join(SHOTS, `${s === "health" ? "home" : s}-${w}.png`);
        writeFileSync(file, Buffer.from(r.data, "base64"));
        shots.push(file.slice(ROOT.length + 1));
      }
    }
    step("screenshots", true, shots.length + " in " + SHOTS.slice(ROOT.length + 1));
  }

  if (SHOTS_ALL) {
    mkdirSync(SHOTS_ALL, { recursive: true });
    let n = 0;
    const names = await js(`return Object.keys(window.ferrule.sections)`);
    const ext = SHOTS_FORMAT === "webp" ? "webp" : "jpg";
    async function shoot(w, h) {
      const o = { format: SHOTS_FORMAT, quality: SHOTS_QUALITY };
      // A phone is drawn at 2x; 1.5x keeps it sharp at two thirds the bytes.
      if (SHOTS_FORMAT === "webp") o.clip = { x: 0, y: 0, width: w, height: h, scale: w < 600 ? 0.75 : 1 };
      return Buffer.from((await cdp("Page.captureScreenshot", o)).data, "base64");
    }
    for (const [w, h] of [[390, 844], [1280, 800]]) {
      await size(w, h);
      for (const theme of ["paper", "forge"]) {
        await js(`document.documentElement.dataset.theme = ${JSON.stringify(theme)}`);
        for (const name of names) {
          await js(`window.ferrule.show(${JSON.stringify(name)})`);
          await sleep(1100);
          writeFileSync(join(SHOTS_ALL, `${name}-${w}-${theme === "paper" ? "light" : "dark"}.${ext}`), await shoot(w, h));
          n++;
        }
      }
    }
    await js(`document.documentElement.dataset.theme = "paper"`);
    if (SHOTS_FORMAT === "webp") {
      await js(`localStorage.setItem("ferrule-lang", "he"); location.reload()`);
      await sleep(500);
      await until("the page in Hebrew", () => js(`return document.documentElement.dir === "rtl" && !document.getElementById("rail").hidden`), 20);
      for (const [w, h] of [[390, 844], [1280, 800]]) {
        await size(w, h);
        for (const name of ["health", "settings"]) {
          await js(`window.ferrule.show(${JSON.stringify(name)})`);
          await sleep(1100);
          writeFileSync(join(SHOTS_ALL, `${name}-${w}-light-he.${ext}`), await shoot(w, h));
          n++;
        }
      }
      await js(`localStorage.removeItem("ferrule-lang"); location.reload()`);
      await sleep(500);
      await until("the page in English again", () => js(`return document.documentElement.dir === "ltr" && !document.getElementById("rail").hidden`), 20);
    }
    step("screenshots of every section", true, n + " in " + SHOTS_ALL.slice(ROOT.length + 1));
  }

  if (MEASURE) {
    await run("measure a cold load of Home", async () => {
      await size(390, 844);
      await cdp("Network.enable");
      await cdp("Network.setCacheDisabled", { cacheDisabled: true });
      const calls = [];
      const onReq = (m) => { const d = JSON.parse(m.data); if (d.method === "Network.requestWillBeSent") calls.push(d.params.request.url); };
      ws.addEventListener("message", onReq);
      await cdp("Page.navigate", { url: link.split("#")[0] + "#health" });
      await until("Home", () => js(`return !!document.querySelector("#main .card, #main .alert, #main .stats")`), 20);
      await sleep(2500);
      ws.removeEventListener("message", onReq);
      const m = await js(`
        const fcp = (performance.getEntriesByName("first-contentful-paint")[0] || {}).startTime || null;
        const res = performance.getEntriesByType("resource").map((r) => ({ name: r.name.replace(location.origin, ""), wire: r.transferSize, body: r.decodedBodySize }));
        const nav = performance.getEntriesByType("navigation")[0];
        return { fcp_ms: fcp && Math.round(fcp), dcl_ms: Math.round(nav.domContentLoadedEventEnd), files: res.filter((r) => !r.name.startsWith("/api/")), api_calls: res.filter((r) => r.name.startsWith("/api/")).length };`);
      const wire = m.files.reduce((t, f) => t + (f.wire || 0), 0);
      console.log("MEASURE " + JSON.stringify({ ...m, wire_bytes: wire, requests_seen: calls.length }));
      return `FCP ${m.fcp_ms} ms, ${wire} bytes of assets, ${m.api_calls} API calls`;
    });
    await cdp("Network.setCacheDisabled", { cacheDisabled: false });
  }

  // Every section once: a stray null, undefined or [object …] is a
  // renderer that appended a value it meant to drop.
  await run("every section renders clean", async () => {
    const bad = [];
    for (const name of await js(`return Object.keys(window.ferrule.sections)`)) {
      await js(`window.ferrule.show(${JSON.stringify(name)})`);
      await sleep(900);
      const t = await js(`return document.getElementById("main").innerText`);
      const m = t.match(/(^|\s)(null|undefined|NaN)+(\s|$)|\[object \w+\]/);
      if (m) bad.push(name + ": " + JSON.stringify(t.slice(Math.max(0, m.index - 40), m.index + 30)));
    }
    if (bad.length) throw new Error(bad.join(" | "));
    return "no null/undefined/[object] on any section";
  });

  // M47 part 6: on a phone, every section fits the width, every control is
  // a finger's size and has a name, and the keyboard gets in without a mouse.
  await run("every section fits a phone", async () => {
    await size(390, 844);
    const problems = [];
    for (const name of await js(`return Object.keys(window.ferrule.sections)`)) {
      await js(`window.ferrule.show(${JSON.stringify(name)})`);
      await sleep(900);
      const r = await js(`
        const vis = (e) => { const r = e.getBoundingClientRect(); const s = getComputedStyle(e); return r.width > 0 && r.height > 0 && s.visibility !== "hidden" && !e.closest("[hidden], dialog:not([open]), .sr"); };
        const named = (e) => {
          const t = (e.getAttribute("aria-label") || "").trim();
          if (t) return true;
          const by = e.getAttribute("aria-labelledby");
          if (by && by.split(" ").some((i) => (document.getElementById(i) || {}).textContent)) return true;
          if (e.labels && [...e.labels].some((l) => l.textContent.trim())) return true;
          if (e.tagName === "INPUT" && ["button", "submit"].includes(e.type) && e.value) return true;
          return !!(e.textContent || "").trim() || !!(e.getAttribute("title") || "").trim();
        };
        const tap = [...document.querySelectorAll("#main button, #main a[href], #main input:not([type=hidden]), #main select, #main textarea, #main [role=button], #main [role=switch], #tabs a, #topbar button, header button")]
          .filter(vis);
        // A checkbox is tapped through its label; a glossary word is inline text.
        const box = (e) => ((e.type === "checkbox" || e.type === "radio") && e.closest("label")) || e;
        const small = tap.filter((e) => { const r = box(e).getBoundingClientRect(); return (r.width < 44 || r.height < 44) && !e.classList.contains("tip") && !(e.tagName === "A" && e.closest("p, .msg, li, .small, .muted, details summary")); })
          .map((e) => e.tagName.toLowerCase() + "[" + (e.getAttribute("aria-label") || e.textContent || e.type || "").trim().slice(0, 24) + "] " + Math.round(e.getBoundingClientRect().width) + "x" + Math.round(e.getBoundingClientRect().height));
        const unnamed = tap.filter((e) => !named(e)).map((e) => e.outerHTML.slice(0, 90));
        return { wide: document.documentElement.scrollWidth - document.documentElement.clientWidth, small, unnamed,
          h1: document.querySelectorAll("#main h1").length,
          cur: [document.querySelectorAll('#rail [aria-current="page"]').length, document.querySelectorAll('#tabs [aria-current="page"]').length] };`);
      if (r.wide > 1) {
        const who = await js(`const cw = document.documentElement.clientWidth; return [...document.querySelectorAll("body *")].filter((e) => { const r = e.getBoundingClientRect(); return r.width > 0 && r.right > cw + 1 && !e.closest("dialog:not([open]), [hidden], #tabs"); }).slice(0, 4).map((e) => e.tagName.toLowerCase() + (e.id ? "#" + e.id : "") + "." + String(e.className.baseVal ?? e.className).slice(0, 30) + " in " + (e.parentElement && (e.parentElement.id || e.parentElement.tagName)) + " right=" + Math.round(e.getBoundingClientRect().right) + " " + JSON.stringify(e.outerHTML.slice(0, 110)))`);
        problems.push(name + ": scrolls sideways by " + r.wide + " px: " + who.join(", "));
      }
      if (r.unnamed.length) problems.push(name + ": no name on " + r.unnamed.slice(0, 3).join(" ; "));
      if (r.small.length) problems.push(name + ": under 44 px: " + r.small.slice(0, 4).join(", ") + (r.small.length > 4 ? " (+" + (r.small.length - 4) + ")" : ""));
      if (r.h1 !== 1) problems.push(name + ": " + r.h1 + " h1");
      if (r.cur[1] > 1 || r.cur[0] > 1) problems.push(name + ": aria-current " + r.cur);
    }
    await size(1280, 900);
    if (problems.length) throw new Error("\n    " + problems.join("\n    "));
    return "no sideways scroll, every control named and 44 px, one h1 each";
  });

  await run("the keyboard gets in", async () => {
    await js(`window.ferrule.show("health"); document.activeElement && document.activeElement.blur(); window.scrollTo(0, 0)`);
    await key("Tab", "Tab");
    const first = await js(`const a = document.activeElement; return a && [a.tagName, a.getAttribute("href"), a.textContent.trim()]`);
    if (!first || first[0] !== "A" || first[1] !== "#main") throw new Error("the first Tab lands on " + JSON.stringify(first));
    await key("Enter", "Enter");
    await until("focus in #main", () => js(`return document.activeElement && document.activeElement.id === "main"`), 5);
    return "Tab → “" + first[2] + "”, Enter → focus in #main";
  });

  await run("Settings polls nothing", async () => {
    await js(`window.ferrule.show("settings")`);
    await sleep(2500);
    await cdp("Network.enable");
    let n = 0;
    const on = (m) => { const d = JSON.parse(m.data); if (d.method === "Network.requestWillBeSent" && d.params.request.url.includes("/api/")) n++; };
    ws.addEventListener("message", on);
    await sleep(20000);
    ws.removeEventListener("message", on);
    // Health for the badges every 15 s, plus approvals: a handful, not a stream.
    if (n > 8) throw new Error(n + " API calls in 20 s on a page that has nothing live");
    return n + " API calls in 20 s";
  });

  step("no script errors", errors.length === 0, errors.slice(0, 5).join(" | "));
  ws.close();
}

main().catch((e) => step("check", false, e.message)).finally(() => {
  for (const p of procs.reverse()) { try { p.kill(); } catch (_) { /* gone */ } }
  for (const s of servers) s.close();
  setTimeout(() => {
    if (!argv.includes("--keep")) rmSync(tmp, { recursive: true, force: true }); else console.log("kept " + tmp);
    const failed = results.filter(([, ok]) => !ok).length;
    console.log(`\n${results.length - failed} passed, ${failed} failed`);
    process.exit(failed || !results.length ? 1 : 0);
  }, 800);
});
