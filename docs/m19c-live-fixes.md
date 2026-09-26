# M19c — live-bot fixes

M19b made a stuck gateway visible. M19c is what the first real bot hit
anyway: a 0.2.0 gateway running as a systemd user unit on an OpenRouter
`:free` model "ran but didn't answer", and every cause was silent.

- The journal showed only ERROR lines: `EnvFilter::from_default_env()` with
  `RUST_LOG` unset keeps errors and drops the rest, so the warnings that
  would have explained it never reached the journal.
- Turns failed on OpenRouter's 404 "No endpoints found that support tool
  use" and on 429s from the shared free pool. The chat got "Something went
  wrong" with a JSON blob, or nothing readable at all.
- A 409 from `getUpdates` (a second poller on the same token, or a webhook
  set on the bot) was retried forever with a warn nobody saw.
- A voice message or a photo was dropped without a word.
- A chat outside `telegram_allowed_chats` got an info line, below the
  default filter.

The goal: **the owner never has to guess.** Every reason the bot doesn't
answer is told to the owner in Telegram in plain words, or shown by
`/status`, `ferrule status` and `ferrule doctor`.

The checklist below is written for Telegram. Discord and Slack (M31) show
in the same `/status`, `ferrule status` and `ferrule doctor`, with their
own checks: see [discord.md](discord.md#health) and
[slack.md](slack.md#health).

## The bot doesn't answer: a checklist

Check these in order. Each step shows more than the one before.

1. **`/status` in Telegram**, from any allowed chat. If it answers, the
   gateway is up and polling. Look at what the chat's turn is doing (a
   tool, "waiting out the model's rate limit, retry 2 in 25 s"), the
   telegram line (a `409 Conflict since …` under it means another program
   is getting the bot's messages) and the recent warnings (an ignored chat
   is named there with its id). No answer at all means the gateway isn't
   getting messages: go to step 2.
2. **`ferrule status`** on the machine. It prints the same report from the
   file the gateway keeps, says so if no gateway is running or if the report
   is stale (a wedged process), and prints where the service's logs are.
3. **`ferrule doctor`**. It checks the bot token (`getMe`), whether a webhook
   is set on the bot (a webhook means Telegram sends the messages elsewhere),
   how many gateways run on this machine (two on one token take turns, and
   both get 409s), and whether the model is an OpenRouter `:free` one (shared
   rate limits, often no tool support).
4. **The journal**: `journalctl --user -u ferrule -f` for a user service
   (`journalctl -u ferrule -f` for a system one, the log file on macOS;
   doctor and `ferrule status` print the right command). With `RUST_LOG`
   unset the gateway logs warnings from everything and info from ferrule,
   and no line carries the bot token or a key.

Most of these reach the owner's chat on their own. A lasting 409, a removed
webhook, a message that isn't text, and a model that has no tool endpoint
or is rate-limited each get a message in plain words. The exact messages
are under "As built".

## Design

### 1. Logs

With `RUST_LOG` unset, the filter is `warn` for everything and `info` for
ferrule's own crates (`ferrule`, `ferrule_core`, `ferrule_gateway`,
`ferrule_providers`, `ferrule_tools`, `ferrule_mcp`, `ferrule_sandbox`,
`ferrule_skills`, …). `RUST_LOG` still overrides it completely. The ring
buffer behind `/status`'s "recent warnings" is unchanged (WARN and up).

Stdout and stderr aren't redacted, and a `reqwest::Error`'s Display
includes the URL — which for Telegram is `…/bot<TOKEN>/method`. Every
channel, provider and MCP HTTP error goes through `without_url()`. A test
runs the real binary against a Telegram URL where nothing listens, with
`RUST_LOG` unset, and checks that the token appears nowhere in its stderr
or in `status.txt`.

### 2. 409 Conflict and webhooks

Telegram answers `getUpdates` with 409 in exactly two cases, and says which
in `description`:

- `Conflict: terminated by other getUpdates request; make sure that only
  one bot instance is running` — another process polls with this token.
- `Conflict: can't use getUpdates method while webhook is active; use
  deleteWebhook to delete the webhook first` — a webhook is set.

At start the adapter calls `getWebhookInfo`. If a webhook is set it calls
`deleteWebhook` with `drop_pending_updates=false` (messages waiting at
Telegram are kept) and tells the owner it did, and why. The same happens if
a webhook shows up later (a 409 whose description names the webhook).
Only the webhook's host is shown: its path is often a secret.

A 409 starts a **conflict episode**. Polling keeps retrying with backoff as
before. Once the episode has lasted `[health] telegram_conflict_secs`
(default 60 s) the owner gets one message naming both causes, the likely
one first. The episode ends when polls have succeeded for that long again
without a 409 (two pollers take turns, so a single good poll doesn't prove
anything), and the owner is told "recovered" — only if they were told
about the conflict. While an episode is open, `/status`'s channels section
shows it; a new `Channel::problem()` method (default `None`) carries it.

### 3. Messages that aren't text

From an allowed chat:

- a caption is taken as the text, with a note appended that the attachment
  wasn't read, so the agent can say so;
- with no text and no caption (voice, audio, photo, video, video note,
  sticker, file, GIF), the bot replies once in plain words naming the kind
  and saying it reads only text for now. An album (one `media_group_id`)
  gets one reply, not one per photo. Logged at info.

Service messages (someone joined, a pinned message) stay silent. Voice
transcription is out of scope; see "What voice would take" below.

### 4. Ignored chats

A message from a chat outside a non-empty `telegram_allowed_chats` is a
warning — at most once per chat per hour — that names the chat id and the
key, so it shows under `/status`'s recent warnings. The stranger still
gets silence.

### 5. Provider failures in plain words

The chat's failure message leads with plain words and keeps the raw error,
clipped, after them:

- OpenRouter's 404 "No endpoints found that support tool use": the model
  has no endpoint that supports tools; pick another (a `:free` one is often
  the cause).
- A 429: the model provider is rate-limiting us, naming OpenRouter's shared
  free pool when the body says so, and how many times it was retried.

The retry loop already emits `AgentEvent::ProviderRetry { attempt,
max_attempts, delay_ms, error }`; that is enough, so there is no new event.
The final error carries how many attempts were made. While a lane sleeps
out a 429 or a `Retry-After`, the busy notice and `/status` say "waiting
out the model's rate limit, retry k in N s", counting down.

### 6. Doctor and status

- The Telegram check calls `getMe` and `getWebhookInfo`; a webhook is a
  warning with the fix (the gateway removes it at start, or `ferrule
  setup` → Telegram).
- A second gateway on the same data directory — a fresh running marker
  whose pid is alive and isn't this process, or another `ferrule gateway`
  process on the machine — is a warning: two pollers are the 409.
- An OpenRouter model id ending `:free` is a warning: shared rate limits
  and often no tool-capable endpoint; use the paid id or set
  `[models] fallback`.
- An installed service prints where its logs are (`journalctl --user -u
  ferrule -f` for a user unit) in both `ferrule doctor` and `ferrule
  status`.

## Defaults for Max to confirm

- 409 threshold 60 s (`[health] telegram_conflict_secs`), and the same
  length of clean polling to call it recovered.
- The webhook is deleted automatically at start, pending updates kept. A
  bot token can't serve a webhook and long polling at once, and ferrule
  was told to use this token; a webhook left in place means a deaf bot.
- The log default `warn,<ferrule crates>=info`.

## What voice would take

A voice message is an OGG/Opus file behind `getFile`. Transcribing it needs
a speech-to-text call (OpenAI's or Groq's `/audio/transcriptions`, which
take OGG directly, or a local whisper.cpp), a provider key and a price for
it in the ledger, a size cap, and the transcript fed in as the text with a
marker that it was heard, not typed. Photos would need the model's image
input, which `ChannelCapabilities::attachments` and `Attachment` already
anticipate but no channel fills yet.

## As built

Three parts: the Telegram adapter and the log default (part 1), provider
failures (part 2), and doctor, status and the end-to-end tests (part 3).

### What the owner sees in Telegram

A webhook found at start (or named by a 409) and removed:

> Your bot had a webhook set (to example.com), so Telegram was sending its
> messages there instead of to me. I removed it so I can receive them;
> messages already waiting at Telegram were kept. If another service needs
> that webhook, it and ferrule can't share this bot token: give one of them
> its own bot from @BotFather.

A webhook that couldn't be removed:

> Your bot has a webhook set (to example.com), so Telegram sends its
> messages there instead of to me, and I couldn't remove it: {error}.
> Remove it with `ferrule setup` → Telegram, or stop the service that set
> it.

409s lasting `[health] telegram_conflict_secs` (60 s), once per episode:

> I'm not getting this bot's messages: Telegram has refused them to me for
> 1 min (409 Conflict: "Conflict: terminated by other getUpdates request;
> make sure that only one bot instance is running"). The cause: another
> program is fetching this bot's messages with the same token — most likely
> a second ferrule gateway (another machine, an old service, a terminal left
> running) or another bot program. Less likely: a webhook set on the bot (I
> remove one when I find it). Stop the other one and I'll pick up again by
> myself. `ferrule doctor` on each machine shows whether a gateway runs
> there.

When the 409 names a webhook, the two causes swap: "a webhook is set on
the bot, so Telegram sends its messages there (I tried to remove it and
couldn't)", then "another program polling with this bot's token".

When polling has been clean for as long again:

> I'm getting this bot's messages again: the 409 Conflict cleared (it
> lasted 3 min). Messages the other program fetched meanwhile went to it,
> not to me.

A message with no text (kind: voice message, video message, audio file,
photo, GIF, video, sticker, file; one reply per album):

> I got your voice message, but I can only read text for now, so I don't
> know what's in it. Please type your message instead.

A caption goes to the agent as the text, followed by: "[The photo attached
to this message wasn't read: ferrule reads only text for now.]"

A chat not in `telegram_allowed_chats` hears, as before M19c: "This bot is
private. Your chat id is {chat} — add it to telegram_allowed_chats in the
ferrule config, or run `ferrule setup`." The log gets one warning per chat
per hour, and `/status` lists it under recent warnings.

A model with no tool endpoint:

> I couldn't reply: this model has no endpoint on OpenRouter that supports
> tools, and ferrule needs tools. Pick another model (a `:free` one is often
> the cause): /model here, or `ferrule model default`.
>
> The error: provider error: HTTP 404 Not Found: {"error":{"code":404,…}}

A 429 on a free model, after the retries:

> I couldn't reply: the model provider is rate-limiting us (HTTP 429). This
> is OpenRouter's shared pool for free models, which everyone on a `:free`
> model draws from. I tried 4 times before giving up. Try again in a few
> minutes, or use the paid model id (without `:free`) or set `[models]
> fallback` so another model answers when this one is busy.
>
> The error: provider temporarily unavailable: HTTP 429 Too Many Requests: … (tried 4 times)

A paid model's 429 leaves out the sentence about the free pool. The raw
error is clipped to 400 characters.

While the wait runs, a message sent to the busy chat hears "Busy for 12 s,
waiting out the model's rate limit, retry 2 in 18 s; your message is queued
— /stop to cancel it.", and `/status` shows the same activity line counting
down.

### Doctor and status

- `ferrule doctor`'s live Telegram check calls `getMe` and
  `getWebhookInfo`. A webhook is a warning naming its host, never its path,
  with the fix.
- It warns when two or more ferrule gateways run on this machine, naming
  their pids. It finds them from the process list (`/proc` on Linux, `ps`
  on macOS), plus the pid in a fresh running marker. On Windows only the
  marker counts.
- It warns about a primary, fallback or other configured model ending in
  `:free`.
- For an installed service, doctor and `ferrule status` print the command
  for its logs.
- The gateway writes log lines without color codes when stderr isn't a
  terminal, so the journal stays readable.

### Tests

`crates/ferrule-cli/tests/live_fixes.rs` runs the real `ferrule` binary
against a mock Bot API and a mock model that answers with OpenRouter's real
404 and 429 bodies. It covers:

- the 409 episode told once, shown by `/status` and `ferrule status`, and
  its end;
- the webhook removed at start, and doctor's webhook warning;
- voice, album and caption messages;
- an ignored chat in `ferrule status`;
- both provider errors reaching the chat, with the countdown in `/status`;
- no log line carrying the token or a color code;
- doctor's two-gateways warning (Linux and macOS).

Unit tests cover each piece in its crate.

### Not verified

- The logs command against a real systemd user unit: the tests have no
  systemd.
- A real OpenRouter account and a real Telegram bot: only their recorded
  bodies are used.
- The macOS `ps` scan runs only in CI.
