# M13 — self-extension: design

Status: design for the build on branch `m13-self-extension`. Written before the code, kept
true as the parts land. Sources: `docs/roadmap.md` §M13, `docs/research-autonomy-and-self-extension.md`
§4, `docs/research-number-one-harness-strategy.md` §4.5/§4.8/§6.2, and Max's msg 3074
("an allow-list of approved sources installs without asking; anything else needs the owner").

The goal in one line: **the agent can add an MCP server or a skill while it runs, and use it in
the same session — from sources the owner approved in advance without asking, from anywhere
else only after the owner says yes, and never with a tool description that tries to steer it.**

Decisions for Max are marked **[Max]**. Each one has a default so the build doesn't wait on it.

Since M32 the same flow also installs WASM tool plugins (`plugin_add`, `plugin_remove`), with
their capabilities shown to the owner: `docs/plugins.md`, `docs/m32-wasm-plugins.md`.

---

## 1. What gets built

| Piece | Where | What it does |
|---|---|---|
| Dynamic tool layer | `ferrule-core::tool` | `ToolSource` trait; `ToolRegistry::attach(source)`. The registry asks each attached source for its tools on every `definitions()`/`call()`, so a tool that appears mid-run is in the *next* provider request. |
| `tools/list_changed` | `ferrule-mcp::client` | The stdout reader forwards the notification to a `watch` channel instead of dropping it. `McpClient::shutdown()` kills the server. `build_tools(client, infos)` builds tools from an already-connected client. |
| Live skills | `ferrule-skills` | `SkillsHandle` — a shared, swappable `SkillSet`. `live_tools(handle)` gives `activate_skill`/`read_skill_file` that read the current set on every call, so a skill installed mid-session is activatable without a restart. |
| Extensions | new crate `ferrule-extensions` | allow-list, pins, scanner, lockfile, approval queue, the `ExtensionManager` (hot-load, re-scan, removal) and the model-facing tools. |
| Wiring | `ferrule-cli` | `[extensions]` config section, `ferrule extensions …` subcommand, the manager replaces the one-shot `connect_mcp_servers` loop. |

The new crate keeps the edits to files M12 also touches (`ferrule-cli/src/{main,config}.rs`,
`Cargo.lock`, `PLAN.md`, `docs/roadmap.md`) small: a config field, a module declaration, the
subcommand arm and the swap from "loop over MCP tools" to "attach the manager".

## 2. The allow-list

### Format — default: publisher granularity

```toml
[extensions]
enabled = true
allow = [
  "npm:@modelcontextprotocol/*",          # anything this npm scope publishes
  "npm:@playwright/mcp",                   # one package, any version the agent pins
  "npm:some-server@1.4.2",                 # one package, only this version
  "pypi:mcp-server-fetch",                 # PyPI has no scopes: whole names only
  "git:https://github.com/anthropics/*",   # every repo of one org
  "git:https://github.com/me/tools@3f2a…", # one repo, one commit
  "url:https://mcp.example.com/*",         # remote (HTTP) servers under a prefix
]
```

An entry is `<kind>:<pattern>[@version]`. Kinds: `npm`, `pypi`, `git`, `url`.

- `/*` at the end means "anything under this prefix". It is the only wildcard.
- Git and URL hosts compare case-insensitively. A trailing `/` or `.git` is ignored. `file://`
  git URLs are accepted, for local mirrors and for the tests.
- **Refused when the config is loaded**, with a warning, not silently:
  - registry-wide entries: `npm:*`, `pypi:*`, `git:https://github.com/*`, `url:*`
  - `http://` URLs, and `git:` URLs that aren't `https://` or `file://`
  - a pypi pattern with a wildcard

**Why publisher granularity by default.** The roadmap's open question offers three choices:
package, publisher, or registry.
- *Registry* ("anything on npm") defeats the list. MCPTox measured a 36.5% average attack
  success rate for poisoned descriptions, and typosquats cost nothing on npm.
- *Package only* is safe but forces the owner to edit config for every new server from a
  publisher they already trust. That is the friction msg 3074 set out to remove.
- *Publisher* (npm scope, git org, URL prefix) matches how trust actually works: you trust
  `@modelcontextprotocol` or `github.com/anthropics`, not one package of theirs.

Single packages and exact versions stay available for tighter lists. **[Max]** to confirm, or
to narrow the default to package-only.

**Where the list may come from.** Only from the owner's config: `$FERRULE_CONFIG` (which
`--config` sets), or the global `~/.config/ferrule/config.toml`. A `ferrule.toml` in the current
directory is always ignored for `allow`, with a warning, because the agent can usually write it.
Otherwise the agent could allow-list itself.

## 3. Pinning and updates

Every install is pinned. The pin is part of the lock entry. What a pin is depends on the kind:

| Kind | Pin | How it's enforced |
|---|---|---|
| npm | exact version, **required** (`pkg@1.4.2`) | launched as `npx -y pkg@1.4.2`. No ranges and no `latest`, so the registry resolves nothing new later. |
| pypi | exact version, **required** (`pkg==0.6.2`) | launched as `uvx pkg==0.6.2`. |
| git | a full 40-hex commit SHA | `rev` may be a branch/tag at install time. It is resolved **once** to a SHA, then checked out into `<data>/extensions/src/<name>-<sha12>`. At every load, `git rev-parse HEAD` must equal the pin and `git status --porcelain` must be clean. Otherwise the server is not started. |
| url | none possible — the server is someone else's | the tool-surface digests below are the pin. |

Every kind also pins its **tool surface**. The lock stores a SHA-256 of each tool's
`(name, description, input schema)`, and of each skill's `SKILL.md`.
- An MCP server whose surface differs at the next start is suspended, not loaded. The same holds
  for a `tools/list_changed` notification mid-session. See §5.
- A skill whose `SKILL.md` digest differs at load is dropped from the set, with a warning.

This matters because an npm version is immutable but what the server *reports* isn't. The
server can compute its descriptions at runtime, and a URL server can change at any moment.

### Updates — default: never automatic

An update is a reinstall with a new pin. `mcp_add … replace=true`, or
`ferrule extensions approve` of a replacing request, goes through the same allow-list check,
approval and scan as a first install. Re-adding an installed name without `replace` is refused.
Nothing polls registries, and nothing moves a pin on its own.

Why: auto-update is exactly the rug-pull path. A trusted package is compromised and the next
version ships a poisoned description or a malicious `postinstall` (the 2025 npm worm pattern).
A manual update costs one tool call.

**[Max]** could later relax this to "patch-level updates of allow-listed packages auto-apply
after passing the scan". That is not built.

## 4. The scan

A deterministic, hand-written matcher in `ferrule-extensions::scan`. There is no regex crate,
no model call and no network.

**What it reads.**
- MCP: each tool's name, its description, and **every string anywhere inside `inputSchema`**
  (property descriptions, titles, enum values, defaults). Poison hides there too.
- Skills: the frontmatter name and description, the whole `SKILL.md` body, and every text file
  the skill directory bundles (references, scripts). Poison can sit one `read_skill_file` away.

**Normalisation first.** Lowercase; zero-width, bidi-control and Unicode tag characters
(U+E0000 block) removed; whitespace runs collapsed. So `I g​n o r e` spread across tricks
still matches, and the removed characters are themselves a finding.

**Rules.** Each is a list of phrases or a small hand check. Each has a severity.

| Rule | Severity | Flags |
|---|---|---|
| `override` | block | "ignore/disregard/forget (all) previous/prior/above instructions", "you are now", "new instructions", "system prompt" |
| `hidden-tag` | block | `<important>`, `<system>`, `</system>`, `[inst]`, `<\|im_start\|>`, `<instructions>` |
| `conceal` | block | "do not tell/mention/inform the user", "without telling the user", "the user must not know", "silently" |
| `secret-access` | block | paths: `~/.ssh`, `id_rsa`, `id_ed25519`, `.env` (as a token), `secrets.env`, `mcp.json`, `/etc/passwd`, `/etc/shadow`, `.aws/credentials`, `.netrc`, `keychain` |
| `secret-word` | warn | words: `api key`/`api_key`/`apikey`, `access token`, `credentials`, `password`, `secret key` — legitimate tools say these all the time |
| `cross-tool` | block (MCP only) | "before using any/this/other tool", "instead of", naming a built-in tool (`shell`, `write_file`, `read_file`, `edit_file`, `web_fetch`, `mcp__`), "when (the) … tool is called" |
| `exfil` | block | "send (it/them/the contents) to", "include … in the (sidenote\|parameter\|argument)", "append … to the url", "upload" + a URL |
| `invisible-chars` | block | any zero-width / bidi / tag character in the raw text |
| `padding` | block (warn for skills) | ≥ 40 consecutive whitespace characters in the raw text (pushes text off-screen) |
| `bad-name` | block (MCP only) | a tool name outside `[A-Za-z0-9_.-]`, or over 64 characters |
| `encoded-blob` | warn | a base64-looking run ≥ 120 chars |
| `too-long` | warn | a description over 4 000 chars |

Skills use the same rules except `cross-tool`, since a skill telling the model which tools to
use is its normal job. "Silently"/"secretly" and `padding` are also relaxed to warn for skills,
because documentation says them legitimately. Schema property *keys* are scanned too, with `_`
read as a space.

**Where it runs.**
1. At install, after the server started and answered `tools/list`, **before any tool is
   registered**. For skills, after the clone and before the copy into the skills dir.
2. At every restart, on the fresh `tools/list` (together with the digest check).
3. On every `notifications/tools/list_changed`: re-list, diff against the approved surface, and
   scan everything new or changed. **This is the hook M17 expects** ("a tool-list change
   mid-session is picked up and re-scanned").
4. For owner-configured servers (`[[mcp.servers]]`), at start and on `list_changed` as well.

**What happens on a hit.**
- **Install, block hit:** the install is refused as a whole — not "load the clean tools". The
  server process is killed, the checkout or staging dir is deleted, and nothing is written to
  the lock. The model gets `refused: scan flagged tool X (rule: override, conceal)`. **The
  flagged text is never echoed back** to the model, because the hit is the poison. The request
  is kept in the pending queue with its findings, so the owner can review it (and waive a false
  positive) with `ferrule extensions approve <id>`.
- **`list_changed` or restart introduces a block hit** on an installed or approved server: the
  whole server is **suspended**. Every one of its tools is unregistered at once, the process is
  shut down, and the lock entry is marked `suspended` with the reason. The model sees the
  tools disappear, and gets one line through `extensions_list`. Resuming takes
  `ferrule extensions resume <name>` (re-scan, owner confirms at a terminal).
- **Owner-configured server:** at start a hit is a loud warning and the tool loads anyway. The
  owner wrote that config by hand, and silently dropping their tool would be a worse failure.
  A block hit on `list_changed` *does* drop the changed tools, because that change was not
  what the owner configured.
- **Warn hits** never block. They are logged, shown in `extensions list` and in the approval
  prompt.

**False positives.** A description that legitimately says "instead of" will trip the list.
There is no separate `waive` command: `ferrule extensions approve <id>` and
`ferrule extensions resume <name>` show the hits with their text (to the owner, not the model),
and when any hit is a block the owner must type the word `waive` — not just `y` — to accept
them. Each accepted block hit becomes a waiver bound to that tool's surface digest, so any later
change to the description re-arms the rule. Waivers live in the lock, in the data dir, and the
model has no tool that writes them.

The scan is a filter, not a proof. It raises the cost of the known MCPTox/Invariant patterns
to zero-effort-detectable. A paraphrased attack can get through. The other layers — the
allow-list, the approval, the sandbox and the credential proxy — are the defence.

## 5. Hot-load lifecycle

```
mcp_add(name, source, …)
  │ parse source → kind, pin required? → validate name ([a-z0-9_-], unique)
  │ allow-listed? ── no ──► approval (§8) ──► pending id returned, nothing runs
  │ yes
  ▼
materialise:  npm/pypi → command line with the exact pin
              git      → clone into staging, checkout SHA, verify command is inside it
              url      → nothing
  ▼
start in the sandbox (same McpClient path as configured servers: Landlock/Seatbelt,
own state dir <data>/mcp/<name>, HOME etc. moved, proxy env)
  ▼
initialize + tools/list  (startup timeout ≥ 60 s — npx may download)
  ▼
scan every tool ── block hit ──► kill, delete staging, refuse (nothing persisted)
  ▼
write lock entry (pins + surface digests) atomically
  ▼
register tools as mcp__<name>__<tool> in the manager's live set
  ▼
spawn watcher: on list_changed → re-list → diff → scan new/changed →
               clean & only additive?  register the additions, update digests
               anything flagged?       suspend server (§4)
               tool removed?           unregister it
```

Skills are the same shape, without a process:
`skill_install(git_url, rev?, path?)` → allow-list/approval → clone → checkout the SHA →
locate `SKILL.md` (at `path`, or the repo root) → frontmatter parse → scan → copy the skill dir
into `<data>/extensions/skills/<name>/` → lock → `SkillsHandle::refresh()`.

**Additive `list_changed` on an installed server.** New tools that pass the scan are loaded and
their digests are added to the lock. This keeps servers that register tools lazily working. A
*changed* description of an already-approved tool is never auto-accepted, even if it scans
clean: any changed digest suspends the **whole installed server** until the owner resumes it.
A rug pull that the scan misses is still caught by the digest. (An owner-*configured* server has
no lock entry to suspend; it only drops the changed or flagged tools.)

## 6. How a new tool reaches the model mid-session

`Agent::request()` calls `self.tools.definitions()` on **every** provider call. The registry
now also asks its attached `ToolSource`s (the manager, and the live skills) each time. So:

1. Turn N: the model calls `mcp_add`. The tool returns after the server is up and scanned, with
   the new tool names in its result text.
2. Turn N+1: the same run's next provider request carries the new tool definitions. The model
   calls `mcp__<name>__<tool>` and the registry routes it to the manager.

No restart, no new session. Skills work the same way: `activate_skill`'s enum and lookup are
read from the live set.

Cost: the tools block changes, so the provider prompt cache is invalidated **once** from the
tools position on. That is accepted and noted in the research doc. Removal invalidates it
once too.

Static tools shadow dynamic ones with the same name. An extension can never replace `shell` or
`write_file`. Names are prefixed `mcp__<server>__`, and the server name is validated to exclude
`__`.

## 7. Removal

- `mcp_remove(name)` / `skill_remove(name)`: **only for things the agent installed**
  (`origin = agent` in the lock). Owner-configured servers can't be removed by the model.
- `ferrule extensions remove <name>` (owner) removes anything in the lock.
- MCP removal: unregister all its tools (gone from the next request), `shutdown()` the process
  (kill + wait), delete the lock entry, delete the git checkout. The state dir
  `<data>/mcp/<name>` is kept, like for configured servers, so a reinstall keeps its login.
  `--purge` deletes it too.
- Skill removal: delete `<data>/extensions/skills/<name>`, the lock entry, refresh the handle.
  If the skill was already activated in the session, its text stays in the transcript. The
  activation tool refuses it from then on.

## 8. Security model and the approval flow

**Who is trusted.**

| Actor | Trust |
|---|---|
| Owner (config file, CLI at a terminal) | full |
| Agent/model | untrusted for anything that widens its own powers |
| Allow-listed source | trusted to *install* without asking; its tool text is still scanned |
| Anything else | runs nothing until the owner approves |
| An approval relayed through the agent ("the owner said yes") | untrusted — ignored |
| A sub-agent (M12) | no extension tools at all; uses installed tools narrowed by its role (a verifier or read-only child: only tools that change nothing) |

**Approval flow — default: a queue the owner clears from the CLI.**

1. The agent calls `mcp_add`/`skill_install` for a non-listed source.
2. The manager writes a request to `<data>/private/extensions/pending/<id>.json` and returns
   `pending approval: <id> — ask the owner to run 'ferrule extensions approve <id>'`
   immediately. **Nothing is fetched, cloned or run.**
3. The owner runs `ferrule extensions pending` / `approve <id>` / `deny <id>`.
   - `approve` requires stdin to be a terminal. It fetches, starts and scans *in the owner's
     process* (in the sandbox), shows the scan with the flagged text, and asks for a y/N.
   - On yes it writes the lock. On no, or on a block hit that isn't waived, nothing is written.
   - `deny` deletes the request. Nothing was ever installed.
4. A running daemon (gateway or chat) polls the lockfile every ~2 s. When it sees a new approved
   entry it hot-loads it; when an entry is gone it unloads it. So the approved server appears
   in the agent's *current* session.

`ferrule chat` in a terminal additionally uses a **TTY approver**: the question appears inline
("Agent wants to install git:https://… (reason) — allow? [y/N]"), for the owner sitting there.
An inline yes installs only when the scan is clean; a block hit sends the request back to the
queue with its findings, for `ferrule extensions approve` and the `waive` word. The
approver is a trait (`Approver`), so the gateway channels (Telegram/WhatsApp) can plug in an
owner-only button later. That is not built, and it is an open edge.

**Why the agent can't approve itself.**
- The pending dir and the lock live under `<data>`. The sandbox makes `<data>/private` unreadable
  and `<data>` unwritable for the shell and for MCP servers (M6/M10: writes are confined to the
  workspace, temp dirs and each server's own state dir).
- No model tool writes the lock, the waivers or the allow-list.
- The allow-list is read only from the owner's config, never from the workspace.
- `approve` needs a TTY, so an agent shell running `ferrule extensions approve` fails even where
  the filesystem would allow it.

**Stated limits.**
- With `sandbox = off`, and on Windows (no sandbox yet), the agent's shell *can* write `<data>`
  and could forge a lock entry. The TTY check can be defeated with `script(1)`. The approval gate
  is only as strong as the sandbox. `doctor` already flags sandbox-off; the approval prompt
  repeats it.
- Code from an allow-listed source runs without the owner seeing it. That is the point of the
  list, and why the list is by publisher and not by registry.
- The scan is heuristic (§4).
- The sandbox still confines every installed server exactly like a configured one: no reads of
  hidden paths, writes only to its state dir and the workspace, network only through the
  credential proxy. A malicious server gets what a configured one gets, not more.

**Git-sourced commands.** A `git:` server's `command` must be a file inside the checkout, or an
interpreter from a fixed list (`node`, `python3`, `python`, `deno`, `bun`) whose first argument
after a short list of harmless flags (no `-c`, `-e`, `--eval`, `--require`, `-m`,
`--allow-run`) is a file inside the checkout. `uvx`/`npx` are not on the list: they fetch and run
code from a registry, which would bypass the pin. Otherwise `mcp_add` could run
`bash -c …` as a "server". That would be an arbitrary-command channel around the shell tool's
own policy.

**Self-written skills** (the Voyager part of the roadmap). The agent writes a draft to
`.ferrule/skill-drafts/<name>/SKILL.md` in the workspace, then calls
`skill_keep(name, check)`. `check` is a shell command, run in the sandbox with the draft dir as
cwd, which must exit 0: the skill's own test or example. Then the scan runs, then the dir is
copied into `<data>/extensions/skills/<name>` and locked with `origin = self`. A skill that
isn't verified isn't kept.
- No approval needed: it is the agent's own text, and it can already write the workspace.
- It is still scanned: a draft could have been assembled from a poisoned web page.

## 9. Persistence across restarts

- `<data>/extensions/extensions.lock.json` — one entry per installed server or skill, holding
  kind, source, pin, command/args (for MCP), origin (`agent` | `self` | `owner`), installed-at,
  surface digests, status (`active` | `suspended`), waivers.
- Writes: temp file in the same dir + `rename`. They are serialised by a `extensions.lock.json.lk`
  file created with `create_new`, taken over after 30 s as stale. The CLI and a daemon may write
  concurrently.
- Checkouts: `<data>/extensions/src/<name>-<sha12>/`. Installed skills:
  `<data>/extensions/skills/<name>/`, added as a discovery root after the project roots, so a
  project skill of the same name wins, as the existing precedence says.
- At startup the manager loads configured servers, then every `active` lock entry. For each:
  verify the pin (git HEAD and clean tree), start, re-list, compare the digests. On a mismatch it
  is suspended, not loaded. A failure to start is logged and skipped. One broken extension never
  blocks the agent.
- Installed things are **not** written into the owner's `config.toml`. The config stays the
  owner's hand-written file, and the lock is the machine-written one. `ferrule extensions list`
  shows both.

## 10. Failure modes

| Failure | Behaviour |
|---|---|
| Non-listed source | queued for approval; nothing runs |
| Owner denies | request deleted; nothing on disk, nothing in the lock |
| Missing or ranged npm/pypi version | refused: "pin an exact version" |
| Git rev doesn't resolve / clone fails | refused; staging dir deleted |
| Git command outside the checkout | refused before anything runs |
| Server doesn't start / `initialize` times out | refused; process killed; nothing persisted |
| Block hit at install | refused, whole install; flagged text not echoed |
| Block hit on `list_changed` | server suspended, tools unregistered mid-session |
| Changed surface at restart | server suspended before any tool is exposed |
| Git checkout drifted (HEAD ≠ pin, dirty tree) | not started; suspended |
| Name collision | refused, unless `replace=true` for an agent-installed entry |
| Server crashes later | existing respawn-once logic; the tools stay listed and calls return the error |
| Lockfile corrupt | extensions disabled for this run with an error; configured servers unaffected; file not overwritten |
| Lock contention | wait up to 5 s, then fail the install with a retryable error |
| `extensions.enabled = false` | the model gets no extension tools; configured servers still go through the manager (scan + `list_changed` re-scan); the lock is still loaded, so owner-approved installs keep working |
| Offline | npm/pypi installs fail at start (npx can't download); git from `file://` works; loaded extensions work if their packages are cached |

## 11. Config

```toml
[extensions]
enabled = false        # [Max] default off: six tool definitions cost tokens every turn,
                       # and self-extension should be something the owner turns on
allow = []             # §2
```

## 12. Tests (hermetic)

No network, no real registry. Fixtures: a python3 MCP server that can emit
`notifications/tools/list_changed`, local git repos built with the `git` CLI in a tempdir, and a
scripted mock provider.

1. An allow-listed `git:file://` server is installed through `mcp_add` and its tool is called in
   the **same** `Agent::run`, driven by the mock provider.
2. A non-listed source returns a pending id; denying it leaves no lock entry, no checkout and no
   process.
3. A poisoned tool description is flagged and nothing is registered or persisted.
4. A `tools/list_changed` that introduces a poisoned tool suspends the server mid-session.
5. A skill is installed from a local git repo and activated in the same session.
6. A pinned install doesn't drift: a new commit on the branch doesn't move the checkout, and a
   tampered checkout or a changed surface refuses to load at restart.

Plus unit tests for the allow-list parser and matcher, every scan rule and its normalisation,
lock round-trip and atomic write, and the registry's dynamic layer.

## 13. Not in M13

- Automatic updates (§3), channel approval buttons (§8), a remote allow-list or catalogue, model-
  assisted scanning, signature checks (npm provenance / sigstore). All are open edges in PLAN.md.
- M17's "MCP hot-add" UX beyond the hook: M13 gives it `ExtensionManager::add_server` and the
  `list_changed` re-scan.
