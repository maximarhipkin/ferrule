// ferrule-relay: hands an OAuth callback (or an encrypted API key) to a
// ferrule that has no inbound port. Design: docs/m20-connections.md §4.
//
// A ferrule makes a random 32-byte secret per consent; the slot id is
// base64url(SHA-256(secret)) and is the OAuth `state`. Only a holder of
// the relay key (a Worker secret, RELAY_KEY) can open a slot or read it,
// and reading needs the secret, not the id. The vendor's redirect (or the
// key form) writes into an open slot once. A value lives 5 minutes, an
// open slot 15, one read deletes it. Nothing is logged.
//
// One Durable Object per slot: strongly consistent, so "first write wins"
// and "one read, then gone" are exact (Workers KV is eventually
// consistent and can't promise either).
//
// M39 adds a WhatsApp webhook mailbox (docs/m39-channels.md §3.2): Meta
// posts to /wa/<box>, the Worker checks the X-Hub-Signature-256 HMAC and
// keeps the body until ferrule takes it (at most 24 hours). <box> is
// base64url(SHA-256("wa:" + RELAY_KEY)), so only the key's holder knows it.
// ferrule gives the mailbox its verify token and app secret through
// /wa/<box>/config, so turning WhatsApp on needs no redeploy.

const OPEN_TTL_MS = 15 * 60 * 1000;
const VALUE_TTL_MS = 5 * 60 * 1000;
const MAX_CB_BYTES = 4096;
const MAX_DROP_BYTES = 8192;
const ID = /^[A-Za-z0-9_-]{43}$/;
const WA_MAX_BODY = 256 * 1024;
const WA_MAX_EVENTS = 1000;
const WA_MAX_BYTES = 5 * 1024 * 1024;
const WA_TTL_MS = 24 * 60 * 60 * 1000;
const WA_TAKE = 100;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const path = url.pathname;
    try {
      if (request.method === "GET" && (path === "/" || path === "/health")) {
        return json({ ok: true, relay: "ferrule-relay", v: 2, wa: true });
      }
      if (path.startsWith("/wa/")) return await whatsapp(path.slice("/wa/".length), url, request, env);
      if (request.method === "POST" && path === "/poll") return await poll(request, env);
      if (request.method === "GET" && path === "/cb") return await callback(url, env);
      if (request.method === "GET" && path === "/key") return page(KEY_PAGE, "text/html");
      if (request.method === "GET" && path === "/key.js") return page(keyScript(), "text/javascript");
      if (request.method === "POST" && path.startsWith("/drop/")) {
        return await drop(path.slice("/drop/".length), request, env);
      }
      return text(404, "not found");
    } catch {
      return text(500, "relay error");
    }
  },
};

// POST /poll, `Authorization: Bearer <relay key>`, {"secret": "<b64url>"}:
// opens the slot if new; its value, deleted as it's read (200), 204 while
// empty, 410 once read.
async function poll(request, env) {
  if (!env.RELAY_KEY || !sameText(bearer(request), env.RELAY_KEY)) {
    return text(401, "relay key required");
  }
  const body = await readCapped(request, 256);
  if (body === null) return text(413, "too large");
  let secret;
  try {
    secret = JSON.parse(body).secret;
  } catch {
    return text(400, "bad json");
  }
  const raw = typeof secret === "string" && ID.test(secret) ? unb64url(secret) : null;
  if (!raw || raw.length !== 32) return text(400, "bad secret");
  const id = b64url(new Uint8Array(await crypto.subtle.digest("SHA-256", raw)));
  return slot(env, id, "take", null);
}

// GET /cb?state=<slot id>&code=… or &error=…: the vendor's redirect.
async function callback(url, env) {
  const state = url.searchParams.get("state") || "";
  if (!ID.test(state)) return done(400, "This link isn't from ferrule, or it's damaged.");
  const value = { kind: "oauth" };
  for (const name of ["code", "error", "error_description", "iss"]) {
    const v = url.searchParams.get(name);
    if (v !== null) value[name] = v;
  }
  if (!value.code && !value.error) return done(400, "The sign-in didn't return anything.");
  const body = JSON.stringify(value);
  if (new TextEncoder().encode(body).length > MAX_CB_BYTES) return done(413, "The sign-in answer is too large.");
  const r = await slot(env, state, "put", body);
  if (r.status === 200) {
    return value.error
      ? done(200, "The sign-in was cancelled or refused. ferrule will tell you in Telegram.")
      : done(200, "Done. Go back to Telegram: ferrule is finishing the connection.");
  }
  if (r.status === 409) return done(409, "This sign-in link was already used.");
  return done(404, "This sign-in has expired. Ask ferrule for a new button.");
}

// POST /drop/<slot id>: the key form's ciphertext, {v, epk, iv, ct}.
async function drop(id, request, env) {
  if (!ID.test(id)) return text(400, "bad slot");
  const body = await readCapped(request, MAX_DROP_BYTES);
  if (body === null) return text(413, "too large");
  let v;
  try {
    v = JSON.parse(body);
  } catch {
    return text(400, "bad json");
  }
  const fields = ["epk", "iv", "ct"];
  if (v.v !== 1 || !fields.every((f) => typeof v[f] === "string")) return text(400, "bad envelope");
  const value = JSON.stringify({ kind: "key", v: 1, epk: v.epk, iv: v.iv, ct: v.ct });
  const r = await slot(env, id, "put", value);
  if (r.status === 200) return text(200, "sent");
  if (r.status === 409) return text(409, "already used");
  return text(404, "expired");
}

// The WhatsApp mailbox: GET <box> (Meta's verify), POST <box> (an event),
// POST <box>/take and <box>/config (ferrule, with the relay key).
async function whatsapp(rest, url, request, env) {
  const [box, op = "", extra] = rest.split("/");
  if (extra !== undefined || !env.RELAY_KEY || !env.MAILBOXES) return text(404, "not found");
  const want = b64url(new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode("wa:" + env.RELAY_KEY))));
  if (!sameText(box, want)) return text(404, "not found");
  const stub = env.MAILBOXES.get(env.MAILBOXES.idFromName(box));
  const inner = (name, body, headers = {}) => stub.fetch("https://mailbox/" + name, { method: "POST", body, headers });
  if (op === "" && request.method === "GET") {
    const q = url.searchParams;
    return inner("verify", JSON.stringify({
      mode: q.get("hub.mode") || "",
      token: q.get("hub.verify_token") || "",
      challenge: q.get("hub.challenge") || "",
    }));
  }
  if (op === "" && request.method === "POST") {
    const body = await readCapped(request, WA_MAX_BODY);
    if (body === null) return text(413, "too large");
    return inner("put", body, { "x-hub-signature-256": request.headers.get("x-hub-signature-256") || "" });
  }
  if ((op === "take" || op === "config") && request.method === "POST") {
    if (!sameText(bearer(request), env.RELAY_KEY)) return text(401, "relay key required");
    const body = await readCapped(request, 4096);
    if (body === null) return text(413, "too large");
    return inner(op, body);
  }
  return text(404, "not found");
}

async function slot(env, id, op, value) {
  const stub = env.SLOTS.get(env.SLOTS.idFromName(id));
  return stub.fetch("https://slot/" + op, { method: "POST", body: value ?? "" });
}

export class Slot {
  constructor(state) {
    this.storage = state.storage;
  }

  async fetch(request) {
    const op = new URL(request.url).pathname.slice(1);
    const now = Date.now();
    const openUntil = await this.storage.get("open_until");
    const live = typeof openUntil === "number" && openUntil > now;
    // After a read the slot keeps only a "used" mark until its window
    // ends, so a replayed callback or a second poll can't reuse it.
    const used = live && (await this.storage.get("used")) === true;
    if (op === "take") {
      if (used) return new Response(null, { status: 410 });
      if (!live) {
        await this.storage.deleteAll();
        await this.storage.put("open_until", now + OPEN_TTL_MS);
        await this.storage.setAlarm(now + OPEN_TTL_MS);
        return new Response(null, { status: 204 });
      }
      const value = await this.storage.get("value");
      if (typeof value !== "string") return new Response(null, { status: 204 });
      await this.storage.delete("value");
      await this.storage.put("used", true);
      return new Response(value, { status: 200, headers: { "content-type": "application/json" } });
    }
    if (op === "put") {
      if (!live) return new Response(null, { status: 404 });
      if (used || (await this.storage.get("value")) !== undefined) return new Response(null, { status: 409 });
      await this.storage.put("value", await request.text());
      const until = Math.min(openUntil, now + VALUE_TTL_MS);
      await this.storage.put("open_until", until);
      await this.storage.setAlarm(until);
      return new Response(null, { status: 200 });
    }
    return new Response(null, { status: 404 });
  }

  async alarm() {
    await this.storage.deleteAll();
  }
}

// One per mailbox. Keys: "config" {verify_token, app_secret}, "seq" (the
// last number given), "bytes", and "e:<seq, 12 digits>" per event.
export class Mailbox {
  constructor(state) {
    this.storage = state.storage;
  }

  async fetch(request) {
    const op = new URL(request.url).pathname.slice(1);
    const config = await this.storage.get("config");
    if (op === "config") {
      let v;
      try {
        v = JSON.parse(await request.text());
      } catch {
        return text(400, "bad json");
      }
      if (typeof v.verify_token !== "string" || typeof v.app_secret !== "string" || !v.verify_token || !v.app_secret) {
        return text(400, "verify_token and app_secret are required");
      }
      await this.storage.put("config", { verify_token: v.verify_token, app_secret: v.app_secret });
      return json({ ok: true });
    }
    if (op === "verify") {
      const v = JSON.parse(await request.text());
      if (!config || v.mode !== "subscribe" || !sameText(v.token, config.verify_token)) return text(403, "forbidden");
      return text(200, v.challenge);
    }
    if (op === "put") {
      if (!config) return text(503, "not set up yet");
      const body = await request.text();
      const sig = request.headers.get("x-hub-signature-256") || "";
      if (!(await validSignature(config.app_secret, body, sig))) return text(401, "bad signature");
      const size = new TextEncoder().encode(body).length;
      const count = await this.count();
      const bytes = (await this.storage.get("bytes")) || 0;
      if (count >= WA_MAX_EVENTS || bytes + size > WA_MAX_BYTES) return text(503, "mailbox full");
      const seq = ((await this.storage.get("seq")) || 0) + 1;
      const now = Date.now();
      await this.storage.put("seq", seq);
      await this.storage.put("bytes", bytes + size);
      await this.storage.put(eventKey(seq), { seq, body, sig, at: now, size });
      if ((await this.storage.getAlarm()) == null) await this.storage.setAlarm(now + WA_TTL_MS);
      return text(200, "ok");
    }
    if (op === "take") {
      let after = 0;
      try {
        after = Number(JSON.parse((await request.text()) || "{}").after) || 0;
      } catch {
        return text(400, "bad json");
      }
      const all = await this.events();
      let bytes = (await this.storage.get("bytes")) || 0;
      for (const e of all) {
        if (e.seq > after) break;
        await this.storage.delete(eventKey(e.seq));
        bytes -= e.size;
      }
      await this.storage.put("bytes", Math.max(0, bytes));
      const rest = all.filter((e) => e.seq > after).slice(0, WA_TAKE);
      return json({ configured: !!config, events: rest.map(({ seq, body, sig, at }) => ({ seq, body, sig, at })) });
    }
    return text(404, "not found");
  }

  // Drops what nobody took within 24 hours, then waits for the next.
  async alarm() {
    const now = Date.now();
    let bytes = (await this.storage.get("bytes")) || 0;
    let next = null;
    for (const e of await this.events()) {
      if (e.at + WA_TTL_MS <= now) {
        await this.storage.delete(eventKey(e.seq));
        bytes -= e.size;
      } else if (next === null) {
        next = e.at + WA_TTL_MS;
      }
    }
    await this.storage.put("bytes", Math.max(0, bytes));
    if (next !== null) await this.storage.setAlarm(next);
  }

  async events() {
    const map = await this.storage.list({ prefix: "e:" });
    return [...map.values()].sort((a, b) => a.seq - b.seq);
  }

  async count() {
    return (await this.storage.list({ prefix: "e:" })).size;
  }
}

function eventKey(seq) {
  return "e:" + String(seq).padStart(12, "0");
}

// Whether `header` is "sha256=" + hex HMAC-SHA256(secret, body).
async function validSignature(secret, body, header) {
  const m = /^sha256=([0-9a-f]{64})$/.exec(header.trim());
  if (!m) return false;
  const key = await crypto.subtle.importKey("raw", new TextEncoder().encode(secret), { name: "HMAC", hash: "SHA-256" }, false, ["verify"]);
  const sig = Uint8Array.from(m[1].match(/../g), (h) => parseInt(h, 16));
  return crypto.subtle.verify("HMAC", key, sig, new TextEncoder().encode(body));
}

function bearer(request) {
  const h = request.headers.get("authorization") || "";
  return h.startsWith("Bearer ") ? h.slice(7) : "";
}

// Compares without stopping at the first difference.
function sameText(a, b) {
  const x = new TextEncoder().encode(a);
  const y = new TextEncoder().encode(b);
  let diff = x.length ^ y.length;
  for (let i = 0; i < Math.max(x.length, y.length); i++) diff |= (x[i] ?? 0) ^ (y[i] ?? 0);
  return diff === 0;
}

// The body as text, or null past `max` bytes (read no further).
async function readCapped(request, max) {
  const declared = Number(request.headers.get("content-length") || "0");
  if (declared > max) return null;
  if (!request.body) return "";
  const reader = request.body.getReader();
  const parts = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.length;
    if (size > max) {
      await reader.cancel();
      return null;
    }
    parts.push(value);
  }
  const all = new Uint8Array(size);
  let at = 0;
  for (const p of parts) {
    all.set(p, at);
    at += p.length;
  }
  return new TextDecoder().decode(all);
}

function b64url(bytes) {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function unb64url(s) {
  try {
    const bin = atob(s.replace(/-/g, "+").replace(/_/g, "/"));
    return Uint8Array.from(bin, (c) => c.charCodeAt(0));
  } catch {
    return null;
  }
}

const SECURITY = {
  "cache-control": "no-store",
  "referrer-policy": "no-referrer",
  "x-content-type-options": "nosniff",
};

function json(value) {
  return new Response(JSON.stringify(value), {
    headers: { "content-type": "application/json", ...SECURITY },
  });
}

function text(status, body) {
  return new Response(body, { status, headers: { "content-type": "text/plain", ...SECURITY } });
}

function page(body, type) {
  return new Response(body, {
    headers: {
      "content-type": type + "; charset=utf-8",
      "content-security-policy":
        "default-src 'none'; script-src 'self'; connect-src 'self'; style-src 'unsafe-inline'; form-action 'none'; frame-ancestors 'none'",
      ...SECURITY,
    },
  });
}

function done(status, message) {
  const body = `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>ferrule</title><body style="font:18px system-ui;margin:2em;max-width:30em"><p>${message}</p></body>`;
  return new Response(body, {
    status,
    headers: {
      "content-type": "text/html; charset=utf-8",
      "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'",
      ...SECURITY,
    },
  });
}

// The key form. Everything it needs is in the fragment, which browsers
// never send: #s=<slot id>&k=<ferrule's one-time P-256 key>&t=<service>.
const KEY_PAGE = `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>ferrule: API key</title>
<body style="font:18px system-ui;margin:2em;max-width:30em">
<h3 id="title">API key</h3>
<p>The key is encrypted on this page, to a key only your ferrule holds. It never goes through the chat.</p>
<form id="form"><input id="key" type="password" autocomplete="off" style="width:100%;font-size:18px">
<p><button type="submit" style="font-size:18px">Send to ferrule</button></p></form>
<p id="status"></p>
<script src="/key.js"></script></body>`;

// Encrypts `secret` to ferrule's public key (raw P-256, base64url) for
// slot `slot`: ECDH, then HKDF-SHA256 (salt = the slot id, info = "ferrule
// key form v1"), then AES-256-GCM with the slot id as associated data.
export async function encryptKey(pub, slot, secret) {
  const enc = new TextEncoder();
  const b64 = (bytes) => {
    let s = "";
    for (const b of new Uint8Array(bytes)) s += String.fromCharCode(b);
    return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  };
  const unb64 = (s) => Uint8Array.from(atob(s.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));
  const curve = { name: "ECDH", namedCurve: "P-256" };
  const theirs = await crypto.subtle.importKey("raw", unb64(pub), curve, false, []);
  const mine = await crypto.subtle.generateKey(curve, true, ["deriveBits"]);
  const shared = await crypto.subtle.deriveBits({ name: "ECDH", public: theirs }, mine.privateKey, 256);
  const hkdf = await crypto.subtle.importKey("raw", shared, "HKDF", false, ["deriveKey"]);
  const aes = await crypto.subtle.deriveKey(
    { name: "HKDF", hash: "SHA-256", salt: enc.encode(slot), info: enc.encode("ferrule key form v1") },
    hkdf,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt"],
  );
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const ct = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: enc.encode(slot) }, aes, enc.encode(secret));
  const epk = await crypto.subtle.exportKey("raw", mine.publicKey);
  return { v: 1, epk: b64(epk), iv: b64(iv), ct: b64(ct) };
}

// The form's script: encryptKey's own source, and the glue around it.
function keyScript() {
  return `"use strict";
const encryptKey = ${encryptKey.toString().replace(/^async function encryptKey/, "async function")};
(() => {
  const params = new URLSearchParams(location.hash.slice(1));
  const slot = params.get("s") || "", pub = params.get("k") || "", title = params.get("t");
  const status = document.getElementById("status");
  if (title) document.getElementById("title").textContent = title + " API key";
  if (!/^[A-Za-z0-9_-]{43}$/.test(slot) || !pub) {
    status.textContent = "This link is incomplete. Ask ferrule for a new one.";
    document.getElementById("form").hidden = true;
    return;
  }
  history.replaceState(null, "", location.pathname);
  document.getElementById("form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const input = document.getElementById("key");
    const secret = input.value.trim();
    if (!secret) return;
    status.textContent = "Sending…";
    try {
      const body = JSON.stringify(await encryptKey(pub, slot, secret));
      input.value = "";
      const r = await fetch("/drop/" + slot, { method: "POST", headers: { "content-type": "application/json" }, body });
      status.textContent = r.ok ? "Sent. Go back to Telegram; you can close this page."
        : r.status === 409 ? "This link was already used." : "This link has expired. Ask ferrule for a new one.";
      if (r.ok) document.getElementById("form").hidden = true;
    } catch (err) {
      status.textContent = "Couldn't send it: " + err;
    }
  });
})();
`;
}
