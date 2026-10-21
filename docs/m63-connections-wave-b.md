# M63 — connections, skills and the catalog (wave B)

The re-audit of 2026-10-21 (Part 2, "Connections, plugins and apps", and
Part 3 #3–#6) found that ferrule could reach a lot by hand and very little
by default. Its competitors ship everyday skills (OpenClaw's weather,
Hermes's keyless maps) and Hermes a list of 66 staff-reviewed MCP
servers, while ferrule had no bundled skills, Gmail as its only mailbox,
Google as its only calendar and sixteen catalog entries (the re-audit's
§2b). Wave B closes those four gaps, one commit per task:

1. [A bundled first-party skills pack](#1-the-bundled-skills-pack) (M63.1).
2. [Email from any provider over IMAP and SMTP](#2-mail-from-any-provider) (M63.2).
3. [Calendars over CalDAV, and read-only ICS](#3-calendars-caldav-and-a-secret-address) (M63.3).
4. [Catalog v2 and one-step add](#4-catalog-v2-and-one-step-add) (M63.4).

The rules every task keeps: keys come through `secret_request` (or the
dashboard's forms) and are bound to hosts that come from the catalog or
the owner, never from the model; the sandbox stays on; what a service
returns is untrusted content (M54.5); a change through a service is a
connected write behind the gates and grants (M19, M56.2). Nothing here
has run against the live services; each section's **Not verified** says
what wasn't.

## 1. The bundled skills pack

Six skills in the Agent Skills format ([the
specification](https://agentskills.io/specification)), compiled into the
binary from `crates/ferrule-skills/bundled/`:

| Skill | What it does | Service | Key |
|---|---|---|---|
| `weather` | current weather and forecasts up to 16 days | Open-Meteo geocoding and forecast | none |
| `news` | headlines from RSS and Atom feeds, the owner's or four defaults | the feeds themselves | none |
| `hacker-news` | front page, new, best, Ask, Show, jobs, threads, users | the official HN API (Firebase) | none |
| `exchange-rates` | rates and conversions, today or a past date or a period | Frankfurter | none |
| `maps` | places, addresses, driving, cycling and walking distance and time | Nominatim and the FOSSGIS OSRM server | none |
| `home-assistant` | entity states, history, actions | the owner's Home Assistant REST API | `HA_TOKEN`, through `secret_request` |

### How they're shipped

- **A scope of their own, `bundled`.** At each start (the gateway's
  skill set, `ferrule skills`, the dashboard's settings),
  `ferrule_skills::bundled::install` writes the pack to
  `<data>/bundled-skills/`: a file whose bytes differ from the binary's is
  written beside it and renamed over it, and a file the pack no longer has
  is removed. Discovery reads that directory after every project, user and
  installed skill and before the learned ones, so a skill of the same name
  anywhere else wins; an owner who wants to change one copies it to
  `~/.config/ferrule/skills/<name>/` and edits the copy.
- **On by default, off one by one.** `[skills] disabled = ["maps"]`,
  `ferrule skills disable maps`, `/skills off maps` in Telegram, the
  dashboard's switch, or `ferrule_admin` `skill_off`: the same list as
  any other skill. `[skills] bundled = false` leaves the whole pack out
  (nothing is written then).
- **Shown as bundled.** `ferrule skills` lists them with the scope
  `bundled` and a note on what that means; the dashboard's Extensions
  page tags them.
- **No triggers.** None of the six has `triggers:`, so none loads by
  itself on a word in a message; the model activates one from the catalog
  when a task matches, as with any skill. (Their scope may trigger, like
  a user skill, if a later version adds some.)
- **Not written by the model.** `<data>` is outside the writable roots
  of the model's commands, and read-only to them in owner mode
  (`owner_mode::protected_paths`); the pack is written again at the next
  start whatever happened to it.
- **Never a third-party install.** The pack is ferrule's own text. Skills
  from elsewhere still come only through `skill_install` from a pinned,
  allow-listed source, scanned and digest-locked (M13), or `ferrule
  import`, one at a time with the owner's yes. Nothing is installed from a
  public hub: in February 2026, 341 of 2,857 skills on OpenClaw's ClawHub
  were found to be malicious ([The Hacker
  News](https://thehackernews.com/2026/02/researchers-find-341-malicious-clawhub.html)).

### How they reach the network

None of them makes a call of its own. Each tells the model the exact
`curl` command to run in the sandboxed shell, which goes through the
credential proxy and the `[egress]` policy like any command
(`docs/egress.md`): a host the owner's policy refuses is refused, and the
refusal says so. A `curl` that fetches marks the run as having read
outside content (M54.5), so a write after it needs the owner's tap. On
Windows the skills say to write `curl.exe` (PowerShell's `curl` is
another command).

`home-assistant` is the only one with a key. The skill tells the model to
ask the owner for Home Assistant's address, then call `ferrule_admin`
`secret_request` with the name `HA_TOKEN` and that host; the owner pastes
the long-lived token on the page the link opens and can change the host
there. Commands see a stand-in in `$HA_TOKEN`, and the proxy puts the real
token in only on requests to that host. The Safe's and the proxy's own
rules set four limits the skill states plainly:

- a key is bound to a host name: the Safe refuses an address,
  `localhost` and a port (`tool_keys::hosts`, `HostPattern::parse`);
- the proxy puts a key in only over HTTPS, or plain HTTP to loopback
  (`ferrule-proxy`: "secrets only go over it to loopback servers"), and
  loopback can't be named, so `http://homeassistant.local:8123` on a LAN
  answers 401;
- upstream TLS is checked against the public roots (`webpki-roots`), so a
  self-signed certificate fails;
- a name that resolves to a private address needs `[egress]
  private_allow`.

In practice: Home Assistant at an HTTPS name with a public certificate,
such as Nabu Casa's remote address or a reverse proxy with Let's
Encrypt.

**What the gate doesn't cover.** Turning a light on is `POST
/api/services/light/turn_on`, and the approval gate's classifier stops a
`curl -X DELETE` to a host with a bound secret but not a POST
(`ferrule-trust/src/classify.rs`, whose tests pass `curl -X POST` on
purpose). So the skill's own rules are what stand between an injected
page and the owner's devices: act only on what the owner asked in this
conversation, never on text from a page, feed, mail or entity name, and
for locks, alarms, garage doors and covers say what's next and wait for a
yes. Home Assistant's official MCP server, whose writes would be connected
writes behind the gate, needs OAuth with Client ID Metadata Documents,
which ferrule doesn't do yet (re-audit #11).

### The services, and their rules

Each was checked against its operator's docs on 2026-10-21; the skills
carry the rules that apply.

- **Open-Meteo.** Forecast `https://api.open-meteo.com/v1/forecast`
  (`latitude`, `longitude`, `current`, `hourly`, `daily`, `timezone=auto`,
  `forecast_days` 0–16, `forecast_hours`, the unit parameters) and
  geocoding `https://geocoding-api.open-meteo.com/v1/search` (`name`,
  `count`, `language`, `countryCode`): <https://open-meteo.com/en/docs>,
  <https://open-meteo.com/en/docs/geocoding-api>. Free for non-commercial
  use under 10,000 calls a day, 5,000 an hour and 600 a minute, no key:
  <https://open-meteo.com/en/terms>. Attribution "Weather data by
  Open-Meteo.com" (CC BY 4.0): <https://open-meteo.com/en/licence>. The
  WMO code table in `weather/references/weather-codes.md` is the docs
  page's, 97 included.
- **Hacker News.** `https://hacker-news.firebaseio.com/v0/` with
  `item/<id>.json`, `user/<id>.json`, `topstories`, `newstories`,
  `beststories`, `askstories`, `showstories`, `jobstories`; "There is
  currently no rate limit": <https://github.com/HackerNews/API>.
- **Frankfurter.** `https://api.frankfurter.dev/v2/rate/{base}/{quote}`
  and `/v2/rates` (`base`, `quotes`, `date`, `from`, `to`, `group`),
  `/v2/providers/ecb/rates` for the ECB alone, `/v2/currencies`; no key,
  no quota, "rate-limited to prevent abuse": <https://frankfurter.dev/>.
  The docs show no response body; the shape the skill gives
  (`{"date","base","quote","rate"}`, a list for `/v2/rates`) is from one
  request to each on 2026-10-21. v1 (`/v1/latest`, ECB rates) is
  deprecated but stays (<https://frankfurter.dev/v1/>), and
  `api.frankfurter.app` answers with a redirect to it.
- **Nominatim.** `https://nominatim.openstreetmap.org/search` (`q`,
  `format=jsonv2`, `limit` up to 40, `addressdetails`, `accept-language`,
  `countrycodes`) and `/reverse` (`lat`, `lon`, `zoom` 0–18):
  <https://nominatim.org/release-docs/latest/api/Search/>,
  <https://nominatim.org/release-docs/latest/api/Reverse/>. The usage
  policy: at most one request a second, a User-Agent or Referer that
  names the application (a library's default is refused), no
  auto-complete, no bulk geocoding, OpenStreetMap attribution:
  <https://operations.osmfoundation.org/policies/nominatim/>.
- **OSRM.** The route service `/route/v1/{profile}/{lon},{lat};{lon},{lat}`
  with `overview`, `steps`, `alternatives`; `routes[].distance` in metres
  and `duration` in seconds: <https://project-osrm.org/docs/v5.24.0/api/>.
  The FOSSGIS demo server: "reasonable, non-commercial use", "Do not
  exceed 1 request per second", car, bike and foot profiles:
  <https://github.com/Project-OSRM/osrm-backend/wiki/Demo-server>; a valid
  User-Agent and attribution: <https://routing.openstreetmap.de/about.html>.
  **Not in the operator's docs:** the per-profile paths
  `routing.openstreetmap.de/routed-{car,bike,foot}/route/v1/driving/…`.
  One request to each on 2026-10-21 gave three different durations for
  the same trip, while `router.project-osrm.org` gave the car's for every
  profile, so the skill uses the former. The openstreetmap.org directions
  link (`engine=fossgis_osrm_car`) is the site's own engine name, from its
  source, not a documented API.
- **Home Assistant.** `GET /api/`, `/api/states`, `/api/states/<id>`,
  `/api/services`, `/api/history/period/<time>?filter_entity_id=…`,
  `POST /api/services/<domain>/<action>` (`?return_response` only for
  actions that return data), `Authorization: Bearer <token>`, tokens from
  the profile's Security tab, port 8123 (80 under the Supervisor):
  <https://developers.home-assistant.io/docs/api/rest/> (now served from
  <https://contributors.home-assistant.io/core/internals/rest-api/>).
- **The news defaults.** BBC News (`feeds.bbci.co.uk/news/rss.xml`), NPR
  News (`feeds.npr.org/1001/rss.xml`), The Guardian's world feed
  (`www.theguardian.com/world/rss`) and DW's English feed
  (`rss.dw.com/rdf/rss-en-all`) each answered 200 with a feed on
  2026-10-21. The publishers' terms for their feeds weren't read.

### Costs

The catalog in the system prompt grows by six entries, about 400 tokens
on every request while skills are on; the prompt prefix changes once, at
the upgrade, and is byte-stable from then on. Nothing else runs until the
model activates a skill.

### Not verified

No skill has run in a live session. The injection scan that
`skill_install` and the triggers use wasn't run on the pack: with no
triggers, nothing scans a bundled skill, and the text was written to
stay clear of the scan's patterns, by reading them, not by running it. The
`curl` commands were checked against the services by hand once, not from
ferrule's sandbox or through its proxy, and not on Windows.

## 2. Mail from any provider

Gmail with an app password (M37) was ferrule's only mailbox. The
connection is now general: `native = "mail"` is any IMAP and SMTP
server, and Gmail is that connection with Gmail's habits.

### What changed

- **One mailbox, two flavours** (`crates/ferrule-connections/src/native/mail.rs`).
  `Mail` holds the address, the user names, the password and the two
  servers, and opens a session per call. `Flavor::Gmail` searches with
  Gmail's own syntax (`X-GM-RAW`) over All Mail on Google's servers and
  keeps its tool names (`gmail_search`, `gmail_read`, `gmail_send`) and
  Google's wording when the password is refused; `gmail.rs` is now only
  that constructor. `Flavor::Imap` searches with RFC 3501's criteria
  (`FROM`, `TO`, `SUBJECT`, `TEXT`, `SINCE`, `BEFORE`, `UNSEEN`; UTF-8
  when a value isn't ASCII) in the mailbox it names, and has `mail_search`,
  `mail_read`, `mail_folders` and, on a write connection, `mail_send`.
- **IMAP** (`imap.rs`): `search_criteria` and `mailboxes` (`LIST`, the
  ones that can be opened) beside Gmail's search; `Part` is `Copy`.
- **Fields and TLS** (`native::normalize_mail`): an address, a password
  (an app password's display spaces taken out when it is 16 letters and
  digits), and per side a host name, a port and `tls` or `starttls`. A
  side with no port takes 993 or 465 (143 or 587 with `starttls`); with no
  security given, 143, 587 and 25 are STARTTLS and every other port TLS
  from the first byte. There is no unencrypted choice, and an address
  isn't accepted as a host (only a test's `127.0.0.1`).
- **Presets** (`catalog.toml`, a new `preset` table on a service): field
  values the catalog fixes, merged in after the form's fields so a typed
  value can't replace them. iCloud Mail, Fastmail and Yahoo Mail are
  presets on a new **Mail** tile; "Any other mailbox" asks for the
  servers. iCloud's IMAP user is the address's local part
  (`imap_user = "{local}"`), as Apple's settings page says; an optional
  `username` field overrides both sides.
- **Bound to the two servers.** `native::hosts("mail")` is the IMAP and
  SMTP hosts, kept on the record; the password is sealed in the connection
  store and the native code connects to nothing else. The connection's
  address joins the owner's own addresses (`own_addresses`), so the wave A
  rule for the owner's scheduled briefings covers it as it covers Gmail.
- **Untrusted reads, gated sends.** A connection's server name is in the
  trust hub's `connected` list, so its reads mark the run as having read
  outside content (M54.5) and its `mail_send` (no `readOnlyHint`) is a
  connected write behind the gates and standing grants (M19, M56.2). That
  list is filled only while `[connections] gate_writes` is on (the
  default), as for every connection.

### Where the password goes in

On the dashboard's Connections page (the Mail tile's form, write-only,
tested by an IMAP login before it's saved), or `ferrule connections
setup mail` at a terminal; in a chat, `connection_request` sends the owner
a button whose reply points there. Not `secret_request`: that saves a
`[secrets]` key, which reaches commands as a placeholder for the proxy to
swap. This password is never handed to a command; ferrule's own code
speaks IMAP and SMTP with it, so it lives in the sealed connection store
like Gmail's, and is bound to its hosts there (the record's `hosts`).

### The providers, checked on 2026-10-21

- **iCloud Mail**: `imap.mail.me.com` 993 SSL, user name the address
  without the domain ("if that fails, use the full address");
  `smtp.mail.me.com` 587, user name the full address; an app-specific
  password: <https://support.apple.com/en-us/102525>. App-specific
  passwords at account.apple.com → Sign-In and Security, with two-factor
  authentication: <https://support.apple.com/en-us/102654>. Apple says
  "SSL" for 587 and "TLS or STARTTLS" if that errors; port 587 is
  STARTTLS by convention and the preset uses it. **Not verified live.**
- **Fastmail**: `imap.fastmail.com` 993 and `smtp.fastmail.com` 465, TLS
  from the first byte (587 with STARTTLS), the full address as user name,
  an app password required:
  <https://www.fastmail.help/hc/en-us/articles/1500000278342-Server-names-and-ports>;
  making one (Settings → Privacy & Security → Connected apps & API
  tokens → Manage app passwords and access; not on the Basic plan, which
  has no IMAP, SMTP or CalDAV):
  <https://www.fastmail.help/hc/en-us/articles/360058752854>.
- **Yahoo Mail**: `imap.mail.yahoo.com` 993, `smtp.mail.yahoo.com` 465 or
  587, the full address, an app password:
  <https://help.yahoo.com/kb/SLN4075.html>; Account Security → Create app
  password: <https://help.yahoo.com/kb/generate-password-sln15241.html>.
- **Gmail** (unchanged): Google recommends "Sign in with Google" and calls
  app passwords a fallback that needs 2-Step Verification; IMAP has been
  always on for personal accounts since January 2025:
  <https://support.google.com/mail/answer/7126229>.
- **Outlook.com and Hotmail: not offered.** Microsoft's settings page
  lists OAuth2/Modern Auth as the authentication for IMAP, POP and SMTP:
  <https://support.microsoft.com/en-us/office/pop-imap-and-smtp-settings-for-outlook-com-d088b986-291d-42b8-9564-9c414e2aa040>.
  Microsoft's earlier article said Basic authentication would be removed
  from Outlook.com:
  <https://support.microsoft.com/office/outlook-and-other-apps-are-unable-to-connect-to-outlook-com-f4202ebf-89c6-4a8a-bec3-3d60cf7deaef>.
  The date, 16 September 2024, is from coverage of Microsoft's Outlook
  blog post ([heise](https://heise.de/-9767989)), not the post itself.
  Outlook.com is left to Microsoft 365's catalog entry (M63.4).

### Not verified

Nothing was run against a real provider: the presets' logins, iCloud's
local-part user name, Yahoo's and Fastmail's app passwords and the
STARTTLS path are untested live. The existing Gmail test (mock IMAP and
SMTP) passes unchanged through the new code; the generic flavour has no
test of its own (the owner's rule: no new tests). The email channel's
`use_connection` still takes a Gmail connection only.

## 3. Calendars: CalDAV and a secret address

Google Calendar was the only calendar, and it needed a service account or
an OAuth client plus the relay. Two native connections now cover the
everyday cases (`crates/ferrule-connections/src/native/caldav.rs`), on a
new **Calendar** tile.

### CalDAV (`native = "caldav"`)

- **Finding the calendars** (RFC 6764 §6, RFC 5397, RFC 4791 §6.2.1): a
  `PROPFIND` for `current-user-principal` at the address given, then at
  `/.well-known/caldav`; a `PROPFIND` of the principal for
  `calendar-home-set`; a `Depth: 1` `PROPFIND` of the home for each
  collection's `resourcetype`, `displayname` and
  `supported-calendar-component-set` (task lists are left out). Kept ten
  minutes.
- **Listing events**: a `calendar-query` `REPORT` with a `time-range` and
  `calendar-data` with `expand`, so the server returns one component per
  occurrence (RFC 4791 §7.8, §9.6.5, §9.9: "date with UTC time"). A server
  that doesn't expand gets ferrule's own expansion (below).
- **Changes** (write connections only, connected writes behind the gate):
  `event_create` is a `PUT` of a new `<uid>.ics` with `If-None-Match: *`
  (§5.3.2); `event_update` fetches the event, rewrites only the series'
  own `SUMMARY`, `DTSTART`, `DTEND`, `LOCATION` and `DESCRIPTION` lines
  (not an alarm's, not an override's; a moved start keeps the length) and
  puts it back with `If-Match` on its `ETag`; `event_delete` is a
  `DELETE`, marked destructive. An event id is its path, and one outside
  this account's calendars is refused.
- **Times.** A day is all-day (`VALUE=DATE`); a time with an offset or `Z`
  is stored in UTC; a time without one is floating, "that time wherever
  the owner is" (RFC 5545 §3.3.5). No `TZID` is written: RFC 5545 wants a
  `VTIMEZONE` for each, and ferrule has no time zone database.
- **The credential's hosts** are the address's host plus a preset's
  `extra_hosts` (`*.icloud.com`); every request, redirects included, is
  checked against them, and a redirect elsewhere is refused with the host
  named. Redirects are followed by ferrule, not by the HTTP client.
- **Parsing**: a small XML reader for multistatus answers (`xml.rs`: local
  names, entities, CDATA) and an iCalendar reader and writer (`ical.rs`:
  unfolding, parameters, escaped text, dates, durations, `RRULE`,
  `EXDATE`, `RECURRENCE-ID`, lines folded at 75 octets). No new
  dependency.

### A secret address (`native = "ics"`)

- One field, `secret_url`: the field name has "secret" in it, so the
  value is among the connection secrets the tool-result scrub hides
  (`Store::secret_values`), and the page treats it as a password (sealed,
  never sent back). `webcal://` is read as `https://`. Error messages
  name the host, never the address.
- `events_list` downloads the calendar (at most 20 MB), and works out the
  occurrences in the range itself: `DAILY`, `WEEKLY` with plain `BYDAY`,
  `MONTHLY` on the start's day or one `BYDAY` like `3TU` or `-1FR`,
  `YEARLY` on the start's date, with `INTERVAL`, `COUNT`, `UNTIL` and
  `EXDATE`; an override (`RECURRENCE-ID`) replaces its occurrence; a
  `CANCELLED` event is left out. Any other rule is shown once, with the
  rule, saying its dates weren't worked out. Times are compared on each
  event's own clock (no time zone database), so an event at the edge of a
  range can be a few hours off.
- Read-only whatever "allow changes" says: there is nothing to write to.

### The providers, checked on 2026-10-21

- **iCloud**: app-specific passwords at account.apple.com → Sign-In and
  Security → App-Specific Passwords, with two-factor authentication:
  <https://support.apple.com/en-us/102654>. **Not in Apple's docs:**
  `caldav.icloud.com` as the CalDAV address and its calendars living on
  `pNN-caldav.icloud.com`; both are from third-party guides (for example
  <https://www.onecal.io/blog/how-to-integrate-icloud-calendar-api-into-your-app>,
  <https://www.busymac.com/docs/busycal/70630-using-busycal-with-icloud>),
  hence `extra_hosts = "*.icloud.com"`.
- **Fastmail**: CalDAV at `https://caldav.fastmail.com/`, the full address
  as user name, an app password required:
  <https://www.fastmail.help/hc/en-us/articles/1500000278342-Server-names-and-ports>.
- **Nextcloud**: `https://<host>/remote.php/dav` is the form the manual's
  sync guide shows
  (<https://docs.nextcloud.com/server/stable/user_manual/en/groupware/sync_windows10.html>);
  app passwords with two-factor sign-in, under the settings' Security
  section (the exact menu words are from the guide's older version).
- **Google Calendar's secret address**: Settings → Settings for my
  calendars → the calendar → Integrate calendar → "Secret address in iCal
  format"; "Only you should know", "Do not share this address", Reset
  makes a new one, and it is "a read-only view of one calendar":
  <https://support.google.com/calendar/answer/37648>.
- **The protocols**: <https://www.rfc-editor.org/rfc/rfc4791>,
  <https://www.rfc-editor.org/rfc/rfc6764>,
  <https://www.rfc-editor.org/rfc/rfc5397>,
  <https://www.rfc-editor.org/rfc/rfc5545>.

### Not verified

Nothing was run against a real server: not iCloud's discovery or its
`pNN` hosts, not Fastmail's or Nextcloud's, not whether each server honours
`expand`, not Google's secret address. The XML and iCalendar readers and
the repeat expansion have no tests (the owner's rule: no new tests); the
existing suite only checks that the catalog parses. Time zones are not
converted: a range is compared on each event's own clock.

## 4. Catalog v2 and one-step add

### The schema

`crates/ferrule-connections/catalog.toml` keeps its tables and gains
(validated in `Service::check_v2`):

| Key | What |
|---|---|
| `kind` | `remote` (the default: the vendor hosts it at `url`), `stdio` (a pinned package run in the sandbox) or `skill` (a bundled skill, `skill`) |
| `auth` | `oauth`, `api_key` (also `key`) or `none` |
| `package` | stdio: `npm:<name>@<x.y.z>`, one exact version (a range or a tag is refused); `args`, `read_only_args` (added unless the add is for writes), `env` (no keys) |
| `integrity` | the npm tarball's integrity as published, kept for the record; `npx` doesn't check it against this, and neither does ferrule yet |
| `registry` | the server's name in the official MCP Registry: an identity, not a vouching |
| `secret` | `{ name, hosts, header, value }`: the key an add asks for and the only hosts it is bound to; a remote entry's hosts must include its url's host |
| `read_only_url` | the address used while read-only (the default), on the url's host |
| `writes` | tools that change something whatever their hints say |
| `preset` | (M63.2) field values the catalog fixes |

### One step

- **From a chat**: `ferrule_admin` `mcp_add` with `catalog` (and `write`
  for changes). Any other field with it is refused: the address, the
  package, the arguments, the header and the key's hosts all come from the
  entry. Outside owner mode it is one card, as every change; the card says
  what runs, where, and which key goes to which hosts.
  - `oauth`: the owner gets the connection's Sign in button
    (`Connections::request`), the M20 flow.
  - a `secret` not saved yet: `secret_request` with the catalog's name and
    hosts (M54 §6.3's link to the Safe), and a continuation registered on
    that name (`keys::when_saved`): when the Safe saves it, the server is
    added and the session that asked is woken with the outcome. If the
    owner changed the sites so the catalog's host isn't among them,
    nothing is added and the bot is told why. A name already saved but
    bound elsewhere, or kept for something else, is refused, never
    rebound.
  - otherwise (key saved, or none needed): `mcp add` with the entry's
    settings (`self_service/catalog.rs` `install`): started in its
    sandbox, listed and scanned, then written to `[[mcp.servers]]`, where
    the running gateway loads it.
  - `skill`: the bundled skill turned on (it usually is).
- **From the dashboard**: Extensions → **From the catalog** lists the
  entries the Connections page doesn't add (stdio, skills, and the remote
  ones with a `secret`), with **Add** (`POST mcp/catalog`, confirmed with
  the same text as the card). A missing key opens the Safe with its name
  and sites filled in, and the same continuation adds the server once it's
  saved. Sign-ins and key forms stay on the Connections page; Google Maps
  and Mapbox are in both places (a key form there, sealed in the
  connection store; or the Safe through the catalog).
- **Managed mode's extensions lock** refuses both, as it refuses `mcp_add`.

### What an added server is held to

- **Marked `catalog = "<id>"`** in `[[mcp.servers]]`. The trust hub's list
  of connected services now takes these too (`connections::set_gate`,
  refreshed whenever the config's servers are), so its reads are outside
  content (M54.5) and every tool without `readOnlyHint`, or in `writes`
  (`write_tools`, enforced in `ferrule_mcp::build_tools`), is a connected
  write behind the gates and standing grants. Only while `[connections]
  gate_writes` is on, as for connections.
- **A stdio entry** runs as `npx -y <name>@<exact version>` with its
  args, sandboxed like every stdio server, with `private_state = true`:
  its home, caches and temp dir are `<data>/mcp-private/<name>`. That
  folder is in the backup's secrets list, so it is hidden from commands
  and the file tools in every mode; `Sandbox::for_helper` replaces it, for
  that server only, by its other entries, so the server reaches its own
  folder and no sibling's. Microsoft 365's server keeps its token cache
  there; its README says the cache's encryption key goes to the OS
  credential store through keytar, or, "without a credential store", to a
  `.cache-key` file beside the cache. Inside the sandbox ferrule expects
  the second (unverified); `MS365_MCP_USE_KEYTAR=0` would force it, but a
  variable named like a key is refused in an MCP server's `env`
  (`mcp_add`'s rule), so it isn't set. **Not on Windows**: the Windows sandbox hides a path with a
  DACL every sandboxed process shares, which would shut the server out of
  its own folder, so there the folder isn't hidden.
- **Its key** is a `[secrets]` entry: commands and the server see a
  placeholder, and the proxy swaps the real value in only on requests to
  the catalog's hosts (M7, M54.2).

### The entries, checked on 2026-10-21

- **Todoist** (`remote`, `oauth` with DCR):
  `https://ai.todoist.net/mcp`, "Primary URL (Streamable HTTP)", OAuth in
  the browser: <https://github.com/Doist/todoist-mcp>. MCP Registry name
  `net.todoist/mcp` with that remote (the registry's API,
  `registry.modelcontextprotocol.io/v0/servers?search=todoist`). The
  `writes` list is the add/update/complete/move/delete tools named in the
  repo's `src/utils/tool-names.ts` on `main`. **Not verified:** that
  Todoist accepts ferrule's dynamic client registration (the README says
  OAuth, not DCR; the re-audit cites Hermes for DCR working).
- **Google Maps Grounding Lite** (`remote`, key): `https://mapstools.googleapis.com/mcp`,
  the key in `X-Goog-Api-Key`, tools `search_places`, `lookup_weather`,
  `compute_routes`, `resolve_names`, `resolve_maps_urls`; no turn-by-turn;
  pay-as-you-go; Google Maps attribution and no use with models that train
  on the input: <https://developers.google.com/maps/ai/grounding-lite>.
  Registry `com.googleapis.mapstools/mcp`. The page doesn't say preview or
  GA.
- **Mapbox** (`remote`, key): `https://mcp.mapbox.com/mcp`, a Mapbox
  access token as `authorization: Bearer <token>`:
  <https://github.com/mapbox/mcp-server/blob/main/docs/hosted-mcp-guide.md>.
  Registry `io.github.mapbox/mcp-server`. Tools (directions, matrix,
  isochrone, geocoding, search, static maps) from the README; which of
  them the hosted endpoint offers isn't stated.
- **Microsoft 365 for a personal account** (`stdio`, `none`):
  `@softeria/ms-365-mcp-server` pinned at **0.170.2**, the latest on npm
  on 2026-10-21 (published 2026-10-19), MIT, integrity as recorded:
  <https://registry.npmjs.org/@softeria%2Fms-365-mcp-server>. Device-code
  sign-in through its `login` tool; `--read-only` "disabling write
  operations"; personal accounts by default; `MS365_MCP_TENANT_ID =
  "consumers"` because, per the README, refresh tokens from the default
  `common` authority are rejected as of June 2026; the token cache under
  the server's home (`~/.config/ms-365-mcp-server/` on Linux, which the
  sandbox puts in the server's state dir): <https://github.com/Softeria/ms-365-mcp-server>.
  Registry `io.github.Softeria/ms-365-mcp-server`. Needs Node (`npx`).
  Work accounts (`--org-mode`, a tenant) aren't this entry. Microsoft's
  own Work IQ MCP needs an Entra work tenant (re-audit Part 2 §2c #7).
- **Google Calendar, Docs, Sheets and People MCP** (`remote`, `oauth`
  with the owner's client, previews beside the Gmail and Drive ones):
  `https://calendarmcp.googleapis.com/mcp/v1`,
  `https://docsmcp.googleapis.com/mcp/v1`,
  `https://sheetsmcp.googleapis.com/mcp/v1`,
  `https://people.googleapis.com/mcp/v1`, with the scopes the page lists
  (Calendar's and People's are read-only only), Developer Preview
  enrolment, and an OAuth client with "Use this client for an AI-powered
  agent" ticked: <https://developers.google.com/workspace/guides/configure-mcp-servers>.
  Whether consumer @gmail.com accounts can use them, the page doesn't say.
- **The six bundled skills** (`skill`): §1.

Left out on purpose: Microsoft's Work IQ (a work tenant only), Slack,
Asana and Dropbox (no DCR for an untrusted client), Home Assistant's own
MCP server (Client ID Metadata Documents, which ferrule doesn't do yet),
finance and password managers (re-audit Part 2 §2c #12, #16).

### The trust model: why a curated list, and no public hub

- **Curated by hand, compiled in.** Each entry names an official,
  maintained server (the vendor's own, or for Microsoft 365 the widely
  used open-source one, pinned), with its address or exact package checked
  against the vendor's docs, and its key's hosts fixed. A compiled-in
  entry is the owner's pre-approval of *what* runs; the card (or owner
  mode) is the approval of *that* it runs. The owner's own entries
  (`[[connections.custom]]`) sit beside them.
- **No auto-install from a public plugin or skill hub.** In February 2026,
  341 of 2,857 skills on OpenClaw's ClawHub were found to be malicious,
  most delivering a credential stealer through fake setup steps ([The
  Hacker News](https://thehackernews.com/2026/02/researchers-find-341-malicious-clawhub.html));
  Snyk's ToxicSkills study found a critical issue in 13.4% of 3,984 skills
  ([Snyk](https://snyk.io/blog/toxicskills-malicious-ai-agent-skills-clawhub)).
  The official MCP Registry "does not make guarantees about moderation,
  and consumers should assume minimal-to-no moderation" ([its moderation
  policy](https://github.com/modelcontextprotocol/registry/blob/main/docs/modelcontextprotocol-io/moderation-policy.mdx),
  read 2026-10-21), so a `registry` name here is an identity, not a
  vouching. (The ClawHub and Snyk figures are the re-audit's, read on
  2026-10-21, not read again for this section.)
- **Pinned, scanned, sandboxed.** A stdio entry is one exact version, never
  `latest`; every added server's tools are scanned before it's written
  (M13's scanner, M17's flow); stdio servers run in the OS sandbox with
  the network through the proxy; remote ones spawn nothing at all, which
  is why vendor-hosted servers are preferred.
- **Keys never from the model.** The key's name and hosts are the
  catalog's; the value goes from the owner to the Safe, never through a
  chat or the model; the model and the server see a placeholder.
- **What it reads is someone else's text** (M54.5), and what it changes
  asks first (M19), owner mode included after outside content.

### Not verified

Nothing was added against the real services: not Todoist's DCR, not a
Maps or Mapbox key through the proxy, not the Microsoft 365 server's
device-code sign-in in the sandbox or its token cache under
`<data>/mcp-private`, not the Google previews. The carve in
`Sandbox::for_helper` (a server reaching its own folder in a hidden one)
wasn't run on Linux or macOS; on Windows the folder isn't hidden. The
`integrity` is recorded, not checked. No new tests (the owner's rule); the
catalog test lists the new names.
