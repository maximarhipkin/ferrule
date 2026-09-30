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
    let order = JS
        .split("const order = [")
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    asked.extend(order.split(',').map(|s| s.trim().trim_matches('"')));
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
