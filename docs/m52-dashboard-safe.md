# M52 — the dashboard Safe: edit the vault without SSH

**Status.** Built 2026-10-21. Source: `docs/research-harness2-media.md`
§4 (M52).

## What changes

The M50 keys panel grows into the **Safe**: the one page that lists
every secret ferrule holds and lets the owner change it — rotate a
value, edit the hosts it may go to, add a new key (exists), remove one
(exists). Today a leaked OpenRouter key means SSHing in to edit
`private/secrets.env` by hand; after M52 it is one card and one paste
from any browser the owner trusts.

The page is a rename and an extension, not a rewrite: it keeps every
M50 rule and adds exactly two write paths.

## The rules that don't change (M50, kept)

- **Write-only.** Values are never read back, never displayed, never
  logged. The list shows names, hosts and where a name is used — never
  a character of a value.
- **Live gateway required.** The page answers only when served by the
  running bot (`ctx.live`), because only the bot's process knows which
  names are taken in its environment.
- **Managed-mode lock.** A managed bot whose policy disallows the shell
  keeps the Safe locked, with the policy's sentence as the reason.
- **Name hygiene.** `shape()`'s rules (capitals/digits/`_`, never
  `FERRULE_`, never a name like `PATH` or `GIT_*` that programs act on
  by themselves) apply to every new name, unchanged.
- **One confirm card per change**, every change in the audit log
  (`dashboard.secret.*`), every failure a sentence — never a bare code.

## Part 1 — rotate: `POST secrets/rotate {name, value}`

The page seals `value` in the browser (M52.1's `asSecret`, merged with
this on `autopilot`), so the handler gets it opened, as every key field.

The missing half of M50. Add and remove refuse any name that's in use
elsewhere, because moving or deleting an in-use name breaks the config.
**Rotate has no such problem: it changes only the value**, so it is the
one operation that is safe for exactly the keys M50 wouldn't touch —
provider keys (`OPENROUTER_API_KEY` and friends, referenced by
`key_env`), connection secrets, channel tokens.

- Allowed when: the name is in the secrets file. It doesn't matter
  whether the config names it — the name and its hosts don't move.
- The value gets the same checks as add: one line, no control
  characters, ≤ 4096 bytes.
- On success the new value is written with `secrets::set` (atomic,
  keeps comments, 0600) **and** pushed into the running process's
  environment (`std::env::set_var`), so providers and connections that
  read `key_env` per request pick it up without a restart. If a
  component is known to read once at start, the success sentence says
  so plainly ("…takes effect after a restart — Restart is on Home").
- The confirm card names what breaks if the new value is wrong:
  "Replace the key for `OPENROUTER_API_KEY`? If the new key is wrong,
  the brain stops answering until you fix it here." The card never
  shows old or new value.
- Audit: `dashboard.secret.rotate` with the name only. A failed write
  leaves the old value in place (write-then-swap, never
  delete-then-write).

## Part 2 — edit hosts: `POST secrets/hosts {name, hosts}`

For `[secrets]` tool keys only (a name in `[secrets]`, value in the
file, not referenced elsewhere — the same test as M50's remove).
Provider keys don't take it: their hosts are the provider's endpoints,
owned by the provider config.

- Same host grammar as add: DNS names and `*.`-wildcards, ≤ 10, never
  an address, never localhost, never empty.
- Writes through `setup::write_secret_hosts` + `Target::save`, so the
  config page's existing undo covers it. The proxy picks the change up
  on its next rules reload; if a reload isn't live, the sentence says
  the new hosts apply within a minute.
- Audit: `dashboard.secret.hosts` with name and new hosts.

## Part 3 — the page

- The Connect page's "keys for tools" panel becomes **Safe**, linked
  from Connect and from Settings; on the phone it lives in the More
  sheet, not the bottom bar (rarely needed, high stakes).
- Each row: the name, its hosts as tags, "used by" in words (the
  provider/connection/tool the config names it for, or "nothing yet"),
  and a `⋯` menu with Rotate, Edit hosts, Remove. Remove stays
  disabled with its reason when the key is in use.
- An empty Safe says one calm sentence and offers the first add (M50's
  empty-state pattern).
- Copy rule: the words "never shown again" sit next to every paste
  box, because that's the truth of write-only.

## Security model

- The page is owner-auth'd like the rest of the dashboard; every write
  needs the confirm card; every write is audited with name, never
  value.
- Rotate-in-place updates the process env only for the exact name
  written — it can't create a variable, and `FERRULE_`-own names and
  loader-acting names are refused by `shape()` even though rotate
  doesn't take new names (defence in depth against a renamed call).
- The shell sandbox still sees only stand-ins; the proxy still swaps
  the real value only for bound hosts. Rotating changes what the proxy
  swaps in, never who can receive it.
- No bulk export, no "reveal" button, ever. That is the Safe's one
  promise and it is stated on the page.

## Done means

With a leaked OpenRouter key: open Safe from a phone, Rotate, paste the
new key, confirm — the bot answers on the new key without a restart,
and the audit log shows `dashboard.secret.rotate` with the name. A
tool key's hosts change from the page and the config undo reverts it.
`make check` green with new tests only on the two write paths: rotate
refuses unknown names and bad values, leaves the old value on a failed
write, updates the process env, and audits without the value; hosts
refuses bad shapes and names in use elsewhere. Docs: this file and the
Safe rows in `docs/dashboard.md`.

## Not in scope

Reading values back (never), per-key spend caps (the ledger's job), a
system-keychain backend (the secrets file stays the one store),
rotating the dashboard's own sign-in from the page (Settings already
has sign-out; a sign-in change stays a `ferrule setup` act).
