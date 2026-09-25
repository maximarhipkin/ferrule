// node --test relay/ — the Worker against an in-memory Durable Object.
import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { Slot, encryptKey } from "./worker.js";

const RELAY_KEY = "test-relay-key-0123456789";

// One Slot object per name, with storage and a clock the tests move.
function fakeEnv() {
  const clock = { now: 1_000_000 };
  const objects = new Map();
  const realNow = Date.now;
  Date.now = () => clock.now;
  const env = {
    RELAY_KEY,
    SLOTS: {
      idFromName: (name) => name,
      get(id) {
        if (!objects.has(id)) {
          const data = new Map();
          const state = {
            alarm: null,
            storage: {
              get: async (k) => data.get(k),
              put: async (k, v) => void data.set(k, v),
              delete: async (k) => void data.delete(k),
              deleteAll: async () => data.clear(),
              setAlarm: async (t) => void (state.alarm = t),
              deleteAlarm: async () => void (state.alarm = null),
              data,
            },
          };
          objects.set(id, { state, slot: new Slot(state) });
        }
        const o = objects.get(id);
        return { fetch: (url, init) => o.slot.fetch(new Request(url, init)) };
      },
    },
  };
  // Runs every alarm that is due, as the runtime would.
  const advance = async (ms) => {
    clock.now += ms;
    for (const { state, slot } of objects.values()) {
      if (state.alarm !== null && state.alarm <= clock.now) {
        state.alarm = null;
        await slot.alarm();
      }
    }
  };
  const restore = () => (Date.now = realNow);
  return { env, advance, objects, restore };
}

function b64url(bytes) {
  return Buffer.from(bytes).toString("base64url");
}

async function newSecret() {
  const secret = crypto.getRandomValues(new Uint8Array(32));
  const id = b64url(new Uint8Array(await crypto.subtle.digest("SHA-256", secret)));
  return { secret: b64url(secret), id };
}

const call = (env, method, path, { body, headers } = {}) =>
  worker.fetch(new Request("https://relay.test" + path, { method, body, headers }), env);

const poll = (env, secret, key = RELAY_KEY) =>
  call(env, "POST", "/poll", {
    body: JSON.stringify({ secret }),
    headers: { authorization: "Bearer " + key, "content-type": "application/json" },
  });

test("health answers", async () => {
  const { env, restore } = fakeEnv();
  const r = await call(env, "GET", "/health");
  assert.equal(r.status, 200);
  assert.deepEqual(await r.json(), { ok: true, relay: "ferrule-relay", v: 1 });
  restore();
});

test("a code reaches the poller once, then it's gone", async () => {
  const { env, objects, restore } = fakeEnv();
  const { secret, id } = await newSecret();
  assert.equal((await poll(env, secret)).status, 204, "opens the slot");
  const cb = await call(env, "GET", `/cb?state=${id}&code=abc123&iss=https%3A%2F%2Fas.test`);
  assert.equal(cb.status, 200);
  assert.ok(!(await cb.text()).includes("abc123"), "the page never echoes the code");
  const first = await poll(env, secret);
  assert.equal(first.status, 200);
  assert.deepEqual(await first.json(), { kind: "oauth", code: "abc123", iss: "https://as.test" });
  const second = await poll(env, secret);
  assert.equal(second.status, 410, "a second read finds it used");
  assert.equal((await call(env, "GET", `/cb?state=${id}&code=replay`)).status, 409, "a replay is refused");
  assert.equal(objects.get(id).state.storage.data.get("value"), undefined, "the value is gone");
  restore();
});

test("the first write wins", async () => {
  const { env, restore } = fakeEnv();
  const { secret, id } = await newSecret();
  await poll(env, secret);
  assert.equal((await call(env, "GET", `/cb?state=${id}&code=first`)).status, 200);
  assert.equal((await call(env, "GET", `/cb?state=${id}&code=second`)).status, 409);
  assert.equal((await (await poll(env, secret)).json()).code, "first");
  restore();
});

test("a slot nobody opened can't be written", async () => {
  const { env, restore } = fakeEnv();
  const { id } = await newSecret();
  const r = await call(env, "GET", `/cb?state=${id}&code=x`);
  assert.equal(r.status, 404);
  const d = await call(env, "POST", `/drop/${id}`, { body: JSON.stringify({ v: 1, epk: "a", iv: "b", ct: "c" }) });
  assert.equal(d.status, 404);
  restore();
});

test("polling needs the relay key", async () => {
  const { env, restore } = fakeEnv();
  const { secret } = await newSecret();
  assert.equal((await poll(env, secret, "wrong")).status, 401);
  assert.equal((await call(env, "POST", "/poll", { body: JSON.stringify({ secret }) })).status, 401);
  const unset = { ...env, RELAY_KEY: undefined };
  assert.equal((await poll(unset, secret)).status, 401, "a relay without a key opens nothing");
  restore();
});

test("the slot id is not enough to read it", async () => {
  const { env, restore } = fakeEnv();
  const { secret, id } = await newSecret();
  await poll(env, secret);
  await call(env, "GET", `/cb?state=${id}&code=abc`);
  // Polling with the id (the public state) as if it were the secret reads another slot.
  assert.equal((await poll(env, id)).status, 204);
  assert.equal((await (await poll(env, secret)).json()).code, "abc");
  restore();
});

test("bad input is refused", async () => {
  const { env, restore } = fakeEnv();
  assert.equal((await poll(env, "short")).status, 400);
  assert.equal((await call(env, "GET", "/cb?state=../x&code=1")).status, 400);
  const { secret, id } = await newSecret();
  await poll(env, secret);
  assert.equal((await call(env, "GET", `/cb?state=${id}`)).status, 400, "neither code nor error");
  const big = "x".repeat(5000);
  assert.equal((await call(env, "GET", `/cb?state=${id}&code=${big}`)).status, 413);
  const huge = JSON.stringify({ v: 1, epk: "a", iv: "b", ct: "x".repeat(9000) });
  assert.equal((await call(env, "POST", `/drop/${id}`, { body: huge })).status, 413);
  assert.equal((await call(env, "POST", `/drop/${id}`, { body: "{" })).status, 400);
  assert.equal((await call(env, "POST", `/drop/${id}`, { body: '{"v":2}' })).status, 400);
  assert.equal((await call(env, "GET", "/anything")).status, 404);
  restore();
});

test("an error from the vendor is passed along", async () => {
  const { env, restore } = fakeEnv();
  const { secret, id } = await newSecret();
  await poll(env, secret);
  await call(env, "GET", `/cb?state=${id}&error=access_denied&error_description=no`);
  assert.deepEqual(await (await poll(env, secret)).json(), {
    kind: "oauth",
    error: "access_denied",
    error_description: "no",
  });
  restore();
});

test("an unread value is deleted after five minutes", async () => {
  const { env, advance, objects, restore } = fakeEnv();
  const { secret, id } = await newSecret();
  await poll(env, secret);
  await call(env, "GET", `/cb?state=${id}&code=abc`);
  await advance(5 * 60 * 1000 + 1);
  assert.equal(objects.get(id).state.storage.data.size, 0, "the alarm wiped it");
  assert.equal((await poll(env, secret)).status, 204);
  restore();
});

test("an open slot closes after fifteen minutes", async () => {
  const { env, advance, restore } = fakeEnv();
  const { secret, id } = await newSecret();
  await poll(env, secret);
  await advance(14 * 60 * 1000);
  await poll(env, secret);
  await advance(2 * 60 * 1000);
  assert.equal((await call(env, "GET", `/cb?state=${id}&code=late`)).status, 404);
  restore();
});

test("pages carry no-store and a strict CSP", async () => {
  const { env, restore } = fakeEnv();
  const r = await call(env, "GET", "/key");
  assert.equal(r.headers.get("cache-control"), "no-store");
  assert.equal(r.headers.get("referrer-policy"), "no-referrer");
  assert.match(r.headers.get("content-security-policy"), /default-src 'none'/);
  const js = await (await call(env, "GET", "/key.js")).text();
  assert.match(js, /deriveBits/);
  new Function(js.replace(/^"use strict";/, "")); // it parses
  restore();
});

test("the key form's envelope round-trips and drops once", async () => {
  const { env, restore } = fakeEnv();
  const curve = { name: "ECDH", namedCurve: "P-256" };
  const ferrule = await crypto.subtle.generateKey(curve, true, ["deriveBits"]);
  const pub = b64url(new Uint8Array(await crypto.subtle.exportKey("raw", ferrule.publicKey)));
  const { secret, id } = await newSecret();
  await poll(env, secret);
  const envelope = await encryptKey(pub, id, "lin_api_SECRET");
  assert.ok(!JSON.stringify(envelope).includes("lin_api_SECRET"));
  const body = JSON.stringify(envelope);
  assert.equal((await call(env, "POST", `/drop/${id}`, { body })).status, 200);
  assert.equal((await call(env, "POST", `/drop/${id}`, { body })).status, 409);
  const got = await (await poll(env, secret)).json();
  assert.equal(got.kind, "key");

  // What ferrule does on its side (the Rust twin is in ferrule-connections).
  const enc = new TextEncoder();
  const epk = await crypto.subtle.importKey("raw", Buffer.from(got.epk, "base64url"), curve, false, []);
  const shared = await crypto.subtle.deriveBits({ name: "ECDH", public: epk }, ferrule.privateKey, 256);
  const hkdf = await crypto.subtle.importKey("raw", shared, "HKDF", false, ["deriveKey"]);
  const aes = await crypto.subtle.deriveKey(
    { name: "HKDF", hash: "SHA-256", salt: enc.encode(id), info: enc.encode("ferrule key form v1") },
    hkdf,
    { name: "AES-GCM", length: 256 },
    false,
    ["decrypt"],
  );
  const plain = await crypto.subtle.decrypt(
    { name: "AES-GCM", iv: Buffer.from(got.iv, "base64url"), additionalData: enc.encode(id) },
    aes,
    Buffer.from(got.ct, "base64url"),
  );
  assert.equal(new TextDecoder().decode(plain), "lin_api_SECRET");
  restore();
});

test("the worker never logs", async () => {
  const src = await import("node:fs").then((fs) => fs.readFileSync(new URL("./worker.js", import.meta.url), "utf8"));
  assert.ok(!/console\./.test(src));
});
