# Moving from OpenClaw or Hermes

`ferrule import` reads an OpenClaw or Hermes Agent home and brings over what
carries across: memories, skills, channel allowlists, the model provider, and
the names of the keys. It only reads files. It needs neither tool installed,
nor node or python, and it changes nothing in the source. The design and its
as-built notes are in [m33-ops.md](m33-ops.md) §3.

```bash
ferrule import openclaw                     # a dry run: what would change
ferrule import openclaw --apply             # write it
ferrule import openclaw --apply --bind-secrets   # and copy the keys it found

ferrule import hermes [--profile work] [--apply] [--bind-secrets]
```

Options:
- `--from <dir>` points at the home when it isn't in the usual place;
- `--workspace <dir>` (OpenClaw) picks the agent workspace;
- `--profile <name>` (Hermes) picks a profile other than the active one.

`ferrule setup` offers the same thing: when it finds either tool, its guided
flow starts with an **Import** step, and the menu has an **Import** item.

## What it looks like

```
Import from Hermes Agent (/home/me/.hermes) — dry run, nothing written; re-run with --apply to write

Memories: 2 to add, 0 already known, 0 to update (superseding the old import)

Skills: 0

Config (/home/me/.config/ferrule/config.toml):
  [gateway] telegram_allowed_chats += 42
  [providers.anthropic] claude-sonnet-4.6 at https://api.anthropic.com/v1 (key: $ANTHROPIC_API_KEY)
  default_provider = "anthropic"

Secrets (names only; values are never shown or put in the config):
  ANTHROPIC_API_KEY: found in .env; --apply --bind-secrets copies it
```

Run it again after `--apply` and every line says there's nothing left to do.
Every step is safe to repeat, so an import that stopped halfway is finished by
running it again.

## Where it looks

| | OpenClaw | Hermes Agent |
|---|---|---|
| Home | `$OPENCLAW_STATE_DIR`, `~/.openclaw` (`~/.openclaw-<profile>` with `$OPENCLAW_PROFILE`, `$OPENCLAW_HOME` in place of `~`), legacy `~/.clawdbot` | `$HERMES_HOME`, `~/.hermes`, `%LOCALAPPDATA%\hermes` on Windows; profiles in `<home>/profiles/<name>`, the active one named in `<home>/active_profile` |
| Config | `openclaw.json` (legacy `clawdbot.json`) or `$OPENCLAW_CONFIG_PATH`, JSON5 | `config.yaml` |
| Keys | literals and `${VAR}` in the config, `<state>/.env` | `<home>/.env`, `api_key:` in `config.yaml` |
| Memory | `<workspace>/MEMORY.md`, `USER.md`, `memory/YYYY-MM-DD*.md` | `memories/MEMORY.md`, `memories/USER.md` |
| Skills | `<workspace>/skills`, `<workspace>/.agents/skills`, `<state>/skills`, `~/.agents/skills` | `skills/<category>/<name>/` |

The formats were read from the source of **OpenClaw v2026.9.6** and **Hermes
Agent v2026.9.24** (both September 2026). Older or newer releases that moved a
file are reported as "not found" rather than guessed at.

## What maps to what

### Memories

They go into ferrule's memory store, where `memory` recall and `ferrule
memory` see them.

- **OpenClaw** `MEMORY.md` is split at top-level bullets, or at paragraphs
  when there are none, and a heading becomes the prefix of the entries under
  it (`Projects: …`). `USER.md` is split the same way and tagged `user`.
  Daily notes are prefixed with their date and tagged `daily`.
- **Hermes** files are split on their `§` separator. `USER.md` entries are
  tagged `user`.
- Every entry is tagged `import:<tool>` and `from:<file>`.

**Re-running:**
- an entry that's already a live memory is left as it is;
- an entry you edited in the other tool **replaces** the old import of it
  from the same file (the old one stays in history);
- an entry you deleted in the other tool stays in ferrule. Ferrule doesn't
  delete memories because the other tool did.

**Held back:** an entry that looks like it carries a secret isn't imported.
That's one with a key-shaped word (`sk-…`, `ghp_…`, `xox…`, `AKIA…`, …), one
the gateway's redactor would change, or one containing any key value the
import found. The summary lists it by file and line, never by content. The
check errs on the side of holding back, so a long hash or id can be held
back too: add such an entry by hand.

`SOUL.md`, `AGENTS.md` and `IDENTITY.md` aren't memories and aren't
imported. `AGENTS.md` is a file ferrule reads as project context too, so the
summary suggests copying it into your project.

### Skills

Each `SKILL.md` directory goes through the same review as a skill the agent
writes itself ([skills.md](skills.md)): the scan findings, bundled scripts and
requested tools are shown, and **you confirm each one at the terminal**. A
"no" installs nothing, and no flag skips the question. The dry run lists them
with their scan verdicts.

Skills are skipped, with the reason in the summary, when:
- there's no terminal (a script, CI);
- there's no ferrule config yet (run `ferrule setup` first);
- a skill of that name is installed already. To replace it, remove it with
  `ferrule extensions`, then import again.

Names are made to fit ferrule's rule (lowercase letters, digits, `-` and `_`,
up to 40 characters): `Weather Tool` becomes `weather-tool`. A name that's too
long, or that two imported skills share, gets a short hash of its path added.
What's installed is a copy with the new name; the source is untouched. The
source is recorded as `import:<tool>:<path>`.

### Channel allowlists

Ids only, **added** to what `[gateway]` already has:

| From | To |
|---|---|
| OpenClaw `channels.telegram.allowFrom` (and legacy `credentials/telegram-*-allowFrom.json`); Hermes `TELEGRAM_ALLOWED_USERS`, `TELEGRAM_GROUP_ALLOWED_CHATS` | `telegram_allowed_chats` |
| OpenClaw `channels.discord.allowFrom`; Hermes `DISCORD_ALLOWED_USERS` | `discord_allowed_users` |
| OpenClaw `channels.discord.dm.groupChannels`; Hermes `DISCORD_ALLOWED_CHANNELS` | `discord_allowed_channels` |
| OpenClaw `channels.slack.allowFrom`; Hermes `SLACK_ALLOWED_USERS` | `slack_allowed_users` |
| Hermes `SLACK_ALLOWED_CHANNELS` | `slack_allowed_channels` |

Not carried over, and said so:
- `"*"` and `ALLOW_ALL_USERS`: ferrule has no "anyone" allowlist, so you
  decide;
- Discord user **names** (Hermes resolves them at runtime; ferrule needs ids);
- OpenClaw's Telegram `groupAllowFrom` (user ids, where ferrule's list holds
  chat ids);
- OpenClaw's pairing approvals, which live in its SQLite database.

The token variable (`telegram_token_env = "TELEGRAM_BOT_TOKEN"`, …) is set
only when ferrule has none for that channel.

### Providers and the model

- The source's default model becomes a `[providers.<name>]` table: a known
  provider (`anthropic`, `openai`, `openrouter`, `gemini`, `ollama`, …) gets
  ferrule's preset, and a custom OpenAI-compatible or Anthropic-compatible
  endpoint gets its `base_url` and API style.
- A provider table you already have is never changed.
- `default_provider` is set only when you have neither `default_provider` nor
  `[models] default`.
- **Not carried over:** OAuth logins (Hermes `nous` and `openai-codex`,
  OpenClaw's `oauth`/`token` auth profiles), `key_cmd`, and Codex Responses
  mode. The summary names each.

### Keys and tokens

**A key is never written into `config.toml`.** The config gets the key's
variable name, and the summary lists every key found, by name only, with
where it was found:

- **Without `--bind-secrets`**, export the variable yourself, or save it with
  `ferrule setup`.
- **With `--apply --bind-secrets`**, the values are copied into ferrule's
  secret store (`<data>/private/secrets.env`, mode 0600, the same file
  `ferrule setup` writes). A name that's already there isn't overwritten.
  `--bind-secrets` without `--apply` is an error.

A `${VAR}` reference stays a reference. Its value is copied only when the
tool's own `.env` defines it; ferrule never reads it from your current
environment. OpenClaw's `store`, `file` and `exec` secret references can't be
carried over, and the summary says which.

## Not imported

- sessions and transcripts (Hermes `state.db`, OpenClaw's SQLite stores);
- scheduled jobs;
- MCP server definitions (a follow-up: they carry keys inline);
- the `SOUL.md` persona;
- OpenClaw `$include`d config files (listed as skipped, with the path);
- anything the YAML or JSON5 reader can't parse (YAML anchors, block scalars):
  listed as skipped, and the rest of the import goes on. A file that can't
  be read at all is reported with its parse error.

One run takes at most 2,000 memories and 200 skills, and says when it hit the
limit.

## Testing it

`cargo test -p ferrule-cli import` runs the importer against fixture homes
for both tools: dry run, apply, a second apply that changes nothing, an edited
entry that supersedes, keys that stay out of the config and the output,
allowlists merged, skills refused without confirmation.

Against a **real install** (not run in CI):

```bash
FERRULE_IMPORT_LIVE_OPENCLAW=~/.openclaw \
FERRULE_IMPORT_LIVE_HERMES=~/.hermes \
  cargo test -p ferrule-cli live_a_real_install -- --ignored --nocapture
```

Either variable alone works. The test imports into a throwaway ferrule home,
twice, prints the summary, and checks that the second run changes nothing.
The source is only read.
