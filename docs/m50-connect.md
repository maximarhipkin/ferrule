# M50 — Connect everything from the dashboard

Status: **built** on branch `m50-connect`, one commit per part (parts 0–7,
plus an OpenRouter fix the walk turned up). "Corrections made while
building" says where the code corrected this plan, and "Verified, and how"
what was run. Decisions: [below](#decisions-max-2026-10-19).

## Why

Max: *"check the agent dashboard, suggest improvements, maybe design the
dashboard new. I need to make the agent easy to use and to connect to
everything, that he will connect to everything with ease."*

M47 made the page friendly and M49 made it look right. Both kept the
page's map: one page per kind of thing. This milestone is about the one
job a new owner has on day one: connect the bot to a brain, to the chat
apps they use, and to their mail, calendar and tools. Today that job is
spread over five pages, two of which are for experts, and several paths
end in "run `ferrule setup` on the machine".

## How the review was done

A fresh install with an empty config and data dir (no key, no Telegram),
served by `ferrule dashboard` from this branch's base (`0c69807`), walked
by hand in a browser at 800 and 375 px, light and dark. The configured
state comes from the existing rig (`scripts/m49_shots.sh`, 28 of 28 checks
passed), at 390 and 1280 px, light and dark. The shots are in
[assets/m50/before](assets/m50/before/): `walk-*` for the empty install,
the rest from the rig.

## What a first-time owner meets

Steps are counted on the page; "outside" is time on another site.

| Path | Steps on the page | Where you get stuck |
|---|---|---|
| **Open the dashboard** | – | `ferrule dashboard` on a machine with no config stops with "no config found. Run `ferrule setup` first." (`config.rs:1738`). The page can't be the first thing you see: a terminal wizard with 15 headings comes first (`setup.rs` `guided`). |
| **A brain (model)** | Home → **Set up** → pick one of 10 cards → **Get a key** (outside) → paste → **Test and save**: 3 clicks and a paste | Ten cards open at once, each with a key field, and nothing says which to pick ([walk-02](assets/m50/before/walk-02-model-step-800.webp)). The no-key choice (ChatGPT plan) is ninth. A wrong key's message is a toast at the bottom of the screen, the field is emptied, and the toast follows you to other pages until closed ([walk-03](assets/m50/before/walk-03-bad-key-toast-800.webp)). The Claude plan needs `claude setup-token` in a terminal. |
| **Telegram** | Channels → paste token → **Test** → **Save** → **Wait for a message** → message the bot (outside) → **Allow** → **Restart**: 6 clicks | The card says "Bot token (from @BotFather)" and nothing about how to make a bot; every other channel has a "How to set it up" fold. **Test fails with "builder error"** on any config without a `[gateway]` table ([walk-06](assets/m50/before/walk-06-telegram-builder-error-800.webp)): `GatewayConfig` derives `Default`, so `telegram_base_url` is empty and the URL has no host (`config.rs:484`, `dashboard/telegram.rs:32`). |
| **Discord, Slack** | none | "Run `ferrule setup` on the machine to add it." (`app.js:2483`). |
| **Gmail** | Connections (More → Connections on a phone) → scroll past the callback-address card and a list of four "missing/unknown" rows → Gmail tile → open the option → follow 4 steps on 2 Google pages (outside) → 2 fields → **Check and connect**: about 5 clicks | The page opens on "Fixed callback address" and a Cloudflare token form ([walk-07](assets/m50/before/walk-07-connections-relay-first-800.webp), [connections-1280-light](assets/m50/before/connections-1280-light.webp)), which no key-based way in needs. The steps' addresses are plain text, not links ([walk-08](assets/m50/before/walk-08-gmail-800.webp)): on a phone you copy them by hand. |
| **Calendar** | – | Only through a Google service account (Cloud Console, JSON key) or your own OAuth client plus the relay. No simple path. |
| **An MCP server** | none that a non-coder would find | Extensions (under Advanced) lists servers but has no **Add**. The only page path is typing `mcp add <name> --url … -y` into the Console. The server side already signs in to *any* MCP address (`Catalog::resolve`, `ferrule-connections/src/catalog.rs:455`); the page has no box for one. |
| **A key for a tool** (`[secrets]`) | none | Nothing on the page adds one. The Config page shows `[secrets]` read-only (`config_page.rs:22`). `mcp add --secret` reads the value from a prompt the Console doesn't have. |

Other findings on the way:

1. **Home says "Your bot is running"** with no model, and with no gateway
   in the process (`app.js` `status()`, line 758). It can't answer anyone.
2. **The no-model notice tells you to run `ferrule setup`**
   (`models.rs:393`) right above a checklist that sets it up here.
3. **Five places to connect things**: Models, Channels, Connections,
   Extensions (Advanced) and Config (Advanced). On a phone all five are
   behind More ([walk-10](assets/m50/before/walk-10-more-sheet-375-dark.webp)).
4. **Everything is open at once.** Channels draws nine cards and over
   thirty fields; the empty Models page shows Default, Fallback, Pins,
   Evaluate and Catalog before the one thing it needs, and that one thing
   ("Connect another provider") is a closed fold
   ([walk-04](assets/m50/before/walk-04-models-empty-800.webp)).
5. **"More in docs/channels.md#telegram"** is plain text pointing at a
   file the owner doesn't have.
6. **Words**: callback address, relay, OAuth, MCP, service account, Rovo,
   webhook, allow-list. M47's glossary explains some when tapped; the
   connect forms use more than it covers.
7. **Gmail as a service and Email as a channel** both ask for a Gmail app
   password, on two pages that don't mention each other.
8. Carried over from the review that started this session: the sidebar's
   "12 h max · idle 30 min" is hard-coded (`app.js:2964`); `http::read`'s
   413/503/408 answer with the plain text "bad request"
   (`dashboard/mod.rs:343`), which the page shows as "HTTP 413"; **Pin a
   chat** assumes Telegram.

What works and stays: keys are tested before they're saved and never come
back; the Telegram wait-and-allow flow; the Gmail steps themselves; the
restart button after a channel change; every security rule below.

## The design

### Goals

1. One place, **Connect**, where an owner sees everything the bot can be
   connected to, what's connected, and what needs them.
2. Every "connect" follows one pattern: what it gives the bot, what you
   need, numbered steps with real links, the fields, one **Connect**
   button, and the answer next to the button.
3. No path on the page ends in "run `ferrule setup`" for something the
   page could do.
4. The bot is never called "running" when it can't answer.
5. Phone first: the whole flow works at 390 px with one thumb.

### Non-goals

- A new visual system (M49's stays), new fonts, a framework, a build step.
- Rewriting the existing forms. They are drawn inside the new panel.
- Translating server sentences into Hebrew (M47 D11 still holds).
- A marketplace or remote catalog: the list is what the binary knows.

### Anti-goals (a failure even if it works)

- Any form that runs a command, takes a path, or sends an existing secret
  somewhere new.
- A key or token that comes back to the page, a log or the model.
- A page that calls a model to draw itself.

### Information architecture

**Connect** (`#connect`) is a new page and the first item of the Setup
group. The existing pages stay, with their `#links`, for managing what is
connected (fallbacks, pins, catalog, allow-lists, disconnect, enable/
disable). Connect is where you *add* and *see state*.

On a phone the bottom bar becomes **Home · Chat · Connect · Tasks · More**;
Usage moves into More's Everyday group. (Decision 2.)

The Connect page, top to bottom:

1. **A search box**: "Gmail", "calendar", "slack", "github" find the tile.
2. **Its brain**: the default model with its state, or, when there is
   none, three recommended choices and "More providers". The three, my
   call: **ChatGPT plan** (sign in, no key), **OpenRouter** (one key,
   hundreds of models), **Anthropic** (Claude). Ollama and the rest sit
   under "More providers".
3. **Where you talk to it**: a tile per channel (Telegram, WhatsApp,
   Email, Discord, Slack, Signal, Matrix, Mattermost, HTTP API), each with
   one word of state: *on*, *off*, *restart needed*, *needs you*.
4. **What it can use**: a tile per service from the catalog (Gmail,
   Google Drive/Calendar, GitHub, Notion, Linear, Jira, Stripe, …), then
   two tiles for anything else:
   - **Any app by its address** (an MCP server's URL): sign in, or paste
     a key.
   - **A key for a tool**: a name, the key, and the sites it may be sent
     to.
5. **Connected**, a short list with **Test**, linking to the page that
   manages each.

Tiles are buttons. State is a word and an icon, never colour alone.

### The connect panel

Pressing a tile opens one panel: a `<dialog>` on desktop, full-screen on a
phone, focus trapped, Esc and Back close it (M47 D4's dialog).

```
┌ Gmail ──────────────────────────────────── × ┐
│ Your bot can search and read your mail, and  │
│ send when you allow it.                      │
│ You need: a Google account with 2-Step       │
│ Verification · about 3 minutes               │
│                                              │
│ 1 Turn on 2-Step Verification ↗              │
│ 2 Make an app password ↗, name it ferrule    │
│ 3 Paste the 16 letters below                 │
│                                              │
│ Gmail address  [                         ]   │
│ App password   [                         ]   │
│ ☐ Allow it to send and change things         │
│                                              │
│ [ Connect ]                                  │
│ ✓ Connected. Try: "what's new in my inbox?"  │
└──────────────────────────────────────────────┘
```

- The body is the existing form for that thing (provider card, channel
  card, Telegram walk-through, connection option). The panel adds the
  header, "You need", the result line and focus handling.
- Links in steps are real links (`target=_blank`, `rel=noopener
  noreferrer`). Addresses already in guide text become links.
- **The answer sits under the button** (`role=status`; `role=alert` for a
  failure), not in a toast. A failure names the field, puts focus on it
  and keeps what was typed. Toasts stay for things that happen in the
  background, and a failure toast closes when you change page. A key
  field is still emptied once sent (see Error recovery).
- **Success says what to try next**: one example message for the bot, or,
  for a channel, "Restart to turn it on" with the button.
- A channel's or service's "How to set it up" is open by default in the
  panel (it's closed in a list of nine today, which is why it was a fold).
- Where Gmail-as-service and Email-as-channel overlap, each panel says so
  in one line with a link to the other.

### Empty states and the first run

- Home's checklist steps open the Connect panel for that step instead of
  growing the card in place.
- Home's status line: "Your bot can't answer yet: it has no brain" with
  **Connect a brain**, when no model can be called; "The page is running
  without the gateway, so no chat app reaches the bot" when there's no
  gateway in the process.
- The no-model notice points at Connect, not at `ferrule setup`.
- Models with no provider: one card, "No brain yet", **Connect a brain**.
  Default, Fallback, Pins, Evaluate and Catalog appear once there is a
  model.
- Extensions with no server: **Add a tool by its address**.
- Connections: services first. The callback-address card moves to the
  end, under "For sign-ins that need a fixed address", and the panel of a
  service that needs it says so with a link. "What each service still
  needs" lists only services you started.

### Error recovery

- Every error a form can return is a sentence: what happened and what to
  do. Transport errors become "Couldn't reach api.telegram.org: …" (the
  reqwest text without its URL, as `probe::transport` already does).
- `http::read`'s refusals reach the page as themselves: 413 "That's too
  big", 503 "Another upload is in progress", 408 "That took too long"
  instead of "HTTP 413".
- A failed check keeps the non-secret values. A key is still wiped once
  sent, as the page's rule says ("typed in, sent, wiped", `app.js` top);
  the error says "paste it again" and focus goes back to the key field.
- Text that tells the owner to run `ferrule setup` is replaced, where the
  page can do the thing, with where on the page it is.
- "More in docs/…" becomes a link to that doc on GitHub at the running
  version's tag.

### New abilities (all four agreed: Decision 1)

**A. Any app by its address, with a sign-in.** A URL box in the "Any app"
panel calls the existing `POST connections/connect` with the URL as the
service. `Catalog::resolve` already builds a service from a URL
(`Service::from_url`: OAuth by discovery and DCR, HTTPS only except
loopback); the page only gains the box.

**B. Any app by its address, with a key.** Address, key and the header
(default `Authorization: Bearer {key}`). This is new: the Config page
can't add a `[[connections.custom]]` entry (its guard refuses any key
ending in `url`, `config_page.rs:61`), and the running catalog is built
once at start (`ferrule-connections/src/service.rs:350`), so a new entry
there couldn't be connected before a restart. It doesn't need either: a
connection's stored record holds its whole `Service`
(`store.rs` `Record`), which is what the tools are built from, as
connections to bare URLs (A) already are. So B builds the `Service` from
the address (`Service::from_url`, then `auth = "api_key"` and the
header), and `connect_key`'s checks run on it as they do for a catalog
service: the key is tried once against the server, then sealed in the
connections store (not `secrets.env`) and sent only to that address's
host. Refused: a name already connected, a header name that isn't a
plain token, a value template without `{key}` or with control
characters. Nothing is written to the config.

**C. A key for a tool** (a new `[secrets]` entry). Name (`A_TO_Z_0_9`),
the key, and the hosts it may be sent to. The server:

- refuses a name that already means something: in the secrets file, in
  `[secrets]`, named by any `*_env` in the config (a provider's, a
  channel's, an MCP server's), or **set in this process's environment**:
  the environment beats the secrets file (`secrets.rs:64`,
  `config_follow.rs:62`), so such a name would send the existing value to
  the new hosts. Nothing existing can move;
- is refused outright when the page isn't served by the running gateway
  (a standalone `ferrule dashboard` can't see the gateway's environment);
- refuses hosts that aren't plain DNS names or `*.example.com`; refuses
  bare `*`, IP literals and loopback;
- writes the value to `private/secrets.env` and the hosts to `[secrets]`,
  audits it, and returns only the name;
- the running gateway follows `[secrets]` already, no restart.

The Config page's guard stays as it is: `secrets` is still a section the
raw editor can't change. **Remove** on a row deletes both halves, only
for a name in `[secrets]` whose value is in the secrets file and that no
`*_env` in the config names; anything else says it's managed elsewhere.

**Managed mode** (my call): A and B are "installing MCP servers", so the
policy's `extensions` lock covers them, as it covers `ferrule mcp add`
(`main.rs:812`). C is for commands, so the `shell` lock covers it.
Locked controls are disabled with the policy's reason; the server
refuses too.

**D. Discord and Slack on the page** (Decision 4: now). Token(s), allowed
users or channels, **Test**, **Save and turn on**, with the same
write-only rules as the other cards. Their settings sit directly in
`[gateway]` (`discord_token_env`, `slack_bot_token_env`, … at
`config.rs:510`), not in a `[gateway.<name>]` table. My call: teach the
card save a flat-key variant instead of moving their settings to tables,
so nobody's config changes shape. Their tests reuse the probes
`ferrule setup` already calls.

**E. Set up in the browser, no terminal** (Decision 3: its own next
milestone). `ferrule dashboard` on a machine with no config would write a
header-only config and serve the page, which walks brain → chat app →
hello; keeping it running after a reboot needs a **Keep it running**
button that installs the user service. Not built here.

**Not in M50: a simpler Calendar.** Calendar stays a service account or
your own OAuth client. A one-click Google sign-in needs a verified Google
app that ferrule doesn't have; that's a follow-up of its own.

### Security model: what stays exactly as it is

- Origin, JSON content type and `X-Ferrule-Csrf` on every POST; the
  SameSite=Strict session cookie (unchanged `http.rs`, `auth.rs`).
- The Config page's guard (`config_page.rs` `WHOLE`, `guarded_key`): not
  loosened. New writes go through purpose-built endpoints with their own
  narrow rules, as the channel cards and connection keys already do.
- Secrets are write-only: a key goes in; only "saved", a name and a
  test's outcome come back. Every new response goes through
  `redact_value` like all `/api/*`.
- The sandbox stays on; nothing new runs a command. MCP by address is
  URL-only; a stdio server stays on the CLI and the self-extension
  approval flow.
- Managed mode: each new control checks `managed::policy()` and is
  disabled with the policy's reason, as Channels does.
- Nothing on the page calls a model; the "try this" line is static text.

## Plan (one commit per part, `make check` green at each)

0. **This doc** and the before shots.
1. **Fixes from the walk**: the gateway's API URLs (Telegram, Discord,
   Slack) when `[gateway]` is missing; truthful Home status; the no-model
   notice; docs links; addresses in guides as links; failure toasts close
   on page change; `http::read` refusals as sentences; the session footer
   from the config.
2. **The connect panel**: the dialog wrapper, inline result line, focus
   on the failing field (non-secret values kept); used first by Home's
   checklist and the model step (recommended three + more).
3. **Connect page and navigation**: `#connect`, search, the three groups
   built from existing endpoints (`/api/models`, `/api/channels`,
   `/api/connections`, `/api/settings`) plus whatever small field is
   missing; the Setup group and the phone bar (Decision 2). The D15 route
   guard and the browser check cover it.
4. **Calmer pages**: Models, Extensions and Connections empty states and
   order (services first, relay last); Telegram's how-to; the Email ⇄
   Gmail cross-links; **Pin a chat** gets a channel picker.
5. **New abilities A–C**, with tests for their security rules only
   (Decision 5): a taken or environment name is refused, a key never
   comes back, the host rules hold, CSRF and the managed locks apply, the
   Config guard is unchanged.
6. **Discord and Slack cards.**
7. **After shots** (a hand walk of the empty install, like the before one,
   plus the rig's set), docs
   (`dashboard.md`, `channels.md`, `connections.md`, roadmap, README
   link), PLAN.md.

## Decisions (Max, 2026-10-19)

1. **New abilities**: all four — A, B, C and D.
2. **Phone bar**: Connect takes Usage's place; Usage moves to More.
3. **Set up with no terminal (E)**: its own next milestone.
4. **Discord and Slack (D)**: now.
5. **Tests**: only for the security rules of the new write paths; the
   rest is held by the existing suite and the browser check.

These were asked before a second read of this doc against the code found
that B is new work rather than a form for something the Config page
already did. The answer stands: B reaches no further than A (an MCP
address the owner types) plus a key that goes only to that address.

## What's left to the builder

- The exact copy of each panel's "You need" and "Try" lines.
- Whether Connect composes its lists in the browser from the existing
  endpoints or gets one small `GET /api/connect`; whichever needs fewer
  new lines, provided nothing on it calls a model.
- Icons from the existing Lucide set.
- Hebrew strings for the new page words (`lang-he.js`), as the M47 test
  requires.

## Verified, and how

- `make check` (fmt, clippy, the whole suite) green before every commit:
  1841 tests at the first run, 1844 at the end (the three below), 0
  failed, 28 ignored. New tests
  cover only the new write paths' rules (Decision 5): taken and
  environment names and the host rule for keys for tools, a key that
  never comes back, an address connected once, the managed locks, and
  CSRF on the three new routes. The last full run had one failure,
  `health::status_answers_from_any_chat_while_a_turn_hangs` (its mock
  model server's read panicked under load); it passed alone three times
  and in a rerun of the whole integration target, and nothing here
  touches it.
- The browser check (`scripts/dashboard_browser_check.mjs`) passed after
  every part, 27 of 27; its checklist step now opens the connect panel.
- Walked by hand on an empty install (`ferrule dashboard`, no key, no
  channel) in the built-in browser at 800 and 375 px, light and dark:
  Home's status; Telegram's Test with a made-up token ("Telegram rejected
  the token (Unauthorized)", was "builder error"); OpenRouter with a
  made-up key (refused in its card, focus back on the key; was "key works
  (470 models)"); WhatsApp's Save with a field missing (answered in the
  panel, focus on the field); Discord's Test (Discord's own 401 sentence),
  Save (flat `discord_*` keys in `[gateway]`, the token in the secrets
  file) and Remove (both gone); Slack's Test (`invalid_auth`); "calendar"
  in Connect's search finds Google's tile; the address and tool-key
  panels, the latter refusing to add without the gateway.
- Before and after shots: [assets/m50/before](assets/m50/before/) and
  [assets/m50/after](assets/m50/after/), the `walk-*` ones by hand, the
  rest from `scripts/m49_shots.sh`'s rig.

**Not verified live**: a real Discord or Slack app, a real MCP server with
a key or a sign-in, the ChatGPT plan's sign-in from the panel, a phone's
own browser, and the panels in Hebrew (their words are English, like the
other sections' content; the nav labels are translated).

## Corrections made while building

1. **B was new work** (the builder check, before any code): the Config
   page guards `url`, keyed connections are sealed in the connections
   store, and the catalog is built once. B builds the service and connects
   it directly instead, and the doc says so.
2. **C's taken names include the environment** (same check): it beats the
   secrets file, so a name set there would send that value. A page served
   without the gateway can't see the gateway's environment and adds
   nothing.
3. **OpenRouter took any key.** Its `/models` answers anyone, so a made-up
   key "worked" and the bot failed at its first message; `probe::models`
   now also asks `/key`. Its preset has no model, so the page connected
   whatever came first in the list; it now starts on the first everyday
   pick of `recommended.toml` the list offers. Found walking the brain
   panel; committed on its own.
4. **The answer sits in the pressed card**, not at the panel's end, which
   was a screen away from the recommended cards' buttons; provider cards
   redraw only on success so an error stays in view.
5. **Connect has an icon of its own** (Lucide's plug): the page test wants
   each section drawn with the icon of its name.
6. **The empty-install rig pass** became a hand walk at the before walk's
   sizes, so before and after compare like for like; the rig's own set is
   still taken.
7. **The managed lock checks come first** on the address routes: a
   managed bot may have no connections set up, and the lock must still
   answer 403, not 404.
8. **Discord and Slack on the page** take tokens and ids; pairing by a DM
   code and Discord's slash commands stay with `ferrule setup` (a Test
   doesn't change Discord). Their cards link to `discord.md`/`slack.md`.
9. **A pre-existing race**: `managed::sigterm_drains_and_exits_zero` read
   the dashboard marker the moment the page answered; the gateway writes
   it just after. The test waits for it now.
