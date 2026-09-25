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
out the model's rate limit, retry k of n in N s", counting down.

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
