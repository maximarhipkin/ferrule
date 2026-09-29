# M41 — daily use: /new, voice messages, typing, backups

**Status:** built, 2026-09-29 (branch `m41-daily-use`); what was verified
and how is in §5. User docs: [`channels.md`](channels.md) (chat commands,
voice messages, the typing indicator), [`discord.md`](discord.md) (the slash
list) and [`backup.md`](backup.md).

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
# language = "he"        # unset: the backend detects it; "he" pins Hebrew
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
  `OPENAI_API_KEY` itself); otherwise off. An `auto` that also sets
  `base_url`, `provider` or `command` is a config error: it says which
  backend was meant rather than guess.
- `api_key_env = ""` sends no key (a local server).
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

`Channel::typing(chat_id, message_id, on) -> Typing`, with a default that
answers `Typing::Unsupported`. The channel says when to call it again:
`Shown { again_in }`, `Limited` (a 429: no more this turn), or `Failed`
(logged, retried at the next interval). One refresher per turn
(`typing.rs`) starts once the turn's stop guard is armed. It calls
`typing(…, true)` at once, then after each `again_in`, and it's stopped, and
awaited, **before** the reply is sent. It stops on `/stop` (the guard's stop
future), on an error, and when the turn ends. A channel that clears typing
explicitly (Matrix) gets `typing(…, false)` then.

| Channel | Call | Refresh |
|---|---|---|
| Telegram | `sendChatAction` `typing` (lasts 5 s) | 4 s |
| Discord | `POST /channels/{id}/typing` (lasts 10 s); skipped while a deferred slash command's answer is pending, which already shows "thinking…" | 8 s |
| Matrix | `PUT …/typing/{user}` `{typing: true, timeout: 30000}`; `{typing: false}` when the turn ends | 25 s |
| WhatsApp | a read receipt with `typing_indicator: {type: "text"}` on the inbound message (lasts 25 s or until the reply); error codes 130429/131056 count as a rate limit | 20 s |
| Slack, Mattermost, email, Signal, HTTP | none (`Unsupported`, never called again) | — |

Typing runs alongside streaming (a streamed reply's edits don't stop it;
the final send does), and for scheduled and dispatched turns too, since
they go through the same lane. `Router::with_typing(bool)` is off by default
in the library, so embedders opt in. The CLI turns it on unless
`[gateway] typing = false`.

While this was built, a flaky `health` test turned out to be a real race:
a status write that began before a clean shutdown could land after the
shutdown removed `status.txt`, leaving it behind. `write_status` and
`write_running` now re-check the closed flag after writing and remove what
they wrote. It's fixed in the same commit.

## 4. `ferrule backup` and `ferrule restore`

### The archive

`ferrule backup [--out FILE] [--include-secrets]` writes one `.tar.gz`:
`manifest.json` first, then `data/<path>` for each file in the data dir,
then `config/config.toml` when a config file resolves.

- SQLite databases, found by their 16-byte header rather than their name,
  are copied with `VACUUM INTO` into a temp dir next to the output, never
  byte-copied while open. What's still in a live WAL is in the copy.
- Everything else as it is: `sessions/`, `ledger.jsonl`, skills, plugins,
  hooks, `learn/`, `trust/`, `mcp/`, `plans/`, `gateway/` state.
- `manifest.json`: `format: 1`, the ferrule version, the instance, the UTC
  time, `secrets`, where the config came from, and each file's path, size
  and sha256. Each tar entry carries exactly the bytes that were hashed,
  even if the file grows during the backup.

**Excluded as caches:** `models/`, `update/`, `bin/`, `worktrees/`,
`eval/`, `telemetry/`, `sandbox/`, `gateway/running.json`,
`gateway/status.txt`, `backup.json`, and `*-wal`, `*-shm` and `*-journal`
files. So is the output file, if it's written inside the data dir.
Symlinks are skipped.

**Secrets** (excluded by default): `private/` (`secrets.env`, the plan
sign-ins, the dashboard's login links), `claude-code/`, `ssh/`,
`proxy/keys/` (the proxy's CA key) and `gateway/matrix/session.json`.
The design's first draft listed `plans/` as sign-in state. It's
`PlanStore`'s task plans, not a secret, so it's backed up.
`gateway/http/clients.json` holds only key hashes and is backed up too.
`--include-secrets` adds them, prints a warning to stderr, and the manifest
records `"secrets": true`.

**The file is always 0600** on Unix (created with that mode), with or
without secrets: transcripts and memory are private too. It's written to
`<out>.partial`, fsynced, then renamed. An existing output is refused.
`ferrule backup` then writes `<data>/backup.json` (file, time, size,
secrets) for the doctor.

### Restore

`ferrule restore FILE [--dry-run]`:

1. **Running.** It refuses while this instance's running marker has a live
   pid, or its service reports running. The refusal names the stop command
   (`Svc::stop_hint`: `launchctl bootout gui/<uid>/<label>`,
   `sudo systemctl stop <unit>`, `systemctl --user stop <unit>`) or the
   foreground gateway's pid.
2. **Verify.** `manifest.json` must be the first entry, and each other
   entry must be a regular file named `data/<normal components>` or
   `config/config.toml`. So no `..`, no absolute path and no links. Nothing
   may be extra, duplicated or missing, and every size and sha256 must
   match. A gzip or tar error reads as "the archive is damaged". A `format`
   above 1 is refused. So is a newer release line: a greater major version,
   or while ferrule is 0.x, a greater minor version, since 0.x minors are
   where the data format moves.
3. **Stage.** The archive is read once. Each entry is hashed while it's
   written into a hidden sibling, `<parent>/.<name>.restore-XXXX`, so the
   data dir is never touched by a check that fails. Any failure removes the
   staging dir.
4. **Swap.** The data dir is renamed to `<data>.pre-restore-<stamp>`, and the
   staged `data/` renamed into its place (on the same filesystem, so both
   renames are atomic). If the second rename fails, the first is undone. If
   the archive has no secrets, the secret paths are copied from the
   pre-restore dir, so the restored instance keeps this machine's keys. A
   restored config replaces the resolved config path (or the instance's
   global one when none exists). The old one is moved to
   `<config>.pre-restore-<stamp>`.

`--dry-run` runs steps 1 and 2 without writing. A running gateway is
reported, not refused, so a backup can be checked at any time. It prints
the moves a restore would make.

Caches stay in the pre-restore dir. The reply points at `models/` there, so
a large download needn't be repeated.

`--instance NAME` (the global flag from M38) picks the instance for both.
The dashboard's console (M37 parity table) runs `backup` as a change, but
refuses `--include-secrets`. It refuses `restore` except `--dry-run`: a
restore needs the gateway stopped, and the page is the gateway.

### Doctor and schedules

`ferrule doctor` shows `backup: the last one was 3 days ago: <file>` (or
"none made yet") as a note, never a warning.

Scheduled backups (`[backup] every/keep/dir`): the scheduler runs agent tasks
(prompts), not CLI commands, so a backup task would need a new task kind.
That doesn't fit naturally; it's a follow-up (§6). A cron or systemd timer
running `ferrule backup` works today, and `docs/backup.md` shows one.

## 5. What was verified, and how

Every check below is an automated test in the workspace suite. They're
hermetic: mock servers on 127.0.0.1, temp dirs, and the real `ferrule`
binary for the CLI ones. The suite went from 1628 passed (28 ignored) to
1659 passed (28 ignored), with clippy `-D warnings` and `cargo fmt --check`
clean after each part.

- **`/new`** (`ferrule-gateway/tests/it/daily_use.rs`):
  - A mock provider that answers 400 to any history containing a poisoned
    message. The first failure has no hint, and the second ends with "send
    /new". `/new` replies "New conversation. The previous one (N messages)
    is saved". The next message succeeds, and the old transcript is on disk
    under `<sid>.<stamp>.jsonl`.
  - `/new` during a slow turn stops it and drops the queued message, and
    the reply counts it.
  - `/new` on an empty chat.
  - A unit test sees the Telegram menu (`new`, `stop`, `status`, `help`)
    registered at start. The Discord registration test counts the two new
    slash commands.
- **Voice** (`tests/it/voice.rs`, and unit tests in `transcribe.rs` and
  `transcription.rs`):
  - A Telegram voice update against a mock Bot API (`getFile` plus the file
    URL) and a mock `/audio/transcriptions`. The agent's input has
    `[voice message, 0:14, transcribed]: …` and the saved path, and there's
    a ledger row.
  - With the service down, the agent still gets the file and a note, and
    the sender gets the reason.
  - With transcription off, the sender is told how to turn it on.
  - Unit tests: backend choice (`auto` with and without a key, a plan-only
    setup), bad settings refused in words, `{file}` templates split without
    a shell, the command backend's stdout and its timeout kill, OGG going up
    as is, a rejected format's wording, and a voice note's length read from
    its last Ogg page.
- **Typing** (`tests/it/typing.rs`, unit tests in `typing.rs`):
  - A 5.5 s mock turn on Telegram gets exactly two `sendChatAction` calls
    (0 s and 4 s), none after the `sendMessage`, and none in the 4.2 s
    after the turn.
  - A 429 on the first call gets no second one, and the reply still goes.
  - `typing = false` sends none.
  - Unit tests of the refresher: it refreshes until stopped and then
    clears, a rate limit ends it, a stop ends it at once, and a channel
    without typing is asked once.
- **Backup** (`ferrule-cli/tests/it/backup.rs`, through the binary):
  - Round trip. A session JSONL, two memories in a `MemoryStore` held open
    (WAL) during the backup, a cron task, a ledger line, a config and a
    secret. Then a backup, a wipe and a restore. Every table's rows in
    `memory.db` and `tasks.db`, and the session, ledger and config bytes,
    come back identical. The archive has no secrets and no `models/`. The
    local secret was carried over. The old data and config are in
    `.pre-restore-*`, no staging dir is left, and doctor shows the age.
  - `--include-secrets` puts `private/` in and sets `"secrets": true`. An
    existing output is refused.
  - A live running marker makes restore refuse, naming the pid, and the data
    dir is unchanged.
  - Refused with nothing moved: one byte changed in a file, an extra file,
    a `data/../../escape` path, a missing file, a gzip cut in half, and a
    manifest claiming ferrule 99.0.0. The intact archive then restores.

**Not verified live:** transcription against the real OpenAI or Groq
endpoints and a real whisper.cpp, including the `verbose_json` → `json`
retry, which only a backend that refuses `verbose_json` exercises; typing
on real clients. Telegram's is tested against a mock Bot API. Discord's,
Matrix's and WhatsApp's requests are written from their API docs, and no
test sends them. Nor a restore refused by a real systemd or launchd
service: the tests fake only the running marker, not the service probe.
Backup and restore on Windows are covered only by CI, and 0600 doesn't
apply there.

## 6. Follow-ups

- Voice on Discord and Slack (they don't download files yet).
- Scheduled backups inside ferrule (`[backup] every/keep/dir`, a task kind
  that runs a command), with pruning.
- Other Telegram file kinds (photos, documents) through the inbox.
- Typing on Slack (only through the assistant-thread status API) and
  Mattermost (a websocket `user_typing` action).
