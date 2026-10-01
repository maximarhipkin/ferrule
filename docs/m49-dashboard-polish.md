# M49 — dashboard visual polish

**Status:** plan written 2026-10-01 (Opus 5.5); the build follows in the
same session (Sonnet 5.5). Branch `m49-dashboard-polish`, cut from main `0786e4a`
(M48 merged as PR #40).

## Why

Max, 1 Oct, after M47 and M48: the whole dashboard needs a visual
redesign. He dislikes the fonts, in English and in Hebrew, and the colours.
In his words, "all the selects and buttons should look cleaner and more
modern. Right now it looks really amateur." The target is the restraint of
Linear, Vercel and Stripe, in light and dark, at 390 and 1280 px.

M47 fixed the structure: navigation, sections, states, phone layout and
accessibility. M49 doesn't touch the structure. It changes how the page
looks: type, colour, controls, icons and density. Every section, route,
ability and test stays.

## Constraints (from the brief, all kept)

- Plain HTML/CSS/JS in `crates/ferrule-cli/src/dashboard/assets/`, with
  no build step and no npm at runtime.
- Nothing loads from another origin: no CDN, no external font or script.
  Self-hosted woff2 subsets are allowed, with the OFL text shipped.
- It works under the M44 path prefix (`<meta name="ferrule-base">`,
  `index_for`) and behind the tunnel. CSS font URLs stay relative.
- Settings the managed policy locks stay read-only (`.field.locked`,
  `.lock`), and look it.
- User text sits in `dir="auto"` elements and is set with `textContent`
  only. Hebrew and Arabic render RTL, and secrets stay redacted.
- Nothing on the page calls a model.
- The CSP stays as it is (`http.rs:337`):
  `default-src 'self'; img-src 'self' data:; …`. The page may use data-URI
  images, but no inline script or style attribute that needs new CSP
  sources.

## Audit — what looks amateur, ranked

This list comes from the M47 "after" screenshots
(`docs/assets/m47/after/`, 60 shots) and from reading `app.css` and
`app.js`. Step 1 re-takes them as the M49 "before" set and confirms each
item; the builder adds anything new it sees there, ranked into this list.

1. **Selects are native.** Each OS draws its own arrow, and select
   heights don't match inputs or buttons. This is the biggest single
   "amateur" signal and the main ask.
2. **Controls are chunky on a desktop.** Every button, input and chip is
   40–44 px, with weight 600 text. That suits a phone, but on a 1280 px
   screen it reads as a toy.
3. **Copper is everywhere.** The primary button, the active nav item,
   the chips, icons, links, the focus ring and the toast border are all
   copper, so nothing stands out. The tan disabled primary looks broken.
4. **Glyph buttons.** The fallback list uses `↑ ↓ ×` as text, the pager
   `← Newer` and `Older →`, and the notice close button `×`. They render
   in the text font at a different weight from the icons and look like
   typos.
5. **Alerts are heavy.** Warn notices are beige blocks with bold amber
   text, and they compete with the content they sit above.
6. **Home's "N other things need your attention" strip** wraps badly at
   390.
7. **The header.** The theme button says a cryptic `auto`. "Log out" is
   plain text. The uptime and version are monospace.
8. **Native `<details>` triangles** sit next to our own chevrons.
9. **Wrapping and truncation.** The provider key placeholder is cut
   off, and "List its models" wraps onto two lines.
10. **Pill chips on a phone** are oversized ovals, not a segmented
    control.
11. **The backup file name** breaks character by character at 390.
12. **Home's hero row has two loud buttons** (Run doctor and a red "Kill
    switch on"). Model and catalog rows have 3–4 equal buttons each.
13. **The spend rings** are 9 px strokes, and the period chips are
    pills.
14. **Page title and subtitle** sit on mismatched baselines in
    `.sec-head`.
15. **The fonts.** Plex reads technical and dated, and Plex Sans Hebrew
    is wide and heavy next to it.

## Decisions

### D1. Fonts: Inter, Heebo for Hebrew only, and Geist Mono

**Chosen:**

| Role | Family | Axes kept | Subset | File | Bytes |
|---|---|---|---|---|---|
| UI, Latin | Inter | wght 400–600, opsz pinned at 14 | Latin | `inter-latin.woff2` | 23,652 |
| Hebrew | Heebo | wght 400–600 | Hebrew | `heebo-hebrew.woff2` | 7,280 |
| Code, numbers in code | Geist Mono | wght 400–600 | Latin + box drawing U+2500–257F | `geist-mono-latin.woff2` | 16,384 |

- **Total:** 47,316 B, against Plex's 128,924 B (sans-400 20,984,
  sans-600 22,260, hebrew-400 33,260, hebrew-600 35,152, mono 17,268).
- **A page with no Hebrew** loads Inter plus Geist Mono, ≈40 KB, against
  Plex's ≈60 KB.
- **Hebrew costs another 7 KB**, against Plex Hebrew's 68 KB.
- **Arabic** falls back to the system font through the `--sans` stack
  (`"Noto Sans Arabic"`, then `system-ui`). An Arabic subset isn't cheap
  and Arabic isn't a UI language.

**Why:**

- **Inter** is the face Linear, Vercel's older UI and much of Stripe's
  product use for this exact job. It has tabular figures and reads well at
  13–15 px.
- **Heebo** is a Hebrew sans built for Roboto-family metrics. Its
  x-height (1082/2048) sits close to Inter's (1118/2048), so mixed
  Hebrew/English lines look like one face. At 7 KB it is the cheapest
  Hebrew that looks right.
- **Geist Mono** pairs with a neo-grotesque UI face, and Vercel ships
  them together. JetBrains Mono (10,228 B for 400 only) was the close
  second, but it has the wider, more "IDE" look.

**Candidates measured and dropped:**

- Hebrew subsets: Rubik 7,020 B (too rounded beside Inter), Assistant
  5,320 B (light, small x-height), Noto Sans Hebrew 10,056 B (fine, but
  larger, with no gain over Heebo).
- Heebo for Latin as well: 26,532 B for Latin + Hebrew. Its Latin is
  Roboto-like and doesn't reach Inter's quality at small sizes.
- Inter with the opsz axis kept: 39,292 B. Pinning opsz at 14 saves
  15.6 KB, and the UI never goes past 28 px.
- Geist Mono 400 only: 9,692 B. The variable 400–600 file, at 16,384 B,
  is kept, because the console and the config editor bold their
  headings.
- Variable weights: all three are variable over 400–600. One file covers
  400, 500 and 600, so the weights cost nothing extra.

**Source.** google/fonts at commit
`9710da1eacb3be272583c3224dcb70f9da6eadbb`:

| File | sha256 |
|---|---|
| `ofl/inter/Inter[opsz,wght].ttf` | `29160a80ff49ddcab2c97711247e08b1fab27a484a329ce8b813d820dc559031` |
| `ofl/heebo/Heebo[wght].ttf` | `18f930b583fa8fe6b40b2f8263b7ac6afbac07adc91a12467874e7467d3ace30` |
| `ofl/geistmono/GeistMono[wght].ttf` | `d00e590b8eb3a59acc329b2d044fd143ae935090b7da33199ebee27cc7de8196` |

The recipe is in Plan step 2. The output files are deterministic; the
first 8 hex digits of their sha256 were `d1481e3a` (Inter), `9d633290`
(Heebo) and `9dea23b6` (Geist Mono).

**Layout features kept:** `kern`, `ccmp`, `locl`, `mark`, `mkmk`, plus
`tnum` and `case` for Inter. **Dropped:** `calt` and `liga`, so `->`, `=>`
and `!=` in model names and commands are never rewritten into arrows, and
other features we don't use.

**Licences.** All three are SIL OFL 1.1 with no Reserved Font Name, so a
subset may keep its name. The copyright lines:

- Inter: "Copyright 2020 The Inter Project Authors (https://github.com/rsms/inter)"
- Heebo: "Copyright 2014 The Heebo Project Authors (https://github.com/OdedEzer/heebo)"
- Geist Mono: "Copyright 2024 The Geist Project Authors (https://github.com/vercel/geist-font.git)"

`assets/fonts/OFL.txt` holds the three lines, then the OFL 1.1 text once.
It is served at `/fonts/OFL.txt` as before. The Plex files and the Plex
copyright line go.

**Line box.** Metrics, all in 2048 units per em:

| | Ascent | Descent | x-height | Cap height |
|---|---|---|---|---|
| Inter | 1984 | −494 | 1118 | 1490 |
| Heebo | 2146 | −862 | 1082 | 1456 |

Heebo's taller ascent and descent would make any line holding a Hebrew
word taller than an English one, and rows would jump. The Heebo
`@font-face` therefore sets `ascent-override:97%; descent-override:24%;
line-gap-override:0%`, which are Inter's own numbers (1984/2048 and
494/2048). The browser check asserts the heights are equal (Plan step 7).

**Cache.** The fonts are served `immutable` for a year, so a font's file
name must change whenever its bytes change. The new names differ from
Plex's, so old caches are never served. Any later font change must rename
the file (for example `inter-latin-2.woff2`). This rule goes in the
`FONTS` doc comment.

### D2. Colour: neutral greys, one indigo accent, copper only in the mark

- **Neutrals** are zinc-like greys with no warm cast. The current
  surfaces are beige-tinted (`#fafaf7`, `#f3f2ec`), which reads as dated
  next to Linear and Vercel.
- **One accent: indigo** (`#4f46e5` light, `#5e5ce6` dark). It is used
  for the primary button, links, the focus ring, the checked state and
  the bottom-tab current item, and nothing else.
- **Copper leaves the UI.** It stays only in the collar mark at the top
  left, as `--brand`, matching `docs/branding/logo.png`, a copper collar
  over steel. Copper as the accent was a strong opinion that made every
  state look like a warning, and it clashes with the warn amber. This
  is the brief's "decide on copper".
- **Status colours** stay quiet. Green, amber and red are used as text
  and icon colour on a soft tint, never as big filled areas. The only
  filled red is the destructive primary button and the count badge.
- **Light and dark are designed separately**, not inverted. Dark
  surfaces step up in lightness (bg < panel < panel-2 < panel-3), the
  accent is a touch lighter, and text accents use a pale `accent-strong`
  (`#a5a6ff`) instead of the button indigo.
- **Token names.** The `copper*` tokens become `accent*`. Theme names
  `paper` and `forge` stay: they are stored in readers' localStorage
  (`ferrule-theme`) and used by `theme.js`.

**The token blocks**, verbatim. They replace the current first `:root{}`
and `[data-theme="forge"]{}` blocks in `app.css`, keeping the same order
and one declaration per line:

```css
:root{
  --bg:#f7f7f8;
  --panel:#ffffff;
  --panel-2:#f4f4f5;
  --panel-3:#e9e9ec;
  --ink:#18181b;
  --ink-2:#3f3f46;
  --muted:#5e5e68;
  --line:#e4e4e7;
  --line-strong:#d4d4d8;
  --control:#8b8b94;
  --accent:#4f46e5;
  --accent-hover:#4338ca;
  --accent-strong:#4338ca;
  --accent-soft:rgba(79,70,229,.09);
  --on-accent:#ffffff;
  --ring:#4f46e5;
  --brand:#c26a35;
  --steel:#52606d;
  --ok:#126b35;
  --ok-soft:rgba(22,163,74,.09);
  --warn:#a14f05;
  --warn-soft:rgba(217,119,6,.12);
  --bad:#bb2626;
  --bad-soft:rgba(220,38,38,.09);
  --knob:#ffffff;
  --scrim:rgba(9,9,11,.45);
  --shadow-1:0 1px 2px rgba(9,9,11,.06);
  --shadow-2:0 4px 12px -2px rgba(9,9,11,.10),0 2px 4px -2px rgba(9,9,11,.06);
  --shadow-3:0 16px 40px -8px rgba(9,9,11,.20),0 4px 10px -4px rgba(9,9,11,.08);
  --chevron:url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='%235e5e68' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'%3E%3Cpath d='M6 9l6 6 6-6'/%3E%3C/svg%3E");
  --checkmark:url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='%23ffffff' stroke-width='3' stroke-linecap='round' stroke-linejoin='round'%3E%3Cpath d='M20 6 9 17l-5-5'/%3E%3C/svg%3E");
  color-scheme:light;
}
[data-theme="forge"]{
  --bg:#0c0c0e;
  --panel:#141417;
  --panel-2:#1b1b1f;
  --panel-3:#26262b;
  --ink:#ededf0;
  --ink-2:#b4b4bc;
  --muted:#93939d;
  --line:#26262b;
  --line-strong:#34343b;
  --control:#696973;
  --accent:#5e5ce6;
  --accent-hover:#6462e9;
  --accent-strong:#a5a6ff;
  --accent-soft:rgba(110,108,255,.16);
  --on-accent:#ffffff;
  --ring:#8583ff;
  --brand:#d98552;
  --steel:#9aa7b4;
  --ok:#4cc38a;
  --ok-soft:rgba(76,195,138,.13);
  --warn:#e5b13f;
  --warn-soft:rgba(229,177,63,.13);
  --bad:#f2776c;
  --bad-soft:rgba(242,119,108,.13);
  --knob:#f4f4f5;
  --scrim:rgba(0,0,0,.6);
  --shadow-1:0 1px 2px rgba(0,0,0,.4);
  --shadow-2:0 4px 12px -2px rgba(0,0,0,.5),0 2px 4px -2px rgba(0,0,0,.4);
  --shadow-3:0 16px 40px -8px rgba(0,0,0,.6),0 4px 10px -4px rgba(0,0,0,.45);
  --chevron:url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='%2393939d' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'%3E%3Cpath d='M6 9l6 6 6-6'/%3E%3C/svg%3E");
  color-scheme:dark;
}
```

**Rules the data-URI tokens must keep:**

- **No `;` anywhere inside them**, so no `;charset=utf8`, and **no `}`**.
  `page_tests.rs::tokens()` splits a block on `;` and ends it at the first
  `}`.
- **The `xmlns` attribute is required.** In the spike, an SVG data URI
  without `xmlns='http://www.w3.org/2000/svg'` drew nothing.
- A data URI can't read a CSS variable, so the stroke colour is spelled
  out in each theme as `%23` plus the hex. `--checkmark` is defined only
  in `:root`, because `--on-accent` is `#ffffff` in both themes. The new
  test `svg_tokens_draw_in_their_theme_colours` ties each one to its
  token.

**Contrast, measured** (WCAG 2 relative luminance; a soft tint is blended
over the surface it sits on, as `page_tests.rs::colour()` does). Every
pair passes:

| Pair | Light | Dark | Needs |
|---|---|---|---|
| ink / bg | 16.55 | 16.73 | 4.5 |
| muted / panel | 6.41 | 6.04 | 4.5 |
| muted / panel-2 | 5.83 | 5.64 | 4.5 |
| muted / panel-3 | 5.29 | 4.95 | 4.5 |
| accent-strong / panel-2 | 7.19 | 7.77 | 4.5 |
| ok / ok-soft over panel-2 | 5.47 | 6.15 | 4.5 |
| warn / warn-soft over panel-2 (closest) | 4.66 | 6.78 | 4.5 |
| bad / bad-soft over panel-2 | 4.88 | 5.14 | 4.5 |
| accent-strong / accent-soft over panel-2 | 6.31 | 6.44 | 4.5 |
| steel / panel | 6.46 | 7.49 | 4.5 |
| on-accent / accent | 6.29 | 5.06 | 4.5 |
| on-accent / accent-hover | 7.90 | 4.70 | 4.5 |
| panel / bad (count badge, danger button) | 6.14 | 6.68 | 4.5 |
| control / bg, panel, panel-2 (field borders, WCAG 1.4.11) | 3.15, 3.38, 3.07 | 3.60, 3.39, 3.16 | 3.0 |
| ring / bg, panel (focus) | 5.87, 6.29 | 6.22, 5.86 | 3.0 |

Two dark values were moved while measuring: `--control` went from
`#62626c` (2.85 on panel-2) to `#696973`, and `--accent-hover` from
`#6866eb` (4.48 with white) to `#6462e9`.

**The old test pair "on-copper on copper-strong" is dropped.**
`accent-strong` is a text colour (pale `#a5a6ff` in dark), and white on it
fails. It is never a button fill now: hover uses `--accent-hover`.

### D3. Type scale

The tokens stay in rem, so the reader's own text size wins. They are
designed mobile-first, and the desktop block below shrinks them:

| Token | Phone | Desktop (fine pointer, ≥ 900 px) | Used for |
|---|---|---|---|
| `--fs-xs` | .75rem (12) | same | tags, kbd, chart labels |
| `--fs-sm` | .8125rem (13) | same | secondary text, table cells, buttons, header meta |
| `--fs-md` | .9375rem (15) | .875rem (14) | body |
| `--fs-input` | 1rem (16) | .875rem (14) | input, select, textarea (16 on a phone stops iOS zooming) |
| `--fs-lg` | 1rem (16) | same | h2, card titles |
| `--fs-xl` | 1.125rem (18) | same | dialog titles |
| `--fs-2xl` | 1.25rem (20) | 1.375rem (22) | page h1 |
| `--fs-3xl` | 1.75rem (28) | same | big numbers in `.stat .v` |

- **Weights:** 400 body. `--fw-medium:500` for buttons, nav, labels, tags
  and table headers. `--fw-strong:600` for headings and the `.stat`
  numbers. Nothing uses 700.
- **Line height:** body 1.5, headings `--lh-tight:1.25`, controls 1.
- **Letter-spacing:** h1 −.015em, h2 −.01em, and 0 under
  `html[lang="he"]`, because Hebrew must not be tracked tight.
- **Numbers:** `font-variant-numeric:tabular-nums` stays on `body`.
- **Header meta in sans:** `.ver`, `.inst`, `.live` and `#uptime` move
  from `var(--mono)` to `var(--sans)` at `--fs-sm`, tabular. Mono stays
  only for code: `code`, `pre`, `.mono`, the console, textareas, file
  names and model references.

### D4. Controls — one system

**Sizes.** One radius for controls (`--r-md`), one for surfaces
(`--r-lg`), and 1 px borders everywhere:

| Token | Phone, or any coarse pointer | Desktop (fine pointer, ≥ 900 px) |
|---|---|---|
| `--h-sm` | 44px | 30px |
| `--h-md` | 44px | 36px |
| `--h-lg` | 48px | 40px |
| `--tap` | 44px | 36px |
| `--r-sm` | 6px | |
| `--r-md` | 8px | |
| `--r-lg` | 12px | |

Every button, input, select and segment is `--h-md`; `.sm` is `--h-sm`.
The phone check's 44 px rule holds because every height is 44 or more
below 900 px.

**Buttons** (`button`, `.btn`):

| Kind | Look |
|---|---|
| Secondary (the default) | panel fill, `--line-strong` border, `--shadow-1`, ink text at weight 500 and `--fs-sm`, padding 0 12px, gap 6px, 16 px icons. Hover panel-2, active panel-3. |
| `.primary` | accent fill and border, on-accent text. Hover and active `--accent-hover`. |
| `.ghost` | no fill or border, no shadow, ink-2 text. Hover panel-2 and ink text. |
| `.danger` | secondary look with bad text. Hover bad-soft. |
| `.danger.primary` | bad fill, panel text. |
| `.icon` | square, width equals its height, padding 0. |
| `.sm` | `--h-sm`, padding 0 10px. |
| `.link` | accent-strong, underline offset 2px, no box. |

- Every button has a 2 px `--ring` focus outline (offset 2px).
- Disabled is opacity .5, no shadow and `cursor:not-allowed`. Hover does
  nothing on a disabled button: use `:hover:not(:disabled)`.
- Busy keeps the existing spinner, whose top colour becomes `--accent`.

**Fields** (`input`, `select`, `textarea`):

- panel fill, a 1 px `--control` border, `--r-md`, height `--h-md`,
  padding 0 10px (textarea 8px 10px), `--fs-input`;
- hover: border `--ink-2`;
- focus: border `--accent`, plus the global `--ring` outline (offset 0 on
  fields, so the ring hugs the border);
- `[aria-invalid=true]`: border bad;
- disabled and read-only: panel-2 fill and muted text. `.field.locked`
  and `.lock` keep their markup and meaning.
- Placeholder: muted.

**Select** (the main ask): a native `<select>`, restyled. No JS
dropdown: native keeps the keyboard, screen readers and the phone
pickers for free, and the brief prefers it.

```css
select{appearance:none;-webkit-appearance:none;background:var(--chevron) no-repeat right 10px center/16px 16px var(--panel);padding-inline-end:34px;cursor:pointer;text-overflow:ellipsis}
html[dir="rtl"] select{background-position:left 10px center}
select option,select optgroup{background:var(--panel);color:var(--ink)}
select[multiple],select[size]:not([size="1"]){background-image:none;padding-inline-end:10px}
```

Its height comes from the shared field rule, so a select is exactly as
tall as the input and button beside it.

**Checkbox and radio:**

```css
input[type=checkbox],input[type=radio]{appearance:none;-webkit-appearance:none;inline-size:16px;block-size:16px;min-height:0;padding:0;margin:0;flex:none;border:1px solid var(--control);background:var(--panel);display:inline-grid;place-content:center;cursor:pointer}
input[type=checkbox]{border-radius:4px}
input[type=radio]{border-radius:50%}
input[type=checkbox]:checked{background:var(--checkmark) center/12px 12px no-repeat var(--accent);border-color:var(--accent)}
input[type=radio]:checked{border-color:var(--accent);box-shadow:inset 0 0 0 4px var(--panel);background:var(--accent)}
```

- They are 20 px on a phone (`@media (max-width:899px),(pointer:coarse)`),
  with a 12 px tick.
- `label.cb` keeps `min-height:var(--tap)`; the phone check measures a
  checkbox by its label.

**Switch** (`.switch`, `role=switch`):

- a 36 × 20 track: off is `--control`, on is `--accent`;
- a 16 px `--knob` circle with `--shadow-1`, 2 px inset;
- the button box stays `--tap` tall and `max(--tap, 44px)` wide on a
  phone, with the track centred in it.

**Segmented control** (`.chips`, the "Range", "Show", Appearance and
Language pickers). The markup stays `div.chips[role=group] >
button.chip[aria-pressed]`, so behaviour and tests don't change.

- **Track:** `display:inline-flex`, panel-2 fill, `--hair` border,
  `--r-md`, padding 2px, gap 2px, `max-inline-size:100%`,
  `overflow-x:auto`.
- **Segment:** `.chip{min-height:var(--h-sm)}`. That is 30 px on a
  desktop, inside a 36 px track (2 px padding plus a 1 px border on each
  side), so the track lines up with a button beside it. On a phone it is
  44 px, the size the phone check measures each chip button against, and
  the track there is 50 px. Padding 0 12px, transparent, no border or
  shadow, ink-2 text at 500, `--r-sm`.
- **Pressed:** panel fill, ink text, and
  `box-shadow:var(--shadow-1),0 0 0 1px var(--line-strong)`.

**Tags** (`.tag`):

- 20 px tall, padding 0 6px, `--r-sm`, `--fw-medium`, `--fs-xs`, line
  height 20px;
- neutral: panel-2 fill, ink-2 text;
- `.ok`, `.warn`, `.bad` and `.accent` use the soft fill with their own
  text colour (`.accent` uses accent-strong).
- `.tag.copper` is renamed `.tag.accent`.

**Alerts and notices** (`.alert`):

- panel fill;
- a 1 px border set to `color-mix(in srgb,var(--<sev>) 35%,var(--line))`;
- a 16 px coloured icon;
- `.what` at `--fw-medium` in ink (not amber, not bold);
- `.fix` in ink-2 at `--fs-sm`;
- `--r-lg`, padding 12px 14px.
- `notice()` puts `icon("alert")` first for warn and bad, and
  `icon("info")` for info, if it doesn't already. The builder checks how
  `notice()` renders today before adding one.

**The rest:**

| Element | Look |
|---|---|
| Cards | panel fill, `--hair` border, no shadow, `--r-lg`, padding 16px on a phone and 20px on a desktop. `.card-head` h2 is `--fs-lg` at 600, its icon 16 px and muted. `.card.accent` (the M47 "do this next" card) has an accent border with no glow ring. |
| Tables | th at `--fw-medium`, `--fs-sm`, muted, no uppercase, with a `--hair` bottom border. td padding 10px 12px. Rows get a panel-2 hover on a fine pointer. |
| Dialogs | `--r-lg`, `--shadow-3`, a `--line` border and a `--scrim` backdrop. Open fades and scales from .98 over `--dur`. |
| Toasts | panel fill, `--line-strong` border, `--shadow-2`, `--r-md`, a 16 px check (ok) or alert (bad) icon. The 4 px copper start border goes. |
| The glossary popover `.tip-def` | the menu look: panel fill, line-strong border, shadow-2, `--r-md`, padding 8px 10px. |
| `<details>` | every summary gets the same custom chevron: `summary{list-style:none} summary::-webkit-details-marker{display:none}`. Opening rotates it 90° (−90° in RTL). |
| Nav | rail current item: panel-3 fill, ink text and icon, weight 500 (no accent). Hover panel-2. Rail icons muted. Bottom tabs current: accent-strong text and icon. Sheet and palette current items: panel-3 fill, ink text. |

**Crowded rows → the overflow menu** (decision D5).

### D5. Fewer buttons per row: `menu()`

A new helper in `app.js`, next to `askBtn`:

```js
// A "⋯" button that opens a short list of the row's rarer actions. The list
// stays in the DOM (hidden), so a route string is still written out here.
function menu(label, items)
// items: [{ label, path, body, kind, icon, onclick }] — falsy entries skipped
```

**Behaviour:**

- **Markup:**
  `div.menu > button.ghost.icon.sm[aria-haspopup="menu"][aria-expanded=false][aria-label=label][title=label]`
  holding `icon("more")`, then `div.menu-list[role=menu][hidden]`.
  Each item is `button[type=button][role=menuitem][tabindex=-1]` with an
  optional icon and a span holding its label. `kind: "danger"` adds the
  class `danger`.
- **Pressing an item** closes the list, then runs `onclick(trigger)` if
  given, else `act(path, typeof body === "function" ? body() : body,
  trigger)`. That `act` call is the same one `btn()` makes. The busy
  target is the menu's trigger button, so the spinner shows on the
  visible "⋯".
  - A destructive route still asks first: `act` gets the server's 409
    and shows `ask()`. Nothing about confirmation moves into the menu.
- **Opening and closing by click:** a `click` on the trigger toggles the
  list. Opening sets `aria-expanded="true"`, removes `hidden`, and
  focuses the first item.
- **The trigger's keys:** its `keydown` handles **only ArrowDown**,
  which opens the list if it's closed and focuses the first item. Enter
  and Space already fire `click` on a native button; handling them in
  keydown as well would toggle the list twice.
- **The list's keys** (one `keydown` on `.menu-list`):
  - ArrowDown and ArrowUp move between items, wrapping at the ends;
  - Home and End jump to the first and last item;
  - Escape closes the list and focuses the trigger (call
    `preventDefault()` so an open dialog doesn't take the Escape);
  - Tab closes the list and lets focus move on normally.
- **Closing from outside:** a `pointerdown` whose target is outside the
  `.menu` wrapper closes the list. It is one `document` listener, added
  on open and removed on close. Opening one menu closes any other: keep
  the open one in a module variable `openMenu`, holding its close
  function.
- **Polls must not close an open menu.** Health polls every 3 s and
  models every 10 s, and a redraw would replace the open list. Neither
  is `live`, so `typing()` gates both. `typing()` gains one line, after
  the `activeElement` check:
  `if (main.querySelector(".menu-list:not([hidden])")) return true;`
  Pressing an item closes the list first, so the `refresh()` after
  `act` still redraws the row.
- **Look:** the list is `position:absolute; inset-inline-end:0;
  top:calc(100% + 4px); min-inline-size:12rem; z-index:var(--z-sheet)`,
  panel fill, line-strong border, `--r-md`, `--shadow-2`, padding 4px.
  Items are full width, start-aligned, `min-height:var(--h-sm)` (30 px on
  a desktop, 44 px on a phone), ghost-styled, `--r-sm`. A danger item
  has bad text.
- **Tests:** none depend on it, because hidden items are excluded by the
  phone check's `vis()` (`e.closest("[hidden]")`). The browser check's
  `click()` finds a button by its exact label even when hidden, but none
  of the labels it clicks moves into a menu.

**Where it goes:**

| Row | Stays visible | Goes into `menu()` |
|---|---|---|
| Home hero (`app.js` ≈819–825) | "Run doctor". When the kill switch is **on**, "Kill switch off" stays visible as primary. | `menu("More actions", [{label:"Kill switch on", path:"kill/on", body:{}, kind:"danger"}])` when it's off |
| Model rows (`app.js` ≈1005–1009) | "Test" (sm) | `menu("More for " + r.reference, [r.default ? null : {label:"Make default", path:"models/default", body:{model:r.reference}}, {label:"Evaluate", onclick:(t) => this.startEval(r.reference, undefined, t)}, {label:"Remove", path:"models/remove", body:{model:r.reference}, kind:"danger"}])` |
| Catalog rows, `addButtons(row)` (`app.js` ≈1032–1041), the not-yet-connected branch only | "Add" (sm) | "Add as default" (catalog/add with `body("default")`), "Add as fallback" (`body("fallback")`) and "Evaluate" |
| Task Advanced fold (≈1957) | already folded | nothing |
| Settings "Getting started" (≈2839–2842) | restyled only | nothing |

**Details for these rows:**

- **"Kill switch on" today** is a plain `btn(…, "kill/on", {}, "danger")`.
  Any confirmation comes from the server's 409, so its menu item is a
  plain `path` item and behaves the same.
- **"Evaluate".** `evalButton(model, provider)` (≈932) builds a button
  whose handler posts `eval/start` and then reloads the eval box. Its
  handler moves into a method on `sections.models`, and every caller
  uses that method:
  ```js
  startEval(model, provider, busy) {
    return act("eval/start", { model, provider, suite: this.suite.value }, busy).then(() => this.loadEval());
  },
  evalButton(model, provider) {
    const b = el("button", { text: "Evaluate" });
    b.onclick = () => this.startEval(model, provider, b);
    return b;
  },
  ```
- **Catalog rows.** In `addButtons`, the "price reference" branch (no
  provider) and the `connected` branch stay as they are, with their
  visible `ev` button. Only the third branch changes:
  ```js
  const body = (as) => ({ provider: row.provider, id: row.id, as });
  return el("div", { class: "row" },
    btn("Add", "catalog/add", body("model"), "sm"),
    menu("More for " + row.id, [
      { label: "Add as default", path: "catalog/add", body: body("default") },
      { label: "Add as fallback", path: "catalog/add", body: body("fallback") },
      { label: "Evaluate", onclick: (t) => this.startEval(row.id, row.provider || undefined, t) },
    ]));
  ```
  Build `ev` only in the two branches that use it.
- **Labels change only inside the menus.** "Default" becomes "Make
  default" (model rows) and "Add as default" (catalog). "Fallback"
  becomes "Add as fallback". Remove's confirmation is the server's 409,
  so it is unchanged.
- **Route strings stay literal**: `"models/default"`, `"models/remove"`,
  `"catalog/add"`, `"kill/on"`, `"eval/start"`.
  `every_route_is_used_by_the_page` needs them in the JS.

### D6. Icons: Lucide 1.49.0, one set

- **What changes.** Every icon is redrawn from Lucide 1.49.0 (ISC; its
  Feather-derived icons are MIT). The 49 current keys keep their names,
  so no caller changes. Three are added: `up`, `down` and `chevron-left`.
- **Format.** Each value is one path string, as today, because
  `icon_names()` and `icon()` expect that. A spike converted the Lucide
  elements into single paths:
  - `circle` becomes two arcs, and `rect` with `rx` becomes a path with
    arcs;
  - `line` and `polyline` become M/L commands;
  - a leading relative `m` becomes an absolute `M`, and its implicit
    pairs a relative `l`.
  - A contact sheet of all 52 was rendered and checked: every one draws
    correctly.
- **Where they are.** The block is in Appendix A, ready to paste over
  `const ICONS = { … };`.
- **The licence goes in a comment above `const ICONS`**, verbatim:

  ```js
  // Icons: Lucide 1.49.0 (https://lucide.dev), ISC licence,
  // Copyright (c) 2026 Lucide Icons and Contributors. The icons derived
  // from Feather are MIT, Copyright (c) 2013-present Cole Bemis.
  // Each is cut down to one path for icon() below.
  ```

  `docs/m49-dashboard-polish.md` (this file) records the same. No extra
  licence file is needed: ISC and MIT only ask for the notice to travel
  with the copies, and this comment is inside the file that holds them.
- **Stroke.** `.ico{stroke-width:1.75}` and size 16 px by default. Card
  heads, empty states and the rail use 16, 20 and 18 px. `icon()` stops
  setting `stroke-width:3` for `more`, because Lucide's `ellipsis` dots
  are drawn as circles.
- **Mirroring in RTL** goes from a single name to a set:
  ```js
  const FLIP = new Set(["chevron", "chevron-left", "logs", "tasks", "chat", "channels"]);
  ```
  `icon()` adds the class `flip` when `FLIP.has(name)`. These are the
  icons whose direction means "forward" or reading order.
  - **Flipped:** chevrons, list-shaped icons whose bullets sit on the
    start side, and speech bubbles whose tail points to the start.
  - **Not flipped:** play (media is always LTR), search, external, check,
    x, up/down, the clocks, the send plane, help, download and the
    rest.

### D7. Layout, density, states and motion

- **Spacing.** The 4 px scale stays. Section gap 24px (desktop 32px),
  card gap 12px (desktop 16px), `.stack` gap 12px, `.row` gap 8px.
- **Page header.** `secHead()` wraps the h1 and `.sub` in
  `div.titles`, stacked, with `.sub` below the title in muted `--fs-sm`.
  Controls sit at the end, centred on the title block. This fixes the
  baseline mismatch (audit 14). It keeps `.sec-head` and `.spacer`.
- **Empty states** (`empty()`):
  - centred, with a 1 px dashed line-strong border, `--r-lg`, padding
    32px 16px;
  - the icon is 20 px inside a 40 px panel-2 circle;
  - `.what` is ink at 600 and `--fs-md`, and the hint (`.empty .hint`) is muted
    `--fs-sm`;
  - the actions sit centred below.
- **Skeletons:** a panel-2 → panel-3 shimmer at 1.4 s, solid panel-2
  under reduced motion (as today).
- **Motion:** `--dur-fast:.12s; --dur:.18s`. Menus, dialogs and toasts
  fade (opacity plus 2–4 px translate). `prefers-reduced-motion` sets both
  durations to 0 and turns off the animations (pulse, sweep, shimmer and
  spinner, as today).
- **The attention strip** (`.strip`, audit 6) is one flex row:
  `align-items:center`, gap 8px, min-height `--tap`, padding 8px 12px,
  `--r-md`, panel fill, warn-tinted border like `.alert.warn`. The text
  span has `min-width:0` and may wrap to two lines; "Home" and the
  chevron are `flex:none`.
- **The header:**
  - **`#theme`** becomes `ghost icon`, showing a monitor (auto), sun
    (paper) or moon (forge) icon. Its `aria-label` and `title` are
    `tr("Theme:") + " " + (follows the system | light | dark)`.
  - **`#logout`** becomes `ghost icon` with `icon("power")` and
    `aria-label`/`title` "Log out" (translated in `translateShell`).
  - **`#uptime`, `.ver` and `.inst`** use sans tabular figures.
- **The provider card:**
  - the key placeholder becomes `"paste a new key"` when connected
    (otherwise `"paste the key"`, unchanged);
  - `.row > input{flex:1 1 12rem;min-width:0}`;
  - "List its models" becomes `ghost sm`.
- **Backup items** get the class `file` (`.item.file`). Below 640 px the
  item wraps: the name block takes the full width, the buttons go under
  it, and the name has `overflow-wrap:anywhere`, not `break-all`
  (audit 11).
- **Spend rings:** stroke 9 → 6, `.pct` 18 px at 600, `.lim` muted 11 px.
  The track stays panel-3.
- **Fallback list:** `↑ ↓ ×` become icon buttons (D4 `.ghost.icon.sm`).
- **Pager:** `icon("chevron-left")` + "Newer", and "Older" +
  `icon("chevron")`. Both flip in RTL.
- **Notice close** (`.x`): `icon("x")` in place of the "×" text.
- **`::selection`:** `color-mix(in srgb,var(--accent) 22%,transparent)`,
  ink text.

### D8. Screenshots and their budget

- **Format.** The browser check gains `--shots-format webp|jpeg` (default
  `jpeg`, so `scripts/m47_shots.sh` is unchanged) and
  `--shots-quality N` (default 70).
- **Scale.** The rig's `size()` uses a device scale factor of 2 below
  600 px and 1 above. With webp, a 390 shot is taken with
  `clip:{x:0,y:0,width:w,height:h,scale:0.75}`, giving 585 × 1266 (1.5×).
  A 1280 shot keeps `scale:1`, giving 1280 × 800, as M47's are. Shrinking
  it would blur the text.
- **Names:** `${name}-${w}-${light|dark}.webp`.
- **Hebrew shots.** The shots-all pass adds, in light only, `health` and
  `settings` at 390 and 1280: `${name}-${w}-light-he.webp`, 4 files.
- **A new wrapper** `scripts/m49_shots.sh <bin> <out> [extra args]` runs
  `--shots-all <out> --shots-format webp --shots-quality 45`.
- **Budget:** two sets of 64 (60 section shots + 4 Hebrew ones), under
  3,000,000 B together (the brief's "about 3 MB").
  - **The spike:** the M47 after set is 60 JPEGs, 4,108,947 B.
    Re-encoded to WebP at these scales with Pillow, it came to 1,410,198 B
    (q40), 1,492,562 B (q45) and 1,556,340 B (q50).
  - **The estimate.** Those inputs already carry JPEG noise, so a clean
    capture should be smaller. At q45, a set of 64 should be ≈1.4–1.6 MB,
    and both sets together ≈2.8–3.2 MB.
  - **Hard cap:** `du -cb docs/assets/m49 | tail -1` ≤ 3,000,000. If it is
    over, re-encode both sets with Plan step 8.4 (Pillow, method 6, q40,
    then q35). Never re-shoot the before set after the CSS has changed.

### D9. What does not change

- Section names, routes, the API and server behaviour.
- The keyboard model: `/`, Ctrl+K, the palette and the shortcuts list.
- The theme storage keys and values: `ferrule-theme` holds
  `auto|paper|forge`.
- The language mechanism and every user-facing English string, except
  these four renames: "Make default", "Add as default", "Add as
  fallback" and "paste a new key". The first three aren't run through
  `tr()`, like their neighbours, and "paste a new key" isn't
  translated either.
- `docs/assets/m47/` and `docs/branding/`, and `README.md`.

## Plan

**Before anything, in every shell**, set the environment once (call this
block **ENV**):

```sh
export RUSTUP_HOME=/workspace/agent/.rustup CARGO_HOME=/workspace/agent/.cargo-home \
  CARGO_HTTP_CAINFO=/tmp/onecli-combined-ca.pem PATH=/workspace/agent/.cargo-home/bin:$PATH \
  NO_PROXY="localhost,127.0.0.1,::1" CARGO_TARGET_DIR=/workspace/agent/.cargo-target CARGO_INCREMENTAL=0
cd /workspace/agent/agentrust-m49
```

**CHECKS**, after every part, before its commit:

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings \
  && cargo test --workspace --no-fail-fast 2>&1 | tee /tmp/m49-test.log \
  | grep -E '^test result:' | awk '{p+=$4; f+=$6; i+=$8} END {print p" passed, "f" failed, "i" ignored"}'
grep -E 'FAILED|panicked' /tmp/m49-test.log | head
```

**BROWSER**, the browser check against the built binary:

```sh
cargo build -p ferrule-cli && node scripts/dashboard_browser_check.mjs \
  --bin "$CARGO_TARGET_DIR/debug/ferrule" 2>&1 | tee /tmp/m49-browser.log | tail -40
```

It must end `N passed, 0 failed`. N is the count recorded in step 1, plus
one for every check M49 adds.

**Commits** are authored as maxim, with no Co-Authored-By and no
"Generated with" lines. The message format is `M49 part N — <what>`.

### 1. Baseline, the shot options, and the before set

1. Record the baseline:
   - run ENV, then CHECKS, and note the passed/failed/ignored line in the
     doc's "Verification" section (step 9.1) as **before**;
   - run BROWSER and note its `N passed`.
2. Extend `scripts/dashboard_browser_check.mjs`:
   - **Options.** Next to `SHOTS_ALL` (line ≈44), add:
     ```js
     const SHOTS_FORMAT = opt("--shots-format") || "jpeg";
     const SHOTS_QUALITY = Number(opt("--shots-quality") || 70);
     ```
     Reject any format other than `jpeg` or `webp` with
     `throw new Error("--shots-format is jpeg or webp")`.
   - **The usage comment** at lines 13–18 gains
     `[--shots-format webp] [--shots-quality 45]`.
   - **A `shoot(w, h)` helper** for the SHOTS_ALL block (≈557–576):
     ```js
     async function shoot(w, h) {
       const o = { format: SHOTS_FORMAT, quality: SHOTS_QUALITY };
       // A phone is drawn at 2x; 1.5x keeps it sharp at two thirds the bytes.
       if (SHOTS_FORMAT === "webp") o.clip = { x: 0, y: 0, width: w, height: h, scale: w < 600 ? 0.75 : 1 };
       return Buffer.from((await cdp("Page.captureScreenshot", o)).data, "base64");
     }
     ```
     The block uses it, with the extension `SHOTS_FORMAT === "webp" ?
     "webp" : "jpg"`.
   - **The Hebrew pass.** After the theme loop, still inside
     `if (SHOTS_ALL)`, add:
     1. set `paper`, then
        `localStorage.setItem("ferrule-lang","he"); location.reload()`;
     2. `until` `dir === "rtl"` and the rail is shown (the same
        condition as the "Hebrew reads right to left" check);
     3. for `[[390, 844], [1280, 800]]` × `["health", "settings"]`,
        `show`, sleep 1100, then shoot to `${name}-${w}-light-he.${ext}`;
     4. restore with `removeItem` and reload, waiting for
        `dir === "ltr"` (copy that check's restore code).
3. Add `scripts/m49_shots.sh`, copying `m47_shots.sh` with its header
   comment rewritten for M49 (WebP; phones at 1.5×, desktops at 1×; q45;
   plus the Hebrew shots):
   ```sh
   exec node "$here/dashboard_browser_check.mjs" --bin "$bin" --shots-all "$out" --shots-format webp --shots-quality 45 "$@"
   ```
   Then run `chmod +x scripts/m49_shots.sh`.
4. Take the before set with the **unchanged** CSS:
   ```sh
   cargo build -p ferrule-cli
   scripts/m49_shots.sh "$CARGO_TARGET_DIR/debug/ferrule" docs/assets/m49/before --measure 2>&1 | tee /tmp/m49-before.log | tail -5
   ls docs/assets/m49/before | wc -l      # 64 (60 + 4 Hebrew)
   du -cb docs/assets/m49/before | tail -1
   grep MEASURE /tmp/m49-before.log       # keep wire_bytes and fcp_ms for the doc
   ```
5. Look at at least these shots with the Read tool and confirm or
   re-rank the audit list in this doc, adding anything missed:
   `health`, `models`, `connections`, `settings`, `tasks` and `usage`, at
   390 and 1280, in light and dark.
6. Run CHECKS (the script changes don't touch Rust). Commit
   `docs/assets/m49/before/`, both scripts and any audit edits as
   **`M49 part 1 — before screenshots, webp shot options, audit
   confirmed`**.

### 2. Fonts

1. **Make the subsets in a throwaway venv** (in `/tmp`, deleted after):
   ```sh
   . /workspace/agent/ca-env.sh
   python3 -m venv /tmp/m49-fontvenv && /tmp/m49-fontvenv/bin/pip install -q --cert /tmp/onecli-combined-ca.pem fonttools brotli
   mkdir -p /tmp/m49-fonts && cd /tmp/m49-fonts
   SHA=9710da1eacb3be272583c3224dcb70f9da6eadbb; G=https://raw.githubusercontent.com/google/fonts/$SHA/ofl
   curl -fsSLo inter.src.ttf "$G/inter/Inter%5Bopsz,wght%5D.ttf"
   curl -fsSLo heebo.src.ttf "$G/heebo/Heebo%5Bwght%5D.ttf"
   curl -fsSLo gm.src.ttf    "$G/geistmono/GeistMono%5Bwght%5D.ttf"
   for f in inter heebo geistmono; do curl -fsSLo $f-OFL.txt "$G/$f/OFL.txt"; done
   sha256sum *.src.ttf   # must match the three sha256 in D1; stop if not
   B=/tmp/m49-fontvenv/bin
   LATIN="U+0000,U+000D,U+0020-007E,U+00A0-00FF,U+0131,U+0152-0153,U+02C6,U+02DA,U+02DC,U+2013-2014,U+2018-201A,U+201C-201E,U+2020-2022,U+2026,U+2030,U+2039-203A,U+2044,U+20AC,U+2122,U+2190-2193,U+2212,U+2318,U+2713,U+2717,U+FB01-FB02"
   HEB="U+0590-05FF,U+FB1D-FB4F,U+20AA"
   $B/fonttools varLib.instancer inter.src.ttf opsz=14 wght=400:600 -q -o inter.ttf
   $B/pyftsubset inter.ttf --unicodes="$LATIN" --layout-features="kern,ccmp,locl,mark,mkmk,tnum,case" --flavor=woff2 --output-file=inter-latin.woff2
   $B/fonttools varLib.instancer heebo.src.ttf wght=400:600 -q -o heebo.ttf
   $B/pyftsubset heebo.ttf --unicodes="$HEB" --layout-features="kern,ccmp,locl,mark,mkmk" --flavor=woff2 --output-file=heebo-hebrew.woff2
   $B/fonttools varLib.instancer gm.src.ttf wght=400:600 -q -o gm.ttf
   $B/pyftsubset gm.ttf --unicodes="$LATIN,U+2500-257F" --layout-features="kern,ccmp,locl,mark,mkmk" --flavor=woff2 --output-file=geist-mono-latin.woff2
   ls -l *.woff2         # ≈23,652 / 7,280 / 16,384
   sha256sum *.woff2 | cut -c1-8   # d1481e3a, 9d633290, 9dea23b6 expected
   ```
   If the sizes differ by more than 1 % (another fonttools version), go
   on, and record the real sizes in the doc instead of the spike's.
2. **Swap the files** in `crates/ferrule-cli/src/dashboard/assets/fonts/`:
   - `git rm` the five `plex-*.woff2`, then copy in the three new ones.
   - Rebuild `OFL.txt`:
     ```sh
     A=crates/ferrule-cli/src/dashboard/assets/fonts
     { head -1 /tmp/m49-fonts/inter-OFL.txt; head -1 /tmp/m49-fonts/heebo-OFL.txt; head -1 /tmp/m49-fonts/geistmono-OFL.txt; echo
       sed -n '/^This Font Software is licensed/,$p' /tmp/m49-fonts/inter-OFL.txt; } > $A/OFL.txt
     grep -c '^Copyright' $A/OFL.txt   # 3
     ```
     If an upstream first line isn't its copyright line, write the three
     lines from D1 by hand instead.
3. **`mod.rs` (lines ≈52–76):**
   - Replace the doc comment and the `FONTS` table:
     ```rust
     /// Inter, Heebo and Geist Mono (OFL 1.1), served from the binary so the
     /// page never depends on a font CDN. Each is one variable file (weights
     /// 400–600) cut to the characters in its `unicode-range`: Inter and Geist
     /// Mono to Latin, Heebo to Hebrew, which only a page showing Hebrew
     /// fetches. They are cached for a year as immutable, so a changed font
     /// must get a new file name.
     const FONTS: &[(&str, &[u8])] = &[
         ("inter-latin.woff2", include_bytes!("assets/fonts/inter-latin.woff2")),
         ("heebo-hebrew.woff2", include_bytes!("assets/fonts/heebo-hebrew.woff2")),
         ("geist-mono-latin.woff2", include_bytes!("assets/fonts/geist-mono-latin.woff2")),
     ];
     ```
     (`cargo fmt` lays it out.)
   - In `index_for` (≈216), `("href", "/fonts/plex-sans-400.woff2")`
     becomes `("href", "/fonts/inter-latin.woff2")`.
4. **`index.html` line 10:**
   `<link rel="preload" href="/fonts/inter-latin.woff2" as="font" type="font/woff2" crossorigin>`.
5. **`app.css`:**
   - **Font faces.** Replace the five `@font-face` rules with three:
     ```css
     @font-face{font-family:"Inter";font-style:normal;font-weight:400 600;font-display:swap;src:url(fonts/inter-latin.woff2) format("woff2");
       unicode-range:U+0000,U+000D,U+0020-007E,U+00A0-00FF,U+0131,U+0152-0153,U+02C6,U+02DA,U+02DC,U+2013-2014,U+2018-201A,U+201C-201E,U+2020-2022,U+2026,U+2030,U+2039-203A,U+2044,U+20AC,U+2122,U+2190-2193,U+2212,U+2318,U+2713,U+2717,U+FB01-FB02}
     @font-face{font-family:"Heebo";font-style:normal;font-weight:400 600;font-display:swap;src:url(fonts/heebo-hebrew.woff2) format("woff2");
       unicode-range:U+0590-05FF,U+FB1D-FB4F,U+20AA;ascent-override:97%;descent-override:24%;line-gap-override:0%}
     @font-face{font-family:"Geist Mono";font-style:normal;font-weight:400 600;font-display:swap;src:url(fonts/geist-mono-latin.woff2) format("woff2");
       unicode-range:U+0000,U+000D,U+0020-007E,U+00A0-00FF,U+0131,U+0152-0153,U+02C6,U+02DA,U+02DC,U+2013-2014,U+2018-201A,U+201C-201E,U+2020-2022,U+2026,U+2030,U+2039-203A,U+2044,U+20AC,U+2122,U+2190-2193,U+2212,U+2318,U+2713,U+2717,U+FB01-FB02,U+2500-257F}
     ```
   - **Font stacks**, in the theme-free `:root`:
     ```css
     --sans:"Inter","Heebo",system-ui,-apple-system,"Segoe UI",Roboto,"Noto Sans Arabic",sans-serif;
     --mono:"Geist Mono",ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;
     ```
   - **The header comment.** Rewrite the "Type:" paragraph:
     ```
     Type: Inter (Latin) and Heebo (Hebrew, behind a unicode-range, so
     only a page showing Hebrew fetches it), Geist Mono for code; one
     variable file each, weights 400-600. OFL 1.1, see /fonts/OFL.txt.
     ```
     The metal-metaphor paragraph is rewritten in step 3.
6. **Tests in `mod.rs`:**
   - In `fonts_are_cached_a_year_and_the_rest_not_at_all` (≈1050),
     `/fonts/plex-sans-400.woff2` becomes `/fonts/inter-latin.woff2`.
   - In `the_index_carries_the_base` (≈1460), the expected string
     becomes `"href=\"/b/b_x/fonts/inter-latin.woff2\""`.
   - **Add, right after the font cache test:**
     ```rust
     #[tokio::test]
     async fn the_new_fonts_and_their_licence_ship() {
         // M49: Inter, Heebo and Geist Mono replace IBM Plex. Each file is a
         // real woff2, cached for a year, and small; the licence names all three.
         let (_d, d) = dash();
         let caps = [
             ("inter-latin.woff2", 26_000),
             ("heebo-hebrew.woff2", 9_000),
             ("geist-mono-latin.woff2", 18_000),
         ];
         assert_eq!(FONTS.len(), caps.len());
         for (name, cap) in caps {
             let r = d.handle(req("GET", &format!("/fonts/{name}"), &[], "")).await;
             assert_eq!((r.status, r.content_type), (200, "font/woff2"), "{name}");
             assert_eq!(&r.body[..4], b"wOF2", "{name}");
             assert_eq!(header(&r, "Cache-Control"), FOREVER, "{name}");
             assert!(r.body.len() < cap, "{name} is {} bytes, over {cap}", r.body.len());
         }
         let r = d.handle(req("GET", "/fonts/OFL.txt", &[], "")).await;
         assert_eq!((r.status, r.content_type), (200, "text/plain; charset=utf-8"));
         assert_eq!(header(&r, "Cache-Control"), FOREVER);
         let licence = String::from_utf8_lossy(&r.body);
         for who in ["The Inter Project Authors", "The Heebo Project Authors", "The Geist Project Authors", "SIL OPEN FONT LICENSE Version 1.1"] {
             assert!(licence.contains(who), "OFL.txt lacks {who}");
         }
         for (what, src) in [("app.css", APP_CSS), ("index.html", INDEX), ("OFL.txt", FONT_LICENSE)] {
             assert!(!src.contains("Plex") && !src.contains("plex-"), "{what} still names Plex");
         }
     }
     ```
   - **Facts it relies on** (checked in the plan phase):
     - `header(&r, name)` (≈1272) returns a `String`, and
       `assert_eq!(String, &str)` compiles. `FOREVER`, `FONTS`,
       `FONT_LICENSE`, `APP_CSS` and `INDEX` are in the parent module;
       the test module already sees them through `use super::*`.
     - `Response.content_type` is a `&'static str`. The OFL route
       (≈678) sets `"text/plain; charset=utf-8"` and FOREVER, and the font
       route (≈681) sets `"font/woff2"` and FOREVER.
     - Fonts are served raw, never gzipped, and the existing test already
       checks `b"wOF2"`.
     - The Plex check is case-sensitive on purpose: a lowercase
       `contains("plex")` would also match words like "complex".
     - The existing test's `"SIL Open Font License"` still matches,
       because the licence body copied from Inter's OFL.txt starts "This
       Font Software is licensed under the SIL Open Font License, Version
       1.1."
7. **Spot-check in the browser:**
   - Run BROWSER; it must pass.
   - Take one shot with `scripts/m49_shots.sh … /tmp/m49-peek`, view
     `health-1280-light.webp` and `settings-390-light-he.webp`, and
     confirm the fonts changed. Then `rm -rf /tmp/m49-peek`.
8. Run CHECKS. Commit **`M49 part 2 — Inter, Heebo and Geist Mono replace
   IBM Plex (47 KB, was 129 KB)`**. Then
   `rm -rf /tmp/m49-fontvenv /tmp/m49-fonts`.

### 3. Tokens and palette

1. **`app.css`, the token blocks:**
   - Replace the first `:root{}` and `[data-theme="forge"]{}` blocks
     verbatim with D2's blocks.
   - In the theme-free `:root{}`:
     ```css
     --sp-1:4px; --sp-2:8px; --sp-3:12px; --sp-4:16px; --sp-5:20px; --sp-6:24px; --sp-8:32px; --sp-10:40px; --sp-12:48px;
     --fs-xs:.75rem; --fs-sm:.8125rem; --fs-md:.9375rem; --fs-input:1rem; --fs-lg:1rem; --fs-xl:1.125rem; --fs-2xl:1.25rem; --fs-3xl:1.75rem;
     --fw-normal:400; --fw-medium:500; --fw-strong:600;
     --lh:1.5; --lh-tight:1.25;
     --r-sm:6px; --r-md:8px; --r-lg:12px; --r-full:999px;
     --h-sm:44px; --h-md:44px; --h-lg:48px;
     --hair:1px solid var(--line);
     --dur-fast:.12s; --dur:.18s; --ease:cubic-bezier(.2,.7,.2,1);
     --focus:2px solid var(--ring);
     ```
     Keep `--sans`, `--mono` (from step 2), the `--z-*` tokens, `--tap`,
     `--top-h`, `--tabs-h` and `--rail-w`.
   - Replace line 102 with:
     ```css
     @media (pointer:fine) and (min-width:900px){:root{--tap:36px; --h-sm:30px; --h-md:36px; --h-lg:40px; --fs-md:.875rem; --fs-input:.875rem; --fs-2xl:1.375rem}}
     ```
2. **Rename copper to accent** across `app.css`:
   - `var(--copper-strong)` → `var(--accent-strong)`,
     `var(--copper-soft)` → `var(--accent-soft)`,
     `var(--on-copper)` → `var(--on-accent)`, then `var(--copper)` →
     `var(--accent)`. Order matters; use
     `sed -i 's/--copper-strong/--accent-strong/g; s/--copper-soft/--accent-soft/g; s/--on-copper/--on-accent/g; s/--copper\b/--accent/g'`.
   - Then `.tag.copper` → `.tag.accent`.
   - `.collar .ring` and `.collar .sweep` take `stroke:var(--brand)`, and
     `.collar .bar` keeps `var(--ink)`.
   - `grep -c copper app.css` must be 0.
3. **`app.js`:** the three `tag(…, "copper")` calls become
   `tag(…, "accent")` (lines ≈992, ≈1154, ≈1264). `grep -n '"copper"'
   app.js` must print nothing.
4. **The `app.css` header comment.** Replace the metal-metaphor paragraph
   (lines 3–6) with:
   ```
   The look (M49, docs/m49-dashboard-polish.md): neutral greys, one indigo
   accent for what you can press or have picked, and quiet status colours
   (ok, warn, bad) that tint text and icons rather than fill blocks. Copper
   is the ferrule's own colour and appears only in the collar mark.
   ```
   Keep the second paragraph and update its test names: `tokens_meet_wcag_aa`
   and `svg_tokens_draw_in_their_theme_colours`.
5. **`page_tests.rs`, in `tokens_meet_wcag_aa`:**
   - Text list: `"copper-strong"` → `"accent-strong"`.
   - Soft loop: `for k in ["ok", "warn", "bad", "accent"]`, with
     `let text = if k == "accent" { "accent-strong" } else { k };`.
   - Replace the `for b in ["copper", "copper-strong"]` block with:
     ```rust
     // Primary buttons print on-accent on the accent, and on its hover.
     for b in ["accent", "accent-hover"] {
         check(format!("on-accent on {b}"), solid("on-accent"), solid(b), 4.5);
     }
     ```
   - Replace the focus-ring check with:
     ```rust
     // Not text, but they must be seen (WCAG 1.4.11): the focus ring on
     // the page and on a card, and a field's border or a switch's track.
     for s in ["bg", "panel"] {
         check(format!("ring on {s} (focus ring)"), solid("ring"), solid(s), 3.0);
     }
     for s in surfaces {
         check(format!("control on {s} (field border)"), solid("control"), solid(s), 3.0);
     }
     ```
6. **`page_tests.rs`, in `the_page_loads_nothing_from_elsewhere`.** The
   CSS lines become:
   ```rust
   // The SVG namespace inside a data-URI image is a name, not an address.
   let css = CSS.replace("http://www.w3.org/2000/svg", "");
   assert!(!css.contains("http://") && !css.contains("https://") && !css.contains("@import"));
   assert!(!css.contains("url(http") && !css.contains("url(//"));
   ```
7. **`page_tests.rs`, add after `every_colour_is_a_token`:**
   ```rust
   #[test]
   fn svg_tokens_draw_in_their_theme_colours() {
       // M49: the select's chevron and the checkbox's tick are data-URI SVGs,
       // which can't read a CSS variable, so each theme spells the colour out.
       // They must match the tokens the contrast test checks.
       for (theme, t) in themes() {
           for (svg, colour) in [("chevron", "muted"), ("checkmark", "on-accent")] {
               let v = &t[svg];
               assert!(v.starts_with("url(\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg'"), "{theme}: --{svg} is {v}");
               let at = v.find("stroke='%23").unwrap_or_else(|| panic!("{theme}: --{svg} has no stroke")) + "stroke='%23".len();
               assert_eq!(format!("#{}", &v[at..at + 6]), t[colour], "{theme}: --{svg} isn't drawn in --{colour}");
           }
       }
   }
   ```
8. **Find every hard-coded colour:**
   - `grep -nE '#[0-9a-fA-F]{3,8}\b|rgba\(' crates/ferrule-cli/src/dashboard/assets/app.css | sed -n '1,200p'`
     must show hits only inside the font faces and the two token blocks.
     The test enforces this.
   - Grep `app.js` for inline colours (`style:`, `fill:`, `"#`).
     Anything found becomes a class that reads a token.
9. Run CHECKS and BROWSER. Commit **`M49 part 3 — neutral palette, one
   indigo accent, copper only in the mark; contrast test covers field
   borders and the ring`**.

### 4. Controls CSS

Edit `app.css` section by section, in place. Keep the section headers.
Every value comes from D4; this is the order and the selectors.

1. **`/* controls */` (≈307–362):**
   - **Buttons:** rewrite the `button,.btn` base:
     - font `var(--fw-medium) var(--fs-sm)/1 var(--sans)`;
     - `min-height:var(--h-md)`, padding 0 12px, `--r-md`, a `--line-strong`
       border, `box-shadow:var(--shadow-1)`;
     - transitions on background, border-color, box-shadow and color.
   - **Button states and kinds:**
     - `button .ico,.btn .ico{width:16px;height:16px}`;
     - `button:hover:not(:disabled)`;
     - disabled with `box-shadow:none;cursor:not-allowed`;
     - `.primary` and its hover use `--accent`/`--accent-hover`;
     - `.ghost` with `box-shadow:none`;
     - `.danger`: border `--line-strong`, bad text, hover bad-soft;
     - `.icon{padding:0;inline-size:var(--h-md)}`;
     - `.sm{min-height:var(--h-sm);padding:0 10px}`;
     - `button.icon.sm{inline-size:var(--h-sm)}`, which replaces ≈470;
     - `.link`.
   - **The busy spinner:** its top colour becomes `--accent`.
   - **Fields:**
     - the `input,select,textarea` base uses `--fs-input`, line height
       1.3, a `--control` border, `--r-md`, padding 0 10px, `min-height:var(--h-md)`
       and a border-color transition;
     - textarea padding 8px 10px;
     - `::placeholder{color:var(--muted)}`;
     - `:hover:not(:disabled):not(:focus)` border `--ink-2`;
     - `:focus-visible` with outline `var(--focus)`, offset 0, border
       `--accent`;
     - `[aria-invalid=true]` border bad;
     - `:disabled,[readonly]` with a panel-2 fill and muted text.
   - **The select, checkbox and radio rules** go in verbatim from D4.
     They replace the `accent-color` line.
2. **`.chips` and `.chip` (≈535–537)** become the segmented control from
   D4. **`.switch` (≈538–542)** follows D4: a 36 × 20 track through
   `::before`, a 16 px `--knob`, on uses `--accent`, and the knob's
   on-position is `inset-inline-start` 18px from the track start. Check
   it in RTL.
3. **`.tag` (≈296–300)** follows D4.
4. **`.alert` (≈363–378)** follows D4. `.alert .x` becomes a
   `ghost icon sm` look: transparent, muted, hover panel-2, 30 px on a
   desktop and `--tap` on a phone.
5. **The rest:**
   - `.card` (≈240) and `.card-head` (≈248);
   - `.card.accent` (≈245): `border-color:var(--accent);box-shadow:none`;
   - `table`, `th` and `td` (≈270–271);
   - `dialog` (≈558–572), the palette (≈573–591) and toasts (≈592–606);
   - `.tip-def` (≈398);
   - all `summary` elements: list-style none, the WebKit marker hidden,
     a CSS chevron. Use `summary::before` with `content:""`,
     `background:var(--chevron)` rotated −90° (90° in RTL) and 0° when
     `[open]`. The page has `.opt summary` and `details.advanced >
     summary` rules today, which already hide the marker: unify on one
     rule.
6. **The nav (≈155–223):**
   - rail `a.on`: panel-3 fill and ink, weight 500, the svg in ink;
   - rail `a:hover`: panel-2;
   - rail svg default: muted;
   - `#tabs a.on`: accent-strong;
   - `#sheet a.on` and `.pal-list li.on`: panel-3 fill and ink.
7. **The menu CSS** (used in step 5), in a new section
   `/* ---------- the overflow menu (M49) ---------- */` before
   `/* toasts */`:
   ```css
   .menu{position:relative;display:inline-flex}
   .menu-list{position:absolute;inset-inline-end:0;top:calc(100% + 4px);z-index:var(--z-sheet);min-inline-size:12rem;padding:4px;display:flex;flex-direction:column;gap:1px;background:var(--panel);border:1px solid var(--line-strong);border-radius:var(--r-md);box-shadow:var(--shadow-2);animation:pop var(--dur) var(--ease)}
   .menu-list button{justify-content:flex-start;inline-size:100%;min-height:var(--h-sm);border:0;box-shadow:none;background:transparent;border-radius:var(--r-sm);padding:0 10px;font-weight:var(--fw-normal);color:var(--ink)}
   .menu-list button:hover,.menu-list button:focus-visible{background:var(--panel-2);outline:none}
   .menu-list button.danger{color:var(--bad)}
   @keyframes pop{from{opacity:0;transform:translateY(-2px)}}
   ```
   On a phone, `.menu-list button` is 44 px through `--h-sm`. In the
   coarse block, add `.menu-list button{min-inline-size:0}`.
8. **The coarse-pointer block (≈625–630):**
   - keep `button{min-inline-size:var(--tap)}` and
     `button.tip,button.link{min-inline-size:0}`;
   - delete the `button.sm,.alert .x,.complete button{min-block-size…}`
     and `.alert .x{inline-size…}` lines, since `--h-sm` is 44 there;
   - check `.complete button`: if its height is set in px elsewhere,
     switch it to `--h-sm`.
   - Add the 20 px checkbox/radio rule here.
9. **Check:**
   - Run BROWSER. The phone check must still find nothing under 44 px.
   - Take a peek set (`scripts/m49_shots.sh … /tmp/m49-peek`) and view
     `models-1280-light`, `models-390-dark`, `settings-1280-dark`,
     `config-1280-light` and `usage-390-light`. Every select must show
     the chevron and be as tall as its neighbours, and the dark options
     must be dark.
   - `rm -rf /tmp/m49-peek`.
10. Run CHECKS. Commit **`M49 part 4 — one control system: buttons,
    fields, custom select and checkbox, segmented chips, switches,
    tags, alerts, cards, dialogs, toasts`**.

### 5. Icons, the menu helper, and the crowded rows

1. **`app.js` ICONS (≈365–420):** paste Appendix A over the whole
   `const ICONS = { … };` block, with the licence comment from D6 directly
   above `const ICONS`. Keep the in-block section comments
   (`// the sections`, …) from Appendix A.
2. **`icon()` (≈422):**
   ```js
   const FLIP = new Set(["chevron", "chevron-left", "logs", "tasks", "chat", "channels"]);
   function icon(name, label) {
     const s = el("svg", { class: "ico" + (FLIP.has(name) ? " flip" : ""), viewBox: "0 0 24 24", "aria-hidden": label ? null : "true", role: label ? "img" : null, "aria-label": label || null });
     s.append(el("path", { d: ICONS[name] || ICONS.more }));
     return s;
   }
   ```
   Only two things change from today's `icon()`: `name === "chevron"`
   becomes `FLIP.has(name)`, and the `"stroke-width"` attribute (3 for
   `more`) goes, because Lucide's `more` (its `ellipsis`) draws three small
   circles that read as dots at the normal stroke. In CSS, `.ico{stroke-width:1.75;inline-size:16px;block-size:16px}`;
   the per-place sizes stay as listed in D6.
3. **`menu(label, items)`** goes after `askBtn` (≈222), following D5's
   behaviour exactly. It needs no new route, so
   `every_route_is_used_by_the_page` is unaffected. The route strings
   live in the callers. In the same step, add D5's line to `typing()`
   (≈2968), after the `activeElement` check:
   `if (main.querySelector(".menu-list:not([hidden])")) return true;`
4. **Use it in the rows** (the table in D5):
   - **The hero (≈819–825):** keep `btn("Run doctor", …)`. If
     `kill.on`, keep `btn("Kill switch off", "kill/off", {}, "primary")`.
     Otherwise use `menu("More actions", [{ label: "Kill switch on",
     path: "kill/on", body: {}, kind: "danger", icon: "power" }])`.
     Today's button posts directly, and any confirmation is the server's
     409, so the menu item behaves the same.
   - **`startEval` first:** add the method and the new `evalButton` from
     D5's details.
   - **Model rows (≈1005–1009):** the row becomes
     `el("div", { class: "row" }, btn("Test", "models/test", { model: r.reference }, "sm"), menu(…))`,
     with the menu items exactly as in D5's table.
   - **Catalog `addButtons` (≈1032–1041):** exactly the code in D5's
     details.
   - **Grep for callers:** `grep -n 'evalButton\|startEval' app.js`
     must show only `evalButton` in `addButtons`' two kept branches, and
     `startEval` in the two menus and in `evalButton`.
5. **Glyph buttons to icons:**
   - **Fallback list (≈1118–1120):**
     ```js
     el("button", { type: "button", class: "ghost icon sm", title: "Earlier", "aria-label": "Earlier", disabled: i === 0, onclick: () => move(-1) }, icon("up")),
     el("button", { type: "button", class: "ghost icon sm", title: "Later", "aria-label": "Later", disabled: i === this.fb.length - 1, onclick: () => move(1) }, icon("down")),
     el("button", { type: "button", class: "ghost icon sm", title: "Take out", "aria-label": "Take out", onclick: () => { this.fb.splice(i, 1); edited(); } }, icon("x")));
     ```
   - **The pager (≈2001/2003):**
     `el("button", { disabled: …, onclick: … }, icon("chevron-left"), el("span", { text: "Newer" }))`
     and `el("button", { … }, el("span", { text: "Older" }), icon("chevron"))`.
   - **Notice close in `notice()` (≈505–519):** `text: "×"` becomes a child
     `icon("x")`, with class `x ghost icon sm`. Keep `title` and
     `aria-label` "Hide for a day". The browser check clicks
     `[data-notice=…] .x`.
6. **The header:**
   - **`index.html` line 26:**
     `<button id="theme" class="ghost icon" type="button" aria-label="Theme: follows the system" title="Theme: follows the system"></button>`
   - **`index.html` line 27:**
     `<button id="logout" class="ghost icon" type="button" aria-label="Log out" title="Log out" hidden></button>`
   - **`themeButtonLabel` (≈3037–3044):**
     ```js
     b.replaceChildren(icon(p === "auto" ? "monitor" : p === "paper" ? "sun" : "moon"));
     const t = tr("Theme:") + " " + (p === "auto" ? tr("follows the system") : p === "paper" ? tr("light") : tr("dark"));
     b.title = t; b.setAttribute("aria-label", t);
     ```
   - **`bootChrome` (≈3166):** next to `find.append(icon("search"))`,
     add `document.getElementById("logout").append(icon("power"));`.
   - **`translateShell` (≈3172–3181):** the `#logout` setter becomes
     `n => { n.title = tr("Log out"); n.setAttribute("aria-label", tr("Log out")); }`.
   - **`lang-he.js`:** delete the keys `"auto"`, `"paper"` and
     `"forge"` (lines ≈147–149). Keep `"Theme:"`, `"follows the
     system"`, `"light"`, `"dark"` and `"Log out"`. Run
     `grep -n 'tr("auto")\|tr("paper")\|tr("forge")' app.js` first; it
     must print nothing once `themeButtonLabel` is changed.
     `every_shell_string_has_a_hebrew_version` fails on stale keys.
   - **CSS (≈146):** `.ver`, `.inst` and `.live` take
     `font:var(--fs-sm) var(--sans)`. Keep the
     `@media (max-width:420px){#uptime,.ver{display:none}}` rule.
7. **The provider card (≈1152–1200):** the placeholder is
   `p.connected ? "paste a new key" : "paste the key"`, "List its
   models" gets `class: "ghost sm"`, and in CSS add
   `.row > input{flex:1 1 12rem;min-width:0}`. Check that this doesn't
   stretch inputs in other rows badly. If it does, scope it with a
   `.key-row` class on that row.
8. **The backup item (≈2822–2828):** `class: "item file"`, plus CSS:
   `@media (max-width:639px){.item.file{flex-wrap:wrap}.item.file>.grow{flex:1 0 100%}}`,
   `.item.file .t{overflow-wrap:anywhere}`.
9. **Check:**
   - `every_icon_used_exists` passes (52 icons > 40);
     `every_route_is_used_by_the_page` passes;
     `every_shell_string_has_a_hebrew_version` passes.
   - Run BROWSER.
   - By hand in a peek set: in `models-1280-light` each row shows "Test"
     plus "⋯". `health-390-light` has one visible action in the hero.
     The header shows three icon buttons.
10. **Test the menu's keyboard behaviour by hand**, with agent-browser
    against a running gateway (the browser check's rig, or
    `ferrule dashboard` on a temp config). Open with Enter, move with
    the arrows, Escape returns focus to the trigger, and an outside click
    closes it. Leave the health hero's menu open for 5 s: it must stay
    open through the 3 s poll (the `typing()` guard). Then add a
    browser-check step (part 7.3).
11. Run CHECKS. Commit **`M49 part 5 — Lucide icons, an overflow menu for
    rare actions, icon buttons for glyphs, header icons`**.

### 6. Layout, states and motion

1. **`secHead()` (≈272):**
   ```js
   return el("div", { class: "sec-head" },
     el("div", { class: "titles" }, el("h1", { text: title }), sub ? el("span", { class: "sub", text: sub }) : null),
     ctrls.length ? frag(el("span", { class: "spacer" }), ...ctrls) : null);
   ```
   CSS:
   ```css
   .sec-head{display:flex;align-items:center;gap:var(--sp-3);flex-wrap:wrap;margin:0 0 var(--sp-5)}
   .sec-head .titles{display:flex;flex-direction:column;gap:2px;min-width:0}
   .sec-head h1{font-size:var(--fs-2xl);line-height:var(--lh-tight);letter-spacing:-.015em;font-weight:var(--fw-strong);margin:0}
   ```
   Add `html[lang="he"] h1,html[lang="he"] h2{letter-spacing:0}`.
   Grep the JS for other `.sec-head` builders that bypass `secHead()`
   (`grep -n '"sec-head"' app.js`) and give them the same structure.
2. **`empty()` (≈232)** draws the icon inside
   `el("span", { class: "empty-ico" }, icon("info"))`, and its hint div
   gets a class: `hint ? el("div", { class: "hint", text: hint }) : null`.
   The CSS follows D7 (`.empty .hint` is the muted second line).
3. **Skeleton and motion:**
   - the skeleton shimmer uses panel-2/panel-3;
   - add the dialog, menu and toast fade-ins;
   - one `@media (prefers-reduced-motion:reduce)` rule, after the
     theme-free `:root`, sets `--dur-fast:0s; --dur:0s`, plus `.menu-list,
     dialog[open], #toast > *{animation:none}`.
4. **Spacing:** the section, card and stack gaps from D7. Card padding
   is 16px, and 20px in the `@media (min-width:900px)` block.
5. **The strip** (`.strip`) follows D7.
6. **Rings** (≈511–519) follow D7.
7. **Check:**
   - Run BROWSER, then take a peek set and view every section once at
     390 light and 1280 dark.
   - Fix anything that looks off. Iterate on CSS only; keep the markup
     as planned.
8. Run CHECKS. Commit **`M49 part 6 — page headers, empty states,
   spacing and motion`**.

### 7. RTL, Hebrew, and the browser-check additions

1. **The line-box check.** In `scripts/dashboard_browser_check.mjs`, add
   `await run("Hebrew doesn't change the line height", …)` right after
   "Hebrew reads right to left". It must not depend on the language
   setting:
   ```js
   const h = await js(`
     const mk = (t) => { const p = document.createElement("p"); p.textContent = t; p.style.cssText = "position:absolute;visibility:hidden;margin:0;white-space:nowrap"; document.body.append(p); return p; };
     const a = mk("Default model"), b = mk("Default model עברית");
     await document.fonts.ready; await document.fonts.load("1em Heebo", "א");
     const r = [a.getBoundingClientRect().height, b.getBoundingClientRect().height, document.fonts.check("1em Heebo", "א")];
     a.remove(); b.remove(); return r;`);
   if (!h[2]) throw new Error("Heebo didn't load");
   if (Math.abs(h[0] - h[1]) > 0.5) throw new Error("a line with Hebrew is " + h[1] + " px, without " + h[0]);
   return "one line is " + h[0] + " px with or without Hebrew";
   ```
   The inline style is set from the check's own `js()` (CDP
   `Runtime.evaluate`), not by the page, so the CSP doesn't apply.
2. **The RTL mirror.** Inside "Hebrew reads right to left", before the
   restore, check that the chevron's transform is mirrored:
   ```js
   const flip = await js(`const i = document.querySelector("#main .ico.flip, #rail .ico.flip"); return i ? getComputedStyle(i).transform : "none-found"`);
   if (!/matrix\(-1/.test(flip)) throw new Error("a .flip icon isn't mirrored in RTL: " + flip);
   ```
   If no `.flip` icon is on the settings page, show `logs` first (its
   rail icon flips).
3. **The overflow menu.** Add `await run("the overflow menu opens and
   closes", …)` right after the check that connects a provider and
   presses its Test (≈380–395), so the models page has a row:
   ```js
   const has = () => js(`return !!document.querySelector("#main .menu > button")`);
   let where = "models";
   await js(`window.ferrule.show("models")`);
   if (!(await until("a model row's menu", has, 8).catch(() => false))) {
     where = "health";            // the hero's menu, while the kill switch is off
     await js(`window.ferrule.show("health")`);
     await until("the hero's menu", has, 8);
   }
   const open = await js(`
     const t = document.querySelector("#main .menu > button"), l = t.parentElement.querySelector(".menu-list");
     t.click();
     return [t.getAttribute("aria-expanded"), l.hidden, document.activeElement === l.querySelector("[role=menuitem]")];`);
   if (open.join() !== "true,false,true") throw new Error("after a click: expanded, hidden, first item focused = " + open);
   const shut = await js(`
     const t = document.querySelector("#main .menu > button"), l = t.parentElement.querySelector(".menu-list");
     l.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
     return [t.getAttribute("aria-expanded"), l.hidden, document.activeElement === t];`);
   if (shut.join() !== "false,true,true") throw new Error("after Escape: expanded, hidden, trigger focused = " + shut);
   return "on " + where + ": a click opens it on its first item, Escape closes it back to the button";
   ```
   - It uses `.click()`, not a synthetic Enter: a synthetic `keydown`
     doesn't make a native button fire `click`, and the menu handles
     Enter only through `click` (D5).
   - `until` throws on a timeout (≈63) rather than returning false,
     hence the `.catch(() => false)` on the first wait.
4. **Hebrew UI by hand:**
   - Read through the Hebrew shots (step 8): `health` and `settings`, 390
     and 1280.
   - Check: the menu opens at the inline end in RTL, the select chevron
     is on the left, the switch knob moves the right way, and the summary
     chevrons point left when closed.
5. Run BROWSER (N + 2 checks now: the line box and the menu; the RTL
   mirror is part of an existing check). Run CHECKS. Commit **`M49 part 7 —
   RTL mirroring, Hebrew line box and overflow menu checks`**.

### 8. After screenshots and the size budget

1. Run `cargo build -p ferrule-cli`, then
   `scripts/m49_shots.sh "$CARGO_TARGET_DIR/debug/ferrule" docs/assets/m49/after --measure 2>&1 | tee /tmp/m49-after.log | tail -5`.
2. View every after shot at least once, plus the matching before shot
   for the audit items. Fix what still looks off, re-shoot, and repeat.
   Remove the stale files before each re-shoot:
   `rm -f docs/assets/m49/after/*`.
3. Size:
   ```sh
   ls docs/assets/m49/after | wc -l           # 64
   du -cb docs/assets/m49 | tail -1           # ≤ 3,000,000
   grep MEASURE /tmp/m49-before.log /tmp/m49-after.log
   ```
   Record the wire bytes and first contentful paint, before and after, in
   the Verification section. The font bytes alone come from `ls -l
   crates/ferrule-cli/src/dashboard/assets/fonts`.
4. **If it's over 3,000,000 B**, re-encode **both** sets at q40 without
   re-shooting, so before and after stay comparable:
   ```sh
   /workspace/agent/.venv-pdf/bin/python - <<'EOF'
   import glob
   from PIL import Image
   for f in glob.glob("docs/assets/m49/*/*.webp"):
       im = Image.open(f); im.load(); im.save(f, "WEBP", quality=40, method=6)
   EOF
   du -cb docs/assets/m49 | tail -1
   ```
   If it's still over, run it once more with `quality=35`. If it's
   still over after that, stop shrinking and record the real total in
   Verification as a decision taken alone. Don't drop shots: the brief
   asks for every section at both widths, in both themes.
   - Re-encoding a WebP that is already lossy loses a little sharpness,
     so view two shots afterwards (`models-390-dark`,
     `settings-1280-light-he`).
   - If the before set was already committed in part 1, the re-encode
     goes into part 8's commit.
5. **Contrast, numerically.** `tokens_meet_wcag_aa` is the numeric check;
   copy its pairs table from D2 into Verification. Also, in the after
   rig, confirm that the computed `color`/`background-color` of a
   primary button, a tag and the muted text match the tokens. Use one
   `js()` call through agent-browser; this is a by-hand check, so note
   the result.
6. Commit **`M49 part 8 — after screenshots (64, WebP)`**.

### 9. Docs

1. **This file:** add a `## Verification` section after the Plan,
   holding:
   - the test counts before and after, and the browser check's N
     before and after;
   - the font sizes as built, and the MEASURE wire bytes and FCP before
     and after;
   - `du -cb docs/assets/m49`;
   - the contrast table;
   - what was checked by hand: the Hebrew shots, the menu keyboard, the
     dark options in a select;
   - what wasn't checked: real iOS Safari, Windows' Segoe fallback for
     Arabic, a real tunnel.

   Set **Status** at the top to "built".
2. **`docs/dashboard.md`:**
   - **The screenshot (186–190):** the image becomes
     `![Settings on a desktop](assets/m49/after/settings-1280-light.webp)`,
     and the paragraph becomes:
     > Sixty-four screenshots (fifteen pages at 390 and 1280 px, light
     > and dark, plus Home and Settings in Hebrew) are in
     > [assets/m49/after](assets/m49/after/), with the pre-M49 ones in
     > [assets/m49/before](assets/m49/before/). The M47 sets are in
     > [assets/m47](assets/m47/). Regenerate with
     > `scripts/m49_shots.sh <ferrule binary> <out dir>`.
   - **The fonts paragraph (244–250)** becomes:
     > **Fonts** are Inter for Latin text, Heebo for Hebrew (fetched
     > only when the page shows Hebrew) and Geist Mono for code: one
     > variable woff2 each, weights 400–600, served from the binary with
     > a one-year cache. They come to about 47 KB in all, and about
     > 40 KB for a page with no Hebrew. Arabic uses the system's font.
     > Their licence is at `/fonts/OFL.txt`. Icons are Lucide, as inline
     > SVG paths in the script. Type sizes are 12, 13, 15 (14 on a
     > desktop), 16, 18, 20–22 and 28 px. Text written by you or the
     > agent sits in `dir="auto"` elements and is set with
     > `textContent` only.
   - **Colours, a new paragraph** right after the fonts paragraph:
     > **Colours** are neutral greys with one indigo accent for what you
     > can press or have picked; green, amber and red mark status. Light
     > and dark are designed separately, and a unit test checks every
     > text pair against WCAG AA in both. Controls are one set: selects,
     > inputs and buttons share a height (36 px with a mouse, 44 px on a
     > phone), and a row's rarer actions sit behind **⋯**.
   - **The Browser check section:** add the `--shots-format`,
     `--shots-quality` and `m49_shots.sh` line after the `--shots-all`
     line.
3. **`PLAN.md`:**
   - **Current State:** add an M49 bullet after the M48 bullet (≈869–881),
     in the same form:
     `**M49 dashboard visual polish** — **built** (2026-10-01, branch
     m49-dashboard-polish, PR #…)`, with 3–5 nested bullets (fonts and
     their size, palette, controls and menu, icons, screenshots).
   - **Session log:** insert, directly above
     `### 2026-10-01 — M48 …`, the entry
     `### 2026-10-01 — M49 dashboard visual polish (Devi, Opus 5.5 plan / Sonnet 5.5 build)`,
     with **Scope.**, **What was built** (one line per part commit),
     **Tests.** (before → after), **Not verified live.** and
     **Follow-ups.**
4. **`docs/roadmap.md`:** add `### M49 — dashboard visual polish` between
   the M48 section's end and `## Other open tracks` (≈1047), in M48's
   format:
   - **Status.** built, PR link;
   - 4–6 bullets;
   - **Done means.** The fonts are replaced and the licence ships; every
     control is in one style; contrast is tested, including the field
     borders; before and after shots are in the tree; every test
     passes.
5. **README:** not touched. Its lines go in the final report: one line
   for the fonts and size, one for "redesigned controls".
6. Commit **`M49 part 9 — docs: design record, dashboard guide, plan and
   roadmap`**.

### 10. Final checks, eval, merge, PR

1. Run CHECKS. Record the final count in Verification and PLAN.md, and
   amend part 9 or add a small commit.
2. Run BROWSER, which must be all green.
3. Run the eval through the real binary against the stdlib mock, as in
   `docs/eval.md` step 1, but with the full suite (no `--tag`):
   ```sh
   mkdir -p /tmp/m49-eval
   python3 evals/starter/mock/model.py --port 8765 & MOCK=$!
   cat > /tmp/m49-eval/eval-mock.toml <<'EOF'
   default_provider = "mock"

   [providers.mock]
   base_url = "http://127.0.0.1:8765/v1"
   api_key_env = "MOCK_KEY"
   model = "mock"
   price_input_per_mtok = 1.0
   price_cached_input_per_mtok = 0.1
   price_output_per_mtok = 5.0
   EOF
   MOCK_KEY=x "$CARGO_TARGET_DIR/debug/ferrule" --config /tmp/m49-eval/eval-mock.toml \
     eval run evals/starter --variant ab 2>&1 | tee /tmp/m49-eval/run.log | tail -15
   kill $MOCK
   ```
   - The result must be engineered 20/20, naive 11/20 and $0.98. Copy
     the summary lines into the report.
   - Kill only `$MOCK`, the PID this shell started. If port 8765 is
     taken, use another port in both places.
4. Fetch and merge:
   `GIT_SSL_CAINFO=/tmp/onecli-combined-ca.pem git fetch origin && git merge origin/main`.
   - **On conflicts** in PLAN.md or the roadmap, keep both sides.
   - **On conflicts in `app.css`, `app.js`, `page_tests.rs` or
     `lang-he.js`**, take main's new behaviour and re-apply M49's styling
     on top. Then grep again for `copper`, `plex`, `tr("auto")` and
     glyph buttons (`"↑"`, `"×"`), because main may have added new ones.
   - After the merge, re-run CHECKS, BROWSER and the eval.
5. Push and open the PR:
   - `GIT_SSL_CAINFO=/tmp/onecli-combined-ca.pem git push -u origin m49-dashboard-polish`.
   - Open it with `curl -s -X POST https://api.github.com/repos/maximarhipkin/ferrule/pulls -d @/tmp/m49-pr.json`,
     where `/tmp/m49-pr.json` has the title "M49: dashboard visual
     polish", `head` m49-dashboard-polish, `base` main and
     `draft:false`. Add no auth header (the proxy injects the token).
   - The body is a summary, the before/after shots of Settings and
     Models at 1280 light (relative repo links), the tests, and "not
     verified live". It carries no trailer and no "Generated with".
   - Don't merge, and don't wait for CI.
6. Write the final report, with the sections the common rules list.
   Delete `/tmp/m49-*`.

## Tests, at a glance

| Test | File | Status | What it proves |
|---|---|---|---|
| `the_new_fonts_and_their_licence_ship` | `dashboard/mod.rs` | new | each of the three woff2 is served as `font/woff2`, is real woff2, is cached for a year, and is under its size cap. OFL.txt names all three projects and the OFL. Nothing names Plex. |
| `fonts_are_cached_a_year_and_the_rest_not_at_all` | `mod.rs` | path changed | the new file name |
| `the_index_carries_the_base` | `mod.rs` | path changed | the preload is prefixed under the M44 base |
| `tokens_meet_wcag_aa` | `page_tests.rs` | changed | the accent tokens in place of copper. on-accent on the accent and on its hover. The ring on bg and panel at 3:1. **New:** `--control` (field border, switch track) on every surface at 3:1. |
| `svg_tokens_draw_in_their_theme_colours` | `page_tests.rs` | new | the chevron is drawn in `--muted` and the tick in `--on-accent`, in each theme. The data URIs keep their namespace and stay parseable. |
| `the_page_loads_nothing_from_elsewhere` | `page_tests.rs` | relaxed | the SVG namespace string is allowed, and nothing else from another origin |
| `every_colour_is_a_token`, `every_icon_used_exists`, `every_route_is_used_by_the_page`, `every_shell_string_has_a_hebrew_version` | `page_tests.rs` | unchanged | still pass with the new CSS, icons, menu and removed theme keys |
| Browser: "Hebrew doesn't change the line height" | `scripts/dashboard_browser_check.mjs` | new | the Heebo overrides keep a mixed line the same height as an English one |
| Browser: the RTL mirror assertion | same | new, inside "Hebrew reads right to left" | `.flip` icons are mirrored |
| Browser: "the overflow menu opens and closes" | same | new | a click opens it with focus on the first item; Escape closes it and focus returns to the trigger |

Every other test is unchanged, and must pass unchanged.

## Risks, and how each is checked

1. **The data-URI chevron and tick.**
   - *What could go wrong:* the tokens parser splits on `;` and stops at
     `}`; the origin test sees `http://`; the colour inside can drift
     from the theme.
   - *How it's checked:* the URIs contain no `;` or `}`; the origin test
     strips the exact SVG namespace; `svg_tokens_draw_in_their_theme_colours`
     ties each colour to its token. The CSP already allows
     `img-src data:`. The step 4.9 peek shows the chevron in both themes.
2. **The copper → accent rename** touches CSS, three JS tag calls and the
   test.
   - *How it's checked:* `grep -c copper app.css` is 0, and
     `grep -n '"copper"' app.js` prints nothing. The contrast test fails
     loudly on any missing token (`t[name]` panics).
3. **Merging origin/main** if another branch edits `app.css` or `app.js`
   meanwhile.
   - *How it's checked:* step 10.4 re-applies and re-greps, then re-runs
     CHECKS, BROWSER and the eval after the merge.
4. **Hebrew/Latin metrics and RTL.**
   - *What could go wrong:* Heebo grows the line box; the switch knob,
     select chevron, menu or summary chevron are on the wrong side.
   - *How it's checked:* the new browser checks (line height and
     mirroring), plus the Hebrew screenshots looked at by hand.
5. **The overflow menu and the 44 px rule.**
   - *What could go wrong:*
     - a button the browser check clicks moves into a menu;
     - a hidden item fails the 44 px rule;
     - a poll redraws the row and closes an open menu;
     - Enter toggles the menu twice.
   - *How it's checked:*
     - The labels the check clicks are "Say hello", "Set up", "Close",
       "Add one", "Save the fallback list", "Check and connect", "Test",
       "Run", "Send", "New task", "Back up now", "Dark" and "System".
       None of them moves into a menu (D5 table).
     - Hidden items are skipped by `vis()`, and visible ones are 44 px
       through `--h-sm`.
     - The `typing()` guard stops polls while a list is open. Step 5.10
       checks it by hand: leave the health hero's menu open for 5 s, and
       it must stay open.
     - Only `click` opens or closes the menu, and the trigger's keydown
       handles only ArrowDown.
6. **The screenshot budget.**
   - Two sets of 64 WebP shots at q45 are estimated at ≈2.8–3.2 MB
     (D8). That sits on the 3,000,000 B cap.
   - *How it's checked:* the `du -cb` cap in step 8.3, with the re-encode
     recipe in 8.4 (both sets, q40, then q35). The before set is taken
     once, in step 1, and never re-shot.
7. **Desktop heights shrinking** could break a layout that assumed 40 or
   44 px.
   - *How it's checked:* the peek sets at 1280 in steps 4.9 and 6.7, and
     the full after set.
8. **The font subset misses a character the UI uses.** Arrows, ⌘, ✓
   and ✗ are in the Latin range; box drawing is in the mono range.
   - *How it's checked:* `grep -oP '[^\x00-\x7F]' app.js lang-he.js | sort -u`
     after step 5 lists the non-ASCII characters. Every non-Hebrew one
     must be in `LATIN` (or be an emoji, which falls back to the system
     font by design).

## Spikes (done in the plan phase, throwaway code deleted)

- **Fonts:** downloaded the three variable sources at the pinned commit,
  instanced and subset them with fonttools, and measured the sizes and
  glyph counts: Inter 320 glyphs, Heebo 97, Geist Mono 365. Also
  measured Rubik, Assistant, Noto Sans Hebrew, JetBrains Mono, Heebo with
  Latin, and Inter with opsz (numbers in D1).
- **Contrast:** a script mirroring `page_tests.rs`'s maths ran over both
  palettes. Two dark values were moved until every pair, the new
  control-border pair included, passed (D2).
- **The select chevron:** a data-URI SVG with no `xmlns` draws nothing;
  with it, it draws in Chromium. `;charset` isn't needed.
- **Icons:** converted 52 Lucide 1.49.0 icons to single paths and
  rendered a contact sheet. All are correct (Appendix A). Lucide 1.49.0
  has no `trash-2` or `circle-help`; `trash` and `circle-question-mark`
  are used.
- **Screenshots:** re-encoded the M47 after set (60 JPEGs, 4,108,947 B)
  to WebP with Pillow, method 6, the 390 shots downscaled to 1.5× and
  the 1280 shots kept at 1×: 1,410,198 B (q40), 1,492,562 B (q45) and
  1,556,340 B (q50). The rig's device scale factor is 2 below 600 px and
  1 above, so only the phone shots get scaled (D8).

## Appendix A — the ICONS block (Lucide 1.49.0, converted)

Paste this over the whole `const ICONS = { … };` in `app.js`, with the
licence comment from D6 directly above it. The keys and their order
follow the current block, plus `up`, `down` and `chevron-left`.

```js
  const ICONS = {
    // the sections
    health: "M15 21v-8a1 1 0 0 0-1-1h-4a1 1 0 0 0-1 1v8M3 10a2 2 0 0 1 .709-1.528l7-6a2 2 0 0 1 2.582 0l7 6A2 2 0 0 1 21 10v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z",
    chat: "M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z",
    models: "M12 20v2M12 2v2M17 20v2M17 2v2M2 12h2M2 17h2M2 7h2M20 12h2M20 17h2M20 7h2M7 20v2M7 2v2M6 4h12a2 2 0 0 1 2 2v12a2 2 0 0 1 -2 2h-12a2 2 0 0 1 -2 -2v-12a2 2 0 0 1 2 -2zM9 8h6a1 1 0 0 1 1 1v6a1 1 0 0 1 -1 1h-6a1 1 0 0 1 -1 -1v-6a1 1 0 0 1 1 -1z",
    channels: "M16 10a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 14.286V4a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2zM20 9a2 2 0 0 1 2 2v10.286a.71.71 0 0 1-1.212.502l-2.202-2.202A2 2 0 0 0 17.172 19H10a2 2 0 0 1-2-2v-1",
    connections: "M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71",
    more: "M11 12a1 1 0 1 0 2 0a1 1 0 1 0 -2 0M18 12a1 1 0 1 0 2 0a1 1 0 1 0 -2 0M4 12a1 1 0 1 0 2 0a1 1 0 1 0 -2 0",
    console: "M12 19h8M4 17l6-6-6-6",
    config: "M10 5H3M12 19H3M14 3v4M16 17v4M21 12h-9M21 19h-5M21 5h-7M8 10v4M8 12H3",
    routing: "M3 19a3 3 0 1 0 6 0a3 3 0 1 0 -6 0M9 19h8.5a3.5 3.5 0 0 0 0-7h-11a3.5 3.5 0 0 1 0-7H15M15 5a3 3 0 1 0 6 0a3 3 0 1 0 -6 0",
    usage: "M3 3v16a2 2 0 0 0 2 2h16M18 17V9M13 17V5M8 17v-3",
    tasks: "M13 5h8M13 12h8M13 19h8M3 17l2 2 4-4M3 7l2 2 4-4",
    logs: "M3 5h1M3 12h1M3 19h1M8 5h1M8 12h1M8 19h1M13 5h8M13 12h8M13 19h8",
    extensions: "M15.39 4.39a1 1 0 0 0 1.68-.474 2.5 2.5 0 1 1 3.014 3.015 1 1 0 0 0-.474 1.68l1.683 1.682a2.414 2.414 0 0 1 0 3.414L19.61 15.39a1 1 0 0 1-1.68-.474 2.5 2.5 0 1 0-3.014 3.015 1 1 0 0 1 .474 1.68l-1.683 1.682a2.414 2.414 0 0 1-3.414 0L8.61 19.61a1 1 0 0 0-1.68.474 2.5 2.5 0 1 1-3.014-3.015 1 1 0 0 0 .474-1.68l-1.683-1.682a2.414 2.414 0 0 1 0-3.414L4.39 8.61a1 1 0 0 1 1.68.474 2.5 2.5 0 1 0 3.014-3.015 1 1 0 0 1-.474-1.68l1.683-1.682a2.414 2.414 0 0 1 3.414 0z",
    agents: "M12 8V4H8M6 8h12a2 2 0 0 1 2 2v8a2 2 0 0 1 -2 2h-12a2 2 0 0 1 -2 -2v-8a2 2 0 0 1 2 -2zM2 14h2M20 14h2M15 13v2M9 13v2",
    memory: "M12 18V5M15 13a4.17 4.17 0 0 1-3-4 4.17 4.17 0 0 1-3 4M17.598 6.5A3 3 0 1 0 12 5a3 3 0 1 0-5.598 1.5M17.997 5.125a4 4 0 0 1 2.526 5.77M18 18a4 4 0 0 0 2-7.464M19.967 17.483A4 4 0 1 1 12 18a4 4 0 1 1-7.967-.517M6 18a4 4 0 0 1-2-7.464M6.003 5.125a4 4 0 0 0-2.526 5.77",
    settings: "M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915M9 12a3 3 0 1 0 6 0a3 3 0 1 0 -6 0",
    // things a button does
    search: "M21 21l-4.34-4.34M3 11a8 8 0 1 0 16 0a8 8 0 1 0 -16 0",
    camera: "M13.997 4a2 2 0 0 1 1.76 1.05l.486.9A2 2 0 0 0 18.003 7H20a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V9a2 2 0 0 1 2-2h1.997a2 2 0 0 0 1.759-1.048l.489-.904A2 2 0 0 1 10.004 4zM9 13a3 3 0 1 0 6 0a3 3 0 1 0 -6 0",
    image: "M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-14a2 2 0 0 1 2 -2zM7 9a2 2 0 1 0 4 0a2 2 0 1 0 -4 0M21 15l-3.086-3.086a2 2 0 0 0-2.828 0L6 21",
    copy: "M10 8h10a2 2 0 0 1 2 2v10a2 2 0 0 1 -2 2h-10a2 2 0 0 1 -2 -2v-10a2 2 0 0 1 2 -2zM4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2",
    check: "M20 6 9 17l-5-5",
    x: "M18 6 6 18M6 6l12 12",
    retry: "M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8M21 3v5h-5",
    stop: "M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-14a2 2 0 0 1 2 -2z",
    send: "M5 12l7-7 7 7M12 19V5",
    up: "M5 12l7-7 7 7M12 19V5",
    down: "M12 5v14M19 12l-7 7-7-7",
    play: "M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z",
    pause: "M15 3h3a1 1 0 0 1 1 1v16a1 1 0 0 1 -1 1h-3a1 1 0 0 1 -1 -1v-16a1 1 0 0 1 1 -1zM6 3h3a1 1 0 0 1 1 1v16a1 1 0 0 1 -1 1h-3a1 1 0 0 1 -1 -1v-16a1 1 0 0 1 1 -1z",
    plus: "M5 12h14M12 5v14",
    edit: "M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497zM15 5l4 4",
    trash: "M10 11v6M14 11v6M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6M3 6h18M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2",
    download: "M12 15V3M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M7 10l5 5 5-5",
    external: "M15 3h6v6M10 14 21 3M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6",
    power: "M12 2v10M18.4 6.6a9 9 0 1 1-12.77.04",
    chevron: "M9 18l6-6-6-6",
    "chevron-down": "M6 9l6 6 6-6",
    "chevron-left": "M15 18l-6-6 6-6",
    // things a place is
    sun: "M8 12a4 4 0 1 0 8 0a4 4 0 1 0 -8 0M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41",
    moon: "M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401",
    monitor: "M4 3h16a2 2 0 0 1 2 2v10a2 2 0 0 1 -2 2h-16a2 2 0 0 1 -2 -2v-10a2 2 0 0 1 2 -2zM8 21L16 21M12 17L12 21",
    globe: "M2 12a10 10 0 1 0 20 0a10 10 0 1 0 -20 0M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20M2 12h20",
    clock: "M2 12a10 10 0 1 0 20 0a10 10 0 1 0 -20 0M12 6v6l4 2",
    calendar: "M8 2v3M16 2v3M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-14a2 2 0 0 1 2 -2zM3 9h18",
    backup: "M3 3h18a1 1 0 0 1 1 1v3a1 1 0 0 1 -1 1h-18a1 1 0 0 1 -1 -1v-3a1 1 0 0 1 1 -1zM4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8M10 12h4",
    file: "M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2zM14 2v5a1 1 0 0 0 1 1h5M10 9H8M16 13H8M16 17H8",
    key: "M2.586 17.414A2 2 0 0 0 2 18.828V21a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h1a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h.172a2 2 0 0 0 1.414-.586l.814-.814a6.5 6.5 0 1 0-4-4zM16 7.5a0.5 0.5 0 1 0 1 0a0.5 0.5 0 1 0 -1 0",
    lock: "M5 11h14a2 2 0 0 1 2 2v7a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-7a2 2 0 0 1 2 -2zM7 11V7a5 5 0 0 1 10 0v4",
    telegram: "M14.536 21.686a.5.5 0 0 0 .937-.024l6.5-19a.496.496 0 0 0-.635-.635l-19 6.5a.5.5 0 0 0-.024.937l7.93 3.18a2 2 0 0 1 1.112 1.11zM21.854 2.147l-10.94 10.939",
    // things the page says
    alert: "M21.73 18l-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3M12 9v4M12 17h.01",
    info: "M2 12a10 10 0 1 0 20 0a10 10 0 1 0 -20 0M12 16v-4M12 8h.01",
    help: "M2 12a10 10 0 1 0 20 0a10 10 0 1 0 -20 0M9.09 9a3 3 0 0 1 5.83 1c0 2-3 3-3 3M12 17h.01",
  };
```
