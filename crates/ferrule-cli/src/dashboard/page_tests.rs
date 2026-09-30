//! M47: guards on the page's own files. The dashboard has no build step and
//! no framework, so nothing else stops a colour that fails contrast, an icon
//! that doesn't exist or a script from another origin from sneaking in.

use std::collections::BTreeMap;

const CSS: &str = include_str!("assets/app.css");
const JS: &str = include_str!("assets/app.js");
const HTML: &str = include_str!("assets/index.html");

type Rgb = [f64; 3];

/// The declarations of one CSS rule block, `--name` → raw value.
fn tokens(css: &str, selector: &str) -> BTreeMap<String, String> {
    let start = css
        .find(&format!("{selector}{{"))
        .unwrap_or_else(|| panic!("no `{selector}` block"))
        + selector.len()
        + 1;
    let end = start + css[start..].find('}').unwrap();
    css[start..end]
        .split(';')
        .filter_map(|d| {
            let (k, v) = d.split_once(':')?;
            let k = k.trim();
            k.starts_with("--")
                .then(|| (k[2..].to_string(), v.trim().to_string()))
        })
        .collect()
}

fn hex(v: &str) -> Rgb {
    let h = v.trim_start_matches('#');
    assert_eq!(h.len(), 6, "expected #rrggbb, got {v}");
    let n = |i: usize| f64::from(u8::from_str_radix(&h[i..i + 2], 16).unwrap());
    [n(0), n(2), n(4)]
}

/// A token as an opaque colour: `#rrggbb` as it is, `rgba(r,g,b,a)` blended
/// over `under` (a soft tint is only ever painted over a surface).
fn colour(v: &str, under: Rgb) -> Rgb {
    if let Some(inner) = v.strip_prefix("rgba(").and_then(|r| r.strip_suffix(')')) {
        let p: Vec<f64> = inner
            .split(',')
            .map(|x| x.trim().parse().unwrap())
            .collect();
        return [0, 1, 2].map(|i| p[i] * p[3] + under[i] * (1.0 - p[3]));
    }
    hex(v)
}

fn luminance(c: Rgb) -> f64 {
    let f = |v: f64| {
        let v = v / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(c[0]) + 0.7152 * f(c[1]) + 0.0722 * f(c[2])
}

fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Both themes, the second laid over the first the way the cascade does.
fn themes() -> [(&'static str, BTreeMap<String, String>); 2] {
    let light = tokens(CSS, ":root");
    let mut dark = light.clone();
    dark.extend(tokens(CSS, "[data-theme=\"forge\"]"));
    [("paper", light), ("forge", dark)]
}

#[test]
fn tokens_meet_wcag_aa() {
    let mut failures = Vec::new();
    for (theme, t) in themes() {
        let get = |name: &str, under: Rgb| colour(&t[name], under);
        let surfaces = ["bg", "panel", "panel-2"];
        let solid = |name: &str| hex(&t[name]);
        let mut check = |what: String, fg: Rgb, bg: Rgb, min: f64| {
            let c = contrast(fg, bg);
            if c < min {
                failures.push(format!("{theme}: {what} is {c:.2}, needs {min}"));
            }
        };
        for s in surfaces {
            let under = solid(s);
            for text in [
                "ink",
                "ink-2",
                "muted",
                "copper-strong",
                "ok",
                "warn",
                "bad",
            ] {
                check(format!("{text} on {s}"), get(text, under), under, 4.5);
            }
            // The tinted backgrounds of tags, notices and status pills.
            for k in ["ok", "warn", "bad", "copper"] {
                let soft = get(&format!("{k}-soft"), under);
                let text = if k == "copper" { "copper-strong" } else { k };
                check(
                    format!("{text} on {k}-soft over {s}"),
                    get(text, under),
                    soft,
                    4.5,
                );
            }
        }
        let panel3 = solid("panel-3");
        check("muted on panel-3".into(), get("muted", panel3), panel3, 4.5);
        let panel = solid("panel");
        check("steel on panel".into(), get("steel", panel), panel, 4.5);
        for b in ["copper", "copper-strong"] {
            check(
                format!("on-copper on {b}"),
                solid("on-copper"),
                solid(b),
                4.5,
            );
        }
        // The count badge and the danger button print the panel colour on it.
        check("panel on bad".into(), solid("panel"), solid("bad"), 4.5);
        // Not text, but the focus ring and the active tab must be seen.
        check(
            "copper on bg (focus ring)".into(),
            solid("copper"),
            solid("bg"),
            3.0,
        );
    }
    assert!(
        failures.is_empty(),
        "contrast failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn every_colour_is_a_token() {
    // After the two token blocks nothing may spell a colour: a hex or an
    // rgba() written into a rule would dodge the contrast test above.
    let after = &CSS[CSS.find("[data-theme=\"forge\"]{").unwrap()..];
    let after = &after[after.find('}').unwrap()..];
    let after = &after[after.find(":root{").unwrap()..]; // the second, theme-free :root
    let bytes = after.as_bytes();
    for (i, w) in bytes.windows(2).enumerate() {
        if w[0] == b'#' && w[1].is_ascii_hexdigit() {
            let run = after[i + 1..]
                .chars()
                .take_while(char::is_ascii_hexdigit)
                .count();
            let next = after[i + 1 + run..].chars().next().unwrap_or(' ');
            let is_id = next.is_ascii_alphanumeric() || next == '-' || next == '_';
            assert!(
                is_id || ![3, 4, 6, 8].contains(&run),
                "a hex colour in a rule: {}",
                &after[i..(i + 12).min(after.len())]
            );
        }
    }
    // Shadows and scrims are tokens too; the font faces and the two token
    // blocks above are the only places rgba() may appear.
    let rules = &after[after.find("*{box-sizing").unwrap()..];
    assert!(!rules.contains("rgba("), "an rgba() in a rule, not a token");
}

fn quoted_after<'a>(src: &'a str, marker: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find(marker) {
        rest = &rest[i + marker.len()..];
        if let Some(name) = rest.strip_prefix('"').and_then(|r| r.split('"').next()) {
            out.push(name);
        }
    }
    out
}

fn icon_names() -> Vec<String> {
    let start = JS.find("const ICONS = {").expect("ICONS") + "const ICONS = {".len();
    let end = start + JS[start..].find("\n  };").unwrap();
    JS[start..end]
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let (k, _) = l.split_once(':')?;
            let k = k.trim().trim_matches('"');
            (l.contains(": \"") && !k.is_empty() && !k.contains(' ')).then(|| k.to_string())
        })
        .collect()
}

#[test]
fn every_icon_used_exists() {
    let have = icon_names();
    assert!(have.len() > 40, "parsed only {} icons", have.len());
    let mut asked: Vec<&str> = quoted_after(JS, "icon(");
    asked.extend(quoted_after(JS, "icon: "));
    asked.extend(quoted_after(JS, "ICON_OF = { health: "));
    // Each section is drawn with the icon of its own name.
    let groups = JS
        .split("const GROUPS = [")
        .nth(1)
        .unwrap()
        .split("\n  ];")
        .next()
        .unwrap();
    for line in groups.lines() {
        let names = line.split_once(", [").map_or("", |(_, l)| l);
        asked.extend(names.split('"').skip(1).step_by(2));
    }
    assert!(asked.len() > 10, "found only {} icon uses", asked.len());
    let missing: Vec<&&str> = asked
        .iter()
        .filter(|a| !have.iter().any(|h| h == **a))
        .collect();
    assert!(missing.is_empty(), "icons used but not drawn: {missing:?}");
}

#[test]
fn the_page_loads_nothing_from_elsewhere() {
    assert!(!CSS.contains("http://") && !CSS.contains("https://") && !CSS.contains("@import"));
    assert!(!CSS.contains("url(http") && !CSS.contains("url(//"));
    for line in HTML.lines() {
        for attr in ["src=", "href="] {
            for part in line.split(attr).skip(1) {
                let v = part.trim_start_matches('"');
                assert!(
                    !v.starts_with("http") && !v.starts_with("//"),
                    "index.html loads from elsewhere: {line}"
                );
            }
        }
    }
    // In the script an address may only be a link the reader clicks, a
    // placeholder, or the SVG namespace: never something the page fetches.
    for line in JS
        .lines()
        .filter(|l| l.contains("http://") || l.contains("https://"))
    {
        assert!(
            line.contains("2000/svg") || line.contains("href:") || line.contains("placeholder"),
            "the script names an address it may load: {line}"
        );
    }
    assert!(!JS.contains("@import") && !JS.contains("importScripts"));
    assert!(
        !JS.contains("XMLHttpRequest") && !JS.contains("WebSocket") && !JS.contains("EventSource")
    );
}

/// The `GLOSSARY` keys and the plain sentence each carries.
fn glossary() -> BTreeMap<String, String> {
    let body = JS
        .split("const GLOSSARY = {")
        .nth(1)
        .expect("a GLOSSARY")
        .split("\n  };")
        .next()
        .unwrap();
    body.lines()
        .filter_map(|l| {
            let (k, v) = l.trim().split_once(": \"")?;
            Some((k.to_string(), v.trim_end_matches("\",").to_string()))
        })
        .collect()
}

#[test]
fn every_tip_has_a_definition() {
    let have = glossary();
    assert!(have.len() >= 15, "parsed only {} terms", have.len());
    for (term, meaning) in &have {
        assert!(
            meaning.split_whitespace().count() >= 5 && meaning.ends_with('.'),
            "{term}: a definition is a sentence, got {meaning:?}"
        );
    }
    let asked = quoted_after(JS, "tip(");
    assert!(!asked.is_empty(), "the page uses no tip()");
    let missing: Vec<&&str> = asked.iter().filter(|a| !have.contains_key(**a)).collect();
    assert!(
        missing.is_empty(),
        "tip() words with no definition: {missing:?}"
    );
}

const API: &str = include_str!("api.rs");

/// The paths `api::route` answers, read from its two `match` blocks: a line
/// that starts a match arm (or continues an `|` list) and the quoted names
/// before its `=>`.
fn routes() -> Vec<String> {
    let start = API.find("pub async fn route").expect("route()");
    let end = API[start..]
        .find("// ---- Health")
        .expect("the Health marker")
        + start;
    let mut out = Vec::new();
    for line in API[start..end].lines() {
        let t = line.trim_start();
        if !(t.starts_with('"') || t.starts_with("| \"")) {
            continue;
        }
        let head = t.split("=>").next().unwrap_or(t);
        for (i, part) in head.split('"').enumerate() {
            if i % 2 == 1 && !part.is_empty() {
                out.push(part.to_string());
            }
        }
    }
    out
}

/// Routes the page has no button for, and why. Anything else that stops
/// being called by `app.js` is an ability lost by accident (D15).
const NOT_CALLED_BY_NAME: &[(&str, &str)] = &[
    (
        "console/parity",
        "the console page never used it; `ferrule console` tests read it",
    ),
    (
        "eval/estimate",
        "the eval card starts a run without asking for the estimate first; only tests use it",
    ),
    (
        "run/cancel",
        "runs are followed on the page, but only doctor and eval have a Cancel, through their own routes",
    ),
];

#[test]
fn every_route_is_used_by_the_page() {
    let routes = routes();
    assert!(
        routes.len() > 60,
        "the parser found too few routes: {routes:?}"
    );
    let mut unused = Vec::new();
    for r in &routes {
        if NOT_CALLED_BY_NAME.iter().any(|(n, _)| n == r) {
            continue;
        }
        // `"/api/telegram/" + path` builds its routes from a prefix.
        let prefix = r.rsplit_once('/').map(|(p, _)| format!("\"/api/{p}/\""));
        let called = JS.contains(&format!("/api/{r}"))
            || JS.contains(&format!("\"{r}\""))
            || prefix.is_some_and(|p| JS.contains(&p));
        if !called {
            unused.push(r.clone());
        }
    }
    assert!(
        unused.is_empty(),
        "app.js no longer calls: {unused:?}. Put it back, or say why in NOT_CALLED_BY_NAME."
    );
}

#[test]
fn the_page_uses_no_native_dialogs() {
    for banned in [
        "window.confirm(",
        "window.prompt(",
        "window.alert(",
        "window.open(",
    ] {
        assert!(!JS.contains(banned), "app.js calls {banned}");
    }
    for line in JS.lines() {
        let t = line.trim_start();
        if t.starts_with("//") {
            continue;
        }
        for banned in ["confirm(", "prompt(", "alert("] {
            for (at, _) in line.match_indices(banned) {
                let before = line[..at].chars().next_back();
                let ok = before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
                assert!(ok, "app.js calls a native dialog: {line}");
            }
        }
    }
}

#[test]
fn the_index_has_landmarks_and_a_skip_link() {
    assert!(HTML.contains("<html lang=\"en\">"));
    assert!(HTML.contains("<header"), "no header landmark");
    assert!(
        HTML.contains("<main id=\"main\" tabindex=\"-1\""),
        "main must take focus on a route change"
    );
    let navs = HTML.matches("<nav ").count();
    assert_eq!(navs, 2, "two navs (sidebar, phone bar)");
    for nav in HTML.split("<nav ").skip(1) {
        assert!(
            nav.split('>').next().unwrap().contains("aria-label="),
            "a nav without a label"
        );
    }
    // The skip link is the first focusable thing and points at main.
    let skip = HTML.find("class=\"skip\"").expect("a skip link");
    assert!(skip < HTML.find("<header").unwrap());
    assert!(HTML.contains("href=\"#main\""));
    assert!(HTML.contains("id=\"toast\"") && HTML.contains("aria-live="));
}

const HE: &str = include_str!("assets/lang-he.js");
const THEME: &str = include_str!("assets/theme.js");

/// The string literal that starts at `s[0]` (an opening quote), raw as
/// written, escapes and all; `None` when it never closes.
fn literal(s: &str) -> Option<&str> {
    let mut escaped = false;
    for (i, c) in s.char_indices().skip(1) {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return Some(&s[1..i]),
            _ => {}
        }
    }
    None
}

/// Every fixed word the page can show in Hebrew: what `tr("…")` and
/// `fill("…", n)` are given, and what carries a `/*tr*/` marker where the
/// call comes later (a nav label, a palette entry).
fn shell_strings() -> Vec<String> {
    let mut out = Vec::new();
    for marker in ["tr(\"", "fill(\"", "/*tr*/\""] {
        for (at, _) in JS.match_indices(marker) {
            // Not `str(`, `attr(` and the like.
            if marker != "/*tr*/\""
                && JS[..at]
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
            {
                continue;
            }
            let quote = at + marker.len() - 1;
            out.push(
                literal(&JS[quote..])
                    .expect("an unclosed string")
                    .to_string(),
            );
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The keys of `lang-he.js`: one entry to a line, `  "key": "value",`.
fn hebrew_keys() -> Vec<String> {
    HE.lines()
        .filter(|l| l.starts_with("  \""))
        .map(|l| literal(&l[2..]).expect("an unclosed key").to_string())
        .collect()
}

#[test]
fn every_shell_string_has_a_hebrew_version() {
    let words = shell_strings();
    assert!(
        words.len() > 100,
        "the scan found too few strings: {}",
        words.len()
    );
    let have = hebrew_keys();
    let missing: Vec<&String> = words.iter().filter(|w| !have.contains(w)).collect();
    assert!(missing.is_empty(), "no Hebrew for: {missing:#?}");
    // And nothing stale: a key no call asks for is a translation nobody sees.
    let stale: Vec<&String> = have.iter().filter(|k| !words.contains(k)).collect();
    assert!(
        stale.is_empty(),
        "lang-he.js has words the page never asks for: {stale:#?}"
    );
    let mut sorted = have.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), have.len(), "a key twice in lang-he.js");
}

#[test]
fn a_hebrew_word_keeps_the_placeholders_of_its_english() {
    for line in HE.lines().filter(|l| l.starts_with("  \"")) {
        let key = literal(&line[2..]).unwrap();
        let rest = &line[2 + key.len() + 2..];
        let val = literal(rest.trim_start_matches(':').trim_start()).expect("a value");
        assert_eq!(
            key.matches("%s").count(),
            val.matches("%s").count(),
            "{key}"
        );
        assert!(!val.contains('<') && !val.contains('>'), "markup in {key}");
        assert!(!val.trim().is_empty(), "an empty translation of {key}");
    }
}

#[test]
fn theme_js_sets_the_language_and_direction_before_paint() {
    assert!(THEME.contains("setAttribute(\"lang\", lang)"));
    assert!(THEME.contains("setAttribute(\"dir\", lang === \"he\" ? \"rtl\" : \"ltr\")"));
    // Before the first paint means a script in <head>, not deferred.
    let script = HTML.find("src=\"/theme.js\"").unwrap();
    assert!(script < HTML.find("<body>").unwrap());
    assert!(!HTML[script - 30..script + 30].contains("defer"));
    // The Hebrew words are fetched by the page, only when chosen: never
    // named in the HTML, so an English reader never downloads them.
    assert!(!HTML.contains("lang-he"));
    assert!(JS.contains("/lang-he.js") && JS.contains("LANG !== \"he\""));
}

#[test]
fn the_hebrew_file_is_data_only() {
    assert!(HE.contains("window.FERRULE_HE = {"));
    for bad in [
        "eval(",
        "Function(",
        "innerHTML",
        "fetch(",
        "import(",
        "document.",
    ] {
        assert!(!HE.contains(bad), "{bad} in lang-he.js");
    }
}
