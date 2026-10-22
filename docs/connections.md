# Connections

Connections give your agent tools in other services: Jira and Confluence,
Gmail, Drive, Sheets, Docs, Calendar, GitHub, Linear and the rest of the
catalog (`ferrule connections catalog`). They're all set up in one place:
the dashboard's **Connections** section. `ferrule connections setup` does
the same from a terminal.

A few rules hold for every connection:

- **Read-only unless you say otherwise.** Each way in has an "allow
  changes" box. A write (sending mail, creating an issue) still asks you
  first, every time.
- **Keys are write-only.** A token, password or JSON key typed on the page
  is tested against the service, sealed on the server, and never shown
  again: not on the page, not in the API, not to the model. Only the
  service's own hosts ever receive it.
- **Every key is tested before it's saved.** If it doesn't work, the page
  says what failed ("Atlassian refused the email and token pair…"). You
  won't see a code, a `state` or the key in that message.
- **No button is shown if it can't work.** Suppose a way in needs
  something that isn't there yet, like the fixed callback address or your
  own Google client. Then its tile says what's missing and offers the
  simpler way in, not a Sign in button. `/connect` in your owner chat
  follows the same rule.

## Which option?

| you want | take | needs | callback address? |
|---|---|---|---|
| Jira issues and Confluence pages | **Atlassian → Email + API token** | a token from id.atlassian.com | no |
| Atlassian's own MCP tools (incl. Compass) | Atlassian → Rovo MCP with an API token | the admin to allow API tokens for Rovo MCP | no |
| the same, signed in in the browser | Atlassian → Rovo MCP with Atlassian sign-in | the relay, plus the admin to list its domain | **yes** |
| Gmail: search, read, send | **Google → Gmail with an app password** | 2-Step Verification on the account | no |
| iCloud, Fastmail or Yahoo mail: search, read, send | **Mail → the provider's tile option** | an app password from the provider | no |
| any other mailbox over IMAP and SMTP | Mail → Any other mailbox | its servers, and an app password if it has them | no |
| iCloud, Fastmail, Nextcloud or any CalDAV calendar: read and change events | **Calendar → the provider's option** | an app password | no |
| a Google calendar (or any published one), read-only, no Cloud project | **Calendar → A calendar's secret address** | the calendar's secret iCal address | no |
| Drive, Sheets, Docs, Calendar (what you share) | **Google → service account** | a Google Cloud project and a JSON key | no |
| Drive, Sheets, Docs, Calendar as you (everything you see) | Google → Advanced: your own OAuth app | the relay, plus an OAuth client in Google Cloud | **yes** |
| Google's own Gmail/Drive MCP servers | Google → Preview | the above, plus preview enrolment | **yes** |
| GitHub, Linear, Stripe, Hugging Face… | the tile's key option, or Sign in | a token from the service | only for Sign in |

The bold rows are the simplest ways in, and each is on its own tile's
first line. A key-based way in never needs the relay.

## The page, top to bottom

1. **Fixed callback address.** This is the relay card, explained [below](#the-relay).
   Once it's set up, it shows `<relay>/cb` with a copy button, and which
   callback each sign-in service uses.
2. **The checklist.** It's built from real checks: is the relay answering,
   is Google's OAuth client saved, did the preview work, where Atlassian
   stands. A line that isn't ready has a button for its next step.
3. **Sign-ins waiting.** These are flows you started but didn't finish.
   Each has **Cancel**. They also expire by themselves after 15 minutes.
4. **Connected.** Each connection shows its state, whether it can write,
   its tool count, when it was connected and when its key expires. It has
   **Test**, which makes one real call and says what came back, and
   **Disconnect**. A connection that stopped working also gets **Fix it**.
5. **Tiles.** Each tile holds one service's ways in, simplest first. Each
   way in lists what it covers, a step-by-step guide that fits a phone
   screen, and its form or button. The Google and Atlassian sign-ins also
   have **Saw an error on their page?**: the messages those pages show, in
   their words, and what to do about each.

**Doctor** (`ferrule doctor`, and the Home banner) names a connection that
stopped, or one whose key expires within a week. It comes with a button
that opens its tile.

## Atlassian (Jira and Confluence)

### 1. Email + API token (simplest)

It works on any site and needs no admin switch. It uses your own Jira
permissions.

1. Open <https://id.atlassian.com/manage-profile/security/api-tokens>,
   signed in as you.
2. Press **Create API token**, name it `ferrule`, pick how long it lasts
   (up to a year), and copy it.
3. On the tile, enter:
   - your site (the address you open Jira at, like `acme.atlassian.net`);
   - the email you sign in with;
   - the token;
   - optionally, the token's end date, so ferrule warns you a week ahead.
4. **Check and connect.** ferrule calls `/rest/api/3/myself` on your site
   with it, and saves it only if that works.

This way in covers Jira issues (search, read, create, update, comment,
move) and Confluence pages (search, read).

### 2. Rovo MCP with an API token

This is Atlassian's own MCP server, with the tools as Atlassian ships them.

1. Your Atlassian admin turns on API tokens: admin.atlassian.com → Rovo →
   Rovo MCP server → Authentication. It's off by default.
2. Make a token as above. A scoped token works too.
3. Enter your email and the token. For a service account's key, leave the
   email empty.

A 401 here usually means the admin switch in step 1 is still off. The page
says so and offers option 1.

### 3. Rovo MCP with Atlassian sign-in

1. Set up the [relay](#the-relay) first.
2. Your admin adds the relay's domain under admin.atlassian.com → Rovo →
   Rovo MCP server → **Domains**. Use the exact pattern the tile shows:
   `https://<your-relay-host>/**`.
3. Press **Sign in** and approve in the browser.

If Atlassian refuses the domain, the page names the pattern to add. It
also shows **Use an API token instead**, which switches you to option 1.

## Google

Three ways in; each covers something different:

| way in | covers | doesn't cover |
|---|---|---|
| Gmail with an app password | Gmail: search and read; send (a write, asks you first) | Drive, Sheets, Docs, Calendar |
| Service account | Drive search and read, Sheets read and write, Docs read, Calendar list and create, **for what you share with it** | Gmail |
| Advanced: your own OAuth app | the same tools as the service account, **as you** (no sharing needed) | Gmail (use the app password) |

### Gmail with an app password

1. Turn on 2-Step Verification:
   <https://myaccount.google.com/signinoptions/twosv>
2. Open <https://myaccount.google.com/apppasswords>, name it `ferrule`,
   and press **Create**.
3. Paste the 16 letters with your Gmail address. Spaces don't matter.

ferrule logs in over IMAP to test it before saving. Sending goes over SMTP
and always asks you first.

On a work account, the app-passwords page may say they aren't available.
That means the admin turned them off, so use the OAuth option.

### Service account (Drive, Sheets, Docs, Calendar)

1. Make a project (or pick one):
   <https://console.cloud.google.com/projectcreate>
2. Turn on the APIs: [Drive, Sheets, Docs and Calendar](https://console.cloud.google.com/flows/enableapi?apiid=drive.googleapis.com,sheets.googleapis.com,docs.googleapis.com,calendar-json.googleapis.com).
3. Create the service account at
   <https://console.cloud.google.com/iam-admin/serviceaccounts>. Name it
   `ferrule` and skip the roles.
4. Open it → **Keys** → **Add key** → **Create new key** → **JSON**. A file
   downloads.
5. Paste the whole file into the tile. ferrule exchanges it for a token
   and lists Drive to test it.
6. Share the files, folders, sheets and calendars ferrule should see with
   the service account's email, as you would with a person.

Work organisations created after May 2024 may block key creation
(`iam.disableServiceAccountKeyCreation`). An admin can allow it;
otherwise use the OAuth option.

### Advanced: your own OAuth app

Order of operations:

1. Set up the [relay](#the-relay).
2. In Google Cloud, go to
   <https://console.cloud.google.com/auth/overview> → **Get started**.
   Choose audience **External**, and give your email as the support and
   contact address.
3. Turn on the APIs (the link in the service-account guide).
4. **Clients** → **Create client** → **Web application**. Under
   **Authorized redirect URIs**, add the relay's `/cb` address exactly as
   the page shows it, with no trailing slash.
5. Paste the client id and secret into the checklist's **Google OAuth
   client** line (`connections/google-client`, write-only).
6. **Audience** → **Publish app.** An app left in Testing stops signing in
   after 7 days. A personal app doesn't need verification: on the warning
   page, press **Advanced** → **Go to (your app)**.
7. Press **Sign in** on the tile.

These are the errors people hit, and what the page tells you for each:

- `redirect_uri_mismatch`: the `/cb` address isn't on the client's list.
- Access blocked or `access_denied`: the app is in Testing and your
  account isn't a test user. Add yourself, or publish the app.
- Stops after a week: the app is still in Testing.
- "API has not been used": turn the API on.
- "Your administrator has blocked this app": a Workspace admin must trust
  it, or use a service account.

## Mail from any provider (M63.2)

The **Mail** tile connects a mailbox over IMAP (search, read) and SMTP
(send, a write that asks you first), the same way as Gmail's app
password, for any provider:

| option | servers (fixed by the catalog) | the password |
|---|---|---|
| iCloud Mail | `imap.mail.me.com:993` (TLS), `smtp.mail.me.com:587` (STARTTLS) | an app-specific password: <https://account.apple.com> → Sign-In and Security → App-Specific Passwords (needs two-factor authentication) |
| Fastmail | `imap.fastmail.com:993`, `smtp.fastmail.com:465` (TLS) | an app password: Settings → Privacy & Security → Manage app passwords and access (not on the Basic plan) |
| Yahoo Mail | `imap.mail.yahoo.com:993`, `smtp.mail.yahoo.com:465` (TLS) | an app password: Account Security → Create app password |
| Any other mailbox | the IMAP and SMTP servers you type, ports 993 and 465 unless you say otherwise | an app password if the provider has them |

- **Encrypted, always.** Ports 143 and 587 use STARTTLS, every other
  port TLS from the first byte; there is no unencrypted choice. A server
  that doesn't offer STARTTLS on 143 or 587 is refused with that reason.
- **Bound to its two servers.** The password is tested by an IMAP login
  before it's saved, sealed in the connection store like every key on
  this page, and only ever sent to the IMAP and SMTP hosts it was saved
  with (a preset's come from the catalog, not from the form).
- **The tools.** `mail_search` (from, to, subject, text, since, before,
  unseen, in a mailbox, INBOX by default), `mail_read`, `mail_folders`,
  and on a connection with "allow changes" `mail_send`. What they read is
  outside content: a write after it asks you first, even in owner mode
  (M54.5). Gmail keeps its own tools and Gmail's search syntax.
- **Outlook.com and Hotmail aren't offered.** Microsoft lists only OAuth2
  ("Modern Auth") for their IMAP and SMTP, so an app password can't sign
  in ([Microsoft's settings
  page](https://support.microsoft.com/en-us/office/pop-imap-and-smtp-settings-for-outlook-com-d088b986-291d-42b8-9564-9c414e2aa040)).

## Calendars over CalDAV, and a secret address (M63.3)

The **Calendar** tile reaches calendars without a Google Cloud project:

| option | address | the password |
|---|---|---|
| iCloud Calendar | `https://caldav.icloud.com/` (fixed) | an app-specific password, with your Apple Account email |
| Fastmail Calendar | `https://caldav.fastmail.com/` (fixed) | an app password with calendar access, with your Fastmail address |
| Nextcloud | yours, like `https://cloud.example.com/remote.php/dav` | an app password (Settings → Security) |
| Any other CalDAV server | its CalDAV address (`/.well-known/caldav` is tried) | its password, an app password if it has them |
| A calendar's secret address | Google Calendar's "Secret address in iCal format", or any `.ics`/`webcal://` address | the address itself is the secret |

- **CalDAV tools.** `calendars_list`; `events_list` (a calendar, from and
  to dates, a week by default, the server working out repeats); and on a
  connection with "allow changes" `event_create`, `event_update` and
  `event_delete`, each a write that asks you first. A start can be a day
  (all day), a time (that time wherever you are) or a time with its offset
  (`2026-10-22T09:00+03:00`). Changing or deleting a repeating event changes
  the whole series.
- **The secret address is read-only**: `events_list` only, the common
  repeats (daily, weekly, monthly on a day or a weekday like the third
  Tuesday, yearly) worked out by ferrule; a rarer rule is shown once, with
  the rule. The address is a credential (anyone with it reads the
  calendar), so it's a secret field: sealed, never shown again, hidden from
  tool results like a password. If it leaks, Reset it in Google Calendar.
- **Bound to its hosts.** The password or address goes only to the host it
  was saved with (iCloud also `*.icloud.com`, where its calendars live). A
  redirect anywhere else is refused, not followed.
- **What's read is outside content.** An event's title and notes are
  whoever made it; a write after reading them asks you first (M54.5).

## The catalog: one step to add a vetted server (M63.4)

Some services aren't a sign-in or a key form on this page: a pinned
local server (Microsoft 365 for an Outlook.com account), a server whose
key the bot asks you for in a chat, a bundled skill. They're on the
dashboard's **Extensions** page under **From the catalog**, each with
**Add**, and the bot can add them for you with `ferrule_admin` `mcp_add`
and `catalog` (one card to approve; at once in owner mode):

- **Everything comes from the catalog**: the server's address or its
  pinned package, its arguments, which of its tools are writes, and the
  key it needs with the sites that key may go to. The bot can't change
  any of it.
- **A missing key** is asked for with `secret_request`: you get a link to
  the Safe with its name and sites filled in, paste the key there, and
  the server is added the moment it's saved (from the page, Add opens the
  Safe the same way).
- **What's added** is started in the sandbox and scanned like any `mcp
  add`, and marked as from the catalog, so what it reads is outside
  content and what it changes asks you first.

The entries, each checked against its vendor's docs on 2026-10-21:
Todoist's hosted server (sign in), Google Maps Grounding Lite and Mapbox
(a key), Microsoft 365 for a personal account (a pinned package that signs
in by itself, its sign-in in a hidden folder), Google's Calendar, Docs,
Sheets and People MCP previews (on the Google tile), and the six bundled
skills. Why a short curated list and no public plugin hub, and every
source: `docs/m63-connections-wave-b.md` §4.

## Any MCP server by its address

A server that isn't in the catalog connects by its address, from the chat
too: the agent's `connection_request` takes an MCP URL (a bare host such
as `mcp.example.com` is taken as `https://`), and the button you get,
`/connect <name>` with a name made from the host, connects that address.
`/connect https://…` does the same by hand.

## Any MCP server with a key

From the dashboard (M50): **Connect → Any app by its address**, then the
address and the key (the header it goes in is under Advanced,
`Authorization: Bearer {key}` unless you change it). The key is tried once
against the server, sealed in the connections store, and sent only to
that address; nothing is written to the config. A server with a sign-in
goes in the same panel's first box.

By hand, a server that takes a header key can be added in the config, and
it shows up as its own tile:

```toml
[[connections.custom]]
name = "mytool"
title = "My tool"
url = "https://mcp.example.com/mcp"
auth = "api_key"
header = "Authorization"
header_value = "Bearer {key}"
covers = "What it's for."
guide = ["Where to make the key.", "Paste it below."]
```

A private address (`127.0.0.1`, a LAN host) is blocked by the egress
policy unless you list it in `[egress] private_allow` (see
[egress.md](egress.md)). The page says which applies.

## The relay

**What it is.** It's a small Cloudflare Worker on *your own* Cloudflare
account (free plan). Its source is in `relay/`: one file with no
dependencies.

**Why a fixed address.** A browser sign-in ends by sending you back to a
callback address. Google checks that address against your OAuth client's
list, and Atlassian checks it against the domains your admin allowed.
Without the relay, the address is a quick tunnel that changes every time,
so these services refuse it. With the relay it is always
`https://<name>.<you>.workers.dev/cb`.

**Set it up** from the card at the top of Connections, in either of two
ways:

- **Deploy one.**
  1. Open <https://dash.cloudflare.com/profile/api-tokens> →
     **Create Token**.
  2. Use the template **Edit Cloudflare Workers** → **Continue to
     summary** → **Create Token**.
  3. Paste the token into the card.

  ferrule finds your account (`GET /accounts`; if the token reaches
  several, it asks which one). It then deploys the Worker, saves its URL
  and key, and checks it end to end. The token is used once and not kept.
- **Use one you already have.** Enter its URL and key. They're checked
  (health, then a slot written and read back) before anything is saved.

The same steps from a terminal: `ferrule connections setup relay`. Use
**Check it** on the card, or `ferrule connections relay check`, to test
it again at any time.

**What it can and can't see.**

- It sees the one-time authorization code on its way back from the
  sign-in page. A code alone is useless: exchanging it needs the PKCE
  verifier, which never leaves your server. A code is kept at most 5
  minutes and is deleted when read. Nothing is logged.
- It can't read anything without the relay key. Only your server has it,
  and strangers can't open or read slots.
- It never sees your tokens, refresh tokens or API keys typed on the
  dashboard. Those go straight from your browser to your server.
- Whoever controls the Cloudflare account could tamper with the Worker.
  That's why it runs on yours and there's no shared default.

## From a terminal

```
ferrule connections setup            # the checklist, each line's next step
ferrule connections setup relay      # the fixed callback address
ferrule connections setup jira       # asks which way in, then its fields (secrets hidden)
ferrule connections setup google --write
ferrule connections list             # what's connected
ferrule connections remove <name>    # revoke where the service allows, delete the key
```

On a machine where ferrule runs as a service, whether a user service
(`systemctl --user`) or a system one, these commands use the service's
config and data. What you add there is what the service sees.

## Verified how

The unit and integration tests run against mock servers. The browser check
(`scripts/dashboard_browser_check.mjs`) connects a local MCP server with a
key through the real page. Live calls to Atlassian, Google and Cloudflare
are in `#[ignore]` tests only. The guides follow the vendors' docs as of
September 2026, and the links above are to those docs.
