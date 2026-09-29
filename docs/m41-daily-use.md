# M41 — daily use: /new, voice messages, typing, backups

**Status:** design, 2026-09-29 (branch `m41-daily-use`). User docs:
[`channels.md`](channels.md) (chat commands, voice messages) and
[`backup.md`](backup.md).

Four small things that someone who talks to ferrule every day from a phone
misses first:

| # | What | Why |
|---|---|---|
| 1 | `/new` (alias `/reset`) in every chat | A poisoned or overlong conversation can only be escaped today by deleting a JSONL file by hand. |
| 2 | Voice messages are transcribed | The owner sends voice notes, in Hebrew. Today they get "I can only read text". |
| 3 | A typing indicator while a turn runs | Between the 👀 and the reply there is no sign of life for a minute or more. |
| 4 | `ferrule backup` / `ferrule restore` | Everything ferrule knows lives in one data dir, and nothing copies it. |

The order is the build order. Each part is its own commit with its tests.

## 0. What stays as it is

- **Every existing test passes as it is**, except the ones that pin the old
  "I can only read text" reply to a Telegram voice message (§2 changes that
  reply on purpose).
- **Sessions keep their ids.** `/new` doesn't invent a new naming scheme: the
  chat keeps `telegram__42`, and the old transcript is renamed next to it.
- **The agent still reads text.** A transcript is text; the audio file stays
  in the inbox and the agent gets its path, as M39 does for every file.
- **Nothing reaches the network in tests.** Every new test runs against a mock
  on 127.0.0.1.
- **The eval starter suite, its graders and the mock model don't change.**

## 1. `/new` and `/reset`

### What it does

`/new` in a chat starts a fresh conversation for that chat's lane:

1. If a turn is running, it is stopped on the same path as the dashboard's
   **Stop** (`Router::stop`, M37): the running turn's reply becomes
   "Stopped from /new: I ended this turn…". This is the per-lane stop, not
   the owner's global `/stop` kill switch, which ends every lane and pauses
   the gateway.
2. Messages queued behind that turn are dropped and counted. The reply says
   how many, so nothing vanishes silently.
3. The lane is retired: the router forgets it, and the lane task ends. The
   next message creates a new lane with a new agent.
4. The transcript `<sessions>/<sid>.jsonl` is renamed to
   `<sid>.<UTC stamp>.jsonl` in the same directory. The new agent starts from
   an empty `<sid>.jsonl`.
5. The reply is plain: "New conversation. The previous one (N messages) is
   saved." N counts the transcript's message records.

**Memory and learnings carry over.** They live in `memory.db` and the learn
store, keyed by nothing that `/new` touches. The renamed transcript stays in
the sessions dir, so the M16 learning pass and `ferrule sessions` still find
it.

**Who may send it.** Anyone the channel admits to the chat (the same people
who can talk to the agent there). It resets only that chat's own lane, so a
group member can't touch anyone else's conversation. It doesn't need the
owner: it's the chat's own escape hatch.

**Why rename, not delete.** The brief says the old transcript stays on disk.
A rename in the same directory is atomic and keeps the file's mtime, which is
what `ferrule sessions` sorts by.

**The old agent must not write again.** `transcript::write` opens the file
by path on every write, so an old agent that wrote after the rename would
re-create `<sid>.jsonl` under the new conversation. The router therefore
waits for the old lane task to end (the stop makes it end within one tool
step; the wait is capped at 10 s, then the task is aborted) before it renames.
The gateway handles messages one at a time, so no new message reaches the
lane while this happens.

### The self-heal hint

A conversation can get into a state the provider rejects every time: a
malformed tool result in the history, an image the model can't take, a
history over the window. Today each new message gets the same error.

The lane remembers the last failure's class and text. When a turn fails with
a 4xx that isn't about keys, access or rate (so not 401, 403, 404 or 429:
bad request, context too long) and the previous turn in the same chat failed
the same way, the error message ends with:

> send /new to start a fresh conversation

A success clears it. A single 400 doesn't get the hint: one bad request can
be a passing provider problem, and telling someone to throw away a
conversation for it is wrong.

### `/help`, the command menu and the terminal

- `/help` lists the chat commands with one line each. The gateway answers it
  itself. The CLI adds the lines for the commands its interceptors own
  (`/model`, `/connect`, `/login`, …), so the list matches what the gateway
  actually handles.
- Telegram's command menu: ferrule registers no menu today. M41 calls
  `setMyCommands` once at startup with `/new`, `/stop`, `/status`, `/help`,
  so they appear under the `/` button. A failure is logged and ignored.
- `ferrule chat` gets `/new` too: it ends the current session, starts a new
  session id and rebuilds the agent. It's cheap: the loop already rebuilds
  agents for `/undo`.

## 2. Voice and audio messages

### Where the transcription happens

In the **router**, once, for every channel. M39's adapters already save what
people send into the workspace inbox and list each file in
`InboundMessage.attachments` with its MIME type and path. Before a lane runs
the agent, it transcribes every attachment whose type is `audio/*` and adds
the result to the message text:

```
[voice message, 0:14, transcribed]: בוא נקבע את הפגישה ליום שלישי
[file saved: inbox/telegram/2026-09-29/voice-1234.ogg (audio/ogg, 21 KB)]
```

So email, Matrix, Mattermost, Signal, WhatsApp and the HTTP API get voice
messages the day this lands. **Telegram** doesn't download files yet (M39 left
it on "I can only read text"): M41 adds the download for voice notes and
audio files only (`getFile`, then the file URL), into the same inbox. Photos,
video and documents keep today's reply. Discord and Slack voice are a
follow-up (§6).

Ferrule makes the call, not the agent: the key never enters the sandbox, and
the agent can't choose to skip it.

### Backends

`[transcription]` in `ferrule.toml`:

```toml
[transcription]
backend = "auto"         # auto | openai | command | off
# the OpenAI-compatible endpoint (OpenAI, Groq, a local server)
provider = "openai"      # borrow base_url and api_key_env from a [providers.X]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
model = "whisper-1"
language = ""            # empty: the backend detects it; "he" pins Hebrew
# or a local program; {file} is the audio file's path
command = "whisper-cli -m ~/models/ggml-small.bin -otxt -of - {file}"
timeout_secs = 120
price_per_minute = 0.006 # for the ledger; 0 for a local server
```

- **openai:** `POST {base_url}/audio/transcriptions`, multipart, with the
  file as sent. OGG/Opus goes up as is (OpenAI and Groq take it). Ferrule
  asks for `verbose_json` to get the duration and the detected language, and
  retries once with plain `json` if the backend rejects that format.
- **command:** runs the template with `{file}` replaced by the path (quoted),
  no shell, stdout is the transcript. It is killed at `timeout_secs`.
- **auto** (the default): `openai` when an OpenAI key is set (an
  `[providers.X]` with an OpenAI base URL whose key variable is set, or
  `OPENAI_API_KEY` itself); otherwise off.
- **off:** nothing is sent anywhere.

The key is read from the environment in the ferrule process, the same way
provider calls read theirs. No language is assumed: the request carries a
language only when the config pins one. Nothing in the path assumes English.

**No ffmpeg, no bundled model.** When a backend rejects the file's format,
the reply says so: "The transcription service didn't accept this audio
format (audio/ogg): …".

### When it can't transcribe

The file is always saved and the agent always gets its path. Then:

| Case | The agent gets | The person gets |
|---|---|---|
| off (no key) | `[voice message: not transcribed, transcription is off]` | once per chat per run: how to turn it on (an OpenAI key, a Groq-style endpoint, or a local command). Plan users (ChatGPT/Claude subscriptions) are told their plan has no API key for this. |
| the backend failed | `[voice message: not transcribed: <reason>]` | the reason, in one line |
| the file was too big to save | M39's existing note | M39's existing reply |

The agent still runs in every case: it can say it couldn't listen, or open
the file with its own tools.

### Ledger and doctor

Every transcription is one ledger line: `call_kind = "transcription"`,
`provider = "transcription"`, the model, latency, outcome, and a cost of
`minutes × price_per_minute` when the duration is known. `ferrule doctor`
gets one line: which backend is active and why, or how to turn it on.

## 3. Typing indicator

`Channel::typing(chat_id, message_id)` with a no-op default, and
`Channel::typing_every()` (default 4 s). While a lane runs a turn, one task
per turn calls it at that interval.

| Channel | Call | Refresh |
|---|---|---|
| Telegram | `sendChatAction` `typing` (lasts 5 s) | 4 s |
| Discord | `POST /channels/{id}/typing` (lasts 10 s) | 8 s |
| Matrix | `PUT …/typing/{user}` `{typing: true, timeout: 30000}`; `false` when the turn ends | 25 s |
| WhatsApp | the Cloud API's typing indicator: a read receipt with `typing_indicator: {type: "text"}` on the inbound message (lasts 25 s or until the reply) | 20 s |
| Slack, Mattermost, email, Signal, HTTP | none (no-op) | — |

It stops at once when the reply is sent, on `/stop` or `/new`, and on an
error. A 429 (`GatewayError::RateLimited`) from a typing call drops typing for
the rest of that turn, so the indicator never competes with the reply for the
rate limit. `[gateway] typing = true` is the default; `false` turns it off.

## 4. `ferrule backup` and `ferrule restore`

### The archive

`ferrule backup [--out FILE] [--include-secrets]` writes one `.tar.gz` of the
instance's data dir:

- SQLite databases (`tasks.db`, `memory.db`, `agents.db`, any other
  `*.db`) are copied consistently with `VACUUM INTO` into a temp dir, never
  byte-copied while open;
- everything else as it is: `sessions/` (JSONL transcripts), `ledger.jsonl`,
  `ferrule.toml`, memory, schedules, `gateway/` state, skills, plugins,
  hooks, `learn/`, `trust/`, `mcp/`, `plans/` (sign-in state for plans is a
  secret, see below);
- `manifest.json` at the top: ferrule version, instance, UTC time, whether
  secrets are included, and each file's path, size and sha256.

**Excluded as caches:** `models/` (downloaded weights), `update/`, `bin/`,
`worktrees/`, `eval/` run output, `telemetry/`, the running marker, and
`*-wal`/`*-shm` files.

**Secrets** (`secrets.env`, `private/`, `plans/`, `claude-code/`, `ssh/`) are
left out by default. `--include-secrets` adds them, prints a warning, writes
the archive with mode 0600, and the manifest records `"secrets": true`.

### Restore

`ferrule restore FILE [--dry-run]`:

1. Refuses while this instance's gateway runs (its running marker has a live
   pid) or its service is running, and says how to stop it.
2. Reads the manifest, verifies every file's sha256 and size, and refuses a
   file that's missing, extra or changed. Refuses an archive from a newer
   major version.
3. Moves the current data dir to `<data>.pre-restore-<stamp>` (never deletes),
   then extracts into a fresh data dir. With `--dry-run` it stops after 2 and
   prints what it would do.

`--instance NAME` (the global flag from M38) picks the instance for both.

### Doctor and schedules

`ferrule doctor` shows the age of the newest backup it knows about (recorded
in `<data>/backup.json` by `ferrule backup`) as info, never as a warning.

Scheduled backups (`[backup] every/keep/dir`): the scheduler runs agent tasks
(prompts), not CLI commands, so a backup task would need a new task kind.
That doesn't fit naturally; it's a follow-up (§6). A cron or systemd timer
running `ferrule backup` works today, and `docs/backup.md` shows one.

## 5. Tests

- `/new`: a session whose history makes the mock provider return 400 twice
  (the second error ends with the hint), then `/new`, then the next message
  works; the old transcript is on disk under its new name.
- Voice: a Telegram voice update with a mock transcription server ends with
  the transcript in the agent's input; with the server down, the agent still
  gets the file and a note, and the person gets the reason.
- Typing: the fake Telegram server sees `sendChatAction` during a long mock
  turn and none after the reply.
- Backup: session, memory, task and a ledger line → backup → wipe → restore →
  identical; refusal while the gateway runs; a corrupted or tampered archive
  is refused.

## 6. Follow-ups

- Voice on Discord and Slack (they don't download files yet).
- Scheduled backups inside ferrule.
- Other Telegram file kinds (photos, documents) through the inbox.
