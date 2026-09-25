// The ferrule dashboard (docs/m22-dashboard.md). Plain DOM, built with
// textContent only: nothing the server sends is ever parsed as HTML.
"use strict";
(function () {
  let csrf = null;

  function el(tag, attrs, ...kids) {
    const e = tag === "svg" || tag === "rect" || tag === "text" || tag === "title"
      ? document.createElementNS("http://www.w3.org/2000/svg", tag)
      : document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === null || v === undefined || v === false) continue;
      if (k === "class") e.setAttribute("class", v);
      else if (k === "text") e.textContent = v;
      else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
      else e.setAttribute(k, v === true ? "" : v);
    }
    for (const kid of kids.flat()) {
      if (kid === null || kid === undefined || kid === false) continue;
      e.append(typeof kid === "string" || typeof kid === "number" ? String(kid) : kid);
    }
    return e;
  }

  async function api(path, body) {
    const opts = { credentials: "same-origin", headers: {} };
    if (body !== undefined) {
      opts.method = "POST";
      opts.headers["Content-Type"] = "application/json";
      if (csrf) opts.headers["X-Ferrule-Csrf"] = csrf;
      opts.body = JSON.stringify(body);
    }
    const r = await fetch(path, opts);
    let data = {};
    try { data = await r.json(); } catch (_) { /* not JSON */ }
    if (r.status === 401) { loggedOut(data.error); throw new Error(data.error || "not logged in"); }
    if (!r.ok) {
      const err = new Error(data.error || ("HTTP " + r.status));
      err.status = r.status;
      err.data = data;
      throw err;
    }
    return data;
  }

  function loggedOut(why) {
    csrf = null;
    stopPolling();
    document.getElementById("nav").hidden = true;
    document.getElementById("logout").hidden = true;
    const main = document.getElementById("main");
    main.replaceChildren(el("div", { class: "card" },
      el("p", { text: why || "Not logged in." }),
      el("p", { class: "muted", text: "Send /dashboard to the bot on Telegram for a new link, or run `ferrule dashboard link` on the machine." })));
  }

  // ---- small helpers ----------------------------------------------------

  function toast(text, bad) {
    let t = document.getElementById("toast");
    if (!t) { t = el("div", { id: "toast" }); document.body.append(t); }
    const card = el("div", { class: "card msg" + (bad ? " alert" : ""), dir: "auto", text });
    t.replaceChildren(card);
    setTimeout(() => { if (card.parentNode) card.remove(); }, bad ? 9000 : 5000);
  }

  // A POST; a destructive one comes back 409 with its question, and goes
  // again with `confirm` once the owner says yes.
  async function act(path, body, button) {
    if (button) button.disabled = true;
    try {
      let r;
      try {
        r = await api("/api/" + path, body || {});
      } catch (e) {
        if (e.status !== 409 || !e.data || !e.data.confirm) throw e;
        if (!window.confirm(e.data.confirm)) return null;
        r = await api("/api/" + path, Object.assign({}, body, { confirm: true }));
      }
      if (r.said) toast(r.said, r.ok === false);
      if (r.links) r.links.forEach((l) => window.open(l.url, "_blank", "noopener"));
      refresh();
      return r;
    } catch (e) {
      toast(e.message, true);
      return null;
    } finally {
      if (button) button.disabled = false;
    }
  }

  function btn(label, path, body, cls) {
    const b = el("button", { class: cls || null, text: label });
    b.onclick = () => act(path, typeof body === "function" ? body() : body, b);
    return b;
  }

  // A button that asks for one value first (window.prompt), then POSTs.
  function ask(label, path, question, now, body) {
    const b = el("button", { text: label });
    b.onclick = () => {
      const v = window.prompt(question, now);
      if (v === null || v.trim() === "" || v.trim() === now) return;
      act(path, body(v), b);
    };
    return b;
  }

  // "0 9 * * * Asia/Jerusalem" → ["0 9 * * *", "Asia/Jerusalem"].
  function splitTz(v) {
    const w = v.trim().split(/\s+/);
    return w.length > 5 ? [w.slice(0, 5).join(" "), w.slice(5).join(" ")] : [w.join(" "), null];
  }

  function ago(unix) {
    if (unix === null || unix === undefined) return "never";
    const s = Math.round(Date.now() / 1000 - unix);
    const f = (n) => (s < 0 ? "in " : "") + n + (s < 0 ? "" : " ago");
    const a = Math.abs(s);
    if (a < 90) return f(a + "s");
    if (a < 5400) return f(Math.round(a / 60) + " min");
    if (a < 129600) return f(Math.round(a / 3600) + " h");
    return f(Math.round(a / 86400) + " d");
  }
  const secs = (s) => (s === null || s === undefined ? "–" : s < 90 ? s + "s" : s < 5400 ? Math.round(s / 60) + " min" : (s / 3600).toFixed(1) + " h");
  const usd = (n) => (n === null || n === undefined ? "–" : "$" + (n < 1 && n > 0 ? n.toFixed(4) : n.toFixed(2)));
  const num = (n) => (n === null || n === undefined ? "–" : n >= 1e6 ? (n / 1e6).toFixed(1) + "M" : n >= 1e3 ? (n / 1e3).toFixed(1) + "k" : String(n));
  const price = (p) => (p ? "$" + p.input + " in · $" + p.output + " out" : "price unknown");
  const tag = (text, cls) => el("span", { class: "tag " + (cls || ""), text });
  const text = (t) => el("span", { class: "msg", dir: "auto", text: t === null || t === undefined ? "" : String(t) });

  function kv(pairs) {
    return el("dl", { class: "kv" }, pairs.filter(Boolean).map(([k, v]) => [el("dt", { text: k }), el("dd", {}, v)]));
  }

  function table(head, rows) {
    return el("div", { class: "scroll" }, el("table", {},
      el("thead", {}, el("tr", {}, head.map((h) => el("th", { text: h })))),
      el("tbody", {}, rows.map((r) => el("tr", {}, r.map((c) => el("td", {}, c)))))));
  }

  // Bars as inline SVG: one per point, the value on hover.
  function bars(points, fmt) {
    const w = 320, h = 90, max = Math.max(...points.map((p) => p.v), 0) || 1;
    const bw = w / Math.max(points.length, 1);
    const svg = el("svg", { class: "chart", viewBox: "0 0 " + w + " " + (h + 14), role: "img" });
    points.forEach((p, i) => {
      const bh = Math.max((p.v / max) * h, p.v > 0 ? 1 : 0);
      svg.append(el("rect", { x: i * bw + 1, y: h - bh, width: Math.max(bw - 2, 1), height: bh },
        el("title", { text: p.k + ": " + fmt(p.v) })));
    });
    if (points.length) {
      svg.append(el("text", { x: 0, y: h + 12, text: points[0].k }));
      svg.append(el("text", { x: w, y: h + 12, "text-anchor": "end", text: points[points.length - 1].k }));
    }
    return svg;
  }

  // ---- the problems banner, on top of every section ----------------------

  function problems(list) {
    if (!list || !list.length) return null;
    return list.map((p) => el("div", { class: "card alert" },
      el("div", { class: "msg bad", dir: "auto", text: p.what }),
      p.fix ? el("div", { class: "muted", text: p.fix }) : null,
      el("div", { class: "row" },
        p.suggest ? btn("Make " + p.suggest + " the default", "models/default", { model: p.suggest }, "primary") : null,
        p.action ? btn(p.action === "kill/off" ? "Turn the kill switch off" : "Fill missing prices", p.action, {}, "primary") : null,
        p.section && p.section !== current ? el("button", { text: "Open " + p.section, onclick: () => show(p.section) }) : null)));
  }

  // ---- sections ----------------------------------------------------------
  // Each: `mount(root)` builds the static part once, `load()` refreshes the
  // live part; `every` is its polling period in seconds (0: by hand only).

  const sections = {};

  sections.health = {
    every: 3,
    mount(root) { this.box = el("div"); root.append(this.box); },
    async load(h) {
      const turns = (h.turns || []).map((t) => [
        el("div", {}, t.place, " ", t.stuck ? tag("stuck", "bad") : null),
        text(t.text),
        el("span", {}, t.busy_secs === null ? "queued" : secs(t.busy_secs), t.activity ? el("div", { class: "muted msg", dir: "auto", text: t.activity }) : null),
        t.busy_secs === null ? "" : btn("Stop", "turn/stop", { session: t.session }, "danger"),
      ]);
      const kill = h.kill || {};
      const hb = h.heartbeat;
      this.box.replaceChildren(
        el("h2", { text: "Health" }),
        el("div", { class: "card" }, kv([
          ["version", h.version],
          ["gateway", h.gateway ? "running" : tag("not in this process", "warn")],
          h.uptime && ["uptime", h.uptime + " (since " + h.started + ")"],
          h.last_start && ["last start", text(h.last_start)],
          h.watchdog && ["watchdog", h.watchdog.ok ? tag("ok", "ok") : el("span", { class: "bad msg", dir: "auto", text: h.watchdog.why })],
          ["kill switch", kill.on ? el("span", {}, tag("on", "bad"), " by " + kill.by + " " + (kill.at || ""), kill.reason ? text(" — " + kill.reason) : null) : tag("off", "ok")],
          hb ? ["heartbeat", el("span", {}, hb.host + " every " + secs(hb.every_secs) + ", last " + ago(hb.last_at), hb.last_error ? el("div", { class: "bad msg", dir: "auto", text: hb.last_error }) : null)] : ["heartbeat", "not set"],
        ]), el("div", { class: "row" },
          kill.on ? btn("Kill switch off", "kill/off", {}, "primary") : btn("Kill switch on", "kill/on", {}, "danger"))),
        el("h3", { text: "Channels" }),
        (h.channels || []).length
          ? table(["channel", "polls", "last ok poll"], h.channels.map((c) => [c.name + (c.stale ? " " : ""), c.polls ? "yes" : "no", el("span", { class: c.stale ? "bad" : "" }, ago(c.last_ok_poll))]))
          : el("p", { class: "muted", text: "No channel in this process." }),
        el("h3", { text: "Running turns" }),
        turns.length ? table(["where", "message", "for", ""], turns) : el("p", { class: "muted", text: "Nothing running." }),
        h.spend ? el("h3", { text: "Spend today" }) : null,
        h.spend ? caps(h.spend) : null);
    },
  };

  function caps(spend) {
    if (spend.error) return el("p", { class: "bad", text: spend.error });
    return el("div", { class: "card" }, kv([
      ["today", usd(spend.today.usd) + " · " + num(spend.today.tokens) + " tokens"],
      ...spend.caps.map((c) => [c.cap.replace(/_/g, " "), c.limit > 0
        ? el("span", { class: c.share >= 1 ? "bad" : c.share >= spend.warn_at ? "warn" : "" },
          Math.round(c.share * 100) + "% of " + (c.cap.startsWith("usd") ? usd(c.limit) : num(c.limit)))
        : "no cap"]),
    ]));
  }

  // The caps as inputs; raising one (or turning it off) comes back with
  // a confirm. 0 = no cap.
  async function capsEditor(box) {
    const x = await api("/api/settings");
    const inputs = x.caps.map((c) => el("input", { type: "number", min: "0", step: c.unit === "usd" ? "0.01" : "1", value: String(c.value), "data-key": c.key }));
    const save = el("button", { class: "primary", text: "Save" });
    save.onclick = () => {
      const changed = {};
      inputs.forEach((i, n) => { if (Number(i.value) !== x.caps[n].value) changed[i.dataset.key] = Number(i.value); });
      if (Object.keys(changed).length) act("settings/caps", { caps: changed }, save).then((r) => { if (r) capsEditor(box); });
    };
    box.replaceChildren(el("summary", { text: "Edit caps" }),
      kv(x.caps.map((c, n) => [c.key.replace(/^max_/, "").replace(/_/g, " ") + (c.unit === "usd" ? " ($)" : ""), inputs[n]])),
      el("p", { class: "muted", text: "0 = no cap. Raising a cap asks first; a running gateway uses the new ones at once." }),
      save);
    box.open = true;
  }

  sections.models = {
    every: 10,
    mount(root) {
      this.box = el("div");
      this.search = el("input", { type: "search", placeholder: "search the catalog", dir: "auto" });
      this.tools = el("select", {}, el("option", { value: "", text: "tool-capable only" }), el("option", { value: "all", text: "all models" }));
      this.sort = el("select", {}, ["in", "out", "context", "name"].map((s) => el("option", { value: s, text: "sort: " + s })));
      this.list = el("div");
      this.rec = el("div");
      this.evalBox = el("div");
      this.suite = el("select", {}, el("option", { value: "smoke", text: "eval: smoke subset (4 tasks)" }), el("option", { value: "starter", text: "eval: whole starter suite (20)" }));
      const go = () => this.catalog(false);
      this.search.addEventListener("change", go);
      this.tools.onchange = go;
      this.sort.onchange = go;
      root.append(this.box,
        el("h3", { text: "Evaluate a candidate" }),
        el("p", { class: "muted", text: "Runs ferrule's starter suite on a model, after its estimated cost and a confirm, under your caps. Pick the tasks here, then Evaluate on a row." }),
        el("div", { class: "row" }, this.suite), this.evalBox,
        el("h3", { text: "Recommended" }), this.rec,
        el("h3", { text: "Catalog" }),
        el("div", { class: "row" }, this.search, this.tools, this.sort,
          el("button", { text: "Refresh", onclick: () => this.catalog(true) }),
          btn("Fill missing prices", "catalog/fill-prices", {})),
        this.list);
      this.catalog(false);
      this.recommend();
    },
    // The estimate is the confirm's question; the run's progress polls in.
    evalButton(model, provider) {
      const b = el("button", { text: "Evaluate" });
      b.onclick = () => act("eval/start", { model, provider, suite: this.suite.value }, b).then(() => this.loadEval());
      return b;
    },
    async loadEval() {
      try {
        const j = (await api("/api/eval")).job;
        if (!j) { this.evalBox.replaceChildren(el("p", { class: "muted", text: "No eval has run from here yet; `ferrule eval report` prints the saved ones." })); return; }
        const f = j.finished;
        const line = (x) => x.passed + "/" + x.planned + " pass · " + num(x.tokens) + " tokens · " + usd(x.usd);
        this.evalBox.replaceChildren(el("div", { class: "card" },
          kv([
            ["model", j.model],
            ["tasks", j.subset === "starter" ? "the whole starter suite" : "the smoke subset"],
            ["progress", j.done + " of " + j.planned + " done" + (j.current ? " · now " + j.current : "") + (j.running ? "" : " · finished")],
            f ? ["result", el("span", {}, line(f.summary), f.summary.stopped ? el("div", { class: "warn msg", dir: "auto", text: "stopped: " + f.summary.stopped }) : null)] : null,
            f ? ["the default", f.baseline ? f.baseline.reference + ": " + line(f.baseline) + " (run " + f.baseline.run_id + ")" : "no saved run on these tasks to compare with"] : null,
            f ? ["saved as", "run " + f.summary.run_id] : null,
            j.error ? ["error", el("span", { class: "bad msg", dir: "auto", text: j.error })] : null,
          ]),
          j.running ? el("div", { class: "row" }, btn("Cancel", "eval/cancel", {}, "danger")) : null,
          j.lines.length ? el("pre", { class: "msg", dir: "auto", text: j.lines.slice(-12).join("\n") }) : null));
      } catch (e) {
        this.evalBox.replaceChildren(el("p", { class: "bad", text: e.message }));
      }
    },
    async load() {
      this.loadEval();
      const m = await api("/api/models");
      const v = m.view;
      const names = v.models.map((r) => r.reference);
      const pick = (label, path, key, extra) => {
        const s = el("select", {}, names.map((n) => el("option", { value: n, text: n })));
        const b = el("button", { text: label });
        b.onclick = () => act(path, Object.assign({ [key]: s.value }, extra ? extra() : {}), b);
        return el("div", { class: "row" }, s, b);
      };
      const chat = el("input", { placeholder: "chat id", size: 10 });
      this.box.replaceChildren(
        el("h2", { text: "Models" }),
        m.fixed ? el("p", { class: "warn", text: "This gateway runs with --provider " + m.fixed + "." }) : null,
        el("div", { class: "card" }, kv([
          ["default", v.default + (v.default_set ? "" : " (the config's first)")],
          ["fallback", v.fallback.length ? v.fallback.join(" → ") : "none"],
          ["last served", v.last_served || "–"],
        ])),
        table(["model", "price", "context", "state", ""], v.models.map((r) => [
          el("span", {}, r.reference, r.default ? " " : null, r.default ? tag("default", "ok") : null, r.aliases.length ? el("div", { class: "muted", text: r.aliases.join(", ") }) : null, r.driver ? el("div", { class: "muted", text: r.driver + " api" }) : null),
          el("span", {}, r.pricing ? price(r.pricing) : tag("no price", "warn"), r.price_source ? el("div", { class: "muted", text: r.price_source }) : null),
          r.context_window ? num(r.context_window) : "–",
          r.down_secs !== null && r.down_secs !== undefined
            ? el("span", { class: "bad msg", dir: "auto", text: "down " + secs(r.down_secs) + ": " + (r.down_reason || "") })
            : r.key_present ? tag("ready", "ok") : tag(r.key_env + " not set", "bad"),
          el("div", { class: "row" },
            r.default ? null : btn("Default", "models/default", { model: r.reference }),
            btn("Test", "models/test", { model: r.reference }),
            this.evalButton(r.reference),
            btn("Remove", "models/remove", { model: r.reference }, "danger")),
        ])),
        el("h3", { text: "Pins" }),
        v.pins.length
          ? table(["chat", "model", ""], v.pins.map((p) => [p.channel + " " + p.chat, p.reference, btn("Unpin", "models/unpin", { chat: p.chat })]))
          : el("p", { class: "muted", text: "No chat is pinned." }),
        el("div", { class: "row" }, chat, pick("Pin", "models/pin", "model", () => ({ chat: chat.value }))),
        el("h3", { text: "Change" }),
        pick("Make default", "models/default", "model"),
        (() => {
          const f = el("input", { placeholder: "fallback, comma-separated", value: v.fallback.join(", ") });
          const b = el("button", { text: "Set fallback" });
          b.onclick = () => act("models/fallback", { models: f.value.split(",").map((s) => s.trim()).filter(Boolean) }, b);
          return el("div", { class: "row" }, f, b);
        })(),
        (() => {
          const p = el("input", { placeholder: "provider", size: 10 });
          const id = el("input", { placeholder: "model id" });
          const a = el("input", { placeholder: "alias (optional)", size: 10 });
          const b = el("button", { text: "Add" });
          b.onclick = () => act("models/add", { provider: p.value, model: id.value, alias: a.value }, b);
          return el("div", { class: "row" }, p, id, a, b);
        })());
    },
    addButtons(row) {
      const ev = this.evalButton(row.id, row.provider || undefined);
      if (!row.provider) return el("div", { class: "row" }, el("span", { class: "muted", text: "price reference" }), ev);
      if (row.connected) return el("div", { class: "row" }, tag("connected", "ok"), ev);
      const body = (as) => ({ provider: row.provider, id: row.id, as });
      return el("div", { class: "row" },
        ev,
        btn("Add", "catalog/add", body("model")),
        btn("Default", "catalog/add", body("default"), "primary"),
        btn("Fallback", "catalog/add", body("fallback")));
    },
    async catalog(force) {
      const q = new URLSearchParams({ search: this.search.value, tools: this.tools.value, sort: this.sort.value });
      if (force) q.set("refresh", "1");
      this.list.replaceChildren(el("p", { class: "muted", text: "Loading…" }));
      try {
        const r = await api("/api/catalog?" + q);
        const f = r.list;
        this.list.replaceChildren(
          el("p", { class: "muted" }, f.sources.map((s) => s.source + ": " + s.models + " models, " + s.from + " " + ago(s.fetched_at) + (s.error ? " (" + s.error + ")" : "")).join(" · ")),
          f.hidden_no_tools ? el("p", { class: "muted", text: f.hidden_no_tools + " hidden: " + f.tools_reason }) : null,
          table(["model", "price / 1M", "context", ""], f.rows.map((m) => [
            el("span", {}, m.id, " ", m.free ? tag(":free", "warn") : null, m.tools === false ? tag("no tools", "bad") : null,
              m.caveat ? el("div", { class: "muted", text: m.caveat }) : null),
            price(m.pricing), m.context ? num(m.context) : "–", this.addButtons(m),
          ])),
          r.total > f.rows.length ? el("p", { class: "muted", text: "Showing " + f.rows.length + " of " + r.total + "; search to narrow." }) : null);
      } catch (e) {
        this.list.replaceChildren(el("p", { class: "bad", text: e.message }));
      }
    },
    async recommend() {
      try {
        const r = await api("/api/recommend");
        this.rec.replaceChildren(
          el("p", { class: "muted", text: "Checked by hand " + r.checked + "; prices live (" + r.from + " " + ago(r.fetched_at) + "). Monthly estimates at your last " + r.usage.days + " days' pace." }),
          r.tiers.map((t) => [el("h4", { text: t.name }), table(["model", "why", "price / 1M", "a month", ""], t.picks.map((p) => [
            el("span", {}, p.id, " ", p.free ? tag(":free", "warn") : null, p.caveat ? el("div", { class: "muted", text: p.caveat }) : null),
            p.why, price(p.pricing), usd(p.monthly_usd),
            p.connected ? tag("connected", "ok") : r.provider ? this.addButtons({ provider: r.provider, id: p.id }) : el("span", { class: "muted", text: "connect OpenRouter" }),
          ]))]),
          r.missing.length ? el("p", { class: "warn", text: "Hidden, no longer listed with tool calls: " + r.missing.join(", ") }) : null);
      } catch (e) {
        this.rec.replaceChildren(el("p", { class: "bad", text: e.message }));
      }
    },
  };

  sections.connections = {
    every: 10,
    mount(root) {
      this.box = el("div");
      root.append(this.box);
    },
    async load() {
      const c = await api("/api/connections");
      if (!c.available) {
        this.box.replaceChildren(el("h2", { text: "Connections" }), el("p", { class: "muted", text: "Connections aren't set up: see `ferrule connections`." }));
        return;
      }
      const svc = el("select", {}, c.services.map((s) => el("option", { value: s, text: s })));
      const write = el("input", { type: "checkbox" });
      const go = el("button", { class: "primary", text: "Connect" });
      go.onclick = () => act("connections/connect", { service: svc.value, write: write.checked }, go);
      this.box.replaceChildren(
        el("h2", { text: "Connections" }),
        c.relay ? null : el("p", { class: "muted", text: "No relay set: sign-ins that need one won't work." }),
        c.connections.length ? table(["service", "state", "scopes", "last use", ""], c.connections.map((s) => {
          const needs = String(s.state).toLowerCase().includes("reconnect");
          return [
            el("span", {}, s.title || s.name, el("div", { class: "muted", text: "via " + s.via })),
            el("span", {}, needs ? tag("needs reconnect", "bad") : tag("connected", "ok"), s.last_error ? el("div", { class: "bad msg", dir: "auto", text: s.last_error }) : null),
            s.write ? "read + write" : "read",
            ago(s.refreshed_at || s.connected_at),
            el("div", { class: "row" },
              btn(needs ? "Reconnect" : "Renew", "connections/connect", { service: s.name, write: s.write }),
              btn("Disconnect", "connections/disconnect", { name: s.name }, "danger")),
          ];
        })) : el("p", { class: "muted", text: "Nothing connected." }),
        c.pending.length ? el("p", { class: "muted", text: "Waiting on a sign-in: " + c.pending.join(", ") }) : null,
        c.asked.length ? el("p", { class: "warn", text: "The agent asked for: " + c.asked.join(", ") }) : null,
        el("h3", { text: "Connect" }),
        el("div", { class: "row" }, svc, el("label", {}, write, " write access"), go),
        el("p", { class: "muted", text: "Services that take an API key are added on the machine: `ferrule connections add <name>`." }));
    },
  };

  sections.usage = {
    every: 30,
    days: "7",
    mount(root) {
      this.box = el("div");
      const pick = el("select", {}, [["1", "today"], ["7", "7 days"], ["30", "30 days"]].map(([v, t]) => el("option", { value: v, text: t })));
      pick.value = this.days;
      pick.onchange = () => { this.days = pick.value; this.load(); };
      // Outside the box the poll redraws, so a half-typed cap survives it.
      const edit = el("details", { class: "card", ontoggle: (e) => { if (e.target.open) capsEditor(e.target); } }, el("summary", { text: "Edit caps" }));
      root.append(el("div", { class: "row" }, el("h2", { text: "Usage" }), pick), this.box, edit);
    },
    async load() {
      const u = await api("/api/usage?days=" + this.days);
      const t = u.total;
      const group = (rows) => table(["", "calls", "tokens", "cost"], rows.map((r) => [text(r.key), String(r.calls), num(r.input_tokens + r.output_tokens), usd(r.usd)]));
      this.box.replaceChildren(
        el("div", { class: "card" }, kv([
          ["cost", usd(t.usd)],
          ["calls", t.calls + (u.malformed ? " (" + u.malformed + " unreadable lines)" : "")],
          ["tokens", num(t.input_tokens) + " in (" + num(t.cached_input_tokens) + " cached) · " + num(t.output_tokens) + " out"],
          ["cache hit", u.cache_hit_pct + "%"],
          ["latency", "p50 " + u.p50_ms + " ms · p95 " + u.p95_ms + " ms"],
          ["errors", u.error_pct + "% · retried " + u.retry_pct + "%"],
        ])),
        u.caps ? caps(u.caps) : null,
        el("h3", { text: "Cost per day" }), bars(u.per_day.map((d) => ({ k: d.key, v: d.usd })), usd),
        el("h3", { text: "Tokens per day" }), bars(u.per_day.map((d) => ({ k: d.key, v: d.input_tokens + d.output_tokens })), num),
        el("h3", { text: "Per model" }),
        table(["model", "calls", "errors", "cache", "p50/p95", "cost"], u.per_model.map((r) => [
          el("span", {}, r.provider + "/" + r.model, el("div", { class: "muted", text: r.shape })), String(r.calls), String(r.errors),
          r.cache_hit_pct + "%", r.p50_ms + "/" + r.p95_ms + " ms", r.priced_calls < r.calls ? el("span", { class: "warn", text: usd(r.usd) + " (" + (r.calls - r.priced_calls) + " unpriced)" }) : usd(r.usd)])),
        el("h3", { text: "Per task" }), u.per_task.length ? group(u.per_task) : el("p", { class: "muted", text: "No scheduled runs." }),
        el("h3", { text: "Per chat" }), u.per_chat.length ? group(u.per_chat) : el("p", { class: "muted", text: "No chats." }));
    },
  };

  sections.tasks = {
    every: 10,
    mount(root) { this.box = el("div"); root.append(this.box); },
    async load() {
      const r = await api("/api/tasks");
      this.box.replaceChildren(
        el("h2", { text: "Tasks" }),
        r.paused ? el("p", { class: "warn", text: "The kill switch is on: nothing runs." }) : null,
        r.tasks.length ? r.tasks.map((t) => el("div", { class: "card" },
          el("div", { class: "row" }, el("strong", { class: "msg", dir: "auto", text: t.name || t.id }), tag(t.kind), t.enabled ? tag("on", "ok") : tag("paused", "warn"), t.builtin ? tag("built in") : null),
          kv([
            ["schedule", t.schedule + " (" + t.timezone + ")"],
            ["next", t.enabled ? ago(t.next_run_at) : "paused"],
            ["to", t.destination],
            ["model", t.model || "the default"],
            ["last runs", t.runs.length ? el("div", {}, t.runs.map((x) => el("div", {},
              tag(x.status, x.status === "succeeded" ? "ok" : x.status === "skipped" ? "warn" : x.status === "running" ? "" : "bad"), " " + ago(x.started_at),
              x.detail ? el("span", { class: "muted msg", dir: "auto", text: " " + x.detail }) : null))) : "none"],
          ]),
          el("div", { class: "row" },
            t.enabled ? btn("Pause", "tasks/pause", { id: t.id }) : btn("Resume", "tasks/resume", { id: t.id }),
            btn("Run now", "tasks/run", { id: t.id }),
            ask("Schedule", "tasks/schedule", t.kind === "cron" ? "Cron schedule (5 fields), then optionally a space and an IANA timezone:" : "When (RFC 3339):",
              t.kind === "cron" ? t.schedule + " " + t.timezone : t.schedule, (v) => {
                const [schedule, timezone] = t.kind === "cron" ? splitTz(v) : [v.trim(), null];
                return { id: t.id, schedule, timezone };
              }),
            ask("Model", "tasks/model", "The model it runs on (provider/model, a provider or an alias), or \"default\":",
              t.model || "default", (v) => ({ id: t.id, model: v.trim() })),
            t.builtin ? null : btn("Delete", "tasks/delete", { id: t.id }, "danger"))))
          : el("p", { class: "muted", text: "No tasks." }));
    },
  };

  sections.logs = {
    every: 0,
    page: 0,
    mount(root) {
      this.kind = el("select", {}, [["all", "everything"], ["audit", "audit log"], ["warn", "warnings and errors"]].map(([v, t]) => el("option", { value: v, text: t })));
      this.q = el("input", { type: "search", placeholder: "filter", dir: "auto" });
      this.box = el("div");
      const go = () => { this.page = 0; this.load(); };
      this.kind.onchange = go;
      this.q.addEventListener("change", go);
      root.append(el("h2", { text: "Logs" }), el("div", { class: "row" }, this.kind, this.q, el("button", { text: "Reload", onclick: () => this.load() })), this.box);
    },
    async load() {
      const r = await api("/api/logs?" + new URLSearchParams({ kind: this.kind.value, q: this.q.value, page: this.page }));
      const pages = Math.max(1, Math.ceil(r.total / r.per_page));
      this.box.replaceChildren(
        table(["when", "what", ""], r.rows.map((x) => [
          el("span", { class: "muted", text: x.at.replace("T", " ").slice(0, 19) }),
          el("span", {}, tag(x.level, /error|fail|engage/.test(x.level) ? "bad" : /warn/.test(x.level) ? "warn" : ""), x.tree ? el("span", { class: "muted", text: " " + x.tree }) : null),
          el("pre", { class: "msg", dir: "auto", text: x.text }),
        ])),
        el("div", { class: "row" },
          el("button", { text: "Newer", disabled: this.page === 0, onclick: () => { this.page--; this.load(); } }),
          el("span", { class: "muted", text: "page " + (r.page + 1) + " of " + pages + " · " + r.total + " entries" }),
          el("button", { text: "Older", disabled: this.page + 1 >= pages, onclick: () => { this.page++; this.load(); } })));
    },
  };

  sections.extensions = {
    every: 0,
    mount(root) { this.box = el("div"); root.append(this.box); },
    async load() {
      const x = await api("/api/settings");
      const w = x.workspace_hooks;
      this.box.replaceChildren(
        el("h2", { text: "Extensions" }),
        el("h3", { text: "MCP servers" }),
        x.mcp.length ? table(["name", "runs", ""], x.mcp.map((m) => [
          el("span", {}, m.name, " ", tag(m.origin), m.disabled ? tag("off", "warn") : null), m.runs,
          el("div", { class: "row" },
            m.origin === "configured" ? (m.disabled ? btn("Enable", "mcp/enable", { name: m.name }) : btn("Disable", "mcp/disable", { name: m.name })) : null,
            btn("Remove", "mcp/remove", { name: m.name }, "danger"))])) : el("p", { class: "muted", text: "None." }),
        el("h3", { text: "Skills" }),
        !x.skills_enabled ? el("p", { class: "muted", text: "Skills are off." })
          : x.skills.length ? table(["skill", "", "what", ""], x.skills.map((s) => [s.name, tag(s.scope), text(s.description),
            s.disabled ? btn("Enable", "skills/enable", { name: s.name }) : btn("Disable", "skills/disable", { name: s.name })])) : el("p", { class: "muted", text: "None found." }),
        x.skills_disabled.length ? el("p", { class: "muted" }, "Disabled: ", x.skills_disabled.map((n, i) => [i ? ", " : "", n, " ", btn("Enable", "skills/enable", { name: n })])) : null,
        el("h3", { text: "Hooks" }),
        x.hooks.length ? table(["event", "matcher", "command"], x.hooks.map((h) => [h.event, h.matcher || "*", el("code", { text: h.command })])) : el("p", { class: "muted", text: "None in the config." }),
        w ? el("div", { class: "card" + (w.trusted ? "" : " alert") },
          el("div", { class: "row" }, el("strong", { text: "Workspace hooks" }), el("code", { text: w.file }), w.trusted ? tag("trusted", "ok") : tag(w.trusted_sha ? "changed since trusted" : "not trusted", "warn")),
          kv([
            ["SHA-256", el("code", { text: w.sha })],
            w.trusted_sha && !w.trusted ? ["trusted", el("code", { text: w.trusted_sha })] : null,
            w.project ? null : ["note", "They won't run until [hooks] project = true is in the config."],
          ]),
          w.parse_error ? el("p", { class: "bad", text: w.parse_error }) : null,
          w.hooks.length ? table(["event", "matcher", "command"], w.hooks.map((h) => [h.event, h.matcher || "*", el("code", { text: h.command })])) : null,
          w.diff ? [el("h4", { text: "What changed since it was trusted" }), el("pre", {}, w.diff.map((d) => el("div", { class: d.op === "+" ? "ok" : d.op === "-" ? "bad" : "muted", text: d.op + " " + d.line })))]
            : el("details", {}, el("summary", { text: "The file" }), el("pre", { class: "msg", dir: "auto", text: w.text })),
          el("div", { class: "row" },
            w.trusted || w.parse_error ? null : btn("Trust this version", "hooks/trust", { sha: w.sha }, "primary"),
            w.trusted_sha ? btn("Untrust", "hooks/untrust", {}, "danger") : null))
          : null);
    },
  };

  sections.agents = {
    every: 5,
    mount(root) { this.box = el("div"); root.append(this.box); },
    async load() {
      const r = await api("/api/agents");
      this.box.replaceChildren(
        el("h2", { text: "Agents" }),
        r.agents.length ? table(["agent", "status", "task", "tokens", "model"], r.agents.map((a) => [
          el("span", {}, a.name, el("div", { class: "muted", text: a.role + " · depth " + a.depth })),
          tag(a.status, a.status === "running" ? "ok" : a.status === "failed" ? "bad" : ""),
          text(a.task), num(a.tokens), a.model || "–",
        ])) : el("p", { class: "muted", text: "No sub-agents running." }));
    },
  };

  // ---- navigation and polling --------------------------------------------

  const order = ["health", "models", "connections", "usage", "tasks", "logs", "extensions", "agents"];
  let current = "health";
  let timer = null;
  let banner = null;
  let lastHealth = null;

  function show(name) {
    current = name;
    for (const a of document.querySelectorAll("#nav a")) a.className = a.dataset.s === name ? "on" : "";
    const root = el("div");
    banner = el("div");
    document.getElementById("main").replaceChildren(banner, root);
    sections[name].mount(root);
    refresh();
  }

  // `auto`: a poll, not a click. A section polled by hand only still gets
  // the problems banner.
  async function refresh(auto) {
    clearTimeout(timer);
    timer = null;
    if (!csrf) return;
    const s = sections[current];
    const name = current;
    try {
      lastHealth = await api("/api/health");
      if (name !== current) return;
      banner.replaceChildren(...(problems(lastHealth.problems) || []));
      document.getElementById("pulse").textContent = lastHealth.uptime ? "up " + lastHealth.uptime : "";
      // Not while the owner is typing or choosing in this section.
      const f = document.activeElement;
      const busy = f && /^(INPUT|SELECT|TEXTAREA)$/.test(f.tagName) && document.getElementById("main").contains(f);
      if (!busy && !(auto === true && !s.every)) await s.load(lastHealth);
    } catch (e) {
      if (!csrf) return;
      banner.replaceChildren(el("div", { class: "card alert", text: e.message }));
    }
    schedule();
  }

  // Cheap when nobody's looking: no polling while the page is hidden, and
  // the server closes the session after its idle timeout anyway.
  function schedule() {
    clearTimeout(timer);
    timer = null;
    // 0: only when asked (logs, extensions).
    const every = sections[current].every ?? 15;
    if (every > 0 && !document.hidden && csrf) timer = setTimeout(() => refresh(true), every * 1000);
  }
  function stopPolling() { clearTimeout(timer); timer = null; }

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) stopPolling(); else if (csrf) refresh();
  });

  window.ferrule = { el, api, sections };

  async function start() {
    // A login link carries its token after '#': the browser never sends it
    // to a server, and it's gone from the address bar at once.
    const token = location.hash.length > 1 ? location.hash.slice(1) : "";
    if (token) {
      history.replaceState(null, "", "/");
      try {
        const r = await api("/api/login", { token });
        csrf = r.csrf;
      } catch (e) { loggedOut(e.message); return; }
    } else {
      try { csrf = (await api("/api/session")).csrf; } catch (_) { return; }
    }
    document.getElementById("logout").hidden = false;
    document.getElementById("logout").onclick = async () => {
      try { await api("/api/logout", {}); } catch (_) { /* gone anyway */ }
      loggedOut("Logged out.");
    };
    const nav = document.getElementById("nav");
    nav.replaceChildren(...order.map((s) => {
      const a = el("a", { href: "#", text: s[0].toUpperCase() + s.slice(1) });
      a.dataset.s = s;
      a.onclick = (e) => { e.preventDefault(); show(s); };
      return a;
    }));
    nav.hidden = false;
    show("health");
  }

  document.addEventListener("DOMContentLoaded", start);
})();
