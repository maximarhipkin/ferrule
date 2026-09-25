// The ferrule dashboard (docs/m22-dashboard.md). Plain DOM, built with
// textContent only: nothing the server sends is ever parsed as HTML.
"use strict";
(function () {
  let csrf = null;

  function el(tag, attrs, ...kids) {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === null || v === undefined || v === false) continue;
      if (k === "class") e.className = v;
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
    if (!r.ok) throw new Error(data.error || ("HTTP " + r.status));
    return data;
  }

  function loggedOut(why) {
    csrf = null;
    document.getElementById("nav").hidden = true;
    document.getElementById("logout").hidden = true;
    const main = document.getElementById("main");
    main.replaceChildren(el("div", { class: "card" },
      el("p", { text: why || "Not logged in." }),
      el("p", { class: "muted", text: "Send /dashboard to the bot on Telegram for a new link, or run `ferrule dashboard link` on the machine." })));
  }

  // The sections; each part of M22 adds its own.
  const sections = {};
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
    document.getElementById("main").replaceChildren(el("p", { class: "muted", text: "Logged in." }));
  }

  document.addEventListener("DOMContentLoaded", start);
})();
