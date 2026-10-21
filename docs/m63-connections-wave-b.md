# M63 — connections, skills and the catalog (wave B)

The re-audit of 2026-10-21 (Part 2, "Connections, plugins and apps", and
Part 3 #3–#6) found that ferrule could reach a lot by hand and very little
by default. Its competitors ship everyday skills (OpenClaw's weather,
Hermes's maps), email beyond Gmail, CalDAV calendars and a list of vetted
servers, while ferrule had no bundled skills, Gmail as its only mailbox,
Google as its only calendar and sixteen catalog entries. Wave B closes
those four gaps, one commit per task:

1. [A bundled first-party skills pack](#1-the-bundled-skills-pack) (M63.1).
2. Email from any provider over IMAP and SMTP (M63.2).
3. Calendars over CalDAV, and read-only ICS (M63.3).
4. Catalog v2 and one-step add (M63.4).

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
