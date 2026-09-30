// The ferrule dashboard (docs/dashboard.md, docs/m37-control-room.md): the
// control room Max runs his agent from. Plain DOM, built with textContent
// only: nothing the server sends is ever parsed as HTML. User and agent
// text sits in `dir="auto"` elements so Hebrew and Arabic read right to
// left. Nothing here calls a model: the page works when every model is
// down. Secrets go one way: typed in, sent, wiped; none ever comes back.
"use strict";
(function () {
  let csrf = null;
  // Served under a prefix behind the panel's proxy (M44): every address the page asks for starts with it.
  const BASE = (document.querySelector('meta[name="ferrule-base"]')?.content || "/").replace(/\/$/, "");

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

  async function api(path, body, extra) {
    const opts = Object.assign({ credentials: "same-origin", headers: {} }, extra || {});
    if (body !== undefined) {
      opts.method = "POST";
      opts.headers["Content-Type"] = "application/json";
      if (csrf) opts.headers["X-Ferrule-Csrf"] = csrf;
      opts.body = JSON.stringify(body);
    }
    const r = await fetch(BASE + path, opts);
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
    document.getElementById("tabs").hidden = true;
    document.getElementById("sheet").hidden = true;
    document.getElementById("logout").hidden = true;
    document.getElementById("live").hidden = true;
    document.getElementById("find").hidden = true;
    document.body.classList.remove("running");
    const main = document.getElementById("main");
    main.replaceChildren(el("div", { class: "card" },
      el("p", { text: why || "Not logged in." }),
      el("p", { class: "muted", text: "Send /dashboard to the bot on Telegram for a new link, or run `ferrule dashboard link` on the machine." })));
  }

  // ---- small helpers -----------------------------------------------------

  // Toasts stack in #toast. A good one goes by itself; an error stays until
  // it is closed, so nobody has to read fast.
  function toast(t, kind) {
    kind = kind === true ? "bad" : kind || "";
    const box = document.getElementById("toast");
    for (const old of box.children) if (old.dataset.text === t) old.remove();
    const card = el("div", { class: "t " + kind, role: kind === "bad" ? "alert" : null },
      el("span", { class: "txt", dir: "auto", text: t }),
      el("button", { class: "ghost icon", "aria-label": "Close", onclick: () => card.remove() }, icon("x")));
    card.dataset.text = t;
    box.append(card);
    while (box.children.length > 3) box.firstChild.remove();
    if (kind !== "bad") setTimeout(() => card.remove(), 5000);
  }

  // The page's own confirm() and prompt(): a native <dialog>, so focus stays
  // inside, Escape closes it and it mirrors right to left. Resolves true or
  // false; with `input`, the typed text, or null when cancelled.
  function ask({ title, text: body, confirm, cancel, danger, input }) {
    return new Promise((resolve) => {
      const back = document.activeElement;
      const d = el("dialog", { class: "dlg", "aria-labelledby": "dlg-title" });
      const box = input ? el("input", { name: "v", value: input.value || "", dir: "auto", autocomplete: "off", "aria-label": input.label || title }) : null;
      let answer = input ? null : false;
      const no = el("button", { type: "button", text: cancel || "Cancel", autofocus: danger || null, onclick: () => d.close() });
      const yes = el("button", { type: "submit", class: danger ? "danger primary" : "primary", text: confirm || "OK" });
      d.append(el("form", {
        method: "dialog",
        onsubmit: (e) => { e.preventDefault(); answer = input ? box.value : true; d.close(); },
      },
        el("h3", { id: "dlg-title", dir: "auto", text: title }),
        body ? el("p", { class: "msg", dir: "auto", text: body }) : null,
        box,
        el("div", { class: "row" }, no, yes)));
      d.addEventListener("close", () => { d.remove(); if (back && back.focus) back.focus(); resolve(answer); });
      document.body.append(d);
      d.showModal();
      if (box) box.select();
    });
  }

  // A link the server wants opened in another tab (a sign-in page): a
  // popup opened after a fetch is blocked on a phone, a tapped link is not.
  function linksDialog(links) {
    const good = links.filter((l) => /^https?:\/\//i.test(l.url));
    if (!good.length) return;
    const d = el("dialog", { class: "dlg", "aria-labelledby": "dlg-title" });
    d.append(el("form", { method: "dialog" },
      el("h3", { id: "dlg-title", text: "Continue in your browser" }),
      good.map((l) => el("a", { class: "btn primary", href: l.url, target: "_blank", rel: "noopener", dir: "auto" }, icon("external"), l.text || "Open")),
      el("div", { class: "row" }, el("button", { text: "Done" }))));
    d.addEventListener("close", () => d.remove());
    document.body.append(d);
    d.showModal();
  }

  // A POST; a destructive one comes back 409 with its question, and goes
  // again with `confirm` once the owner says yes. The button shows it is
  // working and can't be pressed twice.
  async function act(path, body, button) {
    if (button) {
      if (button.getAttribute("aria-busy") === "true") return null;
      button.setAttribute("aria-busy", "true");
    }
    try {
      let r;
      try {
        r = await api("/api/" + path, body || {});
      } catch (e) {
        if (e.status !== 409 || !e.data || !e.data.confirm) throw e;
        const yes = await ask({ title: "Are you sure?", text: e.data.confirm, confirm: "Yes, do it", danger: true });
        if (!yes) return null;
        r = await api("/api/" + path, Object.assign({}, body, { confirm: true }));
      }
      if (r.said) toast(r.said, r.ok === false ? "bad" : "");
      if (r.links) linksDialog(r.links);
      // What was typed next to the button went in: the redraw may drop it.
      const box = button && button.closest(".card, .opt, .check, .alert");
      if (box) box.querySelectorAll("[data-dirty]").forEach((i) => { delete i.dataset.dirty; });
      if (path === "doctor/run" && r.id) doctor.watch(r.id);
      refresh();
      return r;
    } catch (e) {
      toast(e.message, true);
      return null;
    } finally {
      if (button) button.removeAttribute("aria-busy");
    }
  }

  function btn(label, path, body, cls) {
    const b = el("button", { class: cls || null, text: label });
    b.onclick = () => act(path, typeof body === "function" ? body() : body, b);
    return b;
  }

  // A button of the page's kinds: "primary", "danger", "ghost", "link", with
  // an optional icon; `aria` names an icon-only one.
  function button(label, o) {
    o = o || {};
    const b = el("button", { type: "button", class: o.kind || null, "aria-label": o.aria || null, title: o.aria || null },
      o.icon ? icon(o.icon) : null, label ? el("span", { text: label }) : null);
    if (o.onclick) b.onclick = o.onclick;
    return b;
  }

  // A button that asks for one value first, then POSTs.
  function askBtn(label, path, question, now, body) {
    const b = el("button", { text: label });
    b.onclick = async () => {
      const v = await ask({ title: label, text: question, confirm: "Save", input: { value: now } });
      if (v === null || v.trim() === "" || v.trim() === now) return;
      act(path, body(v.trim()), b);
    };
    return b;
  }

  // A card with an optional head: an icon, a title, and things on the right.
  function card(head, ...body) {
    const h = head && (head.title || head.icon || head.right)
      ? el("div", { class: "card-head" }, head.icon ? icon(head.icon) : null,
        head.title ? el("h3", { text: head.title }) : null, head.right || null)
      : null;
    return el("div", { class: "card" + (head && head.cls ? " " + head.cls : "") }, h, body);
  }

  // What a list says when it has nothing: what is missing, why, and what to do.
  function empty(what, hint, ...actions) {
    return el("div", { class: "empty" }, icon("info"),
      el("div", { class: "what", text: what }),
      hint ? el("div", { text: hint }) : null,
      actions.length ? el("div", { class: "row" }, actions) : null);
  }

  // Grey blocks where a section's cards will be, until its first answer.
  function skeleton(n) {
    return frag(Array.from({ length: n || 2 }, () => el("div", { class: "skel card", "aria-hidden": "true" })));
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

  // On a phone every row is a card, each cell labelled by its column.
  function table(head, rows, numcols) {
    return el("div", { class: "tbl cards" }, el("table", { class: "cards" },
      el("thead", {}, el("tr", {}, head.map((h, i) => el("th", { class: (numcols || []).includes(i) ? "num" : null, text: h })))),
      el("tbody", {}, rows.map((r) => el("tr", { class: r.dead ? "dead" : null },
        r.cells.map((c, i) => el("td", { class: (numcols || []).includes(i) ? "num" : null, "data-label": head[i] || "" }, c)))))));
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
      el("div", { class: "body" },
        el("div", { class: "what", text: "This section isn't available in this process" }),
        el("div", { class: "fix msg", dir: "auto", text: e.message }))));
  }

  // ---- M37 pieces --------------------------------------------------------

  // Line icons, one path each, drawn in currentColor on a 24-pixel grid.
  // Every one is drawn here; nothing is fetched. A test checks that each
  // name the page asks for exists.
  const ICONS = {
    // the sections
    health: "M3 11l9-8 9 8M5 10v10h14V10M10 20v-6h4v6",
    chat: "M4 5h16v11H9l-5 4z",
    models: "M7 7h10v10H7zM10 3v4M14 3v4M10 17v4M14 17v4M3 10h4M3 14h4M17 10h4M17 14h4",
    channels: "M4 5h11v8H8l-4 3zM9 16v1h7l4 3v-9h-3",
    connections: "M10 14a4 4 0 0 0 5.66 0l3-3a4 4 0 0 0-5.66-5.66l-1 1M14 10a4 4 0 0 0-5.66 0l-3 3a4 4 0 0 0 5.66 5.66l1-1",
    more: "M5 12h.01M12 12h.01M19 12h.01",
    console: "M4 6l6 6-6 6M12 18h8",
    config: "M4 6h9M17 6h3M4 12h3M11 12h9M4 18h11M19 18h1M15 4v4M9 10v4M17 16v4",
    routing: "M6 3v12M6 21a3 3 0 1 0 0-6 3 3 0 0 0 0 6M18 9a3 3 0 1 0 0-6 3 3 0 0 0 0 6M18 9c0 6-12 3-12 6",
    usage: "M4 20V10M10 20V4M16 20v-7M2 20h20",
    tasks: "M9 6h11M9 12h11M9 18h11M4 6l1 1 2-2M4 12l1 1 2-2M4 18l1 1 2-2",
    logs: "M4 6h16M4 12h16M4 18h10",
    extensions: "M4 4h7v7H4zM13 4h7v7h-7zM4 13h7v7H4zM13 13h7v7h-7z",
    agents: "M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8M4 21a8 8 0 0 1 16 0",
    memory: "M5 4h11a3 3 0 0 1 3 3v13H8a3 3 0 0 1-3-3zM5 17a3 3 0 0 1 3-3h11M9 8h6",
    settings: "M12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6M12 5a7 7 0 1 0 0 14 7 7 0 0 0 0-14M12 2v3M12 19v3M2 12h3M19 12h3M4.9 4.9L7 7M17 17l2.1 2.1M4.9 19.1L7 17M17 7l2.1-2.1",
    // things a button does
    search: "M11 4a7 7 0 1 0 0 14 7 7 0 0 0 0-14M16 16l4 4",
    camera: "M4 8h3l2-3h6l2 3h3v11H4zM12 17a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7",
    image: "M4 5h16v14H4zM4 16l5-5 4 4 3-3 4 4M9 9.5h.01",
    copy: "M8 8h11v12H8zM5 16V4h11",
    check: "M5 12.5l4.5 4.5L19 7",
    x: "M6 6l12 12M18 6L6 18",
    retry: "M20 12a8 8 0 1 1-2.6-5.9M20 4v4h-4",
    stop: "M7 7h10v10H7z",
    send: "M12 19V5M6 11l6-6 6 6",
    play: "M8 5l11 7-11 7z",
    pause: "M8 5v14M16 5v14",
    plus: "M12 5v14M5 12h14",
    edit: "M4 20l1-4L16 5l3 3L8 19zM14 7l3 3",
    trash: "M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6",
    download: "M12 4v11M7 11l5 5 5-5M5 20h14",
    external: "M14 4h6v6M20 4l-9 9M18 14v6H4V6h6",
    power: "M12 3v9M6.3 6.3a8 8 0 1 0 11.4 0",
    chevron: "M9 6l6 6-6 6",
    "chevron-down": "M6 9l6 6 6-6",
    // things a place is
    sun: "M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8M12 2v2M12 20v2M2 12h2M20 12h2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4",
    moon: "M20 14.5A8 8 0 0 1 9.5 4 8 8 0 1 0 20 14.5z",
    monitor: "M3 5h18v11H3zM8 20h8M12 16v4",
    globe: "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18M3 12h18M12 3c3 3 3 15 0 18M12 3c-3 3-3 15 0 18",
    clock: "M12 7v5l3 2M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18",
    calendar: "M4 6h16v14H4zM4 10h16M8 3v4M16 3v4",
    backup: "M3 5h18v4H3zM5 9v10h14V9M10 13h4",
    file: "M6 3h8l4 4v14H6zM14 3v4h4M9 12h6M9 16h6",
    key: "M8 11a4 4 0 1 0 0 8 4 4 0 0 0 0-8M11 12l9-9M16 7l3 3M14 9l2 2",
    lock: "M6 11h12v9H6zM8 11V8a4 4 0 0 1 8 0v3",
    telegram: "M21 4L3 11l6 2 2 7 3-5 5 4zM9 13l12-9",
    // things the page says
    alert: "M12 3l10 18H2zM12 10v5M12 18h.01",
    info: "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18M12 11v5M12 8h.01",
    help: "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18M9.5 9.5a2.5 2.5 0 1 1 3.5 2.3c-.7.4-1 .9-1 1.7M12 17h.01",
  };
  // Decoration by default (hidden from a screen reader); with a label it
  // is an image of its own.
  function icon(name, label) {
    const s = el("svg", { class: "ico" + (name === "chevron" ? " flip" : ""), viewBox: "0 0 24 24", "aria-hidden": label ? null : "true", role: label ? "img" : null, "aria-label": label || null });
    s.append(el("path", { d: ICONS[name] || ICONS.more, "stroke-width": name === "more" ? 3 : null }));
    return s;
  }

  // A <details> that stays open (or shut) across the polls' redraws.
  const opened = new Set();
  function disc(key, cls, summary, ...body) {
    const d = el("details", { class: cls || null, open: opened.has(key) },
      el("summary", {}, summary), ...body);
    d.addEventListener("toggle", () => { if (d.open) opened.add(key); else opened.delete(key); });
    return d;
  }

  // The knobs most people never need, folded away under one word. Open or
  // shut, it stays that way across the polls' redraws.
  function advanced(key, hint, ...body) {
    return disc("adv-" + key, "advanced",
      frag("Advanced", hint ? el("span", { class: "muted small", text: " · " + hint }) : null),
      el("div", { class: "details-body stack" }, ...body));
  }

  // A choice of one, as a row of pressed/unpressed buttons.
  function chips(label, options, value, onpick) {
    return el("div", { class: "chips", role: "group", "aria-label": label },
      options.map(([v, t]) => el("button", {
        type: "button", class: "chip", "aria-pressed": String(v === value), text: t,
        onclick: () => onpick(v),
      })));
  }

  // An on/off switch that says what it switches. The caller does the POST.
  function toggle(label, on, onclick) {
    return el("button", { type: "button", class: "switch", role: "switch", "aria-checked": String(!!on), "aria-label": label, onclick },
      el("span", { class: "knob" }));
  }

  // A button that copies `value`; says so, or selects it to copy by hand.
  function copyBtn(value) {
    const b = el("button", { text: "Copy" });
    b.onclick = async () => {
      try { await navigator.clipboard.writeText(value); b.textContent = "Copied"; } catch (_) {
        toast("Copy it by hand: " + value);
      }
      setTimeout(() => { b.textContent = "Copy"; }, 1600);
    };
    return b;
  }

  // Inputs for a form: a secret is a password box, emptied once it's sent.
  function field(f, value) {
    const attrs = { name: f.name, placeholder: f.placeholder || null, dir: "auto", autocomplete: f.secret ? "new-password" : "off" };
    let i;
    if (f.kind === "textarea") i = el("textarea", Object.assign({ rows: 5, spellcheck: "false" }, attrs));
    else i = el("input", Object.assign({ type: f.secret ? "password" : f.kind === "email" ? "email" : f.kind === "date" ? "date" : "text" }, attrs));
    if (value) i.value = value;
    if (f.secret) i.dataset.secret = "1";
    return el("label", { class: "field" },
      el("span", { text: f.label + (f.optional ? " (optional)" : "") }),
      i,
      f.hint ? el("span", { class: "hint msg", dir: "auto", text: f.hint }) : null);
  }
  const values = (box) => {
    const out = {};
    box.querySelectorAll("input[name], textarea[name], select[name]").forEach((i) => {
      if (i.type === "checkbox") return;
      if (i.value.trim() !== "") out[i.name] = i.value.trim();
    });
    return out;
  };
  const wipeSecrets = (box) => box.querySelectorAll("[data-secret]").forEach((i) => { i.value = ""; delete i.dataset.dirty; });

  // ---- notices (M37 §1) --------------------------------------------------
  // Each strip says what's wrong, has a button per fix that can run from
  // here, and closes for a day unless it's one of the two that never do.

  function fixButton(f) {
    if (f.action) return btn(f.label, f.action, f.body || {}, "primary");
    if (f.section) return el("button", { text: f.label, onclick: () => show(f.section, f.tile ? { tile: f.tile } : null) });
    return null;
  }

  function notice(p) {
    const sev = p.top || p.action === "kill/off" ? "bad" : "warn";
    const fixes = (p.fixes || []).map(fixButton);
    if (!fixes.length) {
      if (p.suggest) fixes.push(btn("Make " + p.suggest + " the default", "models/default", { model: p.suggest }, "primary"));
      if (p.action) fixes.push(btn(p.action === "kill/off" ? "Turn the kill switch off" : "Fill missing prices", p.action, {}, "primary"));
      if (p.section && p.section !== current) fixes.push(el("button", { text: "Open " + p.section, onclick: () => show(p.section) }));
    }
    return el("div", { class: "alert " + sev, "data-notice": p.id || null },
      el("div", { class: "body" },
        el("div", { class: "what msg", dir: "auto", text: p.what }),
        p.fix ? el("div", { class: "fix msg", dir: "auto", text: p.fix }) : null,
        p.back ? el("div", { class: "fix", text: "Back: it's still true a day later." }) : null,
        fixes.length ? el("div", { class: "row" }, fixes) : null),
      p.id && p.closable ? el("button", { class: "x", title: "Hide for a day", "aria-label": "Hide for a day", text: "×",
        onclick: (e) => act("notices/dismiss", { id: p.id }, e.currentTarget) }) : null);
  }

  function problems(list) {
    if (!list || !list.length) return null;
    return el("div", {}, list.map(notice));
  }

  // Away from Home, only this section's problems show in full; the rest are
  // one line that leads there, so a phone's first screen is the section.
  function otherProblems(list) {
    if (!list.length) return null;
    return el("a", { class: "strip", href: "#health" }, icon("alert"),
      el("span", { class: "grow", text: list.length + (list.length === 1 ? " other thing needs" : " other things need") + " your attention" }),
      el("span", { text: "Home" }), icon("chevron"));
  }

  function hiddenNotices(list) {
    if (!list || !list.length) return null;
    return disc("hidden-notices", "card", frag(list.length + " hidden ", el("span", { class: "muted small", text: "· closed for a day, still true" })),
      el("div", { class: "details-body stack" },
        list.map((p) => el("div", { class: "row" },
          el("span", { class: "grow msg", dir: "auto", text: p.what }),
          p.until ? el("span", { class: "muted small", text: "back " + ago(p.until) }) : null,
          btn("Show again", "notices/restore", { id: p.id }))),
        el("div", { class: "row end" }, btn("Show all again", "notices/restore", {}))));
  }

  // ---- approvals: the owner's questions from any chat (M37 §4.3) ---------

  function approvalsCard(list) {
    if (!list || !list.length) return null;
    return el("div", { class: "card" },
      el("h3", { text: "Waiting for you", class: "mt0" }),
      list.map((a) => el("div", { class: "check" },
        led("warn pulse"),
        el("div", { class: "body" },
          el("div", { class: "t msg", dir: "auto", text: a.what }),
          el("div", { class: "d", text: a.chat + " · asked " + secs(a.secs) + " ago" })),
        el("div", { class: "row" },
          btn("Allow", "approvals/answer", { code: a.code, allow: true }, "primary"),
          btn("Refuse", "approvals/answer", { code: a.code, allow: false }, "danger")))));
  }

  // ---- doctor, as a run the page follows (M37 §1.3) ----------------------

  const doctor = {
    id: null, text: "", view: null, box: el("div"),
    async watch(id) {
      this.id = id; this.text = ""; this.view = null;
      if (current !== "health") show("health");
      let from = 0;
      while (this.id === id) {
        let v;
        try { v = await api("/api/run?" + new URLSearchParams({ id, from })); } catch (e) { toast(e.message, true); break; }
        this.text += v.text; from = v.next; this.view = v;
        this.draw();
        if (v.done) break;
        await new Promise((r) => setTimeout(r, 700));
      }
    },
    draw() {
      const v = this.view;
      if (!v) { setKids(this.box); return; }
      const items = v.report && v.report.items ? v.report.items : null;
      const bad = (i) => i.level === "warn" || i.level === "fail";
      setKids(this.box, el("div", { class: "card" },
        el("div", { class: "row" },
          el("strong", { class: "grow", text: v.label }),
          v.done ? tag(v.code === 0 ? "done" : "exit " + v.code, v.code === 0 ? "ok" : "warn") : tag("running " + secs(v.secs)),
          el("button", { class: "ghost", text: "Close", onclick: () => { this.id = null; this.view = null; this.draw(); } })),
        items ? el("div", {}, items.filter(bad).map((i) => el("div", { class: "check" },
          led(i.level === "fail" ? "bad" : "warn"),
          el("div", { class: "body" },
            el("div", { class: "t", text: i.what }),
            el("div", { class: "d msg", dir: "auto", text: [i.text].concat(i.hints || []).join("\n") })),
          (i.fixes || []).length ? el("div", { class: "row" }, i.fixes.map(fixButton)) : null)),
          items.some(bad) ? null : el("p", { class: "ok", text: "Every check passed (" + items.length + ")." }))
          : el("pre", { class: "out msg", dir: "auto", text: this.text || "…" })));
    },
  };

  // ---- sections ----------------------------------------------------------
  // Each: `mount(root)` builds the static part once, `load()` refreshes the
  // live part; `every` is its polling period in seconds (0: by hand only);
  // `live`: it polls even while the owner types (it keeps its own inputs).

  const sections = {};

  // ---- words a stranger may not know (M47) --------------------------------
  // `tip("fallback")` is the word as a button; it opens its plain meaning in
  // place (no hover: a phone has none).
  const GLOSSARY = {
    model: "The AI that reads your message and writes the answer. Different models cost different amounts.",
    fallback: "A second model to use when the first one is down, so a chat keeps going.",
    cap: "A limit on how much your bot may spend or do in a day. When it's reached, the bot stops and says so.",
    turn: "One message from you and everything the bot does to answer it.",
    approval: "A question the bot asks you before doing something risky, like running a command. You answer Allow or Refuse.",
    channel: "A place you talk to your bot from: Telegram, Discord, Slack, this page.",
    connection: "An account your bot signs in to, like Google or GitHub, so it can work there for you.",
    MCP: "A standard way to plug extra tools into your bot, such as a calendar or a database.",
    skill: "A written how-to your bot reads when a task calls for it.",
    hook: "A small command that runs by itself when something happens, like before a tool is used.",
    cron: "A schedule, written as five fields (minute, hour, day, month, weekday). \"0 9 * * *\" is every day at 9:00.",
    routing: "Starting each turn on a cheap model and moving to a stronger one only when the cheap one fails.",
    tunnel: "A private web address that lets you open this page from your phone while your bot runs at home.",
    token: "A secret code that proves who you are to a service. Keep it private.",
    lane: "One conversation's line: messages in a chat wait their turn so answers don't mix.",
    watchdog: "A timer that notices when a turn has made no progress for a while and says so.",
    heartbeat: "A regular \"I'm alive\" signal the bot sends so you know it's running.",
    relay: "A helper server that passes messages between your bot and a service that can't reach it directly.",
    pin: "Fixing one chat to one model, whatever the default is.",
    alias: "A short nickname for a model, like \"fast\".",
  };
  let tips = 0;
  function tip(term) {
    const def = el("span", { class: "tip-def", id: "tip-" + ++tips, role: "note", text: GLOSSARY[term] || "", hidden: true });
    const b = el("button", { type: "button", class: "tip", "aria-expanded": "false", "aria-controls": def.id, text: term });
    b.onclick = () => { def.hidden = !def.hidden; b.setAttribute("aria-expanded", String(!def.hidden)); };
    return frag(b, def);
  }

  // ---- Home (M47): what state it's in, what needs you, what to do next ---

  // The first-run checklist keeps its open panels between polls: a step's
  // form is drawn once into a box that outlives every redraw of the page.
  const home = {
    open: null,
    boxes: {},
    models: null,
    channels: null,
    hidden: () => { try { return localStorage.getItem("ferrule-setup-hidden") === "1"; } catch (_) { return false; } },
    hide() { try { localStorage.setItem("ferrule-setup-hidden", "1"); } catch (_) { /* private mode */ } },
    box(id) { return this.boxes[id] || (this.boxes[id] = el("div", { class: "step-panel stack" })); },
    // The Models section's provider forms, drawn into this box: same code,
    // same calls, so a key typed here is tested and saved the same way.
    modelPanel() {
      const fresh = !this.boxes.model;
      const box = this.box("model");
      if (!this.models) {
        const M = this.models = Object.create(sections.models);
        M.fbEdited = false;
        M.home = true;
        M.drawChoices = function () {
          const ps = (this.choices && this.choices.providers) || [];
          setKids(box, ps.length ? el("div", { class: "tiles" }, ps.map((p) => this.provider(p))) : el("p", { class: "muted", text: "No provider is listed." }));
        };
        M.loadChoices = async function () {
          try { this.choices = await api("/api/models/choices"); } catch (e) { return sectionError(box, e); }
          this.drawChoices();
          refresh();
        };
      }
      if (fresh) this.models.loadChoices();
      return box;
    },
    // The Channels section's Telegram walk-through (token, wait, allow).
    telegramPanel() {
      const fresh = !this.boxes.telegram;
      const box = this.box("telegram");
      if (!this.channels) {
        const C = this.channels = Object.create(sections.channels);
        C.said = {};
        C.load = function () { setKids(box, this.telegram({ configured: false })); };
      }
      if (fresh) this.channels.load();
      return box;
    },
  };

  function status(h) {
    const list = h.problems || [];
    const busy = (h.turns || []).filter((t) => t.busy_secs !== null && t.busy_secs !== undefined);
    if (h.kill && h.kill.on) return ["bad", "Stopped: the kill switch is on, so nothing will run."];
    if (list.some((p) => p.top)) return ["bad", "Your bot needs you: " + list.length + (list.length === 1 ? " thing" : " things") + " to fix."];
    if (list.length) return ["warn", "Your bot is running, with " + list.length + (list.length === 1 ? " thing" : " things") + " to look at."];
    if (busy.length) return ["ok pulse", "Your bot is working on " + (busy.length === 1 ? "a message" : busy.length + " messages") + " now."];
    return ["ok", h.gateway ? "Your bot is running. All quiet." : "All quiet. The gateway isn't running in this process."];
  }

  function checklist(s) {
    const steps = s.steps;
    const n = steps.filter((x) => x.done).length;
    const modelDone = (steps.find((x) => x.id === "model") || {}).done;
    const more = (x) => {
      if (x.id === "hello") {
        const b = button(x.done ? "Open chat" : "Say hello", { kind: x.done ? null : "primary", icon: "send" });
        b.disabled = !modelDone && !x.done;
        b.onclick = async () => {
          if (!x.done) {
            b.setAttribute("aria-busy", "true");
            try { await api("/api/chat/send", { text: "Hi! Introduce yourself in two lines." }); } catch (e) { toast(e.message, true); b.removeAttribute("aria-busy"); return; }
          }
          show("chat");
        };
        return b;
      }
      if (x.done) return null;
      const opened = home.open === x.id;
      const b = button(opened ? "Close" : x.id === "model" ? "Set up" : "Connect", { kind: opened ? null : "primary" });
      b.setAttribute("aria-expanded", String(opened));
      b.onclick = () => { delete home.boxes[x.id]; home.open = opened ? null : x.id; sections.health.draw(); };
      return b;
    };
    return el("section", { class: "card accent", "aria-labelledby": "setup-h" },
      el("div", { class: "card-head" },
        el("h3", { id: "setup-h", text: "Get started" }),
        el("span", { class: "muted small", text: n + " of " + steps.length + " done" }),
        button("Hide this", { kind: "ghost", onclick: () => { home.hide(); sections.health.draw(); } })),
      el("div", { class: "progress", role: "progressbar", "aria-valuemin": "0", "aria-valuemax": String(steps.length), "aria-valuenow": String(n), "aria-label": "Setup progress" },
        steps.map((x) => el("span", { class: x.done ? "seg on" : "seg" }))),
      el("ol", { class: "steps" }, steps.map((x, i) => el("li", { class: "step" + (x.done ? " done" : "") },
        el("span", { class: "mark", "aria-hidden": "true" }, x.done ? icon("check") : String(i + 1)),
        el("div", { class: "body" },
          el("div", { class: "t" }, x.label, x.done ? el("span", { class: "sr", text: " (done)" }) : null,
            x.skippable && !x.done ? el("span", { class: "muted small", text: " · optional" }) : null),
          el("div", { class: "d msg", dir: "auto", text: x.detail })),
        more(x),
        home.open === x.id && !x.done && x.id !== "hello"
          ? (x.id === "model" ? home.modelPanel() : home.telegramPanel()) : null))));
  }

  const QUICK = [
    ["chat", "chat", "Chat", "Talk to your bot here"],
    ["tasks", "tasks", "Tasks", "Things it does on a schedule"],
    ["usage", "usage", "Spending", "What it has cost so far"],
    ["models", "models", "Models", "Pick or add a brain"],
  ];

  sections.health = {
    title: "Home",
    every: 3,
    mount(root) {
      this.box = el("div");
      this.last = null;
      root.append(this.box, doctor.box);
      doctor.draw();
    },
    async load(h) {
      let approvals = [];
      let setup = null;
      await Promise.all([
        api("/api/approvals").then((a) => { approvals = a.approvals; }).catch(() => { /* not here */ }),
        api("/api/setup").then((s) => { setup = s; }).catch(() => { /* an older page's server */ }),
      ]);
      this.last = { h, approvals, setup };
      this.draw();
    },
    draw() {
      if (!this.last) return;
      const { h, approvals, setup } = this.last;
      const kill = h.kill || null;
      const hb = h.heartbeat;
      const ch = h.channels || [];
      const turns = h.turns || [];
      const wd = h.watchdog;
      const [level, said] = status(h);
      const running = turns.filter((t) => t.busy_secs !== null && t.busy_secs !== undefined);
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
      const showList = setup && !setup.done && !home.hidden();
      const tgOpen = setup && setup.done && !(setup.steps.find((x) => x.id === "telegram") || {}).done;
      setKids(this.box,
        el("section", { class: "card hero", "aria-label": "Status" },
          el("div", { class: "row" }, led(level),
            el("p", { class: "lead grow", text: said }),
            btn("Run doctor", "doctor/run", {}),
            kill ? (kill.on ? btn("Kill switch off", "kill/off", {}, "primary") : btn("Kill switch on", "kill/on", {}, "danger")) : null),
          el("p", { class: "muted small m0", text: [h.version ? "v" + h.version : null, h.uptime ? "up " + h.uptime : null, lanes].filter(Boolean).join(" · ") })),
        MANAGED.on ? el("div", { class: "card" },
          el("h3", { text: "Managed" }),
          el("p", { class: "muted small", dir: "auto", text: MANAGED.reason }),
          (MANAGED.locks || []).length ? el("ul", {}, ...MANAGED.locks.map((l) => el("li", { dir: "auto", text: l }))) : null,
          el("p", { class: "small", text: "Commands: " + MANAGED.protection })) : null,
        problems(h.problems),
        showList ? checklist(setup) : null,
        approvalsCard(approvals),
        el("div", { class: "quick" }, QUICK.map(([sec, ic, t, d]) => el("button", { type: "button", class: "quick-tile", onclick: () => show(sec) },
          icon(ic), el("span", { class: "t", text: t }), el("span", { class: "d", text: d }))),
          tgOpen && !showList ? el("button", { type: "button", class: "quick-tile", onclick: () => show("channels", { tile: "telegram" }) },
            icon("telegram"), el("span", { class: "t", text: "Telegram" }), el("span", { class: "d", text: "Talk to your bot from your phone" })) : null),
        running.length ? el("section", { class: "card", "aria-label": "Running now" },
          el("h3", { class: "mt0", text: "Running now" }),
          table(["where", "message", "for", ""], running.map((t) => ({ cells: [
            frag(el("span", { class: "mono small", text: t.place }), t.stuck ? frag(" ", tag("stuck", "bad")) : null),
            frag(text(t.text), t.activity ? el("span", { class: "sub msg", dir: "auto", text: t.activity }) : null),
            el("span", { class: "mono", text: secs(t.busy_secs) }),
            btn("Stop", "turn/stop", { session: t.session }, "danger"),
          ]})))) : null,
        turns.length > running.length ? el("p", { class: "muted small", text: (turns.length - running.length) + " queued behind them." }) : null,
        hiddenNotices(h.hidden),
        disc("home-details", "card", frag("Details ", el("span", { class: "muted small", text: "· gateway, channels, caps" })),
          el("div", { class: "details-body stack" },
            el("p", { class: "muted small m0" }, "A ", tip("channel"), " is where you talk to your bot. The ", tip("watchdog"),
              " and ", tip("heartbeat"), " tell you it is alive; a ", tip("cap"), " limits what it may spend."),
            el("div", { class: "stats" },
              stat("version", h.version ? "v" + h.version : "–"),
              stat("uptime", h.uptime || "–", h.started ? "since " + h.started : null),
              stat("gateway", h.gateway ? "running" : "standalone", h.gateway ? lanes : "the gateway's page has the lanes"),
              stat("watchdog", wd ? (wd.ok ? "ok" : "attention") : "–",
                wd && wd.ok && wd.after_secs ? "no-progress " + secs(wd.after_secs) : (wd && !wd.ok ? "see below" : null)),
              stat("heartbeat", hb ? ago(hb.last_at) : "not set", hb ? hb.host + " · every " + secs(hb.every_secs) : null)),
            rows.length ? kv(rows) : null,
            el("h4", { class: "mb0", text: "Channels" }),
            ch.length ? table(["channel", "polls", "last ok poll", ""], ch.map((c) => ({ cells: [
              frag(led(c.stale ? "bad" : "ok pulse"), " ", c.name,
                c.problem ? el("span", { class: "sub msg", dir: "auto", text: c.problem }) : null),
              c.polls ? "yes" : "no",
              el("span", { class: c.stale ? "bad" : "mono muted", text: ago(c.last_ok_poll) }),
              c.stale ? btn("Restart", "channels/restart", { name: c.name }) : null,
            ]}))) : el("div", { class: "empty", text: "no channel in this process" }),
            h.spend ? el("h4", { class: "mb0", text: "Spend today, against the caps" }) : null,
            h.spend ? capsCard(h.spend) : null)));
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
    title: "Models",
    every: 10,
    mount(root) {
      this.box = el("div");
      this.chooseBox = el("div");
      this.fb = null;
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
      root.append(this.chooseBox, this.box,
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
      this.loadChoices();
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
        el("h3", { text: "Connected models" }),
        el("div", { class: "row mb" }, btn("Fill missing prices", "catalog/fill-prices", {})),
        m.fixed ? el("div", { class: "alert warn" },
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
            frag(el("span", { class: "mono", text: r.context_window ? num(r.context_window) : "–" }),
              r.vision ? el("span", { class: "sub", text: "sees photos" }) : null),
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
        el("h3", { text: "Pin a chat, add a model by id" }),
        el("div", { class: "card" },
          el("div", { class: "row" }, chat, pick("Pin", "models/pin", "model", () => ({ chat: chat.value }))),
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

    // M37 §2: the pickers every model select uses, the fallback list in
    // order, and the providers to connect with a key or a plan.
    async loadChoices() {
      // A half-edited fallback list isn't redrawn under the owner.
      if (this.fbEdited) return;
      let c;
      try { c = await api("/api/models/choices"); } catch (e) { return sectionError(this.chooseBox, e); }
      this.choices = c;
      this.fb = c.fallback.slice();
      this.drawChoices();
    },
    drawChoices() {
      const c = this.choices;
      const label = (m) => m.label + (m.ready ? "" : " (" + (m.missing || "not ready") + ")");
      const def = el("select", { name: "default-model", "aria-label": "Default model" },
        c.default ? null : el("option", { value: "", text: "choose…" }),
        c.models.map((m) => el("option", { value: m.reference, text: label(m), disabled: !m.ready && m.reference !== c.default })));
      def.value = c.default || "";
      const setDef = el("button", { class: "primary", text: "Use" });
      setDef.onclick = () => { if (def.value && def.value !== c.default) act("models/default", { model: def.value }, setDef); };
      const others = c.models.filter((m) => m.reference !== c.default);
      const edited = () => { this.fbEdited = true; this.drawChoices(); };
      const rows = this.fb.map((ref, i) => {
        const s = el("select", { "aria-label": "Fallback " + (i + 1) },
          others.map((m) => el("option", { value: m.reference, text: label(m), disabled: this.fb.includes(m.reference) && m.reference !== ref })));
        s.value = ref;
        s.onchange = () => { this.fb[i] = s.value; edited(); };
        const move = (d) => { const j = i + d; [this.fb[i], this.fb[j]] = [this.fb[j], this.fb[i]]; edited(); };
        return el("div", { class: "row" },
          el("span", { class: "mono muted", text: String(i + 1) }), s,
          el("button", { class: "ghost", text: "↑", title: "Earlier", "aria-label": "Earlier", disabled: i === 0, onclick: () => move(-1) }),
          el("button", { class: "ghost", text: "↓", title: "Later", "aria-label": "Later", disabled: i === this.fb.length - 1, onclick: () => move(1) }),
          el("button", { class: "ghost", text: "×", title: "Take out", "aria-label": "Take out", onclick: () => { this.fb.splice(i, 1); edited(); } }));
      });
      const unused = others.filter((m) => !this.fb.includes(m.reference));
      const save = el("button", { class: "primary", text: "Save the fallback list", disabled: !this.fbEdited });
      save.onclick = async () => {
        const r = await act("models/fallback", { models: this.fb }, save);
        if (r) { this.fbEdited = false; this.loadChoices(); }
      };
      const providers = c.providers || [];
      const on = providers.filter((p) => p.connected);
      const off = providers.filter((p) => !p.connected);
      setKids(this.chooseBox,
        secHead("Models", c.models.length + " connected" + (c.default ? " · default " + c.default : "")),
        el("div", { class: "grid2" },
          el("div", { class: "card" },
            el("h4", { text: "Default", class: "mt0" }),
            el("p", { class: "muted small", text: "Every chat uses it unless the chat is pinned to another." }),
            el("div", { class: "row" }, def, setDef)),
          el("div", { class: "card" },
            el("h4", { text: "Fallback, in order", class: "mt0" }),
            el("p", { class: "muted small", text: "When the default is down, the next one that answers takes the turn." }),
            rows.length ? el("div", { class: "stack fallback" }, rows) : el("div", { class: "empty", text: "no fallback: an outage stops the chat" }),
            el("div", { class: "row mt" },
              unused.length ? el("button", { text: "Add one", onclick: () => { this.fb.push(unused[0].reference); edited(); } }) : null,
              save,
              this.fbEdited ? el("button", { class: "ghost", text: "Discard", onclick: () => { this.fbEdited = false; this.loadChoices(); } }) : null))),
        el("h3", { text: "Providers" }),
        on.length ? el("div", { class: "tiles" }, on.map((p) => this.provider(p))) : el("div", { class: "empty", text: "no provider connected" }),
        off.length ? disc("providers-off", "card", "Connect another provider (" + off.length + ")",
          el("div", { class: "tiles details-body" }, off.map((p) => this.provider(p)))) : null);
    },
    provider(p) {
      const head = el("div", { class: "row" },
        el("strong", { class: "grow", text: p.title }),
        p.connected ? (p.ready ? tag("ready", "ok") : tag("not ready", "bad")) : p.key_set ? tag("key saved", "copper") : null);
      const box = el("div", { class: "card" }, head);
      if (p.plan === "chatgpt" && !p.connected) {
        const f = this.chatgpt;
        box.append(el("p", { class: "muted small", text: "Sign in with your ChatGPT plan: a code shows here, you type it on OpenAI's page. No key." }));
        if (f && f.state === "waiting") {
          box.append(el("p", {}, "On ", el("a", { href: f.page, target: "_blank", rel: "noopener", text: f.page }), " enter:"),
            el("div", { class: "big mono", text: f.user_code }),
            el("div", { class: "row" }, copyBtn(f.user_code), btn("Cancel", "plans/chatgpt/cancel", {}, "danger")));
        } else {
          if (f && f.state === "failed") box.append(el("div", { class: "said bad msg", dir: "auto", text: f.said }));
          const b = el("button", { class: "primary", text: "Sign in" });
          b.onclick = async () => {
            b.disabled = true;
            try { this.chatgpt = await api("/api/plans/chatgpt/start", {}); this.pollChatgpt(); this.drawChoices(); } catch (e) { toast(e.message, true); b.disabled = false; }
          };
          box.append(el("div", { class: "row" }, b));
        }
        return box;
      }
      if (p.plan === "claude-code" && !p.connected && MANAGED.on) {
        box.append(el("p", { class: "muted small", text: MANAGED.claude_plan }));
        return box;
      }
      if (p.plan === "claude-code" && !p.connected) {
        const t = el("input", { type: "password", name: "token", autocomplete: "new-password", placeholder: "sk-ant-oat…", "data-secret": "1" });
        const b = el("button", { class: "primary", text: "Check and save" });
        b.onclick = () => { const v = t.value.trim(); wipeSecrets(box); if (v) act("plans/claude", { token: v }, b); };
        box.append(el("p", { class: "muted small", text: "Run `claude setup-token` on any machine signed in to your Claude plan and paste the token it prints." }),
          el("div", { class: "row" }, t, b));
        return box;
      }
      if (p.needs_key) {
        const k = el("input", { type: "password", name: "key", autocomplete: "new-password", placeholder: p.connected ? "a new key replaces the saved one" : "paste the key", "data-secret": "1" });
        const b = el("button", { class: p.connected ? null : "primary", text: p.connected ? "Replace key" : "Test and save" });
        b.onclick = () => { const v = k.value.trim(); wipeSecrets(box); if (v) act("models/provider", { provider: p.name, key: v }, b).then(() => this.loadChoices()); };
        box.append(frag(
          p.key_url ? el("p", { class: "small" }, el("a", { href: p.key_url, target: "_blank", rel: "noopener", text: "Get a key" }),
            el("span", { class: "muted", text: " · tested before it's saved; it never comes back to this page" })) : null,
          el("div", { class: "row" }, k, b)));
      } else if (!p.connected) {
        box.append(el("div", { class: "row" }, btn("Connect", "models/provider", { provider: p.name }, "primary")));
      }
      if (p.connected || p.key_set) {
        const list = el("div");
        const b = el("button", { text: "List its models" });
        b.onclick = async () => {
          b.disabled = true;
          try {
            const r = await api("/api/models/provider/list?" + new URLSearchParams({ provider: p.name }));
            setKids(list, el("div", { class: "stack mt scroll" },
              r.models.slice(0, 200).map((m) => el("div", { class: "row" },
                el("span", { class: "mono grow", text: m }),
                r.connected.includes(m) ? tag("connected", "ok") : btn("Add", "models/add", { provider: p.name, model: m })))));
          } catch (e) { setKids(list, el("p", { class: "bad small msg", dir: "auto", text: e.message })); }
          b.disabled = false;
        };
        box.append(el("div", { class: "row mt" }, b), list);
      }
      return box;
    },
    async pollChatgpt() {
      while (this.chatgpt && this.chatgpt.state === "waiting") {
        await new Promise((r) => setTimeout(r, 3000));
        try { this.chatgpt = await api("/api/plans/chatgpt/poll"); } catch (_) { break; }
        if (this.chatgpt.state === "done") { toast(this.chatgpt.said); this.chatgpt = null; refresh(); break; }
        if ((current === "models" || (this.home && current === "health")) && !this.fbEdited) this.drawChoices();
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

  // M37 §3: the one place for credentials. On top, the fixed callback
  // address every OAuth sign-in comes back to; then what each service still
  // needs, with the next step as a button; then a tile per service, each
  // way in with what it covers, a guide for the phone, its fields (write-
  // only: nothing typed here ever comes back) and a Test that says what
  // failed. A way in that can't work yet says why instead of a button.
  const CHECK_OK = ["ready", "connected", "available"];
  const day = (unix) => new Date(unix * 1000).toISOString().slice(0, 10);
  const CHECK_BAD = ["unreachable", "domain blocked", "API-token auth off"];

  sections.connections = {
    title: "Connections",
    every: 10,
    said: {},
    mount(root) { this.box = el("div"); root.append(this.box); },
    async load() {
      let c, list = null;
      try {
        c = await api("/api/connections");
      } catch (e) { return sectionError(this.box, e); }
      if (!c.available) {
        setKids(this.box,
          secHead("Connections", "outside services the agent may use"),
          el("div", { class: "empty", text: "Connections aren't set up in this process: run `ferrule connections setup` on the machine." }));
        return;
      }
      try { list = await api("/api/connections/checklist"); } catch (_) { /* the rest still works */ }
      this.c = c;
      this.aim(c);
      setKids(this.box,
        secHead("Connections", c.connections.length + " connected · read-only unless you say otherwise"),
        c.asked.length ? el("div", { class: "alert warn" },
          el("div", { class: "body" },
            el("div", { class: "what", text: "The agent asked for: " + c.asked.join(", ") }),
            el("div", { class: "fix", text: "It waits until you connect it here, and it never sees the token." }),
            el("div", { class: "row" }, c.asked.map((s) => el("button", { class: "primary", text: "Set up " + s, onclick: () => this.focus(s) }))))) : null,
        this.relayCard(c),
        list ? this.checklist(list) : null,
        this.pending(c),
        this.connected(c),
        el("h3", { text: "Services" }),
        el("div", { class: "tiles" }, c.tiles.map((t) => this.tile(c, t))));
      if (this.target) {
        const node = document.getElementById("tile-" + this.target);
        this.target = null;
        if (node) node.scrollIntoView({ behavior: "smooth", block: "start" });
      }
    },

    // Opens a service's tile (by tile or option name) and scrolls to it,
    // on the next draw. `quiet`: the caller loads next anyway.
    focus(name, quiet) {
      this.want = name;
      if (!quiet) this.load();
    },
    // Called with fresh data, before drawing.
    aim(c) {
      const name = this.want;
      this.want = null;
      if (!name) return;
      const tile = c.tiles.find((t) => t.tile === name || t.options.some((o) => o.name === name));
      if (!tile) return;
      const opt = tile.options.find((o) => o.name === name) || tile.options.find((o) => !o.blocked && !o.connected) || tile.options[0];
      opened.add("opt:" + opt.name);
      this.target = tile.tile;
    },

    relayCard(c) {
      const card = el("div", { class: "card pad-lg", id: "relay" });
      const steps = (s) => s && s.length ? el("ul", { class: "steps" }, s.map((x) => el("li", { class: x.ok ? null : "no", text: x.step }))) : null;
      if (c.relay_url) {
        card.append(frag(
          el("div", { class: "row" },
            el("h3", { class: "grow m0", text: "Fixed callback address" }),
            c.relay_live ? tag("working", "ok") : tag("not answering", "bad")),
          el("p", { class: "muted small", text: "Paste this where a service asks for a redirect or callback URL. It stays the same across restarts." }),
          el("div", { class: "callback" }, el("code", { text: c.callback }), copyBtn(c.callback)),
          (c.callbacks || []).length ? disc("callbacks", null, "Which callback each service uses",
            el("div", { class: "details-body" }, kv(c.callbacks.map((x) => [x.service, text(x.callback)])))) : null,
          steps(this.relaySteps),
          el("div", { class: "row mt" },
            this.stepsBtn("Check it", "connections/relay/check", () => ({})),
            el("button", { class: "ghost", text: "Use another relay", onclick: () => { this.relayForms = !this.relayForms; this.load(); } }))));
        if (!this.relayForms) return card;
      } else {
        card.append(
          el("h3", { class: "mt0", text: "Fixed callback address" }),
          el("p", { class: "small", text: "OAuth sign-ins (Google's wizard, Atlassian's OAuth, most MCP servers) send you back to an address that must never change. A small relay on your own Cloudflare account gives you one, free. Key-based ways in don't need it." }));
      }
      // Deploy: a Cloudflare API token, never shown again.
      const deploy = el("div", { class: "opt mt" });
      const tok = el("input", { type: "password", name: "token", autocomplete: "new-password", placeholder: "Cloudflare API token", "data-secret": "1" });
      const acct = this.accounts ? el("select", { name: "account", "aria-label": "Cloudflare account" },
        this.accounts.map((a) => el("option", { value: a.id, text: a.name + " (" + a.id.slice(0, 8) + "…)" }))) : null;
      const go = el("button", { class: "primary", text: acct ? "Deploy to this account" : "Deploy the relay" });
      go.onclick = async () => {
        const token = tok.value.trim() || this.pendingToken;
        if (!token) { toast("Paste the token first.", true); return; }
        const body = { token };
        if (acct) body.account = acct.value;
        this.pendingToken = null;
        wipeSecrets(deploy);
        const r = await act("connections/relay/deploy", body, go);
        if (r && r.choose) {
          // Kept in memory for the second press only; never sent back.
          this.accounts = r.choose; this.pendingToken = token; this.load();
        } else if (r) {
          this.accounts = null; this.relaySteps = r.steps; this.relayForms = false;
        }
      };
      deploy.append(
        el("strong", { text: "Deploy one with a Cloudflare token" }),
        el("ol", { class: "guide" },
          el("li", {}, "Open ", el("a", { href: "https://dash.cloudflare.com/profile/api-tokens", target: "_blank", rel: "noopener", text: "dash.cloudflare.com/profile/api-tokens" }), "."),
          el("li", { text: "Create Token → use the template “Edit Cloudflare Workers” → Continue to summary → Create Token." }),
          el("li", { text: "Copy the token and paste it here. ferrule finds your account, deploys the relay and checks it end to end." })),
        acct ? el("div", { class: "field" }, el("span", { text: "The token reaches several accounts: pick one" }), acct) : el("div", { class: "row" }, tok),
        el("div", { class: "row mt" }, go,
          acct ? el("button", { class: "ghost", text: "Start over", onclick: () => { this.accounts = null; this.pendingToken = null; this.load(); } }) : null));
      // Use one that's already running.
      const use = el("div", { class: "opt mt" });
      const url = el("input", { type: "url", name: "url", placeholder: "https://ferrule-relay.you.workers.dev", autocomplete: "off" });
      const key = el("input", { type: "password", name: "key", placeholder: "the relay's key", autocomplete: "new-password", "data-secret": "1" });
      const check = el("button", { text: "Check and use" });
      check.onclick = async () => {
        const body = { url: url.value.trim(), key: key.value.trim() };
        wipeSecrets(use);
        const r = await act("connections/relay/use", body, check);
        if (r) { this.relaySteps = r.steps; this.relayForms = false; }
      };
      use.append(
        el("strong", { text: "Or use a relay you already have" }),
        el("p", { class: "muted small", text: "Its URL and key; it's checked before anything is saved." }),
        el("div", { class: "stack" }, url, key),
        el("div", { class: "row mt" }, check));
      card.append(deploy, use);
      return card;
    },

    // A button whose answer is a list of steps, each ticked or crossed.
    stepsBtn(label, path, body) {
      const b = el("button", { text: label });
      b.onclick = async () => {
        const r = await act(path, body(), b);
        if (r) { this.relaySteps = r.steps; this.load(); }
      };
      return b;
    },

    checklist(list) {
      if (!list.checks.length) return null;
      const actionBtn = (ch) => {
        const a = ch.action;
        if (!a) return null;
        switch (a.action) {
          case "relay_setup":
          case "relay_use":
            return el("button", { class: "primary", text: a.label, onclick: () => {
              this.relayForms = true;
              const r = document.getElementById("relay");
              if (r) r.scrollIntoView({ behavior: "smooth" });
              this.load();
            } });
          case "relay_check": return this.stepsBtn(a.label, "connections/relay/check", () => ({}));
          case "connect": return el("button", { class: "primary", text: a.label, onclick: () => this.focus(a.service) });
          case "cancel": return btn(a.label, "connections/cancel", { id: a.id }, "danger");
          case "google_client": return el("button", { class: "primary", text: a.label, onclick: () => { this.googleForm = !this.googleForm; this.load(); } });
          default: return null;
        }
      };
      return el("div", { class: "card" },
        el("h3", { class: "mt0", text: "What each service still needs" }),
        list.checks.map((ch) => {
          const cls = CHECK_OK.includes(ch.state) ? "ok" : CHECK_BAD.includes(ch.state) ? "bad" : ch.state === "pending" ? "warn pulse" : "";
          const row = el("div", { class: "check" },
            led(cls),
            el("div", { class: "body" },
              el("div", { class: "t" }, ch.title, " ", tag(ch.state, cls.split(" ")[0] || null)),
              ch.text ? el("div", { class: "d msg", dir: "auto", text: ch.text }) : null,
              ch.id === "google_client" && this.googleForm ? this.googleClientForm() : null),
            actionBtn(ch));
          return row;
        }));
    },

    googleClientForm() {
      const box = el("div", { class: "stack mt" });
      const id = el("input", { name: "id", placeholder: "Client ID (…apps.googleusercontent.com)", autocomplete: "off" });
      const secret = el("input", { type: "password", name: "secret", placeholder: "Client secret", autocomplete: "new-password", "data-secret": "1" });
      const save = el("button", { class: "primary", text: "Save the client" });
      save.onclick = async () => {
        const body = { id: id.value.trim(), secret: secret.value.trim() };
        wipeSecrets(box);
        if (await act("connections/google-client", body, save)) this.googleForm = false;
      };
      box.append(id, secret, el("div", { class: "row" }, save));
      return box;
    },

    pending(c) {
      const flows = c.pending_flows || [];
      if (!flows.length) return null;
      return el("div", { class: "card" },
        el("h3", { class: "mt0", text: "Waiting on a sign-in" }),
        flows.map((f) => el("div", { class: "check" },
          led("warn pulse"),
          el("div", { class: "body" },
            el("div", { class: "t", text: f.title || f.service }),
            el("div", { class: "d", text: "via " + f.via + " · started " + secs(f.age_secs) + " ago · gives up in " + secs(f.expires_in) })),
          btn("Cancel", "connections/cancel", { id: f.id }, "danger"))));
    },

    connected(c) {
      if (!c.connections.length) return null;
      const att = Object.fromEntries((c.attention || []).map((a) => [a.name, a]));
      return frag(
        el("h3", { text: "Connected" }),
        el("div", { class: "stack" }, c.connections.map((s) => {
          const a = att[s.name];
          const needs = s.state === "needs_reconnect" || (a && a.expired);
          const said = this.said[s.name];
          return el("div", { class: "card" },
            el("div", { class: "row" },
              led(needs ? "bad" : a ? "warn" : "ok"),
              el("strong", { class: "grow", text: s.title || s.name }),
              needs ? tag("needs attention", "bad") : a ? tag(a.days_left + " d left", "warn") : tag("connected", "ok"),
              tag(s.write ? "read + write" : "read")),
            el("p", { class: "muted small", text: "via " + s.via + (s.tools !== null && s.tools !== undefined ? " · " + s.tools + " tools" : "") +
              " · connected " + ago(s.connected_at) + (s.refreshed_at ? " · refreshed " + ago(s.refreshed_at) : "") +
              (s.expires_at ? " · sign-in renews " + ago(s.expires_at) : "") + (s.key_expires ? " · key expires " + day(s.key_expires) : "") }),
            a ? el("div", { class: "said bad msg", dir: "auto", text: a.text }) : s.last_error ? el("div", { class: "said bad msg", dir: "auto", text: s.last_error }) : null,
            said ? el("div", { class: "said " + (said.ok ? "ok" : "bad") + " msg", dir: "auto", text: said.said }) : null,
            el("div", { class: "row mt" },
              this.testBtn(s.name),
              needs ? el("button", { class: "primary", text: "Fix it", onclick: () => this.focus(s.service) }) : null,
              btn("Disconnect", "connections/disconnect", { name: s.name }, "danger")));
        })));
    },

    // Test: the answer stays on the card until the next test.
    testBtn(name) {
      const b = el("button", { text: "Test" });
      b.onclick = async () => {
        b.disabled = true; b.textContent = "Testing…";
        try {
          const r = await api("/api/connections/test", { name });
          this.said[name] = { ok: r.ok, said: r.said };
        } catch (e) { this.said[name] = { ok: false, said: e.message }; }
        this.load();
      };
      return b;
    },

    tile(c, t) {
      const att = (c.attention || []).find((a) => a.tile === t.tile);
      return el("div", { class: "card tile", id: "tile-" + t.tile },
        el("div", { class: "head" },
          el("h3", { class: "grow", text: t.title }),
          att ? tag(att.expired ? "expired" : "expires in " + att.days_left + " d", att.expired ? "bad" : "warn") : t.connected ? tag("connected", "ok") : null),
        t.options.length > 1 ? el("p", { class: "muted small m0", text: t.options.length + " ways in, simplest first." }) : null,
        t.options.map((o) => this.option(c, o)));
    },

    option(c, o) {
      const key = "opt:" + o.name;
      const conn = c.connections.find((s) => s.service === o.name);
      const body = el("div", { class: "details-body" });
      if (o.covers) body.append(el("p", { class: "covers msg", dir: "auto", text: "Covers: " + o.covers }));
      if ((o.guide || []).length) body.append(el("ol", { class: "guide" }, o.guide.map((g) => el("li", { class: "msg", dir: "auto", text: g }))));
      if (o.fixed_callback && c.callback) body.append(el("p", { class: "small" }, "Callback to give it: ", el("code", { text: c.callback }), " ", copyBtn(c.callback)));
      if (o.attempt) {
        body.append(el("div", { class: "said bad" },
          el("div", { class: "msg", dir: "auto", text: o.attempt.text }),
          o.attempt.switch_to && o.attempt.switch_to !== o.name ? el("div", { class: "row mt" },
            el("button", { class: "primary", text: "Use an API token instead", onclick: () => this.focus(o.attempt.switch_to) })) : null));
      }
      // What the vendor's page said, in its words, and what to do about it.
      if ((o.symptoms || []).length && !o.connected) {
        body.append(disc("sym:" + o.name, null, "Saw an error on their page?",
          el("div", { class: "details-body" }, kv(o.symptoms.map((x) => [x.saw, text(x.fix)])))));
      }
      if (conn) {
        const said = this.said[conn.name];
        if (said) body.append(el("div", { class: "said " + (said.ok ? "ok" : "bad") + " msg", dir: "auto", text: said.said }));
        body.append(el("div", { class: "row" }, this.testBtn(conn.name), btn("Disconnect", "connections/disconnect", { name: conn.name }, "danger")));
      } else if (o.blocked) {
        // Never a button bound to fail: why it can't work yet, instead.
        body.append(el("div", { class: "alert info" }, el("div", { class: "body" }, el("div", { class: "fix msg", dir: "auto", text: o.blocked }))));
      } else if (o.auth === "api_key") {
        body.append(this.keyForm(o));
      } else {
        const write = el("input", { type: "checkbox" });
        const go = el("button", { class: "primary", text: "Connect" });
        go.onclick = () => act("connections/connect", { service: o.name, write: write.checked }, go);
        body.append(el("div", { class: "row" }, el("label", { class: "cb" }, write, "allow changes (send, edit)"), go),
          el("p", { class: "muted small", text: "Opens the sign-in in a new tab; this page updates when it's done." }));
      }
      return disc(key, "opt",
        frag(el("span", { class: "grow", text: o.option || o.title }),
          o.connected ? tag("connected", "ok") : null,
          o.preview ? tag("preview", "warn") : null,
          o.blocked && !o.connected ? tag("not yet", null) : null,
          tag(o.auth === "api_key" ? "key" : "sign-in")),
        body);
    },

    keyForm(o) {
      const box = el("div", { class: "stack" });
      const fields = el("div", {}, (o.fields || []).map((f) => field(f)));
      const write = el("input", { type: "checkbox" });
      const go = el("button", { class: "primary", text: "Check and connect" });
      go.onclick = async () => {
        const vals = values(fields);
        const missing = (o.fields || []).filter((f) => !f.optional && !vals[f.name]).map((f) => f.label);
        if (missing.length) { toast("Still empty: " + missing.join(", "), true); return; }
        wipeSecrets(box);
        const r = await act("connections/key", { service: o.name, fields: vals, write: write.checked }, go);
        if (!r) return;
        // Emptied and let go of, so the redraw that shows it connected isn't
        // held back as typing.
        box.querySelectorAll("input, textarea").forEach((i) => { if (i.type !== "checkbox") i.value = ""; delete i.dataset.dirty; });
        if (box.contains(document.activeElement)) document.activeElement.blur();
        this.load();
      };
      box.append(fields,
        el("div", { class: "row" }, el("label", { class: "cb" }, write, "allow changes (send, edit)"), go),
        el("p", { class: "muted small", text: "Checked against the service before it's saved, in the secrets file. It never comes back to this page, and the model never sees it." }));
      return box;
    },
  };

  // "0 9 * * *" in words. Anything it can't say plainly stays as the cron
  // line, which the Advanced fold always shows.
  const DAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
  function scheduleWords(t) {
    if (t.kind !== "cron") {
      const d = new Date(t.schedule);
      return isNaN(d) ? "Once, " + t.schedule : "Once, " + d.toLocaleString([], { weekday: "short", day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" });
    }
    const f = String(t.schedule).trim().split(/\s+/);
    if (f.length !== 5) return t.schedule;
    const [mi, ho, dom, mo, dow] = f;
    const two = (n) => String(n).padStart(2, "0");
    let m;
    if (/^\*\/\d+$/.test(mi) && ho === "*" && dom === "*" && mo === "*" && dow === "*") return "Every " + mi.slice(2) + " minutes";
    if (/^\d+$/.test(mi) && ho === "*" && dom === "*" && mo === "*" && dow === "*") return mi === "0" ? "Every hour" : "Every hour, at :" + two(mi);
    if (!/^\d+$/.test(mi) || !/^\d+$/.test(ho) || dom !== "*" || mo !== "*") return t.schedule;
    const at = " at " + two(ho) + ":" + two(mi);
    if (dow === "*") return "Every day" + at;
    if (dow === "1-5") return "Weekdays" + at;
    if (dow === "0,6" || dow === "6,0") return "Weekends" + at;
    if ((m = /^[0-7]$/.exec(dow))) return "Every " + DAYS[Number(dow) % 7] + at;
    return t.schedule;
  }
  const clock = (unix) => new Date(unix * 1000).toLocaleString([], { weekday: "short", hour: "2-digit", minute: "2-digit" });

  // A chart with the numbers under it for a screen reader, which cannot
  // read bars.
  function chart(title, points, fmt, cap) {
    const svg = bars(points, fmt, cap);
    svg.setAttribute("aria-label", title + ": " + points.map((p) => p.k + " " + fmt(p.v)).join(", "));
    return frag(svg,
      el("table", { class: "sr" }, el("caption", { text: title }),
        el("tbody", {}, points.map((p) => el("tr", {}, el("th", { scope: "row", text: p.k }), el("td", { text: fmt(p.v) }))))));
  }

  sections.usage = {
    every: 30,
    days: "7",
    mount(root) {
      this.box = el("div");
      this.more = el("div", { class: "stack" });
      this.pick = el("div");
      // Outside the boxes the poll redraws, so a half-typed cap survives it.
      const edit = el("details", { class: "card", ontoggle: (e) => { if (e.target.open) capsEditor(e.target); } },
        el("summary", { text: "Edit caps" }));
      this.drawPick();
      root.append(secHead("Usage", "what your bot has spent"), this.pick, this.box,
        advanced("usage", "tokens, prices, every model", this.more, edit));
    },
    drawPick() {
      setKids(this.pick, chips("Range", [["1", "Today"], ["7", "7 days"], ["30", "30 days"]], this.days,
        (v) => { this.days = v; this.drawPick(); this.load(); }));
    },
    async load() {
      let u;
      try {
        u = await api("/api/usage?days=" + this.days);
      } catch (e) { return sectionError(this.box, e); }
      const t = u.total;
      const dayCap = u.caps ? (u.caps.caps || []).find((c) => c.cap === "usd_per_day" && c.limit > 0) : null;
      const window_ = this.days === "1" ? "today" : "in the last " + this.days + " days";
      const group = (rows) => table(["", "calls", "tokens", "cost"], rows.map((r) => ({ cells: [
        text(r.key),
        el("span", { class: "mono", text: String(r.calls) }),
        el("span", { class: "mono", text: num(r.input_tokens + r.output_tokens) }),
        el("span", { class: "mono", text: usd(r.usd) }),
      ]})), [1, 2, 3]);
      setKids(this.box,
        el("div", { class: "stats" },
          stat("Spent", usd(t.usd), window_),
          stat("Calls to a model", String(t.calls), u.error_pct ? u.error_pct + "% went wrong" : "none went wrong")),
        u.caps ? capsCard(u.caps, el("span", { class: "muted small", text: "A cap stops your bot from spending more. Change them under Advanced." })) : null,
        t.calls ? card({ title: "Cost per day" }, chart("Cost per day", u.per_day.map((d) => ({ k: String(d.key).slice(-5), v: d.usd })), usd, dayCap ? dayCap.limit : 0))
          : empty("Nothing spent " + window_, "Once your bot answers something, the cost shows up here."));
      setKids(this.more,
        el("div", { class: "stats" },
          stat("tokens in", num(t.input_tokens), num(t.cached_input_tokens) + " cached"),
          stat("tokens out", num(t.output_tokens), "cache hit " + u.cache_hit_pct + "%"),
          stat("latency", "p50 " + u.p50_ms + " ms", "p95 " + u.p95_ms + " ms"),
          stat("retried", u.retry_pct + "%", u.malformed ? u.malformed + " unreadable lines" : "all lines read")),
        el("p", { class: "muted small", text: "egress refused: " + (u.egress_refused.count ? u.egress_refused.count + " · " + u.egress_refused.hosts.map((h) => h.host + " ×" + h.count).join(", ") : "none") }),
        el("h3", { text: "Tokens per day" }),
        chart("Tokens per day", u.per_day.map((d) => ({ k: String(d.key).slice(-5), v: d.input_tokens + d.output_tokens })), num),
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
        secHead("Tasks", r.tasks.length ? r.tasks.length + " scheduled" : "things your bot does on a schedule"),
        r.paused ? el("div", { class: "alert bad" },
          el("div", { class: "body" }, el("div", { class: "what", text: "The kill switch is on: nothing runs." }))) : null,
        r.tasks.length ? el("div", { class: "plain-list" }, r.tasks.map((t) => this.item(t)))
          : empty("No tasks yet", "A task is something your bot does by itself, like a morning summary. Ask it in Chat: \"every weekday at 9, summarise my unread mail\"."));
    },
    item(t) {
      const next = t.enabled && t.next_run_at != null ? "Next: " + clock(t.next_run_at) + " (" + ago(t.next_run_at) + ")" : t.enabled ? "" : "Paused";
      const last = t.runs.length ? t.runs[0] : null;
      return el("div", { class: "card task" },
        el("div", { class: "item" },
          el("div", { class: "grow" },
            el("div", { class: "t msg", dir: "auto", text: t.name || t.id }),
            el("div", { class: "d", text: scheduleWords(t) + (next ? " · " + next : "") })),
          toggle((t.enabled ? "Pause " : "Resume ") + (t.name || t.id), t.enabled,
            (e) => act(t.enabled ? "tasks/pause" : "tasks/resume", { id: t.id }, e.currentTarget)),
          btn("Run now", "tasks/run", { id: t.id })),
        last && last.status !== "succeeded" && last.status !== "running" ? el("p", { class: "small mt0 " + (last.status === "skipped" ? "warn" : "bad") },
          "Last run " + last.status + " " + ago(last.started_at) + (last.detail ? ": " : ""), last.detail ? text(last.detail) : null) : null,
        advanced("task-" + t.id, "schedule line, model, history",
          kv([
            ["kind", tag(t.kind)],
            ["schedule", el("span", { class: "cron", text: t.schedule + (t.kind === "cron" ? "  ·  " + t.timezone : "") })],
            ["delivers to", text(t.destination)],
            ["model", el("span", { class: "mono", text: t.model || "the default" })],
            t.builtin ? ["built in", "yes"] : null,
          ]),
          t.runs.length ? el("div", { class: "runs" }, t.runs.map((x) => el("span", {
            class: "run-dot " + (x.status === "succeeded" ? "ok" : x.status === "skipped" ? "warn" : x.status === "running" ? "" : "bad"),
            title: (x.detail || "") + " · " + ago(x.started_at),
          }, x.status + " · " + ago(x.started_at)))) : el("p", { class: "muted small", text: "never ran" }),
          t.runs.slice(0, 1).map((x) => x.detail ? el("div", { class: "muted small msg mt", dir: "auto", text: x.detail }) : null),
          el("div", { class: "row" },
            askBtn("Schedule", "tasks/schedule", t.kind === "cron" ? "Cron schedule (5 fields), then optionally a space and an IANA timezone:" : "When (RFC 3339):",
              t.kind === "cron" ? t.schedule + " " + t.timezone : t.schedule, (v) => {
                const [schedule, timezone] = t.kind === "cron" ? splitTz(v) : [v.trim(), null];
                return { id: t.id, schedule, timezone };
              }),
            askBtn("Model", "tasks/model", "The model it runs on (provider/model, a provider or an alias), or \"default\":",
              t.model || "default", (v) => ({ id: t.id, model: v.trim() })),
            t.builtin ? null : btn("Delete", "tasks/delete", { id: t.id }, "danger"))));
    },
  };

  sections.logs = {
    every: 0,
    page: 0,
    kindV: "all",
    mount(root) {
      this.pick = el("div");
      this.q = el("input", { type: "search", placeholder: "search — Hebrew works as-is", dir: "auto", "aria-label": "Search the log", size: "28" });
      this.box = el("div");
      const go = () => { this.page = 0; this.load(); };
      this.q.addEventListener("change", go);
      this.drawPick = () => setKids(this.pick, chips("Show", [["all", "Everything"], ["warn", "Problems"], ["audit", "Changes"]], this.kindV,
        (v) => { this.kindV = v; this.drawPick(); go(); }));
      this.drawPick();
      root.append(secHead("Logs", "what happened · secrets and chats never appear here"),
        this.pick, el("div", { class: "row mt" }, this.q, button("Reload", { kind: "ghost", icon: "retry", onclick: () => this.load() })),
        this.box);
    },
    async load() {
      let r;
      try {
        r = await api("/api/logs?" + new URLSearchParams({ kind: this.kindV, q: this.q.value, page: this.page }));
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
            el("span", { class: "txt", dir: "auto" }, x.tree ? el("span", { class: "muted", text: x.tree + "  " }) : null, text(x.text)))))
          : empty("Nothing matches", this.q.value ? "Try a shorter search." : "Nothing of this kind has happened yet."),
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
      const workspace = w ? el("div", { class: "alert block " + (w.trusted ? "ok" : "warn") },
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
          el("span", { class: "muted small", text: "Trust is pinned to the hash: any later edit needs trusting again." }))) : null;
      // Literal paths, so the guard test can see every one.
      const switchFor = (name, off, on_path, off_path) => toggle((off ? "Turn on " : "Turn off ") + name, !off,
        (e) => act(off ? on_path : off_path, { name }, e.currentTarget));
      setKids(this.box,
        secHead("Extensions", "add-ons your bot can use"),
        el("h3", { text: "Tools from other programs" }),
        el("p", { class: "muted small m0" }, "Programs that give your bot new tools, called ", tip("MCP"), " servers."),
        x.mcp.length ? el("div", { class: "plain-list" }, x.mcp.map((m) => el("div", { class: "item" + (m.disabled ? " dead" : "") },
          el("div", { class: "grow" }, el("div", { class: "t", text: m.name }), el("div", { class: "d", text: m.origin === "configured" ? "added in the config" : "from " + m.origin })),
          m.origin === "configured" ? switchFor(m.name, m.disabled, "mcp/enable", "mcp/disable") : tag(m.origin)))) : empty("None connected", "Nothing is adding tools right now."),
        el("h3", { text: "Skills" }),
        !x.skills_enabled ? empty("Skills are off", "Turn them on in the config to let your bot learn how-to guides.")
          : x.skills.length ? el("div", { class: "plain-list" }, x.skills.map((s) => el("div", { class: "item" + (s.disabled ? " dead" : "") },
            el("div", { class: "grow" }, el("div", { class: "t mono", text: s.name }), el("div", { class: "d" }, text(s.description))),
            switchFor(s.name, s.disabled, "skills/enable", "skills/disable")))) : empty("No skills found", "A skill is a how-to guide your bot reads when it needs it."),
        x.skills_disabled.length ? el("p", { class: "muted small" }, "Disabled in the config: ",
          x.skills_disabled.map((n, i) => frag(i ? " · " : "", n, " ", btn("Enable", "skills/enable", { name: n })))) : null,
        w && !w.trusted ? workspace : null,
        advanced("extensions", "hooks, removing, trust",
          el("h3", { text: "Hooks" }),
          x.hooks.length ? table(["event", "matcher", "command"], x.hooks.map((h) => ({ cells: [
            tag(h.event), el("span", { class: "mono", text: h.matcher || "*" }), el("code", { text: h.command }),
          ]}))) : el("div", { class: "empty", text: "none in the config" }),
          w && w.trusted ? workspace : null,
          x.mcp.length ? frag(el("h3", { text: "Remove a tool server" }),
            el("div", { class: "row" }, x.mcp.map((m) => btn("Remove " + m.name, "mcp/remove", { name: m.name }, "danger")))) : null));
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
        secHead("Agents", "helpers your bot starts for a big job"),
        r.agents.length ? el("div", { class: "plain-list" }, r.agents.map((a) => el("div", { class: "item" },
          a.status === "running" ? led("ok pulse") : null,
          el("div", { class: "grow" }, el("div", { class: "t", text: a.name }), el("div", { class: "d" }, text(a.task))),
          a.status === "running" ? tag("running", "ok") : tag(a.status, a.status === "failed" ? "bad" : null))))
          : empty("No helpers running", "When your bot splits a big job, its helpers show up here."),
        advanced("agents", "role, depth, tokens, model",
          r.agents.length ? table(["agent", "role", "tokens", "model"], r.agents.map((a) => ({ cells: [
            el("strong", { text: a.name }),
            a.role + " · depth " + a.depth,
            el("span", { class: "mono", text: num(a.tokens) }),
            el("span", { class: "mono small", text: a.model || "–" }),
          ]})), [2]) : null,
          el("p", { class: "muted small", text: "Helpers are read-only: each works in its own room and sends back a summary. A planner, a worker and a verifier, with limits and a shared budget." })));
    },
  };

  // ---- chat (M37 §4.3, M47) ------------------------------------------------
  // Session dashboard__owner, like any other chat: the page polls what
  // changed since the last revision and merges it by id, so a streamed
  // edit changes its bubble's text in place instead of adding one. A
  // photo is shrunk here, sent as base64 and named in the log, never shown
  // back (the page keeps no bytes of it after sending).

  // Prose with `code` and http(s) links, and fenced blocks, built with
  // el() and textContent only: nothing in a message is ever parsed as HTML.
  const INLINE = /(`[^`\n]+`)|(https?:\/\/[^\s<>()]*[^\s<>().,;:!?'"])/g;
  function inline(t) {
    const out = [];
    let last = 0;
    for (const m of t.matchAll(INLINE)) {
      if (m.index > last) out.push(t.slice(last, m.index));
      if (m[1]) out.push(el("code", { class: "ic", text: m[1].slice(1, -1) }));
      else out.push(el("a", { href: m[2], target: "_blank", rel: "noopener noreferrer", text: m[2] }));
      last = m.index + m[0].length;
    }
    if (last < t.length) out.push(t.slice(last));
    return out;
  }
  // Fences alternate: an odd piece is code, and an unclosed one (a reply
  // still being written) is code too.
  function richText(t) {
    return t.split("```").map((piece, i) => {
      if (i % 2 === 0) return piece.trim() ? el("div", { class: "msg", dir: "auto" }, inline(piece.replace(/^\n+|\n+$/g, ""))) : null;
      const code = piece.replace(/^[\w+.#-]*\n/, "").replace(/\n$/, "");
      return el("div", { class: "codeblock" },
        el("pre", { dir: "ltr" }, el("code", { text: code })),
        el("button", { type: "button", class: "ghost icon", "aria-label": "Copy the code", title: "Copy the code", onclick: () => copyText(code) }, icon("copy")));
    });
  }

  // Copy `t`; the clipboard API needs a secure page, so a plain http
  // address (a LAN bind) falls back to a hidden textarea and execCommand.
  async function copyText(t) {
    let done = false;
    try { await navigator.clipboard.writeText(t); done = true; } catch (_) {
      const ta = el("textarea", { class: "sr", "aria-hidden": "true", tabindex: "-1" });
      ta.value = t;
      document.body.append(ta);
      ta.select();
      try { done = document.execCommand("copy"); } catch (_e) { done = false; }
      ta.remove();
    }
    toast(done ? "Copied" : "Couldn't copy: select the text and copy it by hand.", !done);
    return done;
  }

  // The most a photo may weigh on the wire (the server refuses more), and
  // the sizes the page tries, largest first.
  const PHOTO_MAX = 3500000;
  const PHOTO_TRIES = [[1600, 0.85], [1280, 0.8], [960, 0.7], [640, 0.6]];
  const PHOTO_TYPES = ["image/jpeg", "image/png", "image/webp", "image/gif"];
  const FAILED = /^(I couldn't reply:|something went wrong\.)/;

  // A picked file as `{mime, data (base64), preview}`, shrunk to fit.
  // Whatever the browser can't decode is sent as it is when it is a type the
  // server takes and small enough; anything else is said in words.
  async function preparePhoto(file) {
    const b64 = (url) => url.slice(url.indexOf(",") + 1);
    const asUrl = (blob) => new Promise((res, rej) => { const r = new FileReader(); r.onload = () => res(r.result); r.onerror = rej; r.readAsDataURL(blob); });
    let bmp = null;
    try {
      if (window.createImageBitmap) bmp = await createImageBitmap(file);
      else {
        const img = new Image();
        img.src = await asUrl(file);
        await img.decode();
        bmp = img;
      }
    } catch (_) { bmp = null; }
    if (bmp) {
      const w0 = bmp.width, h0 = bmp.height;
      for (const [side, q] of PHOTO_TRIES) {
        const k = Math.min(1, side / Math.max(w0, h0));
        const c = el("canvas", { width: Math.max(1, Math.round(w0 * k)), height: Math.max(1, Math.round(h0 * k)) });
        const g = c.getContext("2d");
        g.fillStyle = "#fff";
        g.fillRect(0, 0, c.width, c.height);
        g.drawImage(bmp, 0, 0, c.width, c.height);
        const url = c.toDataURL("image/jpeg", q);
        if (b64(url).length * 0.75 <= PHOTO_MAX) return { mime: "image/jpeg", data: b64(url), preview: url, size: Math.round(b64(url).length * 0.75) };
      }
      throw new Error("That photo is too big even after shrinking it. Try another one.");
    }
    if (PHOTO_TYPES.includes(file.type) && file.size <= PHOTO_MAX) {
      const url = await asUrl(file);
      return { mime: file.type, data: b64(url), preview: url, size: file.size };
    }
    throw new Error("This browser can't read that photo. Try a JPEG or PNG.");
  }

  sections.chat = {
    title: "Chat",
    live: true,
    every: 2,
    from: 0,
    entries: new Map(),
    rows: new Map(),
    mount(root) {
      this.top = el("div");
      this.log = el("div", { class: "chat", role: "log", "aria-live": "polite", "aria-relevant": "additions", "aria-label": "Conversation" });
      this.typing = el("div", { class: "typing", role: "status", hidden: true },
        el("span", { class: "dots", "aria-hidden": "true" }, el("i"), el("i"), el("i")),
        el("span", { class: "label", text: "Writing…" }));
      this.input = el("textarea", { rows: 1, dir: "auto", placeholder: "Message your agent", "aria-label": "Message" });
      this.sendBtn = el("button", { type: "button", class: "primary", "aria-label": "Send" }, icon("send"), el("span", { text: "Send" }));
      this.stopBtn = el("button", { type: "button", class: "danger", hidden: true, "aria-label": "Stop the answer" }, icon("stop"), el("span", { text: "Stop" }));
      this.note = el("div", { class: "muted small", role: "status" });
      this.pill = el("button", { type: "button", class: "pill", hidden: true, text: "New messages" }, icon("chevron-down"));
      this.pill.onclick = () => this.toBottom();
      this.preview = el("div", { class: "attached", hidden: true });
      this.photo = null;
      this.gallery = el("input", { type: "file", accept: "image/*", hidden: true, "aria-hidden": "true", tabindex: "-1" });
      this.camera = el("input", { type: "file", accept: "image/*", capture: "environment", hidden: true, "aria-hidden": "true", tabindex: "-1" });
      for (const f of [this.gallery, this.camera]) f.addEventListener("change", () => { if (f.files && f.files[0]) this.attach(f.files[0]); f.value = ""; });
      const attach = el("button", { type: "button", class: "ghost icon", "aria-label": "Attach a photo", title: "Attach a photo", onclick: () => this.pick() }, icon("image"));
      const shoot = matchMedia("(pointer:coarse)").matches
        ? el("button", { type: "button", class: "ghost icon", "aria-label": "Take a photo", title: "Take a photo", onclick: () => this.camera.click() }, icon("camera"))
        : null;
      this.sendBtn.onclick = () => this.send();
      this.stopBtn.onclick = () => this.stop();
      this.input.addEventListener("input", () => this.grow());
      this.input.addEventListener("keydown", (e) => {
        if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); this.send(); }
      });
      this.input.addEventListener("paste", (e) => {
        const f = [...((e.clipboardData && e.clipboardData.files) || [])].find((x) => x.type.startsWith("image/"));
        if (f) { e.preventDefault(); this.attach(f); }
      });
      window.addEventListener("scroll", () => { if (this.pill && this.nearBottom()) this.pill.hidden = true; }, { passive: true });
      root.append(secHead("Chat", "the same agent, in its own session"), this.top, this.log, this.typing,
        el("div", { class: "composer" }, this.pill, this.note, this.preview,
          el("div", { class: "row" }, attach, shoot, el("div", { class: "grow" }, this.input), this.stopBtn, this.sendBtn),
          this.gallery, this.camera));
      this.drawn = false;
      this.rows.clear();
      if (this.wantPick) { this.wantPick = false; setTimeout(() => this.pick(), 60); }
    },
    pick() { if (this.gallery && !this.gallery.disabled) this.gallery.click(); else this.wantPick = true; },
    grow() {
      // Measured at one row, then as many as the text needs, up to eight.
      const cs = getComputedStyle(this.input);
      this.input.rows = 1;
      const line = parseFloat(cs.lineHeight) || parseFloat(cs.fontSize) * 1.3;
      const pad = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom) + parseFloat(cs.borderTopWidth) + parseFloat(cs.borderBottomWidth);
      this.input.rows = Math.min(8, Math.max(1, Math.ceil((this.input.scrollHeight + (parseFloat(cs.borderTopWidth) + parseFloat(cs.borderBottomWidth)) - pad) / line - 0.05)));
    },
    nearBottom() { const d = document.documentElement; return d.scrollHeight - window.scrollY - window.innerHeight < 80; },
    toBottom() { window.scrollTo(0, document.documentElement.scrollHeight); this.pill.hidden = true; },
    async attach(file) {
      this.note.textContent = "Preparing the photo…";
      try {
        this.photo = await preparePhoto(file);
        this.photo.name = file.name || "photo";
        setKids(this.preview,
          el("img", { src: this.photo.preview, alt: "The photo you are about to send", width: 56, height: 56 }),
          el("span", { class: "muted small grow", dir: "auto", text: this.photo.name + " · " + Math.max(1, Math.round(this.photo.size / 1024)) + " KB" }),
          el("button", { type: "button", class: "ghost icon", "aria-label": "Remove the photo", title: "Remove the photo", onclick: () => this.detach() }, icon("x")));
        this.preview.hidden = false;
        this.note.textContent = "";
        this.input.focus();
      } catch (e) { this.photo = null; this.note.textContent = ""; toast(e.message, true); }
    },
    detach() { this.photo = null; this.preview.hidden = true; this.preview.replaceChildren(); },
    async load() {
      let c, ap = { approvals: [] };
      try {
        [c, ap] = await Promise.all([api("/api/chat?from=" + this.from), api("/api/approvals").catch(() => ap)]);
      } catch (e) { return sectionError(this.top, e); }
      setKids(this.top, approvalsCard(ap.approvals));
      // The log starts again after a restart: forget what was drawn.
      if (c.next < this.from) { this.from = 0; this.entries.clear(); this.rows.clear(); this.log.replaceChildren(); return this.load(); }
      for (const e of c.entries) this.entries.set(e.id, e);
      if (c.first) for (const id of [...this.entries.keys()]) if (id < c.first) { this.entries.delete(id); const r = this.rows.get(id); if (r) { r.node.remove(); this.rows.delete(id); } }
      this.from = c.next;
      const listening = c.listening;
      this.input.disabled = !listening;
      this.sendBtn.disabled = !listening;
      this.gallery.disabled = !listening;
      if (!this.sending) this.note.textContent = listening ? "" : (c.why || "The agent isn't listening here: chat works when the dashboard runs inside the gateway (the service).");
      const grew = c.entries.length > 0;
      const stick = this.nearBottom() || !this.drawn;
      this.draw(c.waiting);
      this.typing.hidden = !c.waiting;
      this.stopBtn.hidden = !c.waiting;
      if (grew) {
        if (stick) this.toBottom();
        else if (c.entries.some((e) => e.who !== "you")) this.pill.hidden = false;
      }
      this.drawn = true;
    },
    // Only what changed is touched: a bubble whose text, buttons and
    // "Try again" haven't moved keeps its node, so a selection or a
    // scroll position inside it survives the poll.
    draw(waiting) {
      const list = [...this.entries.values()].sort((a, b) => a.id - b.id);
      const lastId = list.length ? list[list.length - 1].id : 0;
      if (!list.length) {
        if (!this.log.firstChild) this.log.append(el("div", { class: "empty", "data-empty": "1", text: "Nothing yet. Whatever you type here goes to your agent, like a message on Telegram." }));
        return;
      }
      const hint = this.log.querySelector("[data-empty]");
      if (hint) hint.remove();
      let prev = null;
      for (const e of list) {
        const retry = e.id === lastId && e.who !== "you" && !waiting && FAILED.test(e.text);
        const sig = JSON.stringify([e.text, e.buttons, e.photo, retry]);
        let row = this.rows.get(e.id);
        if (!row || row.sig !== sig) {
          const node = this.bubble(e, retry);
          if (row) row.node.replaceWith(node); else (prev ? prev.after(node) : this.log.prepend(node));
          row = { node, sig };
          this.rows.set(e.id, row);
        }
        prev = row.node;
      }
    },
    bubble(e, retry) {
      const mine = e.who === "you";
      return el("div", { class: "bubble" + (mine ? " you" : ""), "data-id": e.id },
        e.photo ? el("div", { class: "photo-chip" }, icon("image"), el("span", { dir: "auto", text: e.photo.name })) : null,
        richText(e.text || ""),
        (e.buttons || []).length ? el("div", { class: "row" }, e.buttons.map((b) => b.url
          ? el("a", { class: "btn", href: b.url, target: "_blank", rel: "noopener noreferrer", text: b.text })
          : el("button", { text: b.text, onclick: (ev) => this.send(b.send, ev.currentTarget) }))) : null,
        el("div", { class: "foot" },
          el("span", { class: "at", text: new Date(e.at * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) }),
          mine ? null : el("button", { type: "button", class: "ghost icon sm", "aria-label": "Copy the answer", title: "Copy the answer", onclick: () => copyText(e.text) }, icon("copy")),
          retry ? el("button", { type: "button", class: "ghost sm", onclick: (ev) => this.again(ev.currentTarget) }, icon("retry"), el("span", { text: "Try again" })) : null));
    },
    // The last thing the owner said, said again.
    again(b) {
      const mine = [...this.entries.values()].filter((e) => e.who === "you").sort((x, y) => y.id - x.id)[0];
      if (mine && mine.text.trim()) this.send(mine.text, b); else toast("Nothing to send again: type it once more.", true);
    },
    async stop() {
      this.stopBtn.disabled = true;
      try { const r = await api("/api/turn/stop", { session: "dashboard__owner" }); toast(r.said || "Stopped."); } catch (e) { toast(e.message, e.status !== 404); } finally { this.stopBtn.disabled = false; this.load(); }
    },
    async send(said, button) {
      if (this.sending) { this.sending.abort(); return; }
      const t = (said !== undefined ? said : this.input.value).trim();
      const photo = said === undefined ? this.photo : null;
      if (!t && !photo) return;
      const b = button || this.sendBtn;
      b.disabled = true;
      try {
        if (photo) {
          // A photo takes a moment on a slow line: say so, and let it be cancelled.
          this.sending = new AbortController();
          this.note.textContent = "Sending photo…";
          this.sendBtn.replaceChildren(icon("x"), el("span", { text: "Cancel" }));
          this.sendBtn.setAttribute("aria-label", "Cancel sending the photo");
          this.sendBtn.disabled = false;
          await api("/api/chat/photo", { text: t, mime: photo.mime, data: photo.data }, { signal: this.sending.signal });
          this.detach();
        } else await api("/api/chat/send", { text: t });
        if (said === undefined) { this.input.value = ""; this.grow(); delete this.input.dataset.dirty; }
        this.load();
      } catch (e) {
        if (e.name === "AbortError") toast("Cancelled: the photo wasn't sent.");
        else toast(e.message, true);
      } finally {
        if (this.sending) {
          this.sending = null;
          this.note.textContent = "";
          this.sendBtn.replaceChildren(icon("send"), el("span", { text: "Send" }));
          this.sendBtn.setAttribute("aria-label", "Send");
        }
        b.disabled = false;
      }
    },
  };

  // ---- console: a ferrule line, never a shell (M37 §4.2) -----------------

  // ---- channels (M39 §9) -------------------------------------------------
  // A card per chat channel: its state from the running gateway, a form
  // (secrets write-only: a card only says whether each is set), Test
  // against the service with what's typed, Save, Remove, and the guide
  // with direct links. Telegram has its own card (M44 §4): a token, a test, a
// wait for the first message and an Allow for its chat. Discord and Slack
// stay with `ferrule setup`.
  const CHANNEL_STATE = {
    on: ["running", "ok"], problem: ["problem", "bad"], stale: ["not polling", "bad"],
    restart: ["restart needed", "warn"], set: ["set up", "ok"], off: ["off", null],
  };

  sections.channels = {
    title: "Channels",
    every: 15,
    said: {},
    mount(root) { this.box = el("div"); root.append(this.box); },
    async load() {
      let c;
      try { c = await api("/api/channels"); } catch (e) { return sectionError(this.box, e); }
      const on = c.channels.filter((x) => x.configured).length;
      const restart = c.gateway && c.channels.some((x) => x.state === "restart");
      setKids(this.box,
        secHead("Channels", on + " of " + c.channels.length + " set up"),
        restart ? el("div", { class: "alert warn" }, el("div", { class: "body" },
          el("div", { class: "what", text: "A channel's settings changed since the gateway started." }),
          el("div", { class: "row" }, btn("Restart the gateway", "gateway/restart", {}, "primary")))) : null,
        el("div", { class: "tiles" }, c.channels.map((x) => this.card(x))));
    },

    card(x) {
      const st = CHANNEL_STATE[x.state] || [x.state, null];
      const s = el("svg", { class: "ico", viewBox: "0 0 24 24", "aria-hidden": "true" });
      s.append(el("path", { d: x.icon }));
      const body = el("div", { class: "stack" });
      if (x.why) body.append(el("div", { class: "said " + (st[1] === "bad" ? "bad" : "") + " msg", dir: "auto", text: x.why }));
      if (!x.form && x.name === "telegram") {
        body.append(this.telegram(x));
      } else if (!x.form) {
        body.append(el("p", { class: "muted small", text: x.configured
          ? "Set up. Change it with `ferrule setup` on the machine."
          : "Run `ferrule setup` on the machine to add it." }));
      } else {
        if ((x.guide || []).length) {
          body.append(disc("chguide:" + x.name, null, x.configured ? "How it was set up" : "How to set it up",
            el("ol", { class: "guide" }, x.guide.map((g) => el("li", { class: "msg", dir: "auto" },
              g.text, g.url ? frag(" ", el("a", { href: g.url, target: "_blank", rel: "noopener", text: g.url.replace(/^https?:\/\//, "") })) : null)))));
        }
        body.append(this.form(x));
        if (x.keys) body.append(this.keys(x));
      }
      body.append(el("p", { class: "muted small m0" }, "More in ", el("code", { text: x.doc }), "."));
      return el("div", { class: "card tile", id: "channel-" + x.name },
        el("div", { class: "head" }, s, el("h3", { class: "grow", text: x.title }), tag(st[0], st[1])),
        body);
    },

    // Telegram: what a terminal's `ferrule setup` does, step by step. The
    // token stays in this page's memory (each call sends it) until the chat
    // is allowed; the state survives the section's redraw.
    telegram(x) {
      const t = this.tg || (this.tg = { token: "", said: null, chats: [], next: null, waiting: false, restart: false });
      const box = el("div", { class: "stack" });
      const token = el("input", { type: "password", name: "token", autocomplete: "new-password", spellcheck: "false", dir: "auto",
        placeholder: x.configured ? "saved: type to replace" : "123456789:AA…", "aria-label": "Telegram bot token" });
      token.value = t.token;
      token.addEventListener("input", () => { t.token = token.value.trim(); });
      const say = (ok, said) => { t.said = said ? { ok, said } : null; this.load(); };
      const call = async (path, extra) => {
        try { return await api("/api/telegram/" + path, Object.assign({ token: t.token }, extra)); }
        catch (e) { say(false, e.message); return null; }
      };
      const test = el("button", { text: "Test" });
      test.onclick = async () => {
        test.disabled = true; test.textContent = "Testing…";
        const r = await call("test");
        if (r) say(r.ok, r.said);
      };
      const save = el("button", { class: "primary", text: x.configured ? "Save the new token" : "Save" });
      save.onclick = async () => {
        save.disabled = true;
        const r = await call("save");
        if (!r) return;
        t.restart = !!r.restart;
        delete token.dataset.dirty;
        say(true, r.said);
      };
      const wait = el("button", { text: t.waiting ? "Waiting…" : "Wait for a message" });
      wait.disabled = t.waiting;
      wait.onclick = async () => {
        t.waiting = true; t.chats = []; t.said = { ok: true, said: "Send any message to your bot now…" };
        delete token.dataset.dirty;
        this.load();
        const until = Date.now() + 120000;
        let offset = null;
        while (Date.now() < until && !t.chats.length && t.waiting) {
          let r;
          try { r = await api("/api/telegram/wait", { token: t.token, offset }); }
          catch (e) { t.waiting = false; return say(false, e.message); }
          if (!r.ok) { t.waiting = false; return say(false, r.said); }
          if (r.next !== null && r.next !== undefined) { offset = r.next; t.next = r.next; }
          t.chats = r.chats || [];
        }
        t.waiting = false;
        say(t.chats.length > 0, t.chats.length ? "Someone wrote to the bot. Allow the chat to let it talk to the agent." : "No message came in 2 minutes. Send one, then try again.");
      };
      const chats = t.chats.map((c) => {
        const allow = el("button", { class: "primary", text: "Allow" });
        allow.onclick = async () => {
          allow.disabled = true;
          const r = await call("allow", { chat: c.id, next: t.next });
          if (!r) return;
          t.chats = t.chats.filter((o) => o.id !== c.id);
          t.restart = !!r.restart || t.restart;
          say(r.ok, r.said);
        };
        return el("div", { class: "row" }, el("b", { class: "msg", dir: "auto", text: c.name }),
          el("span", { class: "muted small grow", text: c.kind + " · chat " + c.id }), allow);
      });
      box.append(
        el("label", { class: "field" }, el("span", { text: "Bot token (from @BotFather)" }), token),
        t.said ? el("div", { class: "said test " + (t.said.ok ? "ok" : "bad") + " msg", dir: "auto", text: t.said.said }) : null,
        chats.length ? el("div", { class: "stack" }, chats) : null,
        el("div", { class: "row" }, test, save, wait,
          t.restart ? btn("Restart", "gateway/restart", {}, "primary") : null),
        el("p", { class: "muted small", text: "A change here starts with the next restart. The token goes to the secrets file, never back to this page." }));
      return box;
    },

    // The HTTP API's keys: a list with Revoke, and a form whose new key is
    // shown once (kept in this page only until "Copied").
    keys(x) {
      const box = el("div", { class: "stack" });
      const when = (t) => t ? new Date(t * 1000).toLocaleString([], { dateStyle: "short", timeStyle: "short" }) : "never";
      const made = this.madeKey;
      if (made) {
        const done = el("button", { text: "Copied, hide it" });
        done.onclick = () => { this.madeKey = null; this.load(); };
        box.append(el("div", { class: "said ok stack" },
          el("div", { text: "The key for " + made.name + ", shown once:" }),
          el("code", { class: "msg", text: made.key }),
          made.webhook_secret ? el("div", { text: "Its webhook's signing secret:" }) : null,
          made.webhook_secret ? el("code", { class: "msg", text: made.webhook_secret }) : null,
          el("div", { class: "row" }, done)));
      }
      box.append(el("h4", { class: "mt0", text: "Keys" }),
        x.keys.length ? el("div", { class: "stack" }, x.keys.map((k) => el("div", { class: "row" },
          el("b", { text: k.name }),
          el("span", { class: "muted small grow", text: "made " + when(k.created) + " · last used " + when(k.last_used) + (k.webhook ? " · webhook " + k.webhook : "") }),
          btn("Revoke", "channels/keys/revoke", { name: k.name }, "danger")))) :
          el("p", { class: "muted small", text: "No key yet, so nothing can call it." }));
      const name = el("input", { type: "text", placeholder: "n8n", "aria-label": "the new key's name", autocomplete: "off" });
      const hook = el("input", { type: "url", placeholder: "webhook for task results (optional, https://…)", "aria-label": "webhook", autocomplete: "off" });
      const add = el("button", { class: "primary", text: "Create a key" });
      add.onclick = async () => {
        const r = await act("channels/keys/add", { name: name.value.trim(), webhook: hook.value.trim() }, add);
        if (!r || !r.key) return;
        this.madeKey = { name: name.value.trim(), key: r.key, webhook_secret: r.webhook_secret };
        this.load();
      };
      box.append(el("div", { class: "row" }, name, hook, add),
        el("p", { class: "muted small", text: "A key works at once, no restart; only its hash is kept. Revoke takes it away at once." }));
      return box;
    },

    input(f) {
      if (f.choices) {
        const sel = el("select", { name: f.name }, (f.optional ? [""] : []).concat(f.choices).map((w) =>
          el("option", { value: w, text: w || "(default)", selected: w === f.value })));
        return el("label", { class: "field" }, el("span", { text: f.label + (f.optional ? " (optional)" : "") }), sel,
          f.hint ? el("span", { class: "hint msg", dir: "auto", text: f.hint }) : null);
      }
      const shown = Object.assign({}, f, {
        kind: f.kind === "list" ? "textarea" : null,
        placeholder: f.secret ? (f.set ? "saved (" + f.env + "): type to replace" : f.env) : null,
      });
      return field(shown, f.secret ? null : f.value);
    },

    form(x) {
      const box = el("div", { class: "stack" });
      const fields = el("div", {}, x.fields.map((f) => this.input(f)));
      const said = this.said[x.name];
      const test = el("button", { text: "Test" });
      test.onclick = async () => {
        test.disabled = true; test.textContent = "Testing…";
        try {
          const r = await api("/api/channels/test", { name: x.name, values: values(fields) });
          this.said[x.name] = { ok: r.ok, said: r.said };
        } catch (e) { this.said[x.name] = { ok: false, said: e.message }; }
        test.disabled = false; test.textContent = "Test";
        const old = box.querySelector(".said.test");
        const now = this.said[x.name];
        const node = el("div", { class: "said test " + (now.ok ? "ok" : "bad") + " msg", dir: "auto", text: now.said });
        if (old) old.replaceWith(node); else box.insertBefore(node, box.lastChild);
      };
      const save = el("button", { class: "primary", text: x.configured ? "Save" : "Save and turn on" });
      save.onclick = async () => {
        const vals = values(fields);
        const r = await act("channels/save", { name: x.name, values: vals }, save);
        if (!r) return;
        wipeSecrets(box);
        box.querySelectorAll("[data-dirty]").forEach((i) => { delete i.dataset.dirty; });
        this.said[x.name] = null;
        this.load();
      };
      // Element.append would print a null as the word "null".
      box.append(fields);
      if (said) box.append(el("div", { class: "said test " + (said.ok ? "ok" : "bad") + " msg", dir: "auto", text: said.said }));
      box.append(el("div", { class: "row" }, test, save,
          x.configured ? btn("Remove", "channels/remove", { name: x.name }, "danger") : null),
        el("p", { class: "muted small", text: "Tokens go to the secrets file, never back to this page, and the model never sees them." }));
      return box;
    },
  };

  sections.console = {
    title: "Console",
    live: true,
    every: 0,
    mount(root) {
      this.input = el("input", { type: "text", spellcheck: "false", autocomplete: "off", autocapitalize: "off", placeholder: "status", "aria-label": "ferrule command" });
      this.chips = el("div", { class: "complete" });
      this.help = el("div", { class: "muted small" });
      this.runBtn = el("button", { class: "primary", text: "Run" });
      this.cancelBtn = el("button", { class: "danger", text: "Cancel", hidden: true });
      this.head = el("div", { class: "muted small" });
      this.out = el("pre", { class: "out", hidden: true });
      this.parity = el("div");
      this.runBtn.onclick = () => this.run();
      this.cancelBtn.onclick = () => this.job && act("console/cancel", { id: this.job }, this.cancelBtn);
      this.input.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); this.run(); } });
      this.input.addEventListener("input", () => {
        clearTimeout(this.t);
        this.t = setTimeout(() => this.complete(), 180);
      });
      root.append(
        secHead("Console", "ferrule commands, run here as on the machine; no shell"),
        el("div", { class: "card" },
          el("div", { class: "console-line" }, el("span", { class: "prompt", text: "ferrule" }), el("div", { class: "grow" }, this.input), this.runBtn, this.cancelBtn),
          this.chips, this.help),
        this.head, this.out, this.parity);
      this.complete();
      this.loadParity();
    },
    async load() { /* driven by the job's own poll */ },
    async complete() {
      let r;
      try { r = await api("/api/console/complete?line=" + encodeURIComponent(this.input.value)); } catch (_) { return; }
      this.help.textContent = r.path && r.path.length ? "ferrule " + r.path.join(" ") + (r.class ? " · " + r.class : "") : "";
      setKids(this.chips, (r.items || []).slice(0, 24).map((it) => el("button", {
        class: "ghost", title: it.help || null, text: it.word,
        onclick: () => {
          const v = this.input.value;
          const cut = /\s$/.test(v) || v === "" ? v : v.replace(/\S+$/, "");
          this.input.value = cut + it.word + " ";
          this.input.focus();
          this.complete();
        },
      })));
    },
    async run(confirm) {
      const line = this.input.value.trim();
      if (!line) return;
      this.runBtn.disabled = true;
      try {
        const r = await api("/api/console/run", confirm ? { line, confirm: true } : { line });
        this.job = r.job.id;
        this.from = 0;
        this.text = "";
        this.out.hidden = false;
        this.out.textContent = "";
        this.head.textContent = "$ ferrule " + line + " · " + r.class;
        this.cancelBtn.hidden = false;
        this.poll();
      } catch (e) {
        if (e.status === 409 && e.data && e.data.confirm) {
          if (await ask({ title: "Run this command?", text: e.data.confirm, confirm: "Run it", danger: true })) return this.run(true);
        } else if (e.status === 403) {
          toast(e.message + (e.data && e.data.page ? " · on this page: " + e.data.page : ""), true);
        } else toast(e.message, true);
      } finally { this.runBtn.disabled = false; }
    },
    async poll() {
      const id = this.job;
      let v;
      try { v = await api("/api/console/job?id=" + encodeURIComponent(id) + "&from=" + this.from); } catch (e) { toast(e.message, true); return; }
      if (id !== this.job) return;
      const stick = this.out.scrollHeight - this.out.scrollTop - this.out.clientHeight < 40;
      if (v.text) this.out.append(v.text);
      if (stick) this.out.scrollTop = this.out.scrollHeight;
      this.from = v.next;
      if (v.done) {
        this.cancelBtn.hidden = true;
        this.head.append(" · " + (v.code === 0 ? "done" : v.why || "exit " + v.code) + " in " + secs(v.secs));
        return;
      }
      setTimeout(() => this.poll(), 500);
    },
    async loadParity() {
      let p;
      try { p = await api("/api/console/parity"); } catch (_) { return; }
      setKids(this.parity, disc("parity", "card", "Every command, and where it lives on this page",
        el("div", { class: "details-body tbl" }, table(["command", "kind", "on the page", "note"],
          p.rows.map((r) => ({ cells: [el("code", { text: r.command }), tag(r.class, r.class === "refused" ? "bad" : r.class === "destructive" ? "warn" : null), r.page || "–", r.note || ""] }))))));
    },
  };

  // ---- config: the file, secrets hidden (M37 §4.4) -----------------------

  sections.config = {
    title: "Config",
    every: 0,
    mount(root) { this.box = el("div"); root.append(this.box); },
    async load() {
      let c;
      try { c = await api("/api/config"); } catch (e) { return sectionError(this.box, e); }
      const set = (key, value, b) => act("config/set", { key, value }, b);
      const row = (f) => {
        let input;
        const shown = f.value === null || f.value === undefined ? "" : String(f.value);
        if (f.kind === "bool" || f.kind === "choice") {
          const opts = f.kind === "bool" ? ["true", "false"] : f.choices || [];
          input = el("select", { name: f.key, "aria-label": f.key },
            el("option", { value: "", text: "default" }),
            opts.map((o) => el("option", { value: o, text: f.kind === "bool" ? (o === "true" ? "on" : "off") : o, selected: shown === o })));
          input.onchange = () => {
            const v = input.value;
            set(f.key, v === "" ? null : f.kind === "bool" ? v === "true" : v, input);
          };
          return el("div", { class: "check" }, el("div", { class: "body" }, el("div", { class: "t" }, el("code", { text: f.key })), f.help ? el("div", { class: "d", text: f.help }) : null), input);
        }
        input = el("input", { type: "number", name: f.key, step: f.kind === "float" ? "any" : "1", placeholder: "default", "aria-label": f.key });
        input.value = shown;
        const save = el("button", { text: "Save" });
        save.onclick = () => {
          const v = input.value.trim();
          set(f.key, v === "" ? null : Number(v), save);
        };
        return el("div", { class: "check" }, el("div", { class: "body" }, el("div", { class: "t" }, el("code", { text: f.key })), f.help ? el("div", { class: "d", text: f.help }) : null), el("div", { class: "row" }, input, save));
      };
      const raw = el("textarea", { rows: 18, spellcheck: "false", class: "mono", "aria-label": "config file" });
      raw.value = c.text || "";
      const said = el("div");
      const check = el("button", { text: "Check" });
      check.onclick = async () => {
        check.disabled = true;
        try {
          const r = await api("/api/config/check", { text: raw.value });
          setKids(said, r.ok
            ? el("div", { class: "said ok", text: "Looks right" + (r.restart ? "; " + r.sections.join(", ") + " take effect after a restart" : "") + "." })
            : el("div", { class: "said bad msg", dir: "auto", text: (r.line ? "Line " + r.line + ": " : "") + r.error }));
        } catch (e) { setKids(said, el("div", { class: "said bad msg", dir: "auto", text: e.message })); } finally { check.disabled = false; }
      };
      const save = el("button", { class: "primary", text: "Save the file" });
      save.onclick = async () => { if (await act("config/save", { text: raw.value }, save)) delete raw.dataset.dirty; };
      setKids(this.box,
        secHead("Config", c.path,
          c.prev ? btn("Undo the last save", "config/undo", {}) : null,
          c.last_good ? btn("Back to the last one that loaded", "config/restore", {}, "danger") : null),
        c.problem ? el("div", { class: "alert bad" }, el("div", { class: "body" },
          el("div", { class: "what", text: "The file doesn't load" + (c.problem.line ? " (line " + c.problem.line + ")" : "") }),
          el("div", { class: "fix msg", dir: "auto", text: c.problem.error }))) : null,
        (c.fields || []).length ? el("div", { class: "card" }, el("h3", { class: "mt0", text: "Settings" }), c.fields.map(row)) : null,
        el("div", { class: "card" },
          el("h3", { class: "mt0", text: "The file" }),
          el("p", { class: "muted small", text: "Secrets show as placeholders and stay as they are when you save. Commands, gates, secret routing and who gets in can only change on the machine." }),
          c.readable === false ? el("div", { class: "empty", text: "The file can't be read." }) : raw,
          said,
          el("div", { class: "row end" }, check, save)));
    },
  };


  // ---- navigation and polling --------------------------------------------
  // A phone gets a bottom bar of five (the rest in a sheet under "More");
  // from 900 px, a sidebar with everything. Same sections, same URLs.
  // The groups put what a beginner touches daily first (D3).

  const GROUPS = [
    ["Everyday", ["health", "chat", "tasks", "memory", "usage"]],
    ["Setup", ["channels", "connections", "models"]],
    ["Advanced", ["logs", "agents", "extensions", "routing", "console", "config", "settings"]],
  ];
  const order = GROUPS.flatMap((g) => g[1]).filter((s) => sections[s]);
  const TABS = ["health", "chat", "tasks", "usage"];
  const ICON_OF = { health: "health" };
  const SUB = { health: "how the bot is doing", chat: "talk to it here", tasks: "things it does on a schedule", memory: "what it remembers", usage: "spend and caps", channels: "where people reach the agent", connections: "services it can use", models: "which brain it thinks with", logs: "what happened", agents: "sub-agents", extensions: "skills, tools, MCP", routing: "which model for what", console: "ferrule commands", config: "the file, secrets hidden", settings: "look, language, backup" };
  let current = "health";
  let MANAGED = { on: false };
  let timer = null;
  let banner = null;
  let lastHealth = null;
  const badges = {};

  const title = (s) => sections[s].title || s[0].toUpperCase() + s.slice(1);

  function navLink(s, withSub) {
    const a = el("a", { href: "#" + s }, icon(ICON_OF[s] || s), el("span", { text: title(s) }),
      withSub && SUB[s] ? el("span", { class: "sub muted small", text: SUB[s] }) : null);
    a.dataset.s = s;
    a.onclick = (e) => { e.preventDefault(); closeSheet(); show(s); };
    return a;
  }

  function buildNav() {
    const rail = document.getElementById("rail");
    const groups = (link) => GROUPS.map(([name, list]) => {
      const mine = list.filter((s) => sections[s] && link.keep(s));
      return mine.length ? frag(el("div", { class: "group", text: name }), mine.map((s) => link.make(s))) : null;
    });
    setKids(rail,
      groups({ keep: () => true, make: (s) => navLink(s) }),
      el("div", { class: "rail-foot" },
        frag(el("b", { text: "session" }), el("br"), "12 h max · idle 30 min", el("br"),
          el("b", { text: "login" }), el("br"), "one-use link")));
    rail.hidden = false;
    const tabs = document.getElementById("tabs");
    const more = el("a", { href: "#more", "aria-haspopup": "dialog" }, icon("more"), el("span", { text: "More" }));
    more.dataset.s = "more";
    more.onclick = (e) => { e.preventDefault(); openSheet(); };
    tabs.replaceChildren(...TABS.filter((s) => sections[s]).map((s) => navLink(s)), more);
    tabs.hidden = false;
    const sheet = document.getElementById("sheet");
    sheet.replaceChildren(el("div", { class: "panel", role: "dialog", "aria-modal": "true", "aria-label": "All sections" },
      el("div", { class: "grip" }),
      groups({ keep: (s) => !TABS.includes(s), make: (s) => navLink(s, true) })));
    sheet.onclick = (e) => { if (e.target === sheet) closeSheet(); };
    markNav();
  }
  let sheetFrom = null;
  function openSheet() {
    const sheet = document.getElementById("sheet");
    sheetFrom = document.activeElement;
    sheet.hidden = false;
    const first = sheet.querySelector("a");
    if (first) first.focus();
  }
  function closeSheet() {
    const sheet = document.getElementById("sheet");
    if (sheet.hidden) return;
    sheet.hidden = true;
    if (sheetFrom && sheetFrom.isConnected && sheetFrom.focus) sheetFrom.focus();
    sheetFrom = null;
  }
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeSheet(); });

  function markNav() {
    for (const a of document.querySelectorAll("#rail a, #tabs a, #sheet a")) {
      const s = a.dataset.s;
      const on = s === current;
      a.classList.toggle("on", on || (s === "more" && !TABS.includes(current)));
      if (on) a.setAttribute("aria-current", "page"); else a.removeAttribute("aria-current");
      const n = s === "more" ? order.filter((x) => !TABS.includes(x)).reduce((t, x) => t + (badges[x] || 0), 0) : badges[s] || 0;
      let dot = a.querySelector(".dot");
      if (n && !dot) { dot = el("span", { class: "dot" }); a.append(dot); }
      if (dot) { if (n) { dot.textContent = n > 9 ? "9+" : String(n); dot.setAttribute("aria-label", n + " need you"); } else dot.remove(); }
    }
  }

  function show(name, opts) {
    if (!sections[name]) name = "health";
    current = name;
    const root = el("div");
    banner = el("div");
    document.getElementById("main").replaceChildren(banner, root);
    sections[name].mount(root);
    if (location.hash !== "#" + name) history.replaceState(null, "", "#" + name);
    markNav();
    window.scrollTo(0, 0);
    // A route change moves focus to the page, so a keyboard or screen-reader
    // user starts at its top instead of on the link they just used.
    if (!(opts && opts.focus === false)) document.getElementById("main").focus({ preventScroll: true });
    if (opts && opts.tile && sections[name].focus) sections[name].focus(opts.tile, true);
    refresh();
  }

  // A pasted or walked-back-to #section link navigates too.
  window.addEventListener("hashchange", () => {
    const h = location.hash.slice(1);
    if (csrf && order.includes(h) && h !== current) show(h);
  });

  // What the owner typed is theirs until they send it: a typed-in input
  // keeps its section from redrawing under it.
  document.addEventListener("input", (e) => {
    const t = e.target;
    if (t && /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName) && t.type !== "checkbox") t.dataset.dirty = "1";
  });
  function typing() {
    const main = document.getElementById("main");
    const f = document.activeElement;
    if (f && /^(INPUT|SELECT|TEXTAREA)$/.test(f.tagName) && main.contains(f)) return true;
    return !!main.querySelector("[data-dirty]");
  }

  // `auto`: a poll, not a click. A section polled by hand only still gets
  // the problems banner. The collar's sweep runs only while a turn does.
  async function refresh(auto) {
    clearTimeout(timer);
    timer = null;
    if (!csrf) return;
    const s = sections[current];
    const name = current;
    try {
      let ap;
      [lastHealth, ap] = await Promise.all([api("/api/health"), api("/api/approvals").catch(() => null)]);
      if (name !== current) return;
      document.getElementById("uptime").textContent = lastHealth.uptime ? "up " + lastHealth.uptime : "";
      if (lastHealth.version) document.getElementById("ver").textContent = "v" + lastHealth.version;
      if (lastHealth.instance) {
        // A named instance (M38): which agent this page runs, in the header and the tab.
        document.getElementById("inst").textContent = lastHealth.instance;
        document.getElementById("inst").hidden = false;
        document.title = "ferrule · " + lastHealth.instance;
      }
      document.getElementById("live-led").className = "led ok pulse";
      document.body.classList.toggle("running",
        (lastHealth.turns || []).some((t) => t.busy_secs !== null && t.busy_secs !== undefined));
      const probs = lastHealth.problems || [];
      badges.health = probs.length;
      badges.connections = probs.filter((p) => p.section === "connections").length;
      if (ap) badges.chat = ap.approvals.length;
      markNav();
      // Home draws the problems itself, under its status sentence.
      setKids(banner, name === "health" ? null : frag(problems(probs.filter((p) => p.section === name)), otherProblems(probs.filter((p) => p.section !== name))));
      if (!(auto === true && !s.every) && (s.live || !typing())) await s.load(lastHealth);
    } catch (e) {
      if (!csrf) return;
      document.getElementById("live-led").className = "led bad";
      banner.replaceChildren(el("div", { class: "alert bad" },
        el("div", { class: "body" }, el("div", { class: "what msg", dir: "auto", text: e.message }))));
    }
    schedule();
  }

  // Cheap when nobody's looking: no polling while the page is hidden, and
  // the server closes the session after its idle timeout anyway.
  function schedule() {
    clearTimeout(timer);
    timer = null;
    // 0: only when asked (console, config, logs, extensions).
    const every = sections[current].every ?? 15;
    if (every > 0 && !document.hidden && csrf) timer = setTimeout(() => refresh(true), every * 1000);
  }
  function stopPolling() { clearTimeout(timer); timer = null; }

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) stopPolling(); else if (csrf) refresh();
  });

  // ---- the command palette (D12) -------------------------------------------
  // `/` or Ctrl/⌘-K: every section and the handful of things worth a
  // shortcut, matched by substring, ignoring case and accents. An ARIA
  // combobox over a listbox, so a screen reader hears the highlighted row.

  const fold = (t) => String(t).normalize("NFD").replace(/[\u0300-\u036f]/g, "").toLowerCase();

  function themeButtonLabel() {
    const b = document.getElementById("theme");
    const t = window.ferruleTheme;
    if (!t) { b.hidden = true; return; }
    const p = t.pick();
    b.textContent = p;
    b.title = "Theme: " + (p === "auto" ? "follows the system" : p === "paper" ? "light" : "dark");
  }

  // Each action: what it is called, words it also answers to, an icon, and
  // `when` (absent = always). `run` may be async.
  const PALETTE_ACTIONS = [
    { label: "Run doctor", words: "check health problems", icon: "health",
      run: async () => { show("health"); await act("doctor/run", {}); } },
    { label: "Stop the running turn", words: "cancel halt", icon: "stop",
      when: () => busyTurns().length > 0,
      run: async () => { for (const t of busyTurns()) await act("turn/stop", { session: t.session }); } },
    { label: "Attach a photo", words: "picture image send upload camera", icon: "image",
      run: () => { show("chat"); if (sections.chat.pick) sections.chat.pick(); else sections.chat.wantPick = true; } },
    { label: "Switch theme", words: "dark light appearance paper forge", icon: "sun",
      run: () => { if (window.ferruleTheme) { window.ferruleTheme.next(); themeButtonLabel(); } } },
    { label: "Sign out", words: "log out", icon: "power",
      run: () => document.getElementById("logout").click() },
    { label: "Keyboard shortcuts", words: "help keys ?", icon: "search",
      run: () => shortcuts() },
  ];
  const busyTurns = () => ((lastHealth && lastHealth.turns) || []).filter((t) => t.busy_secs !== null && t.busy_secs !== undefined);

  function paletteItems() {
    const go = order.map((s) => ({ label: title(s), words: (SUB[s] || "") + " go open", icon: ICON_OF[s] || s, hint: "Go to", run: () => show(s) }));
    return go.concat(PALETTE_ACTIONS.filter((a) => !a.when || a.when()));
  }

  function palette() {
    if (document.querySelector("dialog.pal") || !csrf) return;
    const back = document.activeElement;
    const items = paletteItems().map((it, i) => Object.assign({ id: "pal-" + i, key: fold(it.label + " " + (it.words || "")) }, it));
    let shown = items;
    let at = 0;
    const d = el("dialog", { class: "dlg pal", "aria-label": "Search and commands" });
    const list = el("ul", { class: "pal-list", id: "pal-list", role: "listbox", "aria-label": "Results" });
    const box = el("input", {
      type: "text", class: "pal-input", role: "combobox", "aria-expanded": "true", "aria-controls": "pal-list",
      "aria-autocomplete": "list", autocomplete: "off", autocapitalize: "off", spellcheck: "false",
      placeholder: "Go to a page or run a command…", "aria-label": "Search and commands",
    });
    const paint = () => {
      list.replaceChildren(...(shown.length ? shown.map((it, i) => {
        const li = el("li", { id: it.id, role: "option", class: i === at ? "on" : null, "aria-selected": i === at ? "true" : "false" },
          icon(it.icon), el("span", { class: "l", text: it.label }), it.hint ? el("span", { class: "muted small", text: it.hint }) : null);
        li.onclick = () => choose(it);
        li.onmousemove = () => { if (at !== i) { at = i; paint(); } };
        return li;
      }) : [el("li", { class: "empty", role: "presentation", text: "Nothing matches." })]));
      if (shown[at]) box.setAttribute("aria-activedescendant", shown[at].id); else box.removeAttribute("aria-activedescendant");
      const on = list.querySelector(".on");
      if (on && on.scrollIntoView) on.scrollIntoView({ block: "nearest" });
    };
    const choose = (it) => { d.close(); Promise.resolve(it.run()).catch((e) => toast(e.message, true)); };
    box.oninput = () => {
      const q = fold(box.value).trim();
      shown = q ? items.filter((it) => q.split(/\s+/).every((w) => it.key.includes(w))) : items;
      at = 0;
      paint();
    };
    box.onkeydown = (e) => {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        if (shown.length) at = (at + (e.key === "ArrowDown" ? 1 : shown.length - 1)) % shown.length;
        paint();
      } else if (e.key === "Enter") {
        e.preventDefault();
        if (shown[at]) choose(shown[at]);
      }
    };
    d.append(box, list);
    d.addEventListener("close", () => { d.remove(); if (back && back.isConnected && back.focus && !document.activeElement.closest("#main")) back.focus(); });
    d.addEventListener("click", (e) => { if (e.target === d) d.close(); });
    document.body.append(d);
    paint();
    d.showModal();
    box.focus();
  }

  function shortcuts() {
    const rows = [["/", "Search pages and commands"], ["Ctrl K  ·  ⌘ K", "The same, from anywhere"], ["?", "This list"], ["Esc", "Close what's open"]];
    const d = el("dialog", { class: "dlg", "aria-labelledby": "dlg-title" });
    d.append(el("form", { method: "dialog" },
      el("h3", { id: "dlg-title", text: "Keyboard shortcuts" }),
      el("dl", { class: "keys" }, rows.map(([k, v]) => frag(el("dt", null, el("kbd", { text: k })), el("dd", { text: v })))),
      el("div", { class: "row" }, el("button", { class: "primary", text: "Done" }))));
    d.addEventListener("close", () => d.remove());
    document.body.append(d);
    d.showModal();
  }

  document.addEventListener("keydown", (e) => {
    if (!csrf || e.defaultPrevented) return;
    const inField = e.target && (/^(INPUT|TEXTAREA|SELECT)$/.test(e.target.tagName) || e.target.isContentEditable);
    if ((e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === "k") { e.preventDefault(); palette(); return; }
    if (inField || e.ctrlKey || e.metaKey || e.altKey || document.querySelector("dialog[open]")) return;
    if (e.key === "/") { e.preventDefault(); palette(); }
    else if (e.key === "?") { e.preventDefault(); shortcuts(); }
  });

  window.ferrule = { el, api, sections, show };

  // The collar mark and the theme toggle: present before any login, so
  // even the logged-out card looks like ferrule.
  function bootChrome() {
    const s = el("svg", { class: "collar", viewBox: "0 0 26 26", role: "img" });
    s.append(
      el("line", { class: "bar", x1: 6.5, y1: 19.5, x2: 19.5, y2: 6.5 }),
      el("circle", { class: "ring", cx: 13, cy: 13, r: 9.5, "stroke-dasharray": "44.7 15", transform: "rotate(135 13 13)" }),
      el("circle", { class: "sweep", cx: 13, cy: 13, r: 9.5 }));
    document.getElementById("mark").replaceChildren(s);
    // auto (the system's) → paper → forge; theme.js applied it before paint.
    const t = window.ferruleTheme;
    themeButtonLabel();
    if (t) document.getElementById("theme").onclick = () => { t.next(); themeButtonLabel(); };
    // The skip link: a plain "#main" would put a non-section in the address
    // bar, which a reload reads as a login token.
    document.querySelector(".skip").onclick = (e) => { e.preventDefault(); document.getElementById("main").focus(); };
    const find = document.getElementById("find");
    find.append(icon("search"));
    find.onclick = () => palette();
  }

  async function start() {
    bootChrome();
    // A login link carries its token after '#': the browser never sends it
    // to a server, and it's gone from the address bar at once. A section
    // name after '#' is just navigation, from a saved tab.
    const hash = location.hash.length > 1 ? location.hash.slice(1) : "";
    if (hash && !order.includes(hash)) {
      history.replaceState(null, "", BASE + "/");
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
    document.getElementById("find").hidden = false;
    MANAGED = await api("/api/managed").catch(() => ({ on: false }));
    buildNav();
    show(order.includes(hash) ? hash : "health", { focus: false });
  }

  document.addEventListener("DOMContentLoaded", start);
})();
