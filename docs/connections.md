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

## Any MCP server with a key

A server that takes a header key can be added in the config, and it shows
up as its own tile:

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
