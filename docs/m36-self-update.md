# M36 — Self-update and self-repair (design)

Status: built, 2026-09-27, branch `m36-self-update`. Written before
the code; where the build departs from it, see **As built** at the end.
User guide: [updates.md](updates.md).

Max's bot stopped answering on a ChatGPT plan with HTTP 400 "The
'gpt-5.6-sol' model requires a newer version of Codex". v0.5.1 fixed it by
hand: it pinned the Codex client version to 0.157.1. The same thing will
happen again the next time OpenAI raises a model's minimum, and it will
happen to the `claude` binary and to Ferrule itself. Max wants no SSH and
no terminal: Ferrule keeps itself, `claude` and the ChatGPT client identity
current, and repairs what it can recognise.

Four parts, each its own commit:

1. The ChatGPT client identity stays current by itself (§1).
2. `ferrule update`, and automatic updates for the running service (§2–§4).
3. The `claude` CLI stays current (§5).
4. Self-repair: a failure classifier with repair actions, fallback for
   every provider error, a periodic self-check, a repair log (§6–§7).

---

## 1. The ChatGPT client identity

### 1.1 What the Codex client sends

Read at openai/codex tag `rust-v0.157.1`, commit
`36650394c5b38c2990ccf2a3457165ca3e9d9726` (the M35 pin in `codex.rs`,
`67a709665ac7b50311b93e32612c9a8281684787`, is older; the comment moves to
the new one):

- `version: <version>` on every backend request, `/responses` and
  `/models` alike: the raw package version (`0.157.1`).
- `GET /models?client_version=<MAJOR.MINOR.PATCH>`: the same version with
  any pre-release suffix cut off.
- `User-Agent: codex_cli_rs/<version> (<os> <os version>; <arch>) <terminal>`.
  The version is in the UA too. Ferrule sends `ferrule/<ver> (<os>; <arch>)`
  today, which says "not Codex" and carries no client version. From M36 it
  sends `codex_cli_rs/<version> (<os>; <arch>) ferrule/<ver>`: the same
  version in both places, and still honest about what is calling.
- `originator: codex_cli_rs` (already sent). No version field in the body.
- The other `/responses` headers (`session-id`, `thread-id`, `x-codex-*`,
  `ChatGPT-Account-ID`, `X-OpenAI-Fedramp`) carry no version.

`minimal_client_version` (per model, in `/models` and in the bundled model
list) is enforced by the server only; the Codex client never compares it.
Bundled minimums at the pin: gpt-5.5 0.124.0; gpt-5.6-sol/terra/luna
0.144.0; gpt-6-astra 0.153.0; gpt-6-sol/luna 0.155.0.

### 1.2 Where the latest version comes from

| order | source | why |
|---|---|---|
| 1 | npm `GET https://registry.npmjs.org/-/package/@openai/codex/dist-tags`, field `latest` | ~600 bytes, no auth, no rate limit worth mentioning, and `latest` is exactly what `npm i -g @openai/codex` installs, so it is the version real users run. |
| 2 | GitHub `GET https://api.github.com/repos/openai/codex/releases/latest`, `tag_name` minus `rust-v` | Independent of npm. Unauthenticated calls are limited to 60 an hour per IP, which a daily check never nears. `/releases?per_page=N` is not used: it is dominated by alphas and isn't sorted. |
| 3 | the compiled-in `CLIENT_VERSION` | Offline, or both sources broken. |

A version is accepted only if it parses as semver with no pre-release part
(never an alpha) and is not lower than the compiled-in constant (a source
that goes backwards is ignored, so a stale mirror can't downgrade us).
`FERRULE_CODEX_CLIENT_VERSION` still wins over everything and turns the
fetching off.

### 1.3 The cache

`<data>/codex-client-version.json`: `{version, checked_at, source}`. Read at
start, refreshed when older than 24 hours, written atomically. The refresh
runs in the background after the first use (a turn never waits on npm),
except for the reactive refresh below. A cache file that doesn't parse is
ignored and rewritten.

### 1.4 Reactive repair

A 400 whose body says a model needs a newer client is recognised by shape,
not by the exact sentence: an error object whose message mentions
"requires a newer version" or "newer version of codex", or whose `code` /
`type` names an outdated or unsupported client version, or which has a
`minimal_client_version`/`min_client_version` field. On it the provider:

1. refreshes the version right away, bypassing the cache TTL;
2. if the version changed, retries the request once with the new one;
3. otherwise, or if the retry fails the same way, returns a classified
   `client_too_old` failure (§6), so M21 falls back to the next model and
   the owner gets one plain line.

### 1.5 Model lists

`/models` is asked with the current version, and entries whose
`minimal_client_version` is above it are dropped. The built-in list used
before sign-in is filtered by the bundled minimums of §1.1. Setup, `/model`
and the dashboard all read through `chatgpt_models`, so they all offer only
models the current version can use.

---

## 2. Ferrule updates: the release, the checks, the signature

### 2.1 Source

GitHub releases of `maximarhipkin/ferrule`, the same assets `release.yml`
builds and `install.sh`/`install.ps1` download:

| target | asset |
|---|---|
| x86_64/aarch64 Linux | `ferrule-<arch>-unknown-linux-musl.tar.gz` |
| x86_64/aarch64 macOS | `ferrule-<arch>-apple-darwin.tar.gz` |
| x86_64 Windows | `ferrule-x86_64-pc-windows-msvc.zip` |

each with `<asset>.sha256` (`<hex>  <asset>`) and, from M36,
`<asset>.minisig`. The target is compiled in (`env!("TARGET")` from a build
script), so an update is always the same build flavour that is running.

The list comes from `GET /repos/maximarhipkin/ferrule/releases?per_page=30`:
drafts are skipped always, pre-releases unless `[update] channel =
"prerelease"`, and the highest semver wins (not the newest by date).
`--to vX` fetches `/releases/tags/vX`. The API base URL can be replaced only
in tests (`FERRULE_UPDATE_BASE` read only by `#[cfg(test)]`-gated code and
the test harness's hidden flag, never in a release build's normal path).

### 2.2 Signatures

**Format: minisign.** It is small, documented, has a pure-Rust zero-dependency
verifier (`minisign-verify`), and its signature can carry a *trusted
comment* that is itself signed. Ferrule's releases sign in the prehashed
`ED` mode (Ed25519 over BLAKE2b-512 of the file).

**The trusted comment is `ferrule <tag> <asset>`**, and the updater checks it
equals exactly what it asked for. A signature is therefore bound to one tag
and one asset: an old signed archive can't be replayed as a newer tag, and a
macOS archive can't be served to Linux.

**Keys.** The keypair is generated once (OpenSSL Ed25519). Only the public
key is committed, in `crates/ferrule-cli/src/update/release.pub`, and
compiled in with `include_str!`. The private key goes to the repository's
Actions secret `FERRULE_SIGNING_KEY` (PEM), set through the REST API sealed
with the repository's public key (a libsodium sealed box), and a copy is
kept outside the repo, mode 600. The public key file may list more than one
key: rotation ships a binary that trusts old and new keys, then signs with
the new one.

**Signing in CI.** A step in `release.yml`'s release job, after the
artifacts are downloaded and before `gh release create`, signs each archive
with OpenSSL (no extra tool to install): `openssl dgst -blake2b512 -binary`,
then `openssl pkeyutl -sign -rawin` over the hash, then again over
`signature || trusted comment` for the global signature, then writes the
two-line minisign file. When the secret is absent the step prints a notice
and signs nothing; when it is present, any failure fails the release.

**Policy.** From M36 every release is expected to be signed, so the updater
requires a valid signature for every release it installs, with one
exception set by the facts on the day: if the secret could not be set (§8
records which), the updater requires signatures only once it has seen a
signed release (a sticky flag in the update state) or when the candidate
release carries `.minisig` assets. Either way a present-but-invalid
signature is always a refusal.

### 2.3 Checks, in order

1. the release is not a draft, not a pre-release (unless the channel
   says so), its version is higher than the running one (unless `--to`),
   and it is not pinned (§3.4);
2. the archive's size is under 200 MB and its sha256 equals the `.sha256`
   file's;
3. the minisign signature verifies against a compiled-in key and its
   trusted comment is `ferrule <tag> <asset>`;
4. the extracted binary runs: `<new> --version` prints `ferrule <version>`
   within 10 seconds, and `<version>` is the release's.

### 2.4 Threat model

| threat | what stops it |
|---|---|
| A mirror or proxy that serves a modified archive | sha256 and the signature. A proxy that also rewrites the `.sha256` file still can't sign. |
| A compromised release (assets replaced after the fact by someone with only `contents: write`) | The signature: the key is not in the repo and not in the release. |
| A downgrade: an attacker serves an older, genuinely signed release as the latest | The updater never installs a version at or below the running one, except with an explicit `ferrule update --to`, which asks. The trusted comment binds the signature to its tag, so an old archive can't pose as a new tag. |
| A compromised GitHub account (can push a workflow that signs anything) | **Not stopped.** An attacker who can run workflows with the secret can sign. Mitigations out of scope for M36: an `environment` with required reviewers guarding the secret, or signing offline on Max's machine. The updater at least makes such a release traceable (every install and its signature key id is recorded). |
| A freeze: GitHub or a proxy withholds new releases | Not stopped; doctor shows the date of the last successful check, and the self-check tells the owner when checks have failed for three days. |
| The unprivileged daemon (or a tool it runs) tries to steer the privileged half | The privileged half reads only a small request file, never follows a symlink, re-downloads and re-verifies everything itself, and writes its state where the daemon can't (§3.2). |

---

## 3. Installing: who swaps the binary

### 3.1 The permission problem

A system install is `/usr/local/bin/ferrule`, owned by root, run by the
unit as the `ferrule` user with `ProtectSystem=strict` and
`NoNewPrivileges=yes`. The gateway can't write the binary, can't gain
privileges, and can't restart its own unit. Loosening any of that would
hand the same powers to every command the agent runs (the service's user
is the sandbox's floor). So the half that swaps and restarts must be a
different process with more rights, started by the service manager, not by
the gateway.

### 3.2 The design: an apply unit beside the service

Setup installs, beside `ferrule.service`, a oneshot **`ferrule-update.service`**
that runs `ferrule update --apply` and two triggers for it:

- **`ferrule-update.timer`**: daily, `RandomizedDelaySec=6h`,
  `Persistent=true` (the "about daily, with jitter" check);
- **`ferrule-update.path`**: `PathExists=<data>/update/request` (the daemon
  asks for an immediate run: a consent button, `/update` in a chat, or the
  claude repair of §5.3).

System scope: the apply service runs as root with `NoNewPrivileges=yes`,
`PrivateTmp=yes`, `ProtectHome=read-only`, `ProtectKernelTunables=yes`,
`ProtectControlGroups=yes`, `RestrictSUIDSGID=yes`, a short
`TimeoutStartSec` and the same `FERRULE_CONFIG`/`FERRULE_DATA_DIR`. It is
not `ProtectSystem=strict`: the point of the unit is to write
`/usr/local/bin` and, for `claude`, wherever its package manager installed
it (§5), and it only ever runs the signed `ferrule` binary with a fixed
argument list. User scope: the same three units in
`~/.config/systemd/user`. macOS: a launchd agent `ai.ferrule.update` with
`StartCalendarInterval` (daily, plus a random minute chosen at setup) and
`WatchPaths` on the request file.

Why not let the gateway spawn the apply process directly? systemd and
launchd both kill a service's children when it restarts, and the apply
process must outlive the restart it causes, to watch the new binary's
health and roll back.

Why a root unit and not sudo, polkit or a setuid helper? A oneshot unit is
the smallest privileged surface: no password, no rule file that grants the
`ferrule` user anything, no setuid binary. The only input from the
unprivileged side is "please run now".

**What `--apply` does**, in one process, holding
`<state>/apply.lock`:

1. deletes the request file (read first: `{ferrule: bool, claude: bool,
   to: null}`, at most 4 KiB, opened with `O_NOFOLLOW`, anything else
   ignored);
2. checks for a Ferrule release (§2) unless automatic updates are off and
   the request didn't ask for one; downloads and verifies into a fresh temp
   dir;
3. updates `claude` if asked or due (§5);
4. **waits until idle**: `<data>/gateway/running.json` fresh and listing no
   busy turns (it is rewritten whenever a turn starts or ends), polling for
   up to 6 hours, then gives up until the next run;
5. swaps (§3.3), restarts the service, and waits up to 180 seconds for the
   health marker (§3.4);
6. records an event in the state file.

**State** lives where the daemon can't write: `/var/lib/ferrule/update/`
(root, 0755, file 0644) in system scope; `<data>/update/` in user scope
(the user owns both halves there anyway). `state.json`: `{last_check,
last_check_ok, latest, signed_seen, pinned: [..], events: [{id, kind,
from, to, notes, at}]}` with the last 20 events. The daemon reads it for
status, doctor, the dashboard and the owner notices.

### 3.3 The swap

A copy of the new binary goes beside the old one (`.ferrule.new`, same
directory, so the rename can't cross filesystems), is made executable, and
is renamed over the old one: a running process keeps its open file and
never sees a half-written one. Before that the old binary is hard-linked
(or copied) to `ferrule.previous` in the same directory. On Windows a
running exe can't be replaced but can be renamed, so the running
`ferrule.exe` is renamed to `ferrule.exe.<8 hex>.old` (the name
`install.ps1` uses) and the new one takes its place; old `.old` files are
deleted at the next start.

### 3.4 Health and rollback

The new gateway writes `<data>/gateway/running.json` with its version every
few seconds (M33). The apply process calls the new binary healthy once the
marker names the new version, its pid is alive, and it has been up for at
least 30 seconds with the marker fresh. If that doesn't happen within 180
seconds (the old service's `WatchdogSec` is 120), the apply process moves
`ferrule.previous` back, restarts the service, **pins** the bad version
(`pinned` in the state; it is never retried automatically, `ferrule update
--to` clears it), and records a `rolled_back` event.

### 3.5 Without a service manager

Windows has no service in Ferrule, and a gateway can be run by hand. There
the gateway checks daily itself and tells the owner once per new version:
"Ferrule v0.6.0 is out: run `ferrule update`". `ferrule update` then does
everything in the foreground: check, download, verify, swap (on Windows by
rename-then-replace), and on Unix restarts nothing (it says what to
restart). The rollback is not supervised there: nothing restarts the
process to watch it.

### 3.6 Owner notices

The daemon keeps `<data>/update/told.json`, the id of the last event it has
told. At start, and after each check, it tells the owner about each newer
event once:

- updated: "Updated Ferrule 0.5.1 → 0.6.0: <first line of the release notes>"
- rolled back: "Ferrule 0.6.0 didn't start properly, so I went back to
  0.5.1 and won't try 0.6.0 again. `ferrule update --to v0.6.0` retries it."

Nothing is said otherwise (a quiet check, no update, an update of `claude`
that worked).

---

## 4. Commands, config, surfaces

```
ferrule update            # check, download, verify, install, restart the service
ferrule update --check    # say what's available, change nothing
ferrule update --to v0.6.0   # a specific release, even an older one (asks)
ferrule update --apply    # the privileged half (run by the update unit)
```

`ferrule update` as root with a system service does the work itself; as a
user who can't write the binary it says to use `sudo`. With a service
installed it restarts the service and supervises the rollback as in §3.4.

```toml
[update]
auto = true          # default: on for a service setup installed
channel = "stable"   # or "prerelease"
claude = true        # keep the claude CLI current too
```

`auto` unset means on when the update unit is installed, off otherwise.
Setup's service step says so in one line and offers to turn it off. With
`auto = false` the daily check still runs and offers the update to the
owner as an M19 approval button (Allow writes the request file).

`ferrule status`, `/status`, doctor and the dashboard show: the running
version, the last check and its result, the last update, and whether
automatic updates are on; doctor also shows the `claude` installed and
latest versions, and the repair log's last entries.

`install.sh` on an upgrade (a service unit exists) runs
`ferrule setup --refresh-service`, which rewrites the service's units from
what it already knows (the pinned config and workspace) and adds the update
units, asking nothing. Existing v0.5.x installs need the install one-liner
once; from then on updates are automatic.

---

## 5. `claude` stays current

### 5.1 Detecting the install method

The configured binary is found (`cli::find`), canonicalised (the npm and
pnpm shims are followed to their target), and matched, the way Claude's
own `detectPackageManager` does
([setup](https://code.claude.com/docs/en/setup),
[troubleshooting](https://code.claude.com/docs/en/troubleshoot-install)):

| method | recognised by | update |
|---|---|---|
| native installer | `~/.local/share/claude/versions/` | `claude update` |
| Homebrew | `/Caskroom/<cask>/` | `brew upgrade --cask <cask>` |
| WinGet | `Microsoft\WinGet\Packages` or `Links` | `winget upgrade --id Anthropic.ClaudeCode --exact --silent --disable-interactivity` |
| npm | `node_modules/@anthropic-ai/claude-code` | `npm install -g @anthropic-ai/claude-code@latest` |
| pnpm | the same, under a `.pnpm` store | `pnpm add -g @anthropic-ai/claude-code@latest` |
| apt, dnf, apk | `/usr/bin/claude` owned by the package manager | none: tell the owner the command |
| unknown | anything else | none: tell the owner |

(`claude update` itself calls npm even for a pnpm install, so pnpm gets its
own command.) The latest version is read from npm's dist-tags for
`@anthropic-ai/claude-code` (`latest`), the channel native installs
default to. `DISABLE_AUTOUPDATER=1`, which Ferrule sets for the child,
stops only Claude's background check; `DISABLE_UPDATES=1` blocks every
path, and the updater says so instead of failing silently.

### 5.2 Who runs the update

The update command runs as the **owner of the installed files**. In the
apply unit (root) that means dropping to the owner's uid and gid when the
install isn't root's own; in the gateway (user scope) it runs directly
when the files are writable, and otherwise writes the request file so the
apply unit does it. Ferrule never writes, patches or moves the `claude`
binary or its files; only Claude's own updater or the package manager
does.

### 5.3 When

Daily, in the apply run (§3.2), when `[update] claude` is on. And at once
when a turn fails because `claude` is too old (stderr "needs an update. A
newer version (...) is required", or the stream-json reason
`cli_version_too_old`) or broken ("native binary not installed", missing
binary): the engine asks the repairer, which updates (directly or through
the request file, waiting up to 5 minutes for the apply run's event), and
the turn is retried once.

### 5.4 A Ferrule-managed `claude`?

**No.** Anthropic's native installer already updates itself with a signed
manifest, and the npm, Homebrew and WinGet installs have their own paths;
a second, Ferrule-owned copy would be one more `claude` on PATH for the
user to confuse with theirs, and one more binary Ferrule would have to
download and verify. Setup keeps doing what M35 does: it points at the
official installer. The apply unit covers the one case that needs help,
a root-owned install run by another user.

---

## 6. Self-repair: the classifier

### 6.1 Shape

`ferrule_core::failure`: a `Failure { kind, message, retry_after }`, carried
by a new `CoreError::Failed(Failure)` whose display is the same
`provider error: <message>` the text-only errors have, so logs and
existing matches read the same. `failure::classify(&CoreError) -> Kind`
uses the structured kind when the driver set one, and falls back to the
error text (the M21 `FailureClass` rules, which it subsumes).

Each kind has a repair, whether it may be retried, whether the call falls
back, the owner line and the chat words:

| kind | detected by (structure first) | repair | owner line |
|---|---|---|---|
| `client_too_old` (Codex) | 400 body shape (§1.4) | refresh the client version, retry once | "ChatGPT said Ferrule's Codex client version is too old for <model>; I refreshed it…" (only if the repair failed) |
| `claude_too_old` | stream-json `cli_version_too_old`; stderr text | update `claude`, retry once | "`claude` was too old; I updated it to <v>." / how to update it by hand |
| `claude_missing` | spawn `NotFound`; "native binary not installed" | re-find `claude` on PATH and known install dirs, retry once | where it went, or the install command |
| `signin_expired` (ChatGPT, Claude) | refresh 401/`invalid_grant`; Claude's "not logged in"/401 | none | the command to run; for ChatGPT also `/login chatgpt` from Telegram (device code); for Claude only the server (its sign-in never works from a chat) |
| `usage_limit` | 429 `usage_limit_reached`; Claude's limit event | none (falls back until the reset) | the reset time |
| `rate_limited`, `overloaded`, `server`, `timeout`, `connect` | status / transport | the existing retries | M21's "isn't answering" line |
| `model_gone` | 404, "model not found / does not exist / deprecated" | refresh the model list, fall back to the closest configured fallback | "<model> is gone; <fallback> answers now. `ferrule model default` changes it." |
| `context_too_long` | M21 rules | none (falls back: a bigger window may fit) | none |
| `auth` (API key) | 401/403 | none | "the key for <provider> was refused" |
| `disk_full`, `data_unwritable` | `io::ErrorKind` (`StorageFull`, `PermissionDenied`, `ReadOnlyFilesystem`), ENOSPC/EROFS text | none | plain words, once |
| `unknown` | anything else | none | none |

At most one retry per repair per call; a repair that already ran in this
turn isn't run again.

### 6.2 Fallback for every provider error

Today M21 falls over only on transient errors. From M36 every
provider-origin error (`Provider`, `Transient`, `MalformedResponse`,
`Failed`) first tries the M25 escalation, then M21's fallback list, before
the user sees a failure. Each kind sets how long the failed model stays
down (a usage limit until its reset, a sign-in until it's fixed, a gone
model a day, others the M21 default). Only when nothing is left does the
chat get plain words, with the raw error cut to 400 characters beneath.
`router::failure_text` becomes a call to the classifier.

### 6.3 The config, the disk, the data dir

- **Config unreadable at gateway start**: the gateway doesn't exit into a
  five-second restart loop. With a last good copy
  (`<data>/gateway/config.last-good.toml`, written after every successful
  start), it runs on that and tells the owner what line is wrong; with none,
  it logs the error once and waits (checking the file every 30 seconds)
  instead of exiting.
- **Disk full / data dir unwritable**: the classifier's plain words; the
  self-check reports it once; nothing crash-loops on it (the status writer
  already only logs).

---

## 7. The self-check and the repair log

A task in the gateway runs every 15 minutes (and 2 minutes after start). It
collects **problems** as `key → line`, reusing doctor's checks where they
are cheap and offline-safe:

- plan sign-ins: the ChatGPT token's refresh state, the Claude sign-in
  state (the last turn's result, not a new call);
- `claude`: found, runs `--version`, not below the last known minimum;
- updates: the last check failed for 3 days, a rollback is pinned;
- disk: under 500 MB free on the data dir's filesystem; the data dir
  writable;
- channels: disconnected for more than 10 minutes (the health report).

It repairs what it can (re-find `claude`, refresh the Codex version,
request a `claude` update), and compares the set with the last one
(`<data>/selfcheck.json`): a new problem is told to the owner once, a
cleared one gets "fixed: …" once, an unchanged set says nothing.

Every repair attempt, automatic or from the self-check, appends one line
to `<data>/repairs.jsonl`: `{at, kind, action, ok, detail}` (the file is
cut to its last 500 lines). Doctor and the dashboard show the last ten.

Actions that need consent (installing an update when `auto` is off) go to
the owner as M19 approval buttons through `hub.ask_owner`.

---

## 8. Failure modes

| what | what happens |
|---|---|
| npm and GitHub unreachable | Codex: the cached version, else the constant. Updates: the check fails, state records it, the self-check tells the owner after 3 days. |
| A release without a `.minisig` | Refused once signatures are required (§2.2); `ferrule update --check` says why. |
| The new binary crash-loops | Rolled back within 180 s, pinned, owner told. |
| A turn starts between the idle check and the restart | It's cut off; M33's restart notice tells that chat its turn was interrupted. The window is the few milliseconds of the restart call. |
| The apply unit is missing (an install from before M36) | The daemon's check still runs and tells the owner once: re-run the install one-liner. |
| `claude update` needs a password or a TTY | It runs with stdin closed and a 5-minute timeout; a failure is logged and told. |
| Two apply runs at once | A lock file; the second exits. |

## 9. Tests (hermetic)

No test reaches GitHub, npm or OpenAI: each has a local mock server with an
injectable base URL.

- Codex: learned from npm, cached, refreshed after the TTL, offline falls
  back to the cache then the constant, the env var wins; a backend that
  answers "requires a newer version" gets exactly one refresh and one
  retry; failing again falls back to the next model.
- Updater: a good release installs; a bad checksum, a bad or missing
  signature, a downgrade and a pre-release don't; the swap keeps the
  previous binary; a failed health check rolls back and pins; nothing
  installs while a turn is busy; the owner notice is sent once; on Windows
  a running exe is swapped.
- `claude`: `fake-claude` reports a version and "updates" itself; too old →
  update → retry.
- Classifier: a table of every known shape with its repair and owner text;
  unknown falls back; a fallback that also fails ends in plain words.
- Self-check: tells only on a change.
- Live (`#[ignore]`, run by hand, documented in updates.md): the real
  latest Codex version, the real latest Ferrule release and its signature.

## 10. Out of scope

- A Windows service, and so unattended updates on Windows.
- Delta updates, and mirrors.
- Guarding the signing secret with a GitHub environment with reviewers, or
  offline signing (§2.4).
- Updating MCP servers, plugins or the local model runtime.
- A Ferrule-managed `claude` (§5.4).

---

## As built

Where the code departs from the design above, or settles what it left
open:

- **Parts 1–3 as designed**, plus: `ferrule update --to <tag> --unsigned`
  installs a release from before signing, checked by its checksum only
  (the way back to v0.5.x); a glibc build updates to the musl archive,
  the only Linux one released; `systemctl`, `loginctl` and `launchctl` no
  longer inherit `NOTIFY_SOCKET` (a systemd tool reported `ERRNO=` on the
  gateway's watchdog socket).
- **No `/update` chat command.** The request file is written by the
  consent button (an M19 approval when `auto = false`) and by the claude
  repair. A chat command is a follow-up.
- **Every provider error falls back (§6.2), a refused key included**: a 401
  on one provider shouldn't silence the bot when a fallback has its own
  key. The owner is told once, "its key was refused". Local errors from the
  provider path (`disk_full`, `data_unwritable`) don't fall over: no other
  model fixes them. The chat's last words are
  `I couldn't reply: <plain words>\n\nThe error: <raw, 400 chars>`
  (`failure::chat_text`); an unknown local error reads "something went
  wrong."
- **Repairs wired**: `client_too_old` (Codex refresh and one retry, §1),
  `claude_too_old` and a broken `claude_missing` (update and one retry;
  a `claude` that isn't there at all is looked for again on the next
  spawn, not updated). `model_gone` has no repair of its own: the model is
  down a day and the fallback answers. Each repair is one line in the
  repair log, which lives in `ferrule-core` (`repairs.rs`) so the providers
  can write it; the file is cut in batches (at 550 lines, back to 500).
- **Last-good config (§6.3)**: `gateway/config.last-good.toml` is written
  (mode 600, it can hold secrets' names) after every start that read the
  config. Running on the copy also applies to every later
  `Config::load()` in that process, not just the gateway's first read,
  since 38 call sites (self-extension, the dashboard, tools) reload it. A
  file that reads again makes a **supervised** gateway (systemd's
  `INVOCATION_ID`, launchd's `XPC_SERVICE_NAME`) exit 0 so its manager
  restarts it onto the fixed file; an unsupervised one tells the owner to
  restart it.
- **Self-check (§7)**: its first round tells the problems already present
  (after a restart, the owner hears them once, not never). An update check
  that has never succeeded isn't "failing for 3 days" (`last_ok` unset).
  The Claude sign-in is read from the plan's saved state, not asked of
  `claude`. It tries a `claude` update only when the problem is new, then
  reports it. The Codex refresh stays reactive (a turn's 400), not a
  self-check action. A channel counts as down when it polls and its last
  good poll is over 10 minutes old; push channels (webhooks) aren't
  judged.
- **Disk space**: `statvfs` on Unix, `GetDiskFreeSpaceExW` on Windows.
- **Windows** has no supervised gateway, so a config that reads again
  only gets the "restart ferrule" line there.
