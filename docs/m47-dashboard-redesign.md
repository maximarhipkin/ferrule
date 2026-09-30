# M47 — the dashboard, redesigned

Status: **plan** (phase 1). The audit, the before/after numbers and "what
was verified and how" are filled in as each part lands.

## Why

Max tried the page on a phone and wants it "beautiful, modern and very easy
to use", with more abilities. Most closed-beta users aren't developers. A
stranger should be able to set up a bot, talk to it and understand what it
does without reading docs.

The page today (M22, M24, M37, M44) can do almost everything, but it reads
like an operator's console:

- 13 sections sit in one flat rail.
- Words like "lanes", "watchdog", "heartbeat", "pins", "relay" and "cron"
  have no explanation.
- `window.confirm`/`window.prompt` dialogs break on phones and inside
  in-app browsers.
- Home is dense.
- The chat can't take a photo, copy an answer, retry or stop.

## Goals

1. Home answers "is my bot OK, and what's next?" on one phone screen.
2. A first-run checklist takes a new owner from nothing to a first answer
   without leaving the page. The steps are model, Telegram, hello.
3. One visual system (tokens, icons, components) in light and dark. It
   meets WCAG AA and works at 390 px and 1280 px, in English and in Hebrew
   (RTL).
4. Every current ability stays, and every section is simple by default,
   with expert controls behind "Advanced".
5. New abilities: photo input with real vision, chat polish, a memory view,
   a schedule picker that needs no cron, backup from the page, and a theme
   and language setting.
6. Keyboard, screen readers and reduced motion all work. Transfer size and
   first paint are measured before and after.

## Hard constraints (from the brief, restated as checks)

| Constraint | How it's held |
|---|---|
| Nothing on the page or on the way to it calls a model | New endpoints read stores and files only. Memory search is keyword-only (`MemoryStore::recall`) and never calls the embedder. The chat is the only path that reaches a model, as today. `dashboard_m47::no_new_endpoint_calls_a_model` runs every new GET against a gateway whose model URL is a closed port. |
| No build step, npm, CDN, external font or script | Hand-written `app.js`/`app.css`. The icons are inline SVG paths drawn for Ferrule, with no third-party set. The fonts stay the self-hosted IBM Plex. `the_page_loads_nothing_from_elsewhere` (unit) scans the assets for `http://`, `https://` and `//` URLs outside comments and string literals meant for display. |
| Works under `/b/<bot-id>/`, behind the tunnel, read-only where locked | Every asset goes through `index_for` and the static routes. New API calls use `BASE`. Managed locks from `/api/managed` disable the matching controls with the policy's `why`. Tested under a prefix in `dashboard_m47`. |
| User content stays in `dir="auto"` | The existing `text()` helper and the `el(... text: x, dir: "auto" ...)` one-line form are kept verbatim. The test `dashboard::…` parses them from `app.js` (see Risk R3). New user fields (memory text, task name/prompt, photo name, chat code blocks) use them too, and `dashboard_m47::new_user_fields_render_right_to_left` extends the field list. |
| Redacted: no secret, no transcript in Logs | Every new JSON response goes through `redact_value`, as all `/api/*` do. Logs is untouched. The memory view shows the owner's own memories, redacted like everything else. The backup download never includes secrets (hard-coded `include_secrets = false`). |
| Phone first, then desktop, light and dark | Screenshots at 390 and 1280, light and dark, for every section, before and after. The browser check fails on horizontal scroll at 390 px. |

## Decisions (with reasons)

**D1. Keep one `app.js` and one `app.css`. Load Hebrew lazily.** One script
and one stylesheet keep `index_for` and the CSP simple and cost one request
each. Hebrew strings live in a new `lang-he.js`, fetched only when the
language is Hebrew, so English users don't pay for them.

- Budget: `app.js` + `app.css` gzipped ≤ 45 KB, against ~34 KB today. The
  numbers are measured in Part 1 and again in Part 6.
- No framework and no bundler.

**D2. Palette: "stone and copper".** The copper stays as the single accent,
because it is the ferrule's metal band and it is distinctive among grey
dashboards. It is only used for the primary action, the current nav item
and focus rings.

- Surfaces are warm neutrals (stone) instead of today's cool greys.
- Status colours always come with an icon and a word, never colour alone.
- All pairs are checked for AA by a unit test that parses the token block
  (Part 2).
- Fonts stay IBM Plex Sans and Mono (already self-hosted, OFL, no new
  bytes). *Corrected in Part 2:* IBM Plex Sans Hebrew is already embedded and
  loads by `unicode-range` (the audit found it), so Hebrew needs no
  fallback font and no new bytes.

**D3. Navigation.**

Desktop (≥ 900 px) has a left sidebar in three groups:

- **Everyday**: Home, Chat, Tasks, Memory, Usage.
- **Setup**: Channels, Connections, Models.
- **Advanced**: Logs, Agents, Extensions, Routing, Console, Config file,
  Settings.

A phone has a bottom bar with five items: Home · Chat · Tasks · Usage ·
More. "More" opens a sheet with the same grouped list as the sidebar.

- The current section is shown by the accent colour, a weight change and
  an indicator bar, plus `aria-current="page"`.
- Deep links stay `#section`, so existing links keep working, and
  back/forward work.
- Settings is new: appearance, language, backup, sign out, emergency stop.

Reason: at most five bottom items (HIG and Material), and the things a
beginner touches daily come first.

**D4. Dialogs are in the page, not `window.*`.** A native `<dialog>` handles
the confirm and prompt steps, with focus management and Esc built in. The
`act()` 409-confirm pattern now opens it. Reason: `window.confirm` is
blocked or ugly in Telegram's in-app browser and on iOS, and it can't be
styled or translated.

**D5. Vision: the provider layer decides, per driver, per request.**

The data model:

- Core `Message` gains `images: Vec<ImageRef>`. An `ImageRef` is a file
  path, a MIME type and a display name; history holds paths, not bytes.
- The channel saves the photo to the inbox as today (M39), and the router
  attaches it to the user message.

When a driver builds the wire body, it loads each image the request
carries:

- **A driver that sees images** base64-encodes it into the native part
  (chat `image_url`, Anthropic `image`, Responses/Codex `input_image`).
- **A driver that doesn't** writes a plain note instead: "[This model
  can't see images; the photo is saved as `inbox/…`. Open it with your
  tools if you need it.]"

Why this shape:

- **It stays right with routing and fail-over.** The provider actually
  serving the call makes the choice. A vision primary that falls over to a
  text-only fallback degrades to the note by itself.
- **Only the newest 4 images go as pixels.** Older ones become the note, so
  a long chat doesn't resend every photo. The count is
  `ferrule_core::vision::MAX_IMAGES`.
- **Which models see images.** Each driver has a `vision: bool`. It comes
  from `vision = true|false` in the config when set, and otherwise from the
  name list in `ferrule_providers::vision::by_name`:
  - **Seeing**:
    - `claude-3*` and newer Claude families (`claude-*-4*`, `claude-opus-*`,
      `claude-sonnet-*`, `claude-haiku-*`, `claude-fable-*`);
    - `gpt-4o*`, `gpt-4.1*`, `gpt-4-turbo*`, `gpt-5*`;
    - `o1` (not `o1-mini`), `o3` (not `o3-mini`), `o4-mini`;
    - `gemini-*`, `grok-4*`, `grok-*vision*`;
    - `llava*`, `bakllava`, `moondream*`, `minicpm-v*`, `llama3.2-vision*`,
      `llama-4*`;
    - `*-vl*` (Qwen-VL, Kimi-VL, GLM-4V), `pixtral*`, `mistral-small-3.[1-9]*`,
      `mistral-medium-3*`, `gemma3*` (not `gemma3:1b`).
  - **Text-only**: everything else, including `deepseek-*`, `kimi-k2*`,
    `gpt-3.5*`, `o1-mini`, `o3-mini` and the mock.
  - **The Claude Code plan engine** is always text-only; it gets the note.
- **The user is told once per chat** when the model that will answer can't
  see photos. `Provider::sees_images()` is forwarded by the `Tiered` and
  `RoutedProvider` wrappers.
- **A provider that rejects the image** with a 400 whose message names
  images gets one retry of that request with notes instead of pixels. This
  mirrors the existing "streaming refused, ask plainly" retry in
  `openai_compat.rs`.

**D6. Photo upload: shrink in the browser, check on the server.**

- **In the browser.** The page draws the photo through
  `createImageBitmap(file, {imageOrientation: "from-image"})` into a canvas
  and re-encodes it as JPEG 0.85 with the longest side ≤ 1568 px. That's
  Anthropic's recommended size and fine for OpenAI. The re-encode also
  drops EXIF, including GPS, before anything leaves the phone.
- **What goes up.** The JSON carries base64. A `data:` URL gives the
  preview, which the CSP already allows; `blob:` stays banned.
- **On the server.**
  - The decoded size cap is 3.5 MB (`PHOTO_MAX_BYTES`), which keeps base64
    under Anthropic's 5 MB per image.
  - Only `image/jpeg`, `image/png`, `image/webp` and `image/gif` are
    accepted, and the magic bytes must agree. **SVG is refused**.
  - The photo is saved through `files::Inbox::save`, which names the file
    itself.
- **Telegram photos.** No image crate is added (M40 build diet). The
  channel picks the largest `photo[]` size with its longest side ≤ 2000 px
  and `file_size` ≤ the cap.

**D7. The bigger body limit only after a valid session.**

- `http.rs` reads the head first. The body limit and deadline then come
  from a closure that `mod.rs` passes in.
- Only `POST <base>/api/chat/photo` with a live session cookie gets 5 MB
  and 60 s. Everything else keeps 64 KB and 10 s.
- At most two big bodies are read at once (a `tokio::sync::Semaphore`
  in `http.rs`). A third gets 503 "Another upload is in progress; try again
  in a moment."
- Reason: an unauthenticated client can't make the process allocate
  megabytes.

**D8. The memory view is keyword-only.** `MemoryStore::recall` (SQLite
FTS/LIKE) and `recent`, opened read-only per request from
`<data>/memory.db`. Hybrid recall would call the configured embedder, which
may be a remote model, and the page must never call a model.

**D9. Tasks: a picker that writes cron, plus "New task" on the page.**

- The picker writes the same 5-field cron or RFC 3339 time that `ferrule
  tasks add` takes. Presets: every day, weekdays, every week on…, every
  month on…, every N hours, once.
- The server checks the result and previews the next three runs through
  `ferrule_gateway::initial_next_run_at`, so the words and the parser can't
  disagree.
- Raw cron sits under Advanced, and unknown patterns are shown as "Custom
  schedule".
- A task made on the page answers either in the page chat or in the
  owner's main chat.
- It never takes a gate command. That's a shell line, and it stays on the
  CLI.

**D10. Backup: in process, into `<data>/backups/`, never with secrets.**

- The page starts `backup::backup(Some(<data>/backups/<name>), false)` on a
  blocking task and polls for the result. The newest 3 page-made backups
  are kept.
- Downloading streams the file. It is never held in memory.
- `"backups"` is added to `backup::CACHES`, so a backup never contains the
  last one.
- Reason: a Cloudflare tunnel cuts requests at ~100 s, so the work can't
  hang on one request. The `runs` subprocess path would also work, but in
  process is simpler to test and has nothing to parse.

**D11. Language: English plus Hebrew for the shell only.**

- Covered: nav, section titles, common buttons, the checklist, the palette,
  Settings, dialogs, empty states. The server's own sentences stay in
  English. That keeps it small, and a follow-up can move them.
- The choice is kept in `localStorage["ferrule-lang"]`, per browser, like
  the theme.
- `theme.js` sets `<html lang dir>` before first paint, so there's no
  flash.
- The CSS uses logical properties (`margin-inline-start`, `inset-inline`)
  so RTL mirrors without a second stylesheet.
- A unit test fails if any `t("…")` string in `app.js` has no Hebrew entry.

**D12. The command palette.** It opens on `/` (when not typing) or Ctrl/⌘-K.

- It lists every section plus these actions: New task, Attach a photo,
  Stop the running turn, Back up now, Switch theme, Switch language, Run
  doctor, Sign out, and Keyboard shortcuts (`?`).
- It matches substrings case- and diacritic-insensitively, in both
  languages.
- It's built as an ARIA combobox (`role="combobox"` input, `role="listbox"`
  results, `aria-activedescendant`).

**D13. Screenshots go in the new `docs/assets/m47/` directory.**

- **The conflict.** The shared rules say don't touch `docs/assets`. The
  M47 brief names `docs/assets/m47/` explicitly, and M37 set the same
  precedent with `docs/assets/m37/`.
- **What I'll do.** Add new files only, in the new directory, and change no
  existing image. The decision is called out in the PR and the final
  report.
- **The format.** JPEG at quality 80, viewport only, device scale 1. The
  budget is ≤ 8 MB for the whole directory.
- **How it's read.** "Don't push any image" is read as container images,
  which the brief also forbids and which this milestone doesn't build.

**D14. The icons are drawn for Ferrule.**

- About 40 icons on a 24 × 24 grid, drawn with 1.75 px strokes, round caps
  and `currentColor`, and kept as path strings in `app.js` (`ICONS`).
- They're built with `createElementNS`, so there's no sprite request and no
  CSP change.
- Reason: no third-party licence file has to be added next to the assets.
  The rule says not to touch licence files, and self-drawn paths avoid the
  question.

**D15. Keep abilities by guard, not by memory.** A unit test lists every
API path `api::route` answers and fails if `app.js` no longer calls one.
Deliberate exceptions, such as `console/parity` if the page never used it,
sit in a short allow-list with reasons. Rewriting 118 KB of JS is where
abilities get lost quietly.

## Threat model

| Threat | Mitigation | Check |
|---|---|---|
| An unauthenticated client makes the process allocate big bodies | D7: the big limit only for a live session on one path; two at a time; 413 before reading the body | `dashboard_m47::a_big_body_without_a_session_is_refused_unread` |
| A forged upload (an HTML or SVG named `.jpg`) becomes stored XSS | Magic-byte check; SVG refused; photos are never served back to the page (history shows a name chip); the name is only ever text | `dashboard_m47::photo_mime_must_match_its_bytes`, `…svg_is_refused` |
| Path traversal in the backup download or delete | The name must match `^[A-Za-z0-9._-]+\.tar\.gz$`, be a direct child of `<data>/backups`, and pass a `canonicalize` prefix check | `dashboard_m47::backup_names_cannot_leave_the_backups_dir` |
| Backup leaks keys | `include_secrets` is hard-coded false from the page; the manifest is checked in the test | `dashboard_m47::a_page_backup_has_no_secrets` |
| CSRF on new POSTs | Unchanged gate: Origin, JSON content type and `X-Ferrule-Csrf` on every POST. The download is a GET with a SameSite=Strict cookie; a cross-site page can't send it | `dashboard_m47::new_posts_need_the_csrf_token` |
| Photos sent to a provider the owner didn't expect | Same trust as text (the owner picked the model). Docs say photos go to the answering model. Text-only models never get bytes | vision tests |
| Tasks from the page run shell | No `gate` field from the page; the prompt goes through the same agent sandbox as any turn | `dashboard_m47::a_page_task_cannot_carry_a_gate` |
| Managed bot: the page changes a locked setting | Controls disabled from `/api/managed`, and the server refuses as today; new endpoints check `managed::policy()` locks where one applies | `dashboard_m47::managed_locks_hold_on_new_controls` |
| Secrets in memory text, file names or task prompts | `redact_value` on every JSON response (unchanged) | covered by the existing redaction tests plus one memory case |
| The palette or Hebrew file loads remote code | Same-origin only; the CSP is unchanged (`default-src 'self'`) | `the_page_loads_nothing_from_elsewhere` |

## Failure modes

- **A photo file is gone before the turn is built** (a cleanup, or a
  restore). The driver writes "[a photo was attached here but is no longer
  available]" and the turn goes on.
- **The provider rejects the image** (too big, or the model can't take it).
  One retry with notes, a `tracing::warn!`, and the answer arrives.
- **The browser can't decode the file** (HEIC outside Safari). The page
  says "This browser can't open that photo format. Try a JPEG, or take a
  screenshot of it." iOS already converts HEIC to JPEG for file inputs.
- **The upload is slow over the tunnel.** The deadline is 60 s on the photo
  path, and the page shows progress text ("Sending photo…") with a cancel.
- **Model or gateway down.**
  - Home and every section still load; only the chat needs the gateway.
  - The chat shows its existing "the gateway isn't running" state.
  - The checklist's "say hello" step explains it and links to the fix.
- **`localStorage` is unavailable** (a private tab). The defaults hold:
  system theme, English. Nothing breaks.
- **A backup fails** (disk full). The error, redacted, is shown on the
  Settings card, and the partial file is removed (`backup()` already writes
  `.partial` and renames).
- **The memory DB is missing or locked.** The empty state says so ("No
  memories yet"); a busy DB is retried once after 200 ms. The gateway
  writes in WAL mode, so reads don't block.
- **An old browser without `<dialog>`** (Safari < 15.4). The confirm falls
  back to `window.confirm`. That's feature-detected, and nothing else
  depends on it.

## Out of scope

- Voice recording in the page.
- PWA install and push notifications.
- Several users or roles.
- Translating server-side sentences into Hebrew.
- Photos shown back in chat history after a reload (they're kept as name
  chips).
- PDFs or other documents as vision input (they stay files).
- Image generation, video.
- Drag-and-drop reordering.
- Charts beyond one 7-day bar chart on Usage.
- Vision for the Claude Code plan engine.
- More than the first photo of a Telegram album (the rest get one line:
  "I only looked at the first photo of that album").

## Audit

Screenshots of the page as it was at `855ad17`: every one of the 13
sections, at 390 and 1280 px, light and dark, in
`docs/assets/m47/before/<section>-<width>-<light|dark>.jpg` (52 JPEGs,
3.2 MB). They come from `scripts/m47_shots.sh`, on the mock model with the
browser check's seeded data (one chat turn, one connected service, one
hidden notice, a fallback).

### Before — the numbers

| | Value |
|---|---|
| Tests (`cargo test --workspace --no-fail-fast`) | 1721 passed, 0 failed, 29 ignored, in 71 test binaries |
| `app.js` | 118,452 B raw, 31,730 B gzip -9 |
| `app.css` | 21,065 B raw, 5,619 B gzip -9 |
| `theme.js`, `index.html` | 1,299 B, 1,358 B raw |
| Fonts on a cold load of Home | Plex Sans 400 (21 KB), 600 (22 KB), Plex Mono 400 (17 KB); the two Hebrew cuts load only when Hebrew is on screen |
| Cold load of Home at 390 px, cache off | 203,128 B on the wire for the assets, 5 API calls, FCP 132 ms (headless Chromium, no CPU throttle), DOMContentLoaded 75 ms |
| Compression | None. The server sends every asset whole: 203 KB on the wire where gzip would send about 80 KB |

### The abilities inventory (the input for the D15 guard)

The paths `app.js` calls today, from `grep '"/api/…'` plus the 62
`act(...)`/`btn(...)`/`ask(...)` call sites: `/api/health`, `/api/approvals`,
`/api/session`, `/api/login`, `/api/logout`, `/api/managed`, `/api/models`,
`/api/models/choices`, `/api/models/provider/list`, `/api/catalog`,
`/api/recommend`, `/api/eval`, `/api/run`, `/api/plans/chatgpt/{start,poll}`,
`/api/routing`, `/api/connections`, `/api/connections/{checklist,test}`,
`/api/channels`, `/api/channels/test`, `/api/telegram/{test,save,wait,allow}`,
`/api/chat`, `/api/chat/send`, `/api/console/{complete,run,job,parity}`,
`/api/config`, `/api/config/check`, `/api/usage`, `/api/tasks`, `/api/logs`,
`/api/settings`, `/api/agents`. Step 16's guard keeps every route in
`api::route` reachable from the page.

### Findings, ranked

1. **Confusing (worst).** A new owner lands on Home and sees a tile grid:
   `version`, `uptime`, `gateway`, `watchdog`, `heartbeat`, `kill switch`.
   Nothing says what to do first. Undefined words: "lanes", "watchdog",
   "heartbeat", "fallback", "pins", "catalog", "relay", "cron", "gates",
   "kill switch", and the theme button, which just says `auto`.
   Connections opens on a "Fixed callback address" card about OAuth and a
   Cloudflare relay before any service is shown. → Parts 3 and 4: a status
   sentence, a first-run checklist, a glossary, "Advanced" folds.
2. **Missing.** No photo in chat (or anywhere), no copy/retry/stop, no way
   to create a task (Tasks says "no tasks" and stops), no memory view, no
   backup button, no command palette, no Hebrew shell, no settings page. →
   Part 5.
3. **Ugly and uneven.**
   - The buttons run from 36 px on desktop to 42 px on touch, and the
     fallback row's ↑ ↓ × are bare glyphs under 30 px.
   - Inputs and selects differ in height from section to section.
   - "Log out" and the theme word ("auto") are plain text buttons in the
     header.
   - On the desktop, content sits in a 520 px column with wide empty
     margins on both sides at 1280 px.
   - Usage is a long stack of numbers on a phone.
   - Chat has the composer floating mid-screen with a large gap under it.
   → Part 2.
4. **Cluttered.** Home's tile grid, kill-switch chip, hidden-notices row,
   notice and problem banners all compete for the first screen; on a phone
   the actual status is below the fold. The problems banner repeats above
   every section, which pushes each title down by ~270 px at 390 px (see
   `models-390-light.jpg`). → Part 3: banner only on Home, a count badge
   elsewhere.
5. **Slow / heavy.** No compression (203 KB on the wire for Home); each
   section polls even when its data hasn't changed; `app.js` is one
   118 KB file for 13 sections. FCP is fine (132 ms) because the page is
   small and local, so the win here is bytes and idle polling, not paint.
   → Part 6.
6. **Broken on phones.** `window.confirm`/`window.prompt` for confirmations
   and typed values (Tasks → Schedule and Model, the console's destructive
   commands), which in-app browsers block or style badly. → `ask()`.

What already works and stays: the strict CSP with no inline script, the
`el()`/`textContent` rule, the `text()` helper string the redaction tests
grep for, `dir="auto"` on every piece of user text, the bottom tab bar and
the sidebar, the theme names (`paper`/`forge`, the `ferrule-theme` key and
`window.ferruleTheme`), and the Hebrew font (already shipped, loaded by
`unicode-range`).

## Plan

Commands used below:

```sh
ENV='export RUSTUP_HOME=/workspace/agent/.rustup CARGO_HOME=/workspace/agent/.cargo-home CARGO_HTTP_CAINFO=/tmp/onecli-combined-ca.pem PATH=/workspace/agent/.cargo-home/bin:$PATH NO_PROXY="localhost,127.0.0.1,::1" CARGO_TARGET_DIR=/workspace/agent/.cargo-target CARGO_INCREMENTAL=0'
CHECK='cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace --no-fail-fast'
```

"CHECK" runs after every part, before its commit. Test counts are taken
from the `test result:` lines (summed) and printed at step 1 and at the
end.

### Part 1 — Audit with screenshots (commit "M47 part 1 — audit")

> Correction made while building: the screenshot rig is
> `scripts/m47_shots.sh`, a wrapper over the browser check's own CDP rig
> (`--shots-all DIR`), not a second rig on agent-browser. Reason: the CDP
> path already starts the gateway, the fake Telegram and the MCP server, and
> it is what CI runs, so the shots and the checks can't drift apart.
> `--measure` reads first paint, the asset bytes and the API call count in
> the same run; the 4× CPU throttle in step 3 was left out (no throttle
> makes the before/after runs easier to compare and FCP is 132 ms).

1. **Baseline numbers.**
   - `eval $ENV; cargo build -p ferrule-cli`.
   - The test count: `cargo test --workspace --no-fail-fast 2>&1 | grep '^test result'`,
     summed.
   - Asset sizes: raw and `gzip -9` of `app.js`, `app.css`, `index.html`,
     `theme.js`, and the fonts.
   - Record all of it in the doc's Audit section, under "Before".
2. **A screenshot rig** (new `scripts/m47_shots.sh`, a bash wrapper, no
   packages).
   - It starts `target/debug/ferrule gateway` on a temp `FERRULE_CONFIG`
     and `FERRULE_DATA_DIR` with the stdlib mock model (the same mock
     `ferrule eval` uses; not changed) and no channels. A second run adds a
     fake Telegram (a python3 stdlib server in the script) so the Channels
     and Chat sections have content.
   - It seeds data so no section is empty:
     - two tasks through `ferrule tasks add`;
     - a memory through `ferrule memory add`;
     - one chat turn;
     - a ledger line from that turn.
   - It logs in with the panel token through `POST /api/login`, as
     `tests/it/dashboard.rs::login` does, and hands the cookie to the
     browser. Then it drives `/pnpm/agent-browser`:
     - `agent-browser close` first;
     - `--ignore-https-errors`, `NO_PROXY` set;
     - `set viewport 390 844` / `1280 800`;
     - `set media dark|light` (via `emulateMedia`, or by toggling the
       `ferrule-theme` localStorage key);
     - `open …#<section>`, `wait --text <title>` bounded by `timeout 20`,
       then `screenshot --screenshot-format jpeg --screenshot-quality 80`.
   - Output: `docs/assets/m47/before/<section>-<390|1280>-<light|dark>.jpg`,
     13 × 4 = 52 files.
   - Risk: agent-browser's daemon hangs. Every call is wrapped in
     `timeout 30`, with at most 3 attempts, then the rig falls back to
     `scripts/dashboard_browser_check.mjs --shots` (CDP, already proven in
     CI).
3. **Write the audit.**
   - Look at every screenshot (Read tool on the JPEGs).
   - Rank the findings in five buckets (confusing, cluttered, slow, ugly,
     missing), each with the section, what a beginner would trip on, and
     the fix planned below.
   - Write the abilities inventory: every `api(...)`/`post(...)` path in
     today's `app.js`, the input for D15's guard.
   - First paint: add the `--measure` flag to
     `scripts/dashboard_browser_check.mjs` now, so the "before" numbers
     come from the unchanged page (step 27 extends the script further).
     It reads `performance.getEntriesByName("first-contentful-paint")`
     at 390 px with 4× CPU throttling.
   - Commit: the doc, `docs/assets/m47/before/*`, `scripts/m47_shots.sh`,
     the `--measure` flag.

### Part 2 — The visual system (commit "M47 part 2 — tokens, icons, components")

4. **Tokens at the top of `app.css`**, in one `:root` block plus one
   `[data-theme="dark"]` block and the `prefers-color-scheme` mirror
   `theme.js` already relies on:
   - **type**: `--fs-xs .75rem`, `--fs-sm .875rem`, `--fs-md 1rem`,
     `--fs-lg 1.125rem`, `--fs-xl 1.25rem`, `--fs-2xl 1.5rem`,
     `--fs-3xl 1.875rem`; `--lh-body 1.55`; `--fw-regular 400`,
     `--fw-medium 500`, `--fw-bold 600`;
   - **space**: `--sp-1 4px`, `--sp-2 8px`, `--sp-3 12px`, `--sp-4 16px`,
     `--sp-5 20px`, `--sp-6 24px`, `--sp-8 32px`, `--sp-10 40px`,
     `--sp-12 48px`;
   - **colour, light**: `--bg #F7F6F3`, `--surface #FFFFFF`,
     `--surface-2 #F0EEEA`, `--border #E2DED7`, `--text #1C1917`,
     `--text-2 #57534E`, `--accent #B4531C`, `--on-accent #FFFFFF`,
     `--ok #15803D`, `--warn #A16207`, `--bad #B91C1C`, `--focus #B4531C`;
   - **colour, dark**: `--bg #141210`, `--surface #1C1917`,
     `--surface-2 #26221F`, `--border #3A3531`, `--text #F5F2EE`,
     `--text-2 #B8B0A7`, `--accent #E8915C`, `--on-accent #1C1917`,
     `--ok #4ADE80`, `--warn #FACC15`, `--bad #F87171`;
   - **shape**: `--r-sm 6px`, `--r-md 10px`, `--r-lg 14px`, `--r-full 999px`;
     `--shadow-1/2/3`;
   - **motion**: `--dur-fast 120ms`, `--dur 180ms`,
     `--ease cubic-bezier(.2,.8,.2,1)`, all 0 under
     `prefers-reduced-motion: reduce`;
   - **layers**: `--z-nav 10`, `--z-sheet 40`, `--z-toast 60`,
     `--z-dialog 100`;
   - **touch**: `--tap 44px`.

   No hex value appears in `app.css` outside these blocks. The skill's
   `validate-tokens.cjs --dir crates/ferrule-cli/src/dashboard/assets` is
   run as a check, and its hits outside the token block are fixed.
   - Test: unit `dashboard::tokens_meet_wcag_aa` in `mod.rs`. It parses
     both blocks and asserts ≥ 4.5 for text/bg, text/surface, text-2/bg,
     text-2/surface, on-accent/accent, accent/bg (as link text), and
     ok/warn/bad against surface, in light and dark (with the WCAG
     relative-luminance formula). Where a proposed value fails, the value
     is changed, not the test.
5. **Icons.** An `ICONS` map in `app.js` and `icon(name, label?)`, which
   returns an `<svg aria-hidden="true">`, or `role="img"` plus
   `aria-label` when labelled.
   - The ~40 names: home, chat, tasks, memory, usage, more, channels,
     connections, models, logs, agents, extensions, routing, console,
     file, settings, search, camera, image, copy, check, x, retry, stop,
     send, sun, moon, monitor, globe, download, trash, plus, chevron,
     alert, info, lock, play, pause, clock, key, telegram.
   - Test: unit `every_icon_used_exists` greps `icon("…")` in `app.js`
     against the map's keys.
6. **Components**, as CSS plus JS helpers in `app.js`:
   - `button({label, icon, kind: "primary"|"quiet"|"danger"|"icon", busy})`:
     min-height `--tap`; while busy it shows a spinner and sets
     `aria-busy` and `disabled`.
   - `field({label, name, type, help, error, value, locked})`: a visible
     `<label for>`, help text under the input, the error under the input
     with `role="alert"`, `inputmode`/`autocomplete` hints, and a
     read-only look for managed locks with the `why` as help.
   - `card({title, icon, actions, body})`.
   - `list(rows, columns)`: a `<table>` at ≥ 640 px, and at < 640 px CSS
     turns each row into a card, using `data-label` on each cell.
   - `toast(text, kind)`: in `#toasts` (`aria-live="polite"`), gone after
     4 s; errors stay until closed.
   - `empty({icon, title, text, action})`.
   - `skeleton(lines)`: a shimmer, static under reduced motion.
   - `ask({title, text, confirm, danger, input})`, which returns a Promise:
     a native `<dialog>` with a `window.confirm` fallback (D4).
   - `sheet(content)`: a bottom sheet on a phone, a popover on desktop.
   - `tip(term)`: a glossary term, `<abbr>`-like with a tap-to-open
     popover, shown to screen readers through `aria-describedby`.
   - Focus: `:focus-visible` gets a 2 px `--focus` outline at 2 px offset
     everywhere. `touch-action: manipulation` on controls.
7. **Swap without changing behaviour.** Keep today's section code and
   markup where it works, and re-point its classes at the new components.
   The existing `text()` helper string is untouched (R3). Run CHECK and
   `node scripts/dashboard_browser_check.mjs --bin $CARGO_TARGET_DIR/debug/ferrule --chromium /usr/bin/chromium`.

### Part 3 — Home and the first run (commit "M47 part 3 — home and first-run checklist")

8. **New endpoint `GET /api/setup`**, in a new `crates/ferrule-cli/src/dashboard/setup.rs`,
   `pub fn view(ctx) -> Answer`:

   ```json
   {"done": false,
    "steps": [
      {"id": "model",    "done": true,  "label": "Give your bot a brain", "detail": "Using openai/gpt-5.2", "sees_photos": true},
      {"id": "telegram", "done": false, "label": "Connect Telegram",       "detail": "So you can talk to it from your phone", "skippable": true},
      {"id": "hello",    "done": false, "label": "Say hello",              "detail": "Send it a first message"}]}
   ```

   - **model** is done when `ctx.models` resolves the default and its
     `Entry` has a key or a plan. It uses the same test the health
     problem "no model" uses; that function is reused, not copied.
   - **telegram** is done when `[channels.telegram]` is configured and,
     with a live gateway, the channel has no `problem()`.
   - **hello** is done when `<data>/ledger.jsonl` has at least one record
     (`crate::ledger::read_records(.., None)`, stopping at the first) or
     the page chat has a bot entry.
   - Route: `"setup" => super::setup::view(ctx)` in `api::route` (GET).
   - Test: `dashboard_m47::the_setup_checklist_follows_the_bot` goes from
     a fresh home (all false), to a model key written (model true), to a
     Telegram config with a fake Bot API (telegram true), to one Telegram
     message answered by the mock (hello true, done true).
9. **Home rewritten** (`sections.health` keeps its key, so `#health` deep
   links still work; the title becomes "Home"). Top to bottom:
   1. **The status** as one sentence with an icon and a colour: "Your bot
      is running", "Needs your attention (2)", "Stopped (emergency
      stop is on)" or "The gateway isn't running". Under it: the model, the
      connected channels, and today's spend against the cap (with a `tip`
      for "cap").
   2. **Problems.** Each shows `what` in plain words plus its first `fix`
      as a primary button. Other fixes go under "More options". The
      existing `problems[].fixes` shape is unchanged.
   3. **The first-run checklist** (step 10), shown while `!done` and not
      hidden.
   4. **Approvals waiting**, as today, with bigger Yes/No buttons.
   5. **Quick actions**: Talk to it, New task, Back up now, Open in
      Telegram (when the bot's username is known from `channels` data).
   6. **Running now**: the turns list with plain words ("Working on your
      message for 12 s") and a Stop button. "Lane", "watchdog" and
      "heartbeat" move under "Details", each with a `tip`.
   7. **Notices**: dismissible as today (`data-notice`, `.x` and the
      "hidden" `<details>` are kept for the browser check).
10. **The checklist UI.**
    - Progress reads "1 of 3 done", with a progress bar
      (`role="progressbar"`).
    - Each step opens in place:
      - **model** embeds today's provider form (`models/provider`, a key
        field with "Where do I get a key?" links for OpenAI, Anthropic and
        OpenRouter) and the "Sign in with ChatGPT" flow
        (`plans/chatgpt/start`/`poll`/`cancel`);
      - **telegram** embeds the M37 inline Telegram flow (`telegram/test`,
        `save`, `wait`, `allow`) with its BotFather steps in plain words;
      - **hello** is a button that jumps to Chat with "Hello!" typed in.
    - The checklist disappears when done. "Hide this" sets
      `localStorage["ferrule-setup-hidden"]`, and Settings can show it
      again.
    - Test: the browser check (step 27) walks it with the mock.
11. **Glossary.** A `GLOSSARY` map (term → one plain sentence) for every
    technical word left on the page: model, fallback, cap, turn,
    approval, channel, connection, MCP, skill, hook, cron, routing,
    tunnel, token, lane, watchdog, heartbeat, relay, pin, alias.
    - `tip(term)` renders it.
    - Unit test `every_tip_has_a_definition`.

### Part 4 — Navigation and the sections (commit "M47 part 4 — navigation, palette, sections")

12. **The shell** (`index.html` plus `app.js`).
    - Landmarks: `<header>` (bot name, instance, and a status dot with
      text), `<nav id="rail">` (sidebar; the id is kept for the browser
      check), `<main id="main" tabindex="-1">`, `<nav id="tabbar">` (the
      phone bottom bar), `#toasts`, and a skip link "Skip to content".
    - `ORDER` and `GROUPS` replace today's flat `order`.
    - After navigation, focus moves to `#main` (`focus-on-route-change`).
    - The bottom bar reserves `env(safe-area-inset-bottom)`. `min-height: 100dvh`.
    - Badges: problems on Home, approvals on Chat.
    - Test: unit `the_index_has_landmarks_and_a_skip_link`.
13. **The command palette** (D12): `palette()` and `PALETTE_ACTIONS`.
    - Shortcuts: `/` and Ctrl/⌘-K open it; `?` lists the shortcuts; Esc
      closes; ↑/↓/Enter pick.
    - The "Stop the running turn" action posts `turn/stop` with
      `{session: "dashboard__owner"}`.
    - Test: browser check (step 27) opens it with Ctrl-K, types "tas",
      presses Enter, and expects `#tasks` shown with focus in `#main`.
14. **Sections, moved to the new system.** Simple by default, "Advanced"
    as a `<details class="advanced">`. Every endpoint call stays (D15).

    | Section | Simple view | Behind "Advanced" |
    |---|---|---|
    | **Chat** | Step 17 | — |
    | **Tasks** | Cards: name, schedule in words, "Next: Thu 09:00", on/off switch, Run now, the schedule picker (step 24), New task | Raw cron, model per task, run history, delete |
    | **Memory** (new) | Step 21 | Tags, ids |
    | **Usage** | Today and the last 7 days against the cap, as an inline-SVG bar chart with a visually hidden table for screen readers | 30 days, per-model table, prices, "fill prices" |
    | **Channels** | One card per channel: status in words, Test, Connect/Disconnect; Telegram's inline setup | Keys add/revoke, restart, remove, raw settings |
    | **Connections** | A tile grid: service, what it lets the bot do in one line, Connect button, key form in place | Relay deploy/use/check, Google OAuth client, write access, test |
    | **Models** | "Your bot's brain: X" with a "sees photos" badge (`vision` from `/api/models`, step 15), Change, Add a model (key or ChatGPT sign-in) | Fallbacks, pins, aliases, catalog, recommend, candidate eval, prices, exact ids |
    | **Logs** | Filter chips (Everything, Problems, Changes), search, plain rows | Kinds, raw fields |
    | **Agents** | List with what each is for | Details |
    | **Extensions** | Skills, MCP servers and hooks with on/off switches, each with one line | Caps, trust/untrust hooks, remove |
    | **Routing** | (Advanced group) today's view in the new components | — |
    | **Console** | (Advanced group) as today, restyled; the `.console-line input` and `.complete button` selectors are kept | — |
    | **Config file** | (Advanced group) the editor as today; check, save, undo, restore | — |
    | **Settings** (new) | Step 23 | — |

    Every `window.confirm`/`prompt` becomes `ask()`, and `window.open`
    becomes a plain `<a target="_blank" rel="noopener">` the user taps.
    Test: `the_page_uses_no_native_dialogs` (unit; greps `app.js` for
    `window.confirm(` and `prompt(` outside the `ask()` fallback).
15. **`/api/models` gains `vision: bool` per entry**, from
    `Entry::sees_images()` (step 19). This adds one field to the existing
    JSON; nothing is removed.
16. **The guard from D15**: unit `every_route_is_used_by_the_page` in
    `dashboard/mod.rs` tests. It extracts the quoted paths in
    `api::route`'s two `match` blocks with a small parser over
    `api.rs`'s source (`include_str!`) and checks that each appears in
    `APP_JS` as `"<path>"` or `` `<path>` ``, apart from an allow-list with
    reasons.
    Run CHECK, and the browser check with its selectors updated.

### Part 5 — New abilities

#### 5a. Vision in the provider layer (commit "M47 part 5a — photos reach models that can see")

17. **Chat polish** (`sections.chat` in `app.js`; this is page-only, but it
    lands with 5b so the attach button has somewhere to go). The pieces:
    - **incremental render**: entries are keyed by `id`; edited entries
      are replaced in place, never re-rendered wholesale;
    - **the scroll**: it sticks to the bottom only when the reader is
      within 80 px of it, and otherwise shows a "New messages" pill;
    - **a typing state**: the `waiting` field shows three dots, with the
      text "Writing…" under reduced motion, and `aria-live="polite"` on
      the log;
    - **Stop**: shown while `waiting`, it posts `turn/stop
      {session:"dashboard__owner"}`;
    - **Copy** on each bot bubble: `navigator.clipboard.writeText`,
      falling back to a select plus `execCommand("copy")`, then the
      toast "Copied";
    - **Retry** on the last message when its answer was a failure: it
      resends the same text;
    - **code blocks**: fenced ```` ``` ```` blocks become
      `<pre dir="ltr"><code>`, each with a Copy button; inline
      `` `code` ``; links only for `http(s)` URLs, with
      `rel="noopener noreferrer"`; everything is built with `el()` and
      `textContent`, never `innerHTML`;
    - **keys**: Enter sends, Shift+Enter adds a new line, and the
      textarea grows up to 8 lines.
18. **Core types** (`crates/ferrule-core`):
    - **`message.rs`**:
      - `pub struct ImageRef { pub path: String, pub mime: String, #[serde(default)] pub name: Option<String> }`.
      - `Message.images: Vec<ImageRef>` with
        `#[serde(default, skip_serializing_if = "Vec::is_empty")]`.
      - `Message::user_with_images(text, images)`.
      - All ~29 struct literals get `images: vec![]`; the compiler finds
        them.
    - **new `vision.rs`**:
      - `pub const MAX_IMAGES: usize = 4`.
      - `pub fn pixel_set(msgs: &[Message]) -> HashSet<(usize, usize)>`:
        the newest `MAX_IMAGES` (message index, image index) pairs.
      - `pub fn note(img: &ImageRef, why: NoteWhy) -> String`, where
        `NoteWhy` is `TextOnly`, `Older` or `Missing`. The wording:
        - TextOnly: "[This model can't see images; the photo is saved as
          {name}. Open it with your tools if you need it.]"
        - Older: "[An earlier photo, saved as {name}.]"
        - Missing: "[A photo was attached here but is no longer
          available.]"
    - **`provider.rs`**: `fn sees_images(&self) -> bool { false }`, a
      default method on `Provider`. `routing.rs`'s `Tiered` forwards it to
      the tier the turn starts on.
    - **`agent.rs`**:
      - `pub fn attach_images(&mut self, images: Vec<ImageRef>)` stores
        `pending_images`.
      - `run_inner` builds the goal message with
        `Message::user_with_images(goal, take(pending_images))`.
      - Context estimation adds 1,600 tokens for each image in
        `pixel_set`.
      - `pub fn sees_images(&self) -> bool` delegates to the provider.
    - **Tests** (unit, `ferrule-core`):
      - `vision::newest_four_images_go_as_pixels`;
      - `message::old_transcripts_without_images_still_load`, where the
        JSON of an M46 message deserializes;
      - `agent::attached_images_ride_on_the_next_user_message`, using the
        existing `CapturingProvider`.
19. **Drivers** (`crates/ferrule-providers`):
    - **`vision.rs` (new)**:
      - `pub fn by_name(model: &str) -> bool`, the D5 list, lowercased,
        on the part after the last `/`.
      - `pub(crate) fn load(img, cap) -> Result<(String, String), NoteWhy>`
        returns (mime, base64). It reads the file, caps it at 3.5 MB, and
        checks the magic bytes.
      - `pub(crate) fn image_rejected(message: &str) -> bool`, the 400
        detector.
    - **`DriverOptions`** gains `vision: Option<bool>`.
    - **`build()`** passes the options to `OpenAiCompatProvider::new`
      too. Its signature gains `options`, and the callers in `ferrule-cli`
      are updated.
    - **`openai_compat.rs`**: `to_wire` takes the message's pixel flags.
      A user message with pixels becomes
      `"content": [{"type":"text","text":…}, {"type":"image_url","image_url":{"url":"data:<mime>;base64,<b64>"}}]`.
      Text-only drivers append the notes to the text.
    - **`anthropic.rs`**: `Role::User` pushes
      `{"type":"image","source":{"type":"base64","media_type":…,"data":…}}`
      blocks before the text block. The cache marks are unchanged.
    - **`responses.rs`**: user content becomes
      `[{"type":"input_text","text":…}, {"type":"input_image","image_url":"data:…"}]`.
      `codex.rs` inherits it through `payload`, and `cache_key` also
      hashes the image paths.
    - **Each driver**: `sees_images()` returns
      `self.vision.unwrap_or_else(|| vision::by_name(&self.model))`, and on
      an `image_rejected` 400 it retries once with `pixels = ∅`.
    - **`ferrule-plans/src/claude/engine.rs`** (`ClaudeCode`) renders
      notes only.
    - **`ferrule-cli`**:
      - The config `vision: Option<bool>` goes on `ProviderConfig` and
        `ModelConfig`, is documented in the config template next to
        `context_window`, and becomes `Entry.vision`.
      - `Entry::client` passes it into `DriverOptions`.
      - `Entry::sees_images()`.
      - `RoutedProvider` forwards `sees_images`.
    - **Tests** (unit, in each driver's `mod tests`, using the existing
      `mock_server` pattern):
      - `openai_compat::a_photo_goes_as_an_image_url_part`;
      - `openai_compat::a_text_only_model_gets_the_note_not_the_bytes`;
      - `anthropic::a_photo_goes_as_a_base64_image_block`;
      - `responses::a_photo_goes_as_input_image`;
      - `vision::names_that_see_and_names_that_dont` (a table covering
        every family in D5);
      - `openai_compat::an_image_400_is_retried_with_notes`.

#### 5b. Photos in from Telegram and the page (commit "M47 part 5b — Telegram photos and chat photo upload")

20. **Telegram**
    (`crates/ferrule-gateway/src/channels/telegram.rs`):
    - **Renames.** `TgAudio` becomes `TgFile` (`file_id`, `name`, `size`,
      `caption`, `mime`), `parsed.audio` becomes `parsed.file`, and
      `take_audio` becomes `take_file`.
    - **A new `photo_size(photo: &[Value], cap: u64) -> Option<TgFile>`**
      picks the largest size with `max(width, height) ≤ 2000` and
      `file_size ≤ cap`. The file is named `photo.jpg`, MIME
      `image/jpeg`.
    - **The existing "I can only read text" replies** for a photo (in
      `unread_kind` and `cannot_read`) apply only when there's no inbox.
    - **Albums.** The first photo is taken; later photos of the same
      `media_group_id` are dropped with one reply: "I only looked at the
      first photo of that album."
    - **Wording.** `files::note` for `image/*` becomes neutral: "[The
      sender attached a photo: {rel} (image/jpeg, 240 KB).]". The
      can't-see sentence moves to the driver note (D5). The other kinds
      keep today's text.
    - **Router** (`router.rs`). It works on any `image/*` attachment, so
      photos from the other channels with an inbox (Matrix, Mattermost,
      email) reach a vision model too, with no channel-specific code:
      - `fn is_image(a) -> bool` checks the `image/` prefix and that
        `url` is a file.
      - Before `run_user`, it calls
        `agent.attach_images(inbound.attachments.iter().filter(is_image).map(to_ref).collect())`.
      - `told_no_vision: bool` is kept per lane next to `told_off`. On
        the first photo, if `!agent.sees_images()`, it sends: "I saved
        your photo, but {model} can't see images, so I only get the file.
        Pick a model marked “sees photos” in the dashboard's Models to
        change that." `{model}` is the display ref.
21. **The page upload**:
    - **`POST /api/chat/photo`** (`chat.rs`, `pub async fn photo(ctx, body) -> Answer`).
      - Request:
        `{"text": "what is this?", "name": "IMG_2041.jpg", "mime": "image/jpeg", "data": "<base64>"}`.
      - It checks the MIME against the magic bytes, caps the size, saves
        through `DashboardChannel`'s `Inbox` (a new `with_inbox`, wired
        where the gateway builds the channel, as
        `Inbox::new(workspace, 4)`, the way Matrix, Mattermost and email
        build theirs from `max_file_mb`), and pushes an inbound message
        with the attachment.
      - 200: `{"ok": true, "saved": "inbox/dashboard/2026-10-01/17-photo.jpg", "sees": true}`.
        When the model can't see, it adds `"sees": false` and a `"said"`
        notice with the wording above.
      - Errors:
        - 413: "That photo is too big: the page sends up to 3.5 MB. Try a
          smaller one."
        - 415: "Send a JPEG, PNG, WebP or GIF photo."
        - 400: "That file isn't a photo the bot can read."
        - 503: the existing chat-off text from `chat::send`.
    - **`ChannelCapabilities.attachments`** becomes true for the
      dashboard channel.
    - **Entries.** `GET /api/chat` entries gain an optional
      `"photo": {"name": "IMG_2041.jpg"}`, drawn as a chip with an icon.
      The live preview is the page's own `data:` URL, kept for this page
      session only.
    - **`http.rs`** (D7): `read(stream, limit: impl Fn(&Head) -> Limit)`,
      where `Limit { body: usize, deadline: Duration }` and
      `static BIG: Semaphore = Semaphore::const_new(2)`. `mod.rs`'s
      `limit_for(&self, head)` checks the method, the path after the
      base, and `self.sessions.check(cookie, host, revoked_ms).is_some()`,
      the same check the API gate already makes.
    - **The page**: an attach button (the image icon) and a camera button
      that appears on touch devices (`<input type="file" accept="image/*"
      capture="environment">`). There's also a hidden plain picker for
      desktop. Before sending, it shows the preview with an ✕. The shrink
      is D6. Progress text reads "Sending photo…", and a Cancel uses
      `AbortController`.
22. **Tests** (new `crates/ferrule-cli/tests/it/vision.rs`, registered in
    `tests/it/main.rs`). Helpers from `dashboard.rs` are reused: `home`,
    `gateway`, `login`, `Page`, `FakeTelegram`.
    - **The mock `Server`** is extended to keep request bodies
      (`bodies()`); existing callers don't change. **`FakeTelegram`** is
      extended to answer `getFile` and serve `/file/bot<token>/<path>`,
      and gains `say_photo(from, file_id, sizes, caption)`.
    - `a_telegram_photo_reaches_a_vision_model_as_pixels`: the model is
      named `gpt-4o` on the chat mock. The request body has an
      `image_url` part whose base64 decodes to the fake's bytes, and the
      file sits under `<workspace>/inbox/telegram/`.
    - `a_text_only_model_gets_the_saved_file_and_the_user_a_notice`: the
      model is `deepseek-chat`. No `image_url` in the body; the note names
      the saved path; Telegram got the "can't see images" notice exactly
      once across two photos.
    - `vision_false_in_config_wins_over_the_name`: `gpt-4o` with
      `vision = false` behaves as text-only.
    - `a_photo_over_the_cap_is_refused_with_a_reason`: every size in the
      fake's `photo[]` is over the cap. The user gets the refusal, and the
      model gets the refused note.
    - `an_album_is_read_once`.
    - `a_page_photo_reaches_the_model` (vision) and
      `a_page_photo_to_a_text_only_model_says_so` (`"sees": false`, and
      the notice is in `said`).
    - `a_page_photo_is_checked`: SVG gives 415; PNG bytes labelled
      `image/jpeg` give 400; 4 MB gives 413; without the CSRF header, 403.
    - `a_big_body_without_a_session_is_refused_unread`: it sends only the
      head with `Content-Length: 5000000` and expects 413 within 2 s,
      without the body being sent.
    - `under_a_prefix_the_photo_path_still_gets_its_limit`:
      `/b/b_test/api/chat/photo`.

#### 5c. Memory, tasks, backup, settings (commit "M47 part 5c — memory, schedule picker, backup, settings")

23. **Memory** (new `crates/ferrule-cli/src/dashboard/memory.rs`):
    - **`GET /api/memory?q=&limit=`** (limit ≤ 100, default 50) calls
      `MemoryStore::recall(q, limit)` when `q` isn't blank, else
      `recent(limit)`. The store is opened per request on
      `spawn_blocking`.
      - Found: `{"available": true, "memories": [{"id": 12, "text": "…", "tags": ["pref"], "at": 1727700000}]}`.
      - With no DB: `{"available": false, "memories": [], "why": "No memories yet. Your bot saves facts when you ask it to remember something."}`.
    - **`POST /api/memory/forget {id, confirm}`**:
      - 409 `{"confirm": "Forget this for good? Your bot won't recall it again."}`;
      - then 200 `{"ok": true, "said": "Forgot it (and 2 older versions)."}`,
        using `forget(id).len() - 1`;
      - 404: "There's no memory 12.";
      - audited through `ctx.hub` as `memory.forget {id, by: "dashboard"}`.
    - **The page**: `sections.memory`, with a search field (debounced
      300 ms), cards (text in `dir="auto"`, tags, a relative date) and
      Forget (`ask`, danger). The empty state explains how memories get
      made.
    - **Tests** (`dashboard_m47.rs`):
      - `memory_lists_searches_and_forgets`: seeded by `ferrule memory
        add` before the gateway starts. The test checks recent, a query
        hit and a query miss, then forget with confirm, gone after;
      - `forgetting_needs_confirm_and_csrf`;
      - `memory_search_calls_no_model`: `[memory]` has an embedder URL
        pointing at a port whose listener counts connections, and the
        count stays 0.
24. **Tasks**:
    - **`POST /api/tasks/preview {kind: "cron"|"once", schedule, timezone}`**
      gives `{"ok": true, "next": [ts, ts, ts]}`.
      - It uses `initial_next_run_at` three times, stepping `now` past
        each result.
      - Error 400: `` "`61 9 * * *` (Asia/Jerusalem): <parser error>" ``.
    - **`POST /api/tasks/add {name, prompt, kind, schedule, timezone, to: "chat"|"owner", model?}`**
      calls `TasksAdmin::add(NewPageTask, by)`, which is new in
      `tasks_admin.rs`, audited as `task.add`, and mirrors `TasksCmd::Add`
      but without `gate`.
      - `to: "chat"` sends the answer to channel `dashboard`, chat
        `owner`. `"owner"` uses `ctx.owner_chat`. The default is owner if
        known, else chat.
      - 200: `{"ok": true, "id": "…", "said": "Added “Morning news”. Next run: Thu 1 Oct, 09:00 (Asia/Jerusalem)."}`.
      - Errors: "Give the task a name.", "Tell the bot what to do in this
        task.", "Pick when it runs.", "The model X isn't connected.", the
        parser text as above, and 400 "Tasks from the page can't run a
        gate command; use `ferrule tasks add --gate` for that." when
        `gate` is present.
    - **The page**: `schedulePicker({kind, schedule, timezone})`, used by
      "New task" and by "Change schedule" (`tasks/schedule`).
      - Presets: every day at, weekdays at, every week on {day} at, every
        month on {1–28} at, every {1,2,3,4,6,8,12} hours, and once on
        {date} at {time}.
      - The timezone defaults to `Intl.DateTimeFormat().resolvedOptions().timeZone`,
        in a searchable select.
      - The preview is live through `tasks/preview`: "Runs every weekday
        at 09:00 (Asia/Jerusalem). Next: Thu 1 Oct 09:00, Fri 2 Oct 09:00,
        Mon 5 Oct 09:00."
      - Raw cron is under Advanced. `describeCron(expr)` covers exactly
        the preset shapes and says "Custom schedule: `…`" otherwise.
    - **Tests** (`dashboard_m47.rs`):
      - `a_task_added_on_the_page_is_listed_and_audited`;
      - `the_preview_matches_the_parser_for_every_preset`: the presets are
        written as their cron strings; the test runs them through
        `/api/tasks/preview` and checks the next run against
        `initial_next_run_at` computed locally;
      - `a_bad_schedule_says_why`;
      - `a_page_task_cannot_carry_a_gate`;
      - `a_once_task_in_the_past_is_refused`.
    - The JS `describeCron` is covered by the browser check, which types
      each preset and compares the words against a fixed table.
25. **Backup** (new `crates/ferrule-cli/src/dashboard/backup_page.rs`):
    - **`POST /api/backup`** gives 202
      `{"ok": true, "said": "Backing up… this takes a few seconds."}`.
      - A second request while one runs gets 409 "A backup is already
        running."
      - The job state is kept in `Ctx.backups: Arc<Mutex<BackupJob>>`.
      - It runs `crate::backup::backup(Some(<data>/backups/<instance>-backup-<stamp>.tar.gz), false)`
        on `spawn_blocking`, then prunes to the newest 3 `*.tar.gz` there.
    - **`GET /api/backups`** returns
      `{"running": false, "error": null, "last": "<backup::doctor_line()>", "files": [{"name": "…", "bytes": 482133, "at": 1727760000}], "secrets": false}`.
    - **`GET /api/backups/download?name=`** returns 200 `application/gzip`
      with `Content-Disposition: attachment; filename="<name>"` and
      `Cache-Control: no-store`, streamed.
      - `http.rs` gains `Response::file(path, mime, disposition)`, written
        with `tokio::io::copy`; the CSP header stays.
      - 400: "That isn't a backup name."; 404: "There's no backup called
        <name>."
    - **`POST /api/backups/delete {name, confirm}`**: 409 `{"confirm":
      "Delete this backup file?"}`, then `{ok, said}`.
    - **In `backup.rs`**: `"backups"` joins `CACHES`, and `backup()`
      takes the out path as today. No behaviour changes for the CLI.
    - **Tests** (`dashboard_m47.rs`):
      - `a_page_backup_downloads_and_has_no_secrets`: the test polls
        `/api/backups` until `running` is false, downloads, un-gzips with
        `flate2` and `tar`, finds `manifest.json` with `"secrets": false`,
        and checks there's no `data/private/` entry;
      - `backup_names_cannot_leave_the_backups_dir`, with `../x.tar.gz`,
        `a/b.tar.gz`, `x.tar.gz%00`, and a symlink pointing out;
      - `only_three_page_backups_are_kept`;
      - `a_backup_does_not_contain_the_last_one`.
    - The existing `tests/it/backup.rs` must stay green.
26. **Settings** (`sections.settings`):
    - **Appearance**: System, Light or Dark, in `localStorage["ferrule-theme"]`
      as `theme.js` already reads it.
    - **Language**: English or עברית, in `localStorage["ferrule-lang"]`.
      `theme.js` sets `lang` and `dir` before paint. `app.js` loads
      `lang-he.js`, a new asset (`HE` map, `window.FERRULE_HE`), before the
      first render when Hebrew is on.
    - **Backup**: Back up now, the list of files with Download and Delete,
      and `last`.
    - **First-run checklist**: Show again.
    - **Emergency stop**: `kill/on`/`kill/off`, as on today's Home, with
      `ask` and a `tip`.
    - **Sign out**: `/api/logout`.
    - **Server side**: `mod.rs` adds `const LANG_HE: &str = include_str!("assets/lang-he.js")`,
      the static route `"/lang-he.js"`, and an `index_for` entry, so it is
      not preloaded, only rewritten for the base (a `<meta
      name="ferrule-lang-he" content="/lang-he.js">` that `app.js` reads).
    - **Tests** (unit, `mod.rs`):
      - `lang_he_is_served_under_a_prefix`;
      - `every_shell_string_has_a_hebrew_version` (regex `t\("([^"]+)"\)`
        over `APP_JS` against the keys in `LANG_HE`);
      - `theme_js_sets_dir_before_paint` (the string check).

### Part 6 — Quality (commit "M47 part 6 — accessibility, speed, screenshots")

27. **The browser check** (`scripts/dashboard_browser_check.mjs`),
    updated and extended. It still needs Node 22 and no packages, and CI
    still runs it on all three OSes.
    - It keeps every existing step, with the selectors moved to the new
      DOM where they changed. `#rail`, `#main`, `.card`, `.alert`,
      `data-notice`, `.x`, `.fallback select`, `#tile-<name>`,
      `.console-line input`, `.complete button`, `.composer textarea`,
      `.bubble` and `pre.out` are all kept on purpose, so the diff stays
      small.
    - New steps:
      - the first-run checklist is shown, then gone after the chat
        answers;
      - Ctrl-K palette navigation;
      - photo attach: it sets a 1×1 PNG on the file input through
        `DataTransfer`, sends, and expects an answer and the `sees` notice
        with the mock;
      - Copy on a bot bubble, checked by reading back the toast;
      - a new task through the picker ("every weekday at 09:00") appears
        in the list;
      - memory search;
      - a backup downloads (it checks that `/api/backups` lists a file);
      - Settings → dark, then Hebrew: `document.documentElement.dir ===
        "rtl"` and the nav label reads "בית".
    - Per section at 390 px:
      - `scrollWidth <= innerWidth`, so there's no horizontal scroll;
      - every visible `button, a, input, select, [role=button]` is at
        least 44 × 44 px (listing offenders, with an allow-list for inline
        text links);
      - every control has an accessible name (text, `aria-label`,
        `aria-labelledby` or `<label for>`);
      - there's exactly one `aria-current="page"` in each of the rail and
        the tab bar.
    - Keyboard-only: from load, Tab reaches the skip link first; Enter
      moves focus to `#main`; the palette opens with `/`.
    - `--measure` prints the transfer sizes (`performance.getEntriesByType("resource")`,
      with `transferSize`, and gzip sizes computed in Node from the
      binary's responses) and the FCP, under 4× CPU throttle at 390 px.
    - `--shots DIR --themes light,dark` saves every section at 390 and
      1280 in both themes as JPEG through CDP
      `Page.captureScreenshot {format: "jpeg", quality: 80}`.
28. **Polling only what's visible.**
    - Health and approvals are polled every 15 s for the badges when Home
      or Chat isn't the current section; the current section polls at its
      `every`. Nothing is polled when `document.hidden`.
    - The chat polls at 1 s while `waiting` and at 4 s otherwise.
    - The browser check counts `/api/` requests over 20 s on Settings (no
      section poll), with an upper bound in the script.
29. **Screen-reader order and roles**, checked in the browser check and by
    reading the accessibility tree once with `agent-browser snapshot`,
    saved as text in the doc:
    - one `h1` for each section's title;
    - `role="status"` on the Home status sentence;
    - `role="log"` on the chat;
    - `aria-live="polite"` on toasts;
    - badges carry visually hidden text ("2 problems");
    - status dots always come with a word.
30. **After screenshots.** Run `--shots docs/assets/m47/after --themes light,dark`,
    which covers the 15 sections (13, plus Memory and Settings) × 4.
    - Every image is looked at. Anything off is fixed, up to three passes
      per section (the frontend-engineer skill's loop), and the problems
      and fixes are logged in the doc.
    - Before and after are compared at both widths, and the numbers
      (sizes, FCP, request count) go into the doc.

### Part 7 — Docs, final checks, PR (commit "M47 part 7 — docs")

31. **Docs.**
    - **`docs/m47-dashboard-redesign.md`**: the audit, the decisions as
      finally built, and "Verified, and how". That covers the test names,
      the browser check output, the screenshots index (one table: section
      × width × theme, before and after), the numbers, and "not verified
      live" (a real phone over the tunnel, real vision providers).
    - **`docs/dashboard.md`**:
      - new screenshots (links into `docs/assets/m47/after/`);
      - "What's on it", rewritten around the new navigation;
      - new sections: first run, photos, memory, the schedule picker,
        backup, settings and language;
      - "Keyboard shortcuts" (`/`, Ctrl/⌘-K, `?`, Esc, Enter/Shift+Enter
        in chat);
      - the browser check's new steps;
      - M37's section kept, with a pointer.
    - **`docs/channels.md`**: Telegram photos, the vision notice, the
      album rule.
    - **`docs/models.md`**: `vision = true|false`, the name list, the note
      that text-only models get the file.
    - **`PLAN.md`**: an "M47 — dashboard redesign" milestone section, plus
      one Session Log entry.
    - **`docs/roadmap.md`**: an M47 row.
    - **README lines** go in the final report only. README.md is not
      touched.
32. **The eval check**: `target/debug/ferrule eval run evals/starter
    --variant ab` against the stdlib mock must still read engineered
    20/20, naive 11/20, $0.98. It's unchanged by design: nothing in
    `evals/` or the mock is touched. The vision changes add `images: []`,
    which is skipped in serialization, so the request bodies the mock sees
    are byte-identical for text turns. Check:
    `openai_compat::a_text_turn_body_is_unchanged` compares against a
    golden body taken before the change.
33. **Merge main, push, PR, CI.**
    - `GIT_SSL_CAINFO=/tmp/onecli-combined-ca.pem git fetch origin && git merge origin/main`.
      On a PLAN.md or roadmap conflict, keep both sides. Then CHECK plus
      the browser check.
    - Push once, with `git push -u origin m47-dashboard-redesign`.
    - Open a non-draft PR with `curl -X POST https://api.github.com/repos/maximarhipkin/ferrule/pulls`
      (no auth header), whose body is the summary and the decisions
      (D13 called out).
    - Poll `…/actions/runs?branch=m47-dashboard-redesign` for up to about
      40 minutes. Fix reds in batches (Linux, macOS, Windows), re-running
      CHECK locally before each push.
    - No merge, no tag, no release.
34. **Clean-up**: only the one `CARGO_TARGET_DIR`; the temp homes from the
    rig are removed; `agent-browser close`.

## Risks, and how each is checked

- **R1 — Vision touches four drivers, two wrappers, the core message
  and history.**
  - A text turn's wire body must not change: this is the golden test in
    step 32, and the eval must still read 20/20, 11/20, $0.98.
  - Old transcripts must still load: `old_transcripts_without_images_still_load`.
  - Fail-over and routing are covered because the per-driver decision is
    tested for each driver, and `Tiered`/`RoutedProvider` forwarding has a
    unit test each.
- **R2 — The upload size, the deadline and the tunnel.**
  - Body limits are tested on both sides (with a session, without one,
    under a prefix).
  - The tunnel itself isn't in CI. The 60 s deadline and the ~500 KB
    typical photo after shrinking are the margin, and "not verified
    live: a real phone over trycloudflare" goes in the report.
- **R3 — The size of the `app.js` rewrite, and losing an ability or the
  RTL guard.**
  - D15's `every_route_is_used_by_the_page` guards the abilities.
  - The existing `dir="auto"` test parses exact source strings: `const
    text = (t) => el("span", { class: "msg", dir: "auto"`, and one-line
    `el(... text: X, dir: "auto" ...)` forms for 14 named fields such as
    `t.text` and `x.detail`. Those forms and variable names are kept.
  - The browser check visits every section with no script errors.
- **R4 — The `docs/assets` rule against the brief's `docs/assets/m47/`.**
  D13: new files only, a budget of ≤ 8 MB, JPEG, stated in the PR and
  the report.
- **R5 — Screenshot tooling in the container.**
  - agent-browser runs through `/pnpm/agent-browser`, and Chromium is at
    `/usr/bin/chromium`.
  - Calls are bounded with `timeout`, with the CDP script as the fallback.
    Both are tried in step 2 before anything depends on them.
- **R6 — Windows CI and the extended browser check.** Photo input through
  `DataTransfer` and clipboard permissions differ in headless Chrome.
  Copy is checked through the toast, not the clipboard, and the file
  input is set through CDP `DOM.setFileInputFiles` if `DataTransfer` is
  flaky.
- **R7 — Hebrew layout.** Logical CSS properties throughout. The RTL
  screenshots of Home, Chat and the tab bar are added to the after set,
  and the browser check asserts `dir="rtl"` and no horizontal scroll in
  Hebrew.


## Corrections made while building

These are places where the plan met the code and the code won. Each is
also in the final report.

1. **Part 1: the screenshot rig.** The plan said to script agent-browser.
   The browser check's CDP rig is already proven in CI and boots the whole
   gateway, so `--shots-all` and `--measure` were added to it instead
   (`scripts/m47_shots.sh` wraps them). One rig to keep working, not two.
2. **Part 2: theme names and tokens.** The doc wrote `[data-theme="dark"]`
   and a new colour vocabulary (`--surface`, `--text`, `--accent`). The page
   already ships `data-theme="paper"|"forge"` with `theme.js`, the
   `ferrule-theme` key and `window.ferruleTheme`, and 400 lines of CSS use
   `--panel`, `--ink`, `--copper`. Renaming them would touch every rule for
   no reader-visible gain, so the **existing names are kept and extended**
   (space, type in rem, weights, radii, shadows, motion, layers, tap size,
   focus). The colour *values* changed where the AA test demanded it: the
   dark `--ink-2`/`--muted` are opaque hex instead of alpha, the light
   `--muted` and `--copper` are darker, `--copper-strong` is the text/link
   copper.
3. **Part 2: the shell is evolved, not rebuilt.** `index.html` already has
   `#top`, `#rail`, `#main`, `#tabs`, `#sheet` and `#toast`, and the browser
   check drives them. They stay, gaining landmarks and a skip link in Part 4.
4. **Part 2: `tip()`** renders as an inline definition that expands under
   the word, not a popover: a popover needs positioning code and fails on a
   320 px screen.
5. **Part 2: no `prefers-color-scheme` mirror.** `theme.js` always sets
   `data-theme`, so the stylesheet needs no media-query copy of the dark
   block.
6. **Part 2: the WCAG test has a sibling.** `every_colour_is_a_token` fails
   if a rule after the token blocks spells a hex or `rgba()`, otherwise a
   hard-coded colour would dodge `tokens_meet_wcag_aa`.
7. **Part 2: the tests live in `dashboard/page_tests.rs`**, next to the
   assets they read, not in `mod.rs`.
8. **Part 3: `GET /api/setup` is its own endpoint.** The plan derived the
   checklist in the page from `/api/health`, `/api/models` and the chat
   log. Three requests, and the "hello" step needs the ledger. One small
   server-side view (`dashboard/setup.rs`) answers it, tested through the
   real binary (`tests/it/dashboard_m47.rs`).
9. **Part 3: steps open in place by reusing the section code.** The plan
   drew a fresh panel per step. The model and Telegram panels borrow
   `sections.models` and `sections.channels` (`Object.create` with a
   different mount point), so the two flows cannot drift apart. Their
   boxes live in `home.boxes` so the 3-second redraw does not wipe a
   half-typed key.
10. **Part 3: Home draws its own notices.** The global banner is skipped
    on Home (`refresh()`), otherwise every problem showed twice. Other
    sections keep the banner.
11. **Part 3: the old `.steps li::before` tick** belonged to the doctor
    list and leaked a tofu glyph onto the checklist's `<ol>`. It is now
    scoped to `ul.steps`.
12. **Part 3: the setup test uses a fresh fake Telegram for its second
    gateway.** One fake shared by two gateways in turn was flaky (the first
    process can still be mid-poll and swallow the update).
13. **Part 4: the palette starts with what exists.** `PALETTE_ACTIONS` holds
    only actions the page can already do (go to a section, switch theme,
    stop the running turn, sign out, the shortcut list). "New task",
    "Attach a photo", "Back up now" and "Switch language" arrive with their
    features in Part 5, so no palette entry ever leads nowhere.
14. **Part 4: `vision` on `/api/models` moved to 5a.** Step 15 wrote the
    field before `Entry::sees_images()` exists. It ships with the provider
    layer, and the Models "sees photos" badge with it.
15. **Part 4: Memory and Settings show in the nav only once they exist.**
    `GROUPS` lists a section only if `sections[name]` is defined, so the
    nav needs no edit when 5c adds them.
16. **Part 4: `#toast` keeps its id.** The plan called it `#toasts`; the
    browser check, the CSS and `toast()` all use `#toast`.
17. **Part 4: the route guard has a short allow-list.** `console/parity`,
    `eval/estimate` and `run/cancel` are served but the page never called
    them (each has its reason in `NOT_CALLED_BY_NAME`). Routes that end in
    a name (`telegram/<verb>`) count as used when the page builds
    `"/api/telegram/"` plus the verb. Paths in `app.js` must therefore be
    literal strings, not `base + "/enable"`; a first draft of the
    extensions switches was caught by exactly this test.
18. **Part 4: the problems banner is per section.** With the whole list on
    every page, the two mock-model notices filled a 390 px screen before
    the section began. Home still lists every problem. Elsewhere the
    section's own problems show in full and the rest are one line that
    leads to Home (`otherProblems`).
19. **Part 4: Models, Channels and Connections keep their Part 2/3 layout.**
    They already have tiles, per-option folds (`.opt`) and the step panels
    that Home reuses (correction 9). Re-wrapping them in an "Advanced"
    fold would have made the Home step panels and the browser check's
    `details[open] input[name="key"]` path deeper for no gain, so only
    Usage, Tasks, Logs, Extensions and Agents were rewritten with the new
    `advanced()`, `chips()` and `toggle()` pieces. The Logs "kinds" select
    became three chips (Everything, Problems, Changes).
