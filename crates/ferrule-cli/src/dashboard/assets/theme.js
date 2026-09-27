// Light (paper) or dark (forge) before the first paint: the owner's pick
// if they made one, else the system's, followed live while it's "auto".
// A file of its own: the page's CSP runs no inline script.
"use strict";
(function () {
  var pick = null;
  try { pick = localStorage.getItem("ferrule-theme"); } catch (e) { /* no storage */ }
  if (pick !== "paper" && pick !== "forge") pick = "auto";
  var dark = window.matchMedia ? matchMedia("(prefers-color-scheme: dark)") : null;
  var apply = function () {
    var t = pick === "auto" ? (dark && dark.matches ? "forge" : "paper") : pick;
    document.documentElement.setAttribute("data-theme", t);
    document.documentElement.setAttribute("data-theme-pick", pick);
  };
  apply();
  if (dark && dark.addEventListener) dark.addEventListener("change", function () { if (pick === "auto") apply(); });
  // auto → paper → forge → auto, for the toggle in app.js.
  window.ferruleTheme = {
    pick: function () { return pick; },
    next: function () {
      pick = pick === "auto" ? "paper" : pick === "paper" ? "forge" : "auto";
      try { if (pick === "auto") localStorage.removeItem("ferrule-theme"); else localStorage.setItem("ferrule-theme", pick); } catch (e) { /* no storage */ }
      apply();
      return pick;
    },
  };
})();
