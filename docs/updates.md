# Updates and self-repair

Ferrule keeps itself current, keeps the `claude` CLI current for the
Claude plan, keeps the ChatGPT plan's client identity current, and repairs
what it recognises when something breaks. The design, with the reasons, is
[m36-self-update.md](m36-self-update.md).

## Upgrading from v0.6.0 or older: run the one-liner once

Installs from before v0.7 don't have the update units (v0.7.0 is the
first signed release). Run the install
one-liner once more (the same one you installed with):

```sh
curl -fsSL https://raw.githubusercontent.com/maximarhipkin/ferrule/main/install.sh | sh
```

It replaces the binary and, when a service is set up, runs
`ferrule setup --refresh-service`, which rewrites the service's units from
what they already know and adds the update units. It asks nothing. From
then on updates are automatic. On Windows it's the `irm … install.ps1 | iex`
line. Until then `ferrule doctor` warns that the service has no update
units.

## Ferrule itself

```
ferrule update               # check, download, verify, install, restart the service
ferrule update --check       # say what's available, change nothing
ferrule update --to v0.7.0   # a specific release, even an older one (asks; unpins it)
ferrule update --to v0.6.0 --unsigned   # a release from before signing: checksum only
```

A release is installed only when:

- its archive matches the release's `SHA256SUMS`, and
- its `.minisig` signature verifies against the key compiled into the
  running binary (`release.pub` in the repository), with a trusted comment
  naming that exact tag and archive, and
- it is newer than what runs (`--to` may go back), not a draft, and not a
  pre-release unless `channel = "prerelease"`.

### Automatic updates for a service

`ferrule setup` installs, beside the service, an **apply unit** that does
the privileged half: the gateway can't write its own binary or restart its
own unit, and shouldn't be able to.

| | Linux (systemd) | macOS (launchd) |
|---|---|---|
| the apply job | `ferrule-update.service` (oneshot, `ferrule update --apply`) | agent `ai.ferrule.update` |
| daily | `ferrule-update.timer`, random time within 6 h, catches up after boot | `StartCalendarInterval`, a minute picked at setup |
| on request | `ferrule-update.path` watches `<data>/update/request` | `WatchPaths` on the same file |

The apply job checks for a release, updates `claude` when it's due, then
**waits until no turn is running** (up to 6 hours), swaps the binary
(keeping the old one as `ferrule.previous`), restarts the service, and
watches it. If the new gateway isn't healthy within 180 seconds, the old
binary goes back, the service restarts again, and the bad version is
**pinned**: it is never tried automatically again. `ferrule update --to
<tag>` retries it and unpins it.

You hear about it once, in your chat:

- "Updated Ferrule 0.7.0 → 0.7.1: <the release notes' first line>"
- "Ferrule 0.7.1 didn't start properly, so I went back to 0.7.0 and won't
  try 0.7.1 again. `ferrule update --to v0.7.1` retries it."

A quiet check says nothing.

### Config

```toml
[update]
auto = true          # unset: on when the update units are installed
channel = "stable"   # or "prerelease"
claude = true        # keep the claude CLI current too
```

With `auto = false` the daily check still runs, and a new release comes to
you as an approval button; Allow asks the apply unit to install it now.

### Without a service manager

A gateway run by hand, and every Windows install (there is no Windows
service yet), checks daily itself and tells you once per release: "Ferrule
v0.7.1 is out: run `ferrule update`". `ferrule update` then does it all in
the foreground. On Windows the running `ferrule.exe` is renamed aside
(`ferrule.exe.<hex>.old`, deleted at the next start) and the new one takes
its place; restart ferrule yourself. Nothing watches a hand-run gateway, so
there is no rollback there.

### Where things are

- The apply job's state, the last check, pinned versions and the last 20
  events: `/var/lib/ferrule/update/state.json` for a system service (the
  gateway can read it, not write it), `<data>/update/state.json` otherwise.
- `ferrule status`, `/status` in a chat, `ferrule doctor` and the dashboard
  show the running version, the last check and its result, the last update,
  and whether automatic updates are on.

## The `claude` CLI

The Claude plan runs Anthropic's `claude`, and a new model or a changed
stream format needs a recent one. With `[update] claude = true` (the
default) Ferrule updates it daily when npm's `latest` is newer, and at once
when a turn fails because it's too old or broken; that turn is then retried
once.

Ferrule never touches `claude`'s files itself. It tells how `claude` was
installed by where the binary lives and runs that install's own updater, as
the user who owns the files, with no input and a 5-minute limit:

| installed with | updated by |
|---|---|
| the native installer (`~/.local/share/claude/versions/`) | `claude update` |
| Homebrew | `brew upgrade --cask <cask>` |
| WinGet | `winget upgrade --id Anthropic.ClaudeCode …` |
| npm | `npm install -g @anthropic-ai/claude-code@latest` |
| pnpm | `pnpm add -g @anthropic-ai/claude-code@latest` |
| apt, dnf, apk, anything else | nothing: doctor and the owner notice give the command |

`DISABLE_UPDATES=1` in the environment stops it, and doctor says so.

A root-owned `claude` used by a user gateway is updated through the apply
unit (the gateway asks it and waits up to 5 minutes). The system apply unit
runs with `ProtectHome=read-only`, so it can't update a `claude` that lives
under `/home`; a system service can't run one from there anyway (its unit
hides `/home`), so install `claude` system-wide for a system service.

A routine update is quiet; a failed one is told once, until one works.
Doctor shows the installed and latest versions and how it gets updated.

## The ChatGPT plan's client version

OpenAI's backend refuses newer models to an old Codex client. Ferrule sends
the version the real Codex CLI has now: learned from npm's `@openai/codex`
`latest` (then GitHub's latest `openai/codex` release, then a compiled-in
version), cached a day in `<data>/codex-client-version.json` and refreshed
in the background. A "requires a newer version of Codex" answer refreshes
it at once and retries once. `FERRULE_CODEX_CLIENT_VERSION` overrides it.

## Self-repair

### When a model call fails

Every failure is classified (a refused key, an expired sign-in, a usage
limit, a gone model, a too-old client, a full disk…). Every error that came
from the provider now tries the M25 escalation and then the
[fallback list](models.md) before you see anything; the failed model stays
down for as long as its failure says (a usage limit until it resets). Only
local failures, like a full disk, don't fall over. When nothing is left, the
chat gets plain words and the raw error, cut short:

```
I couldn't reply: the plan's usage limit is reached. Set `[models] fallback` …

The error: provider error: …
```

### A config that stops reading

After every start that read the config, the gateway keeps a copy,
`<data>/gateway/config.last-good.toml`. If an edit breaks the config, the
gateway doesn't crash-loop: it runs on that copy, tells you what's wrong
with the file, and checks it every 30 seconds. A service then restarts onto
the fixed file by itself; a gateway run by hand tells you to restart it.
With no copy (the very first start), it logs the error once and waits for
the file to read.

### The self-check

Two minutes after start, then every 15 minutes, the gateway looks for:

- the data dir not writable, or under 500 MB free;
- update checks failing for 3 days, or a rolled-back version still pinned;
- a plan whose sign-in has expired or run out;
- `claude` missing or not running (it tries one update first);
- a polling channel that hasn't reached its server for 10 minutes.

It tells you about a new problem once ("Self-check: • …") and once when it
clears ("• fixed: …"); an unchanged set says nothing. The last set is in
`<data>/selfcheck.json`; doctor and the dashboard show it.

### The repair log

Every repair, automatic or from the self-check, is one line in
`<data>/repairs.jsonl` (`{at, kind, action, ok, detail}`, the last 500).
`ferrule doctor` and the dashboard show the last ten.

## Network

On a machine with an outbound allowlist, updates need:

- `api.github.com`, `github.com` and `objects.githubusercontent.com`
  (`release-assets.githubusercontent.com` too): Ferrule's releases, and the
  Codex version's second source;
- `registry.npmjs.org`: the Codex version, and `claude`'s latest version and
  its npm or pnpm update;
- whatever `claude`'s own updater and Homebrew or WinGet use, for those
  installs.

This is Ferrule's own traffic, not the agent's; the [egress policy](egress.md)
doesn't apply to it. Behind a TLS-inspecting proxy, `FERRULE_EXTRA_CA`
names its PEM bundle.

## Live tests

The test suite never reaches the network. These `#[ignore]`d tests do, by
hand:

```sh
cargo test -p ferrule-providers --test live codex_client_version -- --ignored --nocapture
cargo test -p ferrule-cli update::tests::live_the_real_release_list_parses -- --ignored --nocapture
```

The first learns the Codex version from the real npm and GitHub; the
second reads the real release list and a release's assets.
