// The ferrule dashboard (docs/m22-dashboard.md, docs/dashboard.md) in the
// "collar" design: a ferrule is the metal collar that keeps a tool's handle
// from splitting under load; this page is the collar around the agent.
// Plain DOM, built with textContent only: nothing the server sends is ever
// parsed as HTML. User and agent text sits in `dir="auto"` elements so
// Hebrew and Arabic read right to left. Nothing here calls a model: the
// page works when every model is down.
"use strict";
(function () {
  let csrf = null;

  // ---- helpers -----------------------------------------------------------

  function el(tag, attrs, ...kids) {
    const e = ["svg", "rect", "text", "circle", "line", "path", "g", "title"].includes(tag)
      ? document.createElementNS("http://www.w3.org/2000/svg", tag)
      : document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === null || v === undefined || v === false) continue;
      if (k === "class") e.setAttribute("class", v);
      else if (k === "text") e.textContent = v;
      else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
      else e.setAttribute(k, v === true ? "" : v);
    }
    for (const kid of kids.flat(2)) {
      if (kid === null || kid === undefined || kid === false) continue;
      e.append(typeof kid === "string" || typeof kid === "number" ? String(kid) : kid);
    }
    return e;
  }
  const frag = (...kids) => { const f = document.createDocumentFragment(); kids.flat(2).forEach((k) => { if (k || k === 0) f.append(typeof k === "string" || typeof k === "number" ? String(k) : k); }); return f; };
  // replaceChildren with el()'s rules: arrays flatten, null/false drop (a
  // raw replaceChildren(null) would print "null", an array "[object …]").
  const setKids = (node, ...kids) => node.replaceChildren(...kids.flat(2).filter((k) => k || k === 0));
  const text = (t) => el("span", { class: "msg", dir: "auto", text: t === null || t === undefined ? "" : String(t) });
  const tag = (t, cls) => el("span", { class: "tag " + (cls || ""), text: t });
  const led = (cls) => el("span", { class: "led " + (cls || "") });

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
    document.getElementById("rail").hidden = true;
    document.getElementById("logout").hidden = true;
    document.getElementById("live").hidden = true;
    document.body.classList.remove("running");
    const main = document.getElementById("main");
    main.replaceChildren(el("div", { class: "card" },
      el("p", { text: why || "Not logged in." }),
      el("p", { class: "muted", text: "Send /dashboard to the bot on Telegram for a new link, or run `ferrule dashboard link` on the machine." })));
  }

  // ---- small helpers -----------------------------------------------------

  function toast(t, bad) {
    const box = document.getElementById("toast");
    const card = el("div", { class: "t", dir: "auto", text: t });
    card.style.borderLeftColor = bad ? "var(--bad)" : "var(--copper)";
    box.replaceChildren(card);
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
  const price = (p) => (p ? "$" + p.input + " · $" + p.output : "price unknown");

  function kv(pairs) {
    return el("dl", { class: "kv" }, pairs.filter(Boolean).map(([k, v]) =>
      frag(el("dt", { text: k }), el("dd", {}, v))));
  }

  function table(head, rows, numcols) {
    return el("div", { class: "tbl" }, el("table", {},
      el("thead", {}, el("tr", {}, head.map((h, i) => el("th", { class: (numcols || []).includes(i) ? "num" : null, text: h })))),
      el("tbody", {}, rows.map((r) => el("tr", { class: r.dead ? "dead" : null },
        r.cells.map((c, i) => el("td", { class: (numcols || []).includes(i) ? "num" : null }, c)))))));
  }

  function secHead(title, sub, ...controls) {
    const ctrls = controls.filter(Boolean);
    return el("div", { class: "sec-head" },
      el("h2", { text: title }),
      sub ? el("span", { class: "sub", text: sub }) : null,
      ctrls.length ? frag(el("span", { class: "spacer" }), ...ctrls) : null);
  }

  function stat(k, v, d) {
    return el("div", { class: "stat" },
      el("div", { class: "k", text: k }),
      el("div", { class: "v", text: v }),
      d ? el("div", { class: "d", text: d }) : null);
  }

  // Bars as inline SVG: one per point, the value on hover, an optional
  // dashed cap line across the chart.
  function bars(points, fmt, cap) {
    const w = 560, h = 110, pad = 6;
    const max = Math.max(cap || 0, ...points.map((p) => p.v), 0) || 1;
    const bw = (w - pad * 2) / Math.max(points.length, 1);
    const svg = el("svg", { class: "bars", viewBox: "0 0 " + w + " " + (h + 30), role: "img" });
    points.forEach((p, i) => {
      const bh = Math.max((p.v / max) * (h - 14), p.v > 0 ? 1.5 : 0);
      const x = pad + i * bw + bw * 0.16, wid = bw * 0.68;
      svg.append(el("rect", { x, y: h - bh, width: wid, height: bh, class: p.dim ? "dim" : null },
        el("title", { text: p.k + ": " + fmt(p.v) })));
      if (points.length <= 12 || i % Math.ceil(points.length / 10) === 0)
        svg.append(el("text", { x: x + wid / 2, y: h + 12, "text-anchor": "middle", text: p.k }));
    });
    if (cap) {
      const y = h - (cap / max) * (h - 14);
      svg.append(el("line", { class: "cap-line", x1: pad, x2: w - pad, y1: y, y2: y }));
      svg.append(el("text", { x: w - pad, y: y - 4, "text-anchor": "end", text: "cap " + fmt(cap), fill: "var(--bad)" }));
    }
    return svg;
  }

  // The cap ring — the collar as a gauge. A null share is a cap that's off.
  function ring(label, share, limitText, warnAt) {
    const r = 50, C = 2 * Math.PI * r;
    const off = share === null || share === undefined;
    const frac = off ? 0 : Math.min(share, 1);
    const cls = off ? "" : share >= 1 ? "over" : share >= (warnAt ?? 0.8) ? "near" : "";
    return el("div", { class: "ring " + cls },
      el("svg", { viewBox: "0 0 118 118" },
        el("circle", { class: "track", cx: 59, cy: 59, r }),
        el("circle", {
          class: "val", cx: 59, cy: 59, r,
          "stroke-dasharray": C.toFixed(1),
          "stroke-dashoffset": (C * (1 - frac)).toFixed(1),
          transform: "rotate(-90 59 59)",
        }),
        el("text", { class: "pct", x: 59, y: 62, "text-anchor": "middle", text: off ? "off" : Math.round(share * 100) + "%" }),
        el("text", { class: "lim", x: 59, y: 78, "text-anchor": "middle", text: limitText })),
      el("div", { class: "lbl", text: label }));
  }

  function capRings(spend) {
    return el("div", { class: "rings" }, (spend.caps || []).map((c) => ring(
      c.cap.replace(/_/g, " "), c.share,
      c.limit > 0 ? "of " + (String(c.cap).startsWith("usd") ? usd(c.limit) : num(c.limit)) : "no cap",
      spend.warn_at)));
  }

  // Today's spend against the caps, as rings.
  function capsCard(spend, extra) {
    if (spend.error) {
      return el("div", { class: "alert warn" },
        el("span", { class: "sev", text: "brass" }),
        el("div", { class: "body" }, el("div", { class: "what msg", dir: "auto", text: String(spend.error) })));
    }
    return el("div", { class: "card" },
      capRings(spend),
      el("p", { class: "muted small", text: "Today: " + usd(spend.today.usd) + " · " + num(spend.today.tokens) + " tokens" +
        (spend.per_run ? " · per run: " + (spend.per_run.usd > 0 ? usd(spend.per_run.usd) : "no $ cap") +
          " / " + (spend.per_run.tokens > 0 ? num(spend.per_run.tokens) : "no token cap") : "") }),
      extra || null);
  }

  // A section whose API isn't in this process (503) or errored still says
  // so in place, and the rest of the page keeps working.
  function sectionError(box, e) {
    box.replaceChildren(el("div", { class: "alert warn" },
      el("span", { class: "sev", text: "brass" }),
      el("div", { class: "body" },
        el("div", { class: "what", text: "This section isn't available in this process" }),
        el("div", { class: "fix msg", dir: "auto", text: e.message }))));
  }

  // ---- the problems banner, on top of every section ----------------------

  function problems(list) {
    if (!list || !list.length) return null;
    return el("div", {}, list.map((p) => {
      const sev = p.top || p.action === "kill/off" ? "bad" : "warn";
      return el("div", { class: "alert " + sev },
        el("span", { class: "sev", text: sev === "bad" ? "rust" : "brass" }),
        el("div", { class: "body" },
          el("div", { class: "what msg", dir: "auto", text: p.what }),
          p.fix ? el("div", { class: "fix msg", dir: "auto", text: p.fix }) : null),
        el("div", { class: "row" },
          p.suggest ? btn("Make " + p.suggest + " the default", "models/default", { model: p.suggest }, "primary") : null,
          p.action ? btn(p.action === "kill/off" ? "Turn the kill switch off" : "Fill missing prices", p.action, {}, "primary") : null,
          p.section && p.section !== current ? el("button", { text: "Open " + p.section, onclick: () => show(p.section) }) : null));
    }));
  }

  // ---- sections ----------------------------------------------------------
  // Each: `mount(root)` builds the static part once, `load()` refreshes the
  // live part; `every` is its polling period in seconds (0: by hand only).

  const sections = {};

  sections.health = {
    every: 3,
    mount(root) { this.box = el("div"); root.append(this.box); },
    async load(h) {
      const kill = h.kill || null;
      const hb = h.heartbeat;
      const ch = h.channels || [];
      const turns = h.turns || [];
      const wd = h.watchdog;
      const lanes = ch.length ? ch.map((c) => c.name).join(" + ") + (ch.length === 1 ? " lane" : " lanes") : null;
      const rows = [
        h.last_start ? ["last start", text(h.last_start)] : null,
        wd && !wd.ok ? ["watchdog", el("span", { class: "bad msg", dir: "auto", text: h.watchdog.why })] : null,
        hb && hb.last_error ? ["heartbeat", el("span", { class: "bad msg", dir: "auto", text: hb.last_error })] : null,
        kill ? ["kill switch", kill.on
          ? frag(tag("engaged", "bad"), kill.by ? " by " + kill.by : "", kill.at ? " " + kill.at : "", kill.reason ? text(" — " + kill.reason) : null)
          : frag(tag("off", "ok"), " ", el("span", { class: "muted", text: "every run proceeds" }))] : null,
        h.workspace ? ["workspace", text(h.workspace)] : null,
        (h.updates || []).length ? ["updates", el("div", {}, ...h.updates.map((l) => el("div", { class: "msg", dir: "auto", text: l })))] : null,
        (h.repairs || []).length ? ["repairs", el("div", {}, ...h.repairs.map((l) => el("div", { class: "msg", dir: "auto", text: l })))] : null,
      ].filter(Boolean);
      setKids(this.box, 
        secHead("Health", "what ferrule is doing right now",
          kill ? (kill.on ? btn("Kill switch off", "kill/off", {}, "primary") : btn("Kill switch on", "kill/on", {}, "danger")) : null),
        el("div", { class: "stats" },
          stat("version", h.version ? "v" + h.version : "–"),
          stat("uptime", h.uptime || "–", h.started ? "since " + h.started : null),
          stat("gateway", h.gateway ? "running" : "standalone", h.gateway ? lanes : "the gateway's page has the lanes"),
          stat("watchdog", wd ? (wd.ok ? "ok" : "attention") : "–",
            wd && wd.ok && wd.after_secs ? "no-progress " + secs(wd.after_secs) : (wd && !wd.ok ? "see below" : null)),
          stat("heartbeat", hb ? ago(hb.last_at) : "not set", hb ? hb.host + " · every " + secs(hb.every_secs) : null)),
        rows.length ? el("div", { class: "card" }, kv(rows)) : null,
        el("h3", { text: "Channels" }),
        ch.length ? table(["channel", "polls", "last ok poll"], ch.map((c) => ({ cells: [
          frag(led(c.stale ? "bad" : "ok pulse"), " ", c.name,
            c.problem ? el("span", { class: "sub msg", dir: "auto", text: c.problem }) : null),
          c.polls ? "yes" : "no",
          el("span", { class: c.stale ? "bad" : "mono muted", text: ago(c.last_ok_poll) }),
        ]}))) : el("div", { class: "empty", text: "no channel in this process" }),
        el("h3", { text: "Running turns" }),
        turns.length ? table(["where", "message", "for", ""], turns.map((t) => ({ cells: [
          frag(el("span", { class: "mono small", text: t.place }), t.stuck ? frag(" ", tag("stuck", "bad")) : null),
          frag(text(t.text), t.activity ? el("span", { class: "sub msg", dir: "auto", text: t.activity }) : null),
          t.busy_secs === null || t.busy_secs === undefined ? el("span", { class: "muted", text: "queued" }) : el("span", { class: "mono", text: secs(t.busy_secs) }),
          t.busy_secs === null || t.busy_secs === undefined ? null : btn("Stop", "turn/stop", { session: t.session }, "danger"),
        ]}))) : el("div", { class: "empty", text: "nothing running" }),
        h.spend ? el("h3", { text: "Spend today — against the caps" }) : null,
        h.spend ? capsCard(h.spend) : null);
    },
  };

  // The caps as inputs; raising one (or turning it off) comes back with
  // a confirm. 0 = no cap.
  async function capsEditor(box) {
    let x;
    try {
      x = await api("/api/settings");
    } catch (e) {
      box.replaceChildren(el("summary", { text: "Edit caps" }),
        el("div", { class: "details-body" }, el("p", { class: "bad", text: e.message })));
      return;
    }
    const inputs = x.caps.map((c) => el("input", { type: "number", min: "0", step: c.unit === "usd" ? "0.01" : "1", value: String(c.value), "data-key": c.key }));
    const save = el("button", { class: "primary", text: "Save" });
    save.onclick = () => {
      const changed = {};
      inputs.forEach((i, n) => { if (Number(i.value) !== x.caps[n].value) changed[i.dataset.key] = Number(i.value); });
      if (Object.keys(changed).length) act("settings/caps", { caps: changed }, save).then((r) => { if (r) capsEditor(box); });
    };
    box.replaceChildren(el("summary", { text: "Edit caps" }),
      el("div", { class: "details-body" },
        kv(x.caps.map((c, n) => [c.key.replace(/^max_/, "").replace(/_/g, " ") + (c.unit === "usd" ? " ($)" : ""), inputs[n]])),
        el("p", { class: "muted small", text: "0 = no cap. Raising a cap asks first; a running gateway uses the new ones at once." }),
        save));
    box.open = true;
  }

  sections.models = {
    every: 10,
    mount(root) {
      this.box = el("div");
      this.search = el("input", { type: "search", placeholder: "search the catalog", dir: "auto", size: "24" });
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
        el("div", { class: "card" },
          el("div", { class: "row" }, this.suite,
            el("span", { class: "muted small", text: "an estimate first, then a confirm; runs under your caps" })),
          this.evalBox),
        el("h3", { text: "Recommended" }), this.rec,
        el("h3", { text: "Catalog" }),
        el("div", { class: "row" }, this.search, this.tools, this.sort,
          el("button", { text: "Refresh", onclick: () => this.catalog(true) })),
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
        if (!j) { setKids(this.evalBox, el("p", { class: "muted small", text: "No eval has run from here yet; `ferrule eval report` prints the saved ones." })); return; }
        const f = j.finished;
        const line = (x) => x.passed + "/" + x.planned + " pass · " + num(x.tokens) + " tokens · " + usd(x.usd);
        setKids(this.evalBox, el("div", {},
          kv([
            ["model", el("span", { class: "mono", text: j.model })],
            ["tasks", j.subset === "starter" ? "the whole starter suite" : "the smoke subset"],
            ["progress", j.done + " of " + j.planned + " done" + (j.current ? " · now " + j.current : "") + (j.running ? "" : " · finished")],
            f ? ["result", frag(
              el("strong", { text: line(f.summary) }),
              f.summary.stopped ? el("div", { class: "warn msg", dir: "auto", text: "stopped: " + f.summary.stopped }) : null)] : null,
            f ? ["the default", f.baseline ? f.baseline.reference + ": " + line(f.baseline) + " (run " + f.baseline.run_id + ")" : "no saved run on these tasks to compare with"] : null,
            f ? ["saved as", el("span", { class: "mono small", text: "run " + f.summary.run_id + " · ferrule eval report --run " + f.summary.run_id })] : null,
            j.error ? ["error", el("span", { class: "bad msg", dir: "auto", text: j.error })] : null,
          ]),
          j.running ? el("div", { class: "row" }, btn("Cancel", "eval/cancel", {}, "danger")) : null,
          j.lines.length ? el("pre", { class: "msg", dir: "auto", text: j.lines.slice(-12).join("\n") }) : null));
      } catch (e) {
        setKids(this.evalBox, el("p", { class: "bad", text: e.message }));
      }
    },
    async load() {
      this.loadEval();
      let m;
      try {
        m = await api("/api/models");
      } catch (e) { return sectionError(this.box, e); }
      const v = m.view;
      const names = v.models.map((r) => r.reference);
      const pick = (label, path, key, extra) => {
        const s = el("select", {}, names.map((n) => el("option", { value: n, text: n })));
        const b = el("button", { text: label });
        b.onclick = () => act(path, Object.assign({ [key]: s.value }, extra ? extra() : {}), b);
        return el("div", { class: "row" }, s, b);
      };
      const chat = el("input", { placeholder: "chat id", size: 10, dir: "auto" });
      const last = (v.last_served || [])[0];
      setKids(this.box, 
        secHead("Models", v.models.length + " connected" + (v.default ? " · default " + v.default : ""),
          btn("Fill missing prices", "catalog/fill-prices", {})),
        m.fixed ? el("div", { class: "alert warn" },
          el("span", { class: "sev", text: "brass" }),
          el("div", { class: "body" },
            el("div", { class: "what", text: "This gateway runs with --provider " + m.fixed + "." }),
            el("div", { class: "fix", text: "Every chat uses it until a restart, whatever the default says." }))) : null,
        el("div", { class: "stats" },
          stat("default", v.default || "–", v.default_set ? "set in config" : "the config's first"),
          stat("fallback", v.fallback.length ? v.fallback.join(" → ") : "none", "on outage"),
          stat("last served", last ? last.reference : "–", last ? last.session : null)),
        v.models.length ? table(["model", "price / 1M", "context", "state", ""], v.models.map((r) => ({
          dead: !r.key_present,
          cells: [
            frag(el("strong", { text: r.reference }), r.default ? frag(" ", tag("default", "copper")) : null,
              el("span", { class: "sub", text: (r.aliases.length ? "alias: " + r.aliases.join(", ") + " · " : "") + r.driver })),
            r.pricing
              ? frag(el("span", { class: "mono", text: price(r.pricing) }), r.price_source ? el("span", { class: "sub", text: r.price_source }) : null)
              : frag(tag("no price", "warn"), el("span", { class: "sub", text: "the dollar caps can't see its spend" })),
            el("span", { class: "mono", text: r.context_window ? num(r.context_window) : "–" }),
            r.down_secs !== null && r.down_secs !== undefined
              ? frag(tag("down " + secs(r.down_secs), "bad"),
                r.down_reason ? el("span", { class: "sub msg", dir: "auto", text: r.down_reason }) : null)
              : frag(
                r.key_present ? tag("ready", "ok") : tag(r.key_env + " not set", "bad"),
                r.plan ? el("span", { class: "sub", text: r.plan + " plan" + (r.usage ? " · " + r.usage : "") }) : null),
            el("div", { class: "row" },
              r.default ? null : btn("Default", "models/default", { model: r.reference }),
              btn("Test", "models/test", { model: r.reference }),
              this.evalButton(r.reference),
              btn("Remove", "models/remove", { model: r.reference }, "danger")),
          ],
        })), [2]) : el("div", { class: "empty", text: "no models connected" }),
        el("h3", { text: "Pins" }),
        v.pins.length
          ? table(["chat", "model", ""], v.pins.map((p) => ({ cells: [
            el("span", { class: "mono", text: p.channel + " " + p.chat }),
            frag(el("span", { class: "mono", text: p.reference }),
              p.resolved && p.resolved !== p.reference ? el("span", { class: "sub", text: "→ " + p.resolved }) : null),
            btn("Unpin", "models/unpin", { chat: p.chat, channel: p.channel }),
          ]})))
          : el("div", { class: "empty", text: "no chat is pinned" }),
        el("h3", { text: "Change" }),
        el("div", { class: "card" },
          el("div", { class: "row" }, chat, pick("Pin", "models/pin", "model", () => ({ chat: chat.value }))),
          pick("Make default", "models/default", "model"),
          (() => {
            const f = el("input", { placeholder: "fallback, comma-separated", value: v.fallback.join(", "), dir: "auto" });
            const b = el("button", { text: "Set fallback" });
            b.onclick = () => act("models/fallback", { models: f.value.split(",").map((s) => s.trim()).filter(Boolean) }, b);
            return el("div", { class: "row" }, f, b);
          })(),
          (() => {
            const p = el("input", { placeholder: "provider", size: 10 });
            const id = el("input", { placeholder: "model id", dir: "auto" });
            const a = el("input", { placeholder: "alias (optional)", size: 10 });
            const b = el("button", { text: "Add" });
            b.onclick = () => act("models/add", { provider: p.value, model: id.value, alias: a.value }, b);
            return el("div", { class: "row" }, p, id, a, b);
          })()));
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
      setKids(this.list, el("p", { class: "muted small", text: "Loading…" }));
      try {
        const r = await api("/api/catalog?" + q);
        const f = r.list;
        setKids(this.list, 
          el("p", { class: "muted small", text: f.sources.map((s) => s.source + ": " + s.models + " models, " + s.from + " " + ago(s.fetched_at) + (s.error ? " (" + s.error + ")" : "")).join(" · ") || "no catalog sources" }),
          f.hidden_no_tools ? el("p", { class: "muted small", text: f.hidden_no_tools + " hidden: " + f.tools_reason }) : null,
          f.rows.length ? table(["model", "price / 1M", "context", ""], f.rows.map((m) => ({ cells: [
            frag(el("span", { class: "mono", text: m.id }), m.free ? frag(" ", tag(":free", "warn")) : null,
              m.tools === false ? frag(" ", tag("no tools", "bad")) : null,
              m.caveat ? el("span", { class: "sub", text: m.caveat }) : null),
            el("span", { class: "mono", text: price(m.pricing) }),
            el("span", { class: "mono", text: m.context ? num(m.context) : "–" }),
            this.addButtons(m),
          ]})), [2]) : el("div", { class: "empty", text: "no catalog rows match" }),
          r.total > f.rows.length ? el("p", { class: "muted small", text: "Showing " + f.rows.length + " of " + r.total + "; search to narrow." }) : null);
      } catch (e) {
        setKids(this.list, el("p", { class: "bad", text: e.message }));
      }
    },
    async recommend() {
      try {
        const r = await api("/api/recommend");
        setKids(this.rec, 
          el("p", { class: "muted small", text: "Checked by hand " + r.checked + " · prices live (" + r.from + " " + ago(r.fetched_at) + ") · monthly estimates at your last " + (r.usage ? r.usage.days : 30) + " days' pace." }),
          r.tiers.map((t) => frag(
            el("h4", { text: t.name }),
            table(["model", "why", "price / 1M", "a month", ""], t.picks.map((p) => ({ cells: [
              frag(el("span", { class: "mono", text: p.id }), p.free ? frag(" ", tag(":free", "warn")) : null,
                p.caveat ? el("span", { class: "sub", text: p.caveat }) : null),
              text(p.why),
              el("span", { class: "mono", text: price(p.pricing) }),
              el("span", { class: "mono", text: usd(p.monthly_usd) }),
              p.connected ? tag("connected", "ok") : r.provider ? this.addButtons({ provider: r.provider, id: p.id }) : el("span", { class: "muted", text: "connect OpenRouter" }),
            ]})), [3]))),
          r.missing.length ? el("p", { class: "warn small", text: "Hidden, no longer listed with tool calls: " + r.missing.join(", ") }) : null);
      } catch (e) {
        setKids(this.rec, el("p", { class: "bad", text: e.message }));
      }
    },
  };

  // M25: which models a turn climbs, why it climbed, and what each tier cost.
  sections.routing = {
    every: 30,
    days: "7",
    mount(root) {
      this.box = el("div");
      const pick = el("select", {}, [["1", "today"], ["7", "7 days"], ["30", "30 days"]].map(([v, t]) => el("option", { value: v, text: t })));
      pick.value = this.days;
      pick.onchange = () => { this.days = pick.value; this.load(); };
      root.append(secHead("Routing", "start cheap · climb only on failure", pick), this.box);
    },
    async load() {
      let r;
      try {
        r = await api("/api/routing?days=" + this.days);
      } catch (e) { return sectionError(this.box, e); }
      const v = r.routing, st = r.stats, sg = r.suggestion;
      const tiers = el("input", { placeholder: "models, cheap first, comma-separated", size: 42, dir: "auto",
        value: v.tiers.length ? v.tiers.map((t) => t.name).join(", ") : [sg.cheap, sg.strong].filter(Boolean).map((p) => p.reference).join(", ") });
      const cap = el("input", { type: "number", min: "0", step: "0.5", size: 8, title: "daily $ above cheap",
        value: v.strong_daily_usd === null || v.strong_daily_usd === undefined ? "" : String(v.strong_daily_usd) });
      const on = el("button", { class: "primary", text: v.on ? "Save" : "Turn on" });
      on.onclick = () => act("routing/set", {
        tiers: tiers.value.split(",").map((s) => s.trim()).filter(Boolean),
        strong_daily_usd: cap.value === "" ? null : Number(cap.value),
      }, on);
      const reasons = (m) => Object.entries(m || {}).map(([k, n]) => k + " ×" + n).join(", ");
      setKids(this.box, 
        el("p", { class: "muted", text: "Every turn starts on the cheapest tier and moves up only when it fails: a call error, invalid tool calls, a failed check or Stop hook, no progress. The next turn starts cheap again." }),
        el("div", { class: "stats" },
          stat("state", v.on ? "on" : v.enabled ? "set" : "off",
            v.on ? (v.de_escalate ? "de-escalates next turn" : "stays up for the chat") : (v.enabled ? "set but not usable" : null)),
          stat("strong spend today", usd(v.strong_spent_today), v.strong_daily_usd ? "of " + usd(v.strong_daily_usd) + " cap" : "no cap"),
          stat("escalations", String(st.escalations || 0), "over " + r.days + "d")),
        (v.problems || []).length ? v.problems.map((p) =>
          el("div", { class: "alert warn" },
            el("span", { class: "sev", text: "brass" }),
            el("div", { class: "body" }, el("div", { class: "what msg", dir: "auto", text: p })))) : null,
        el("h3", { text: "Tiers" }),
        v.tiers.length ? table(["", "tier", "model", "price / 1M", "context", "key"], v.tiers.map((t, i) => ({ cells: [
          el("span", { class: "mono muted", text: String(i + 1) }),
          tag(i === 0 ? "cheap" : i === v.tiers.length - 1 ? "strong" : "mid", i === 0 ? "copper" : null),
          el("span", { class: "mono", text: t.reference }),
          el("span", { class: "mono", text: t.pricing ? price(t.pricing) : "price unknown" }),
          el("span", { class: "mono", text: t.context_window ? num(t.context_window) : "–" }),
          t.key_present ? tag("ready", "ok") : tag(t.key_env + " not set", "bad"),
        ]}))) : el("div", { class: "empty", text: "no tiers set" }),
        el("h3", { text: "Escalations" }),
        (st.days || []).length ? table(["day", "count", "why"], st.days.map((d) => ({ cells: [
          el("span", { class: "mono", text: d.day }),
          el("span", { class: "mono", text: String(d.escalations) }),
          text(reasons(d.reasons)),
        ]})), [1]) : el("div", { class: "empty", text: "none in this window" }),
        el("h3", { text: "Spend per tier" }),
        (st.tiers || []).length ? frag(
          bars(st.tiers.map((t, i) => ({ k: t.tier, v: t.usd, dim: i > 0 })), usd),
          table(["tier", "calls", "cost"], st.tiers.map((t) => ({ cells: [
            text(t.tier), el("span", { class: "mono", text: String(t.calls) }), el("span", { class: "mono", text: usd(t.usd) }),
          ]})), [1, 2])) : el("div", { class: "empty", text: "no routed calls in this window" }),
        el("h3", { text: "Change" }),
        sg.said ? el("p", { class: "muted small msg", dir: "auto", text: sg.said }) : null,
        el("div", { class: "card" }, el("div", { class: "row" }, tiers, cap, on, v.enabled ? btn("Turn off", "routing/unset", {}, "danger") : null)));
    },
  };

  sections.connections = {
    every: 10,
    mount(root) { this.box = el("div"); root.append(this.box); },
    async load() {
      let c;
      try {
        c = await api("/api/connections");
      } catch (e) { return sectionError(this.box, e); }
      if (!c.available) {
        setKids(this.box, 
          secHead("Connections", "oauth grants to outside services"),
          el("div", { class: "empty", text: "Connections aren't set up in this process: see `ferrule connections`." }));
        return;
      }
      const svc = el("select", {}, c.services.map((s) => el("option", { value: s, text: s })));
      const write = el("input", { type: "checkbox" });
      const go = el("button", { class: "primary", text: "Connect" });
      go.onclick = () => act("connections/connect", { service: svc.value, write: write.checked }, go);
      setKids(this.box, 
        secHead("Connections", c.connections.length + " connected · read-only unless you say otherwise"),
        c.relay ? null : el("div", { class: "alert warn" },
          el("span", { class: "sev", text: "brass" }),
          el("div", { class: "body" }, el("div", { class: "what", text: "No relay set: sign-ins that need one won't work." }))),
        c.connections.length ? table(["service", "state", "scopes", "last use", ""], c.connections.map((s) => {
          const needs = String(s.state).toLowerCase().includes("reconnect");
          return { cells: [
            frag(el("strong", { text: s.title || s.name }), el("span", { class: "sub", text: "via " + s.via })),
            frag(needs ? tag("needs reconnect", "bad") : tag("connected", "ok"),
              s.last_error ? el("span", { class: "sub msg", dir: "auto", text: s.last_error }) : null),
            s.write ? "read + write" : "read",
            el("span", { class: "mono muted", text: ago(s.refreshed_at || s.connected_at) }),
            el("div", { class: "row" },
              btn(needs ? "Reconnect" : "Renew", "connections/connect", { service: s.name, write: s.write }, needs ? "primary" : null),
              btn("Disconnect", "connections/disconnect", { name: s.name }, "danger")),
          ]};
        })) : el("div", { class: "empty", text: "nothing connected" }),
        c.pending.length ? el("p", { class: "muted small", text: "Waiting on a sign-in: " + c.pending.join(", ") }) : null,
        c.asked.length ? el("div", { class: "alert warn" },
          el("span", { class: "sev", text: "brass" }),
          el("div", { class: "body" },
            el("div", { class: "what", text: "The agent asked for: " + c.asked.join(", ") }),
            el("div", { class: "fix", text: "It waits until you connect it here — it never sees the token." })),
          btn("Connect " + c.asked[0], "connections/connect", { service: c.asked[0] }, "primary")) : null,
        el("h3", { text: "Connect" }),
        el("div", { class: "card" },
          el("div", { class: "row" }, svc, el("label", { class: "cb" }, write, "write access"), go),
          el("p", { class: "muted small", text: "The OAuth code comes back through your own relay or a quick tunnel — no inbound port. Services that take an API key are added on the machine: `ferrule connections add <name>`." })));
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
      const edit = el("details", { class: "card", ontoggle: (e) => { if (e.target.open) capsEditor(e.target); } },
        el("summary", { text: "Edit caps" }));
      root.append(secHead("Usage", "the ledger · every call accounted for", pick), this.box, edit);
    },
    async load() {
      let u;
      try {
        u = await api("/api/usage?days=" + this.days);
      } catch (e) { return sectionError(this.box, e); }
      const t = u.total;
      const dayCap = u.caps ? (u.caps.caps || []).find((c) => c.cap === "usd_per_day" && c.limit > 0) : null;
      const group = (rows) => table(["", "calls", "tokens", "cost"], rows.map((r) => ({ cells: [
        text(r.key),
        el("span", { class: "mono", text: String(r.calls) }),
        el("span", { class: "mono", text: num(r.input_tokens + r.output_tokens) }),
        el("span", { class: "mono", text: usd(r.usd) }),
      ]})), [1, 2, 3]);
      setKids(this.box, 
        el("div", { class: "stats" },
          stat("cost", usd(t.usd), u.malformed ? u.malformed + " unreadable lines" : "all lines read"),
          stat("calls", String(t.calls), "errors " + u.error_pct + "% · retried " + u.retry_pct + "%"),
          stat("tokens in", num(t.input_tokens), num(t.cached_input_tokens) + " cached"),
          stat("tokens out", num(t.output_tokens), "cache hit " + u.cache_hit_pct + "%"),
          stat("latency", "p50 " + u.p50_ms + " ms", "p95 " + u.p95_ms + " ms")),
        el("p", { class: "muted small", text: "egress refused: " + (u.egress_refused.count ? u.egress_refused.count + " · " + u.egress_refused.hosts.map((h) => h.host + " ×" + h.count).join(", ") : "none") }),
        u.caps ? el("h3", { text: "Caps" }) : null,
        u.caps ? capsCard(u.caps, el("span", { class: "muted small", text: "Edit caps below — 0 turns one off." })) : null,
        el("div", { class: "grid2" },
          el("div", {}, el("h3", { text: "Cost per day" }),
            bars(u.per_day.map((d) => ({ k: String(d.key).slice(-5), v: d.usd })), usd, dayCap ? dayCap.limit : 0)),
          el("div", {}, el("h3", { text: "Tokens per day" }),
            bars(u.per_day.map((d) => ({ k: String(d.key).slice(-5), v: d.input_tokens + d.output_tokens })), num))),
        el("h3", { text: "Per model" }),
        u.per_model.length ? table(["model", "calls", "errors", "cache", "p50 / p95", "cost"], u.per_model.map((r) => ({ cells: [
          frag(el("span", { class: "mono", text: r.provider + "/" + r.model }), el("span", { class: "sub", text: r.shape })),
          String(r.calls), String(r.errors), r.cache_hit_pct + "%",
          el("span", { class: "mono", text: r.p50_ms + " / " + r.p95_ms + " ms" }),
          r.notional_usd != null
            ? frag(el("span", { class: "mono", text: usd(r.usd) }), el("span", { class: "sub", text: "plan; " + usd(r.notional_usd) + " at API prices" }))
            : r.priced_calls < r.calls
              ? frag(el("span", { class: "mono", text: usd(r.usd) }), el("span", { class: "sub warn", text: (r.calls - r.priced_calls) + " unpriced — no per-token price" }))
              : el("span", { class: "mono", text: usd(r.usd) }),
        ]})), [1, 2, 3, 4, 5]) : el("div", { class: "empty", text: "no calls in this window" }),
        el("div", { class: "grid2" },
          el("div", {}, el("h3", { text: "Per task" }), u.per_task.length ? group(u.per_task) : el("div", { class: "empty", text: "no scheduled runs" })),
          el("div", {}, el("h3", { text: "Per chat" }), u.per_chat.length ? group(u.per_chat) : el("div", { class: "empty", text: "no chats" }))));
    },
  };

  sections.tasks = {
    every: 10,
    mount(root) { this.box = el("div"); root.append(this.box); },
    async load() {
      let r;
      try {
        r = await api("/api/tasks");
      } catch (e) { return sectionError(this.box, e); }
      setKids(this.box, 
        secHead("Tasks", r.tasks.length + " scheduled · gates skip at zero token cost"),
        r.paused ? el("div", { class: "alert bad" },
          el("span", { class: "sev", text: "rust" }),
          el("div", { class: "body" }, el("div", { class: "what", text: "The kill switch is on: nothing runs." }))) : null,
        r.tasks.length ? r.tasks.map((t) => el("div", { class: "task" },
          el("div", { class: "head" },
            el("span", { class: "name msg", dir: "auto", text: t.name || t.id }),
            tag(t.kind),
            t.enabled ? tag("on", "ok") : tag("paused", "warn"),
            t.builtin ? tag("built in") : null,
            el("span", { class: "spacer", style: "flex:1" }),
            el("span", { class: "cron", text: t.schedule + (t.kind === "cron" ? "  ·  " + t.timezone : "") })),
          kv([
            ["next", t.enabled ? el("span", { class: "mono", text: ago(t.next_run_at) }) : tag("paused", "warn")],
            ["delivers to", text(t.destination)],
            ["model", el("span", { class: "mono", text: t.model || "the default" })],
          ]),
          t.runs.length ? el("div", { class: "runs" }, t.runs.map((x) => el("span", {
            class: "run-dot " + (x.status === "succeeded" ? "ok" : x.status === "skipped" ? "warn" : x.status === "running" ? "" : "bad"),
            title: (x.detail || "") + " · " + ago(x.started_at),
          }, x.status + " · " + ago(x.started_at)))) : el("p", { class: "muted small", text: "never ran" }),
          t.runs.slice(0, 1).map((x) => x.detail ? el("div", { class: "muted small msg", dir: "auto", text: x.detail, style: "margin-top:6px" }) : null),
          el("div", { class: "row", style: "margin-top:10px" },
            t.enabled ? btn("Pause", "tasks/pause", { id: t.id }) : btn("Resume", "tasks/resume", { id: t.id }, "primary"),
            btn("Run now", "tasks/run", { id: t.id }),
            ask("Schedule", "tasks/schedule", t.kind === "cron" ? "Cron schedule (5 fields), then optionally a space and an IANA timezone:" : "When (RFC 3339):",
              t.kind === "cron" ? t.schedule + " " + t.timezone : t.schedule, (v) => {
                const [schedule, timezone] = t.kind === "cron" ? splitTz(v) : [v.trim(), null];
                return { id: t.id, schedule, timezone };
              }),
            ask("Model", "tasks/model", "The model it runs on (provider/model, a provider or an alias), or \"default\":",
              t.model || "default", (v) => ({ id: t.id, model: v.trim() })),
            t.builtin ? null : btn("Delete", "tasks/delete", { id: t.id }, "danger"))))
          : el("div", { class: "empty", text: "no tasks" }));
    },
  };

  sections.logs = {
    every: 0,
    page: 0,
    mount(root) {
      this.kind = el("select", {}, [["all", "everything"], ["audit", "audit log"], ["warn", "warnings and errors"]].map(([v, t]) => el("option", { value: v, text: t })));
      this.q = el("input", { type: "search", placeholder: "filter — Hebrew works as-is", dir: "auto", size: "28" });
      this.box = el("div");
      const go = () => { this.page = 0; this.load(); };
      this.kind.onchange = go;
      this.q.addEventListener("change", go);
      root.append(secHead("Logs", "audit + warnings · redacted · never transcripts"),
        el("div", { class: "row" }, this.kind, this.q, el("button", { text: "Reload", onclick: () => this.load() })),
        this.box);
    },
    async load() {
      let r;
      try {
        r = await api("/api/logs?" + new URLSearchParams({ kind: this.kind.value, q: this.q.value, page: this.page }));
      } catch (e) { return sectionError(this.box, e); }
      const pages = Math.max(1, Math.ceil(r.total / r.per_page));
      const lvl = (x) => {
        const l = String(x.level).toLowerCase();
        return /error|fail|engage/.test(l) ? "bad" : /warn/.test(l) ? "warn" : x.kind === "audit" ? "audit" : "info";
      };
      setKids(this.box, 
        r.rows.length ? el("div", { class: "logwrap" }, r.rows.map((x) =>
          el("div", { class: "logline" },
            el("span", { class: "when", text: String(x.at).replace("T", " ").slice(5, 19) }),
            el("span", { class: "lvl " + lvl(x), text: x.level }),
            el("span", { class: "txt", dir: "auto" }, x.tree ? el("span", { class: "muted", text: x.tree + "  " }) : null, text(x.text))))) : el("div", { class: "empty", text: "no log lines match" }),
        el("div", { class: "pager" },
          el("button", { text: "← Newer", disabled: this.page === 0, onclick: () => { this.page--; this.load(); } }),
          el("span", { text: "page " + (r.page + 1) + " of " + pages + " · " + r.total + " entries" }),
          el("button", { text: "Older →", disabled: this.page + 1 >= pages, onclick: () => { this.page++; this.load(); } })));
    },
  };

  sections.extensions = {
    every: 0,
    mount(root) { this.box = el("div"); root.append(this.box); },
    async load() {
      let x;
      try {
        x = await api("/api/settings");
      } catch (e) { return sectionError(this.box, e); }
      const w = x.workspace_hooks;
      setKids(this.box, 
        secHead("Extensions", "mcp · skills · hooks — every change is an audit line"),
        el("h3", { text: "MCP servers" }),
        x.mcp.length ? table(["name", "runs", "origin", ""], x.mcp.map((m) => ({ dead: m.disabled, cells: [
          frag(el("strong", { text: m.name }), m.disabled ? frag(" ", tag("off", "warn")) : null),
          el("span", { class: "mono small", text: m.runs }),
          tag(m.origin),
          el("div", { class: "row" },
            m.origin === "configured" ? (m.disabled ? btn("Enable", "mcp/enable", { name: m.name }) : btn("Disable", "mcp/disable", { name: m.name })) : null,
            btn("Remove", "mcp/remove", { name: m.name }, "danger")),
        ]}))) : el("div", { class: "empty", text: "none configured" }),
        el("h3", { text: "Skills" }),
        !x.skills_enabled ? el("div", { class: "empty", text: "skills are off" })
          : x.skills.length ? table(["skill", "scope", "what", ""], x.skills.map((s) => ({
            dead: s.disabled,
            cells: [
              frag(el("span", { class: "mono", text: s.name }), s.disabled ? frag(" ", tag("off", "warn")) : null),
              tag(s.scope),
              text(s.description),
              s.disabled ? btn("Enable", "skills/enable", { name: s.name }) : btn("Disable", "skills/disable", { name: s.name }),
            ],
          }))) : el("div", { class: "empty", text: "none found" }),
        x.skills_disabled.length ? el("p", { class: "muted small" }, "Disabled in the config: ",
          x.skills_disabled.map((n, i) => frag(i ? " · " : "", n, " ", btn("Enable", "skills/enable", { name: n })))) : null,
        el("h3", { text: "Hooks" }),
        x.hooks.length ? table(["event", "matcher", "command"], x.hooks.map((h) => ({ cells: [
          tag(h.event), el("span", { class: "mono", text: h.matcher || "*" }), el("code", { text: h.command }),
        ]}))) : el("div", { class: "empty", text: "none in the config" }),
        w ? el("div", { class: "alert " + (w.trusted ? "ok" : "warn"), style: "display:block" },
          el("div", { class: "row" },
            el("strong", { text: "Workspace hooks" }),
            el("code", { text: w.file }),
            w.trusted ? tag("trusted", "ok") : tag(w.trusted_sha ? "changed since trusted" : "not trusted", "warn")),
          kv([
            ["SHA-256", el("code", { text: w.sha })],
            w.trusted_sha && !w.trusted ? ["trusted", el("code", { text: w.trusted_sha })] : null,
            w.project ? null : ["note", "They won't run until [hooks] project = true is in the config."],
          ]),
          w.parse_error ? el("p", { class: "bad msg", dir: "auto", text: w.parse_error }) : null,
          w.hooks.length ? table(["event", "matcher", "command"], w.hooks.map((h) => ({ cells: [
            tag(h.event), el("span", { class: "mono", text: h.matcher || "*" }), el("code", { text: h.command }),
          ]}))) : null,
          w.diff ? frag(
            el("h4", { text: "What changed since it was trusted" }),
            el("pre", {}, w.diff.map((d) => el("div", { class: d.op === "+" ? "diff-add" : d.op === "-" ? "diff-del" : "diff-ctx", text: d.op + " " + d.line }))))
            : el("details", {}, el("summary", { text: "The file" }), el("pre", { class: "msg", dir: "auto", text: w.text })),
          el("div", { class: "row" },
            w.trusted || w.parse_error ? null : btn("Trust this version", "hooks/trust", { sha: w.sha }, "primary"),
            w.trusted_sha ? btn("Untrust", "hooks/untrust", {}, "danger") : null,
            el("span", { class: "muted small", text: "Trust is pinned to the hash: any later edit needs trusting again." }))) : null);
    },
  };

  sections.agents = {
    every: 5,
    mount(root) { this.box = el("div"); root.append(this.box); },
    async load() {
      let r;
      try {
        r = await api("/api/agents");
      } catch (e) { return sectionError(this.box, e); }
      setKids(this.box, 
        secHead("Agents", "sub-agents · read-only · isolated contexts, summaries back only"),
        r.agents.length ? table(["agent", "status", "task", "tokens", "model"], r.agents.map((a) => ({ cells: [
          frag(el("strong", { text: a.name }), el("span", { class: "sub", text: a.role + " · depth " + a.depth })),
          a.status === "running" ? frag(led("ok pulse"), " ", tag("running", "ok")) : tag(a.status, a.status === "failed" ? "bad" : null),
          text(a.task),
          el("span", { class: "mono", text: num(a.tokens) }),
          el("span", { class: "mono small", text: a.model || "–" }),
        ]})), [3]) : el("div", { class: "empty", text: "no sub-agents running" }),
        el("p", { class: "muted small", text: "spawn_agent / wait / resume / close — planner, worker and verifier roles, a worktree per child, tree limits and a shared budget." }));
    },
  };

  // ---- navigation and polling --------------------------------------------

  const order = ["health", "models", "routing", "connections", "usage", "tasks", "logs", "extensions", "agents"];
  let current = "health";
  let timer = null;
  let banner = null;
  let lastHealth = null;

  function show(name) {
    current = name;
    for (const a of document.querySelectorAll("#rail a")) a.classList.toggle("on", a.dataset.s === name);
    const root = el("div");
    banner = el("div");
    document.getElementById("main").replaceChildren(banner, root);
    sections[name].mount(root);
    if (location.hash !== "#" + name) history.replaceState(null, "", "#" + name);
    refresh();
  }

  // A pasted or walked-back-to #section link navigates too.
  window.addEventListener("hashchange", () => {
    const h = location.hash.slice(1);
    if (csrf && order.includes(h) && h !== current) show(h);
  });

  // `auto`: a poll, not a click. A section polled by hand only still gets
  // the problems banner. The collar's sweep runs only while a turn does.
  async function refresh(auto) {
    clearTimeout(timer);
    timer = null;
    if (!csrf) return;
    const s = sections[current];
    const name = current;
    try {
      lastHealth = await api("/api/health");
      if (name !== current) return;
      document.getElementById("uptime").textContent = lastHealth.uptime ? "up " + lastHealth.uptime : "";
      if (lastHealth.version) document.getElementById("ver").textContent = "v" + lastHealth.version;
      const liveLed = document.getElementById("live-led");
      if (liveLed) liveLed.className = "led ok pulse";
      document.body.classList.toggle("running",
        (lastHealth.turns || []).some((t) => t.busy_secs !== null && t.busy_secs !== undefined));
      const p = problems(lastHealth.problems);
      setKids(banner, p);
      // Not while the owner is typing or choosing in this section.
      const f = document.activeElement;
      const busy = f && /^(INPUT|SELECT|TEXTAREA)$/.test(f.tagName) && document.getElementById("main").contains(f);
      if (!busy && !(auto === true && !s.every)) await s.load(lastHealth);
    } catch (e) {
      if (!csrf) return;
      const liveLed = document.getElementById("live-led");
      if (liveLed) liveLed.className = "led bad";
      banner.replaceChildren(el("div", { class: "alert bad" },
        el("span", { class: "sev", text: "rust" }),
        el("div", { class: "body" }, el("div", { class: "what msg", dir: "auto", text: e.message }))));
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

  // The collar mark, the theme toggle: present before any login, so even
  // the logged-out card looks like ferrule.
  function bootChrome() {
    const s = el("svg", { class: "collar", viewBox: "0 0 26 26", role: "img" });
    s.append(
      el("line", { class: "bar", x1: 6.5, y1: 19.5, x2: 19.5, y2: 6.5 }),
      el("circle", { class: "ring", cx: 13, cy: 13, r: 9.5, "stroke-dasharray": "44.7 15", transform: "rotate(135 13 13)" }),
      el("circle", { class: "sweep", cx: 13, cy: 13, r: 9.5 }));
    document.getElementById("mark").replaceChildren(s);

    // Theme: paper (light) ↔ forge (dark), kept in localStorage; the head
    // script already applied the saved or system choice before first paint.
    const html = document.documentElement;
    const apply = (forge) => {
      html.setAttribute("data-theme", forge ? "forge" : "paper");
      document.getElementById("theme").textContent = forge ? "paper" : "forge";
      try { localStorage.setItem("ferrule-theme", forge ? "forge" : "paper"); } catch (_) { /* no storage */ }
    };
    apply(html.getAttribute("data-theme") === "forge");
    document.getElementById("theme").onclick = () => apply(html.getAttribute("data-theme") !== "forge");
  }

  function buildRail() {
    const rail = document.getElementById("rail");
    rail.replaceChildren(...order.map((s, i) => {
      const a = el("a", { href: "#" + s },
        el("span", { class: "n", text: String(i + 1).padStart(2, "0") }),
        s);
      a.dataset.s = s;
      a.onclick = (e) => { e.preventDefault(); show(s); };
      return a;
    }), el("div", { class: "rail-foot" },
      frag(el("b", { text: "session" }), el("br"), "12 h max · idle 30 min", el("br"),
        el("b", { text: "login" }), el("br"), "one-use link")));
    rail.hidden = false;
  }

  async function start() {
    bootChrome();
    // A login link carries its token after '#': the browser never sends it
    // to a server, and it's gone from the address bar at once. A section
    // name after '#' is just navigation, from a saved tab.
    const hash = location.hash.length > 1 ? location.hash.slice(1) : "";
    if (hash && !order.includes(hash)) {
      history.replaceState(null, "", "/");
      try {
        csrf = (await api("/api/login", { token: hash })).csrf;
      } catch (e) { loggedOut(e.message); return; }
    } else {
      try { csrf = (await api("/api/session")).csrf; } catch (_) { return; }
    }
    const logout = document.getElementById("logout");
    logout.hidden = false;
    logout.onclick = async () => {
      try { await api("/api/logout", {}); } catch (_) { /* gone anyway */ }
      loggedOut("Logged out.");
    };
    document.getElementById("live").hidden = false;
    buildRail();
    show(order.includes(hash) ? hash : "health");
  }

  document.addEventListener("DOMContentLoaded", start);
})();
