# M31 — channels: Discord, then Slack

**Status:** design, 2026-09-26 (branch `m31-discord-slack`). User guides:
[`discord.md`](discord.md), [`slack.md`](slack.md).

Ferrule has one real chat channel, Telegram (M2, hardened by M19b/M19c,
streaming since M27). The strategy doc (§4–5) orders the next ones Discord →
Slack → WhatsApp. M31 builds the first two to the same standard as Telegram:
- the owner is recognized;
- strangers never reach the model;
- a dead connection is never silent;
- approvals, streaming, the 👀 receipt and the owner's commands all work;
- the tokens never reach the model.

WhatsApp is out of scope. §9 lays out the options.

## 0. What stays as it is

- **Telegram's behaviour, byte for byte.** Every Telegram test in
  `channels/telegram.rs`, `tests/streaming.rs` and the gateway's own tests
  passes unmodified. Telegram strings, audit fields and session ids don't
  change. Where channel-neutral code moves into the core, Telegram's case is
  the old code path.
- **One daemon.** `ferrule gateway` runs every configured channel at once,
  each as its own task on the one inbound funnel (M1's design). Lanes stay
  per `session_id(channel, chat)`, so `discord__123` and `slack__U42` resume
  from their JSONL transcripts across restarts, exactly as `telegram__42`
  does.
- **Text only.** Telegram reads text and captions, and answers anything else
  with "I can only read text for now". Discord and Slack do the same: a
  message's text is read, an attachment is not, and a message that is *only*
  an attachment gets the same one-line reply.

## 1. The shape

```
crates/ferrule-gateway/src/channels/
  ws.rs              WebSocket connect: ws:// or wss://, rustls + webpki roots,
                     HTTPS_PROXY CONNECT tunnel, NO_PROXY honoured
  access.rs          the allowlist/stranger/pairing rules both share (§3)
  discord/mod.rs     DiscordChannel: Channel impl, REST, allowlist, sends
  discord/gateway.rs the Gateway WebSocket session state machine
  discord/ratelimit.rs  buckets + global limit
  slack/mod.rs       SlackChannel: Channel impl, Web API, allowlist, sends
  slack/socket.rs    the Socket Mode connection loop
  slack/mrkdwn.rs    Markdown → mrkdwn
```

**The WebSocket crate** is `tokio-tungstenite` with rustls, webpki roots
and `ring`, the same TLS stack `reqwest` already pulls in. It adds
`tungstenite`, `sha1` and a few small crates, and no OpenSSL or aws-lc, so
the musl and Windows targets build as they do today. `release.yml` is run
once on the branch to prove all five targets.

Proxies: `tokio-tungstenite` has no proxy support, and reqwest honours
`HTTPS_PROXY`. So `ws.rs` opens a `CONNECT host:443` tunnel through
`HTTPS_PROXY`/`https_proxy` (unless `NO_PROXY` matches) before the TLS
handshake. The REST calls and the socket then take the same route out.

### Channel trait additions (defaults keep every adapter as it is)

- `fn message_limit(&self) -> Option<usize>`: the channel's size cap for one
  message, in UTF-16 units. M27's editor rolls over at
  `min(pacing.limit, message_limit)`. Telegram returns `None`, so it keeps
  4000. Discord returns 2000, and Slack 3900 (Slack truncates `text` at 40k,
  but advises ~4k for readability).
- `fn stream_every(&self) -> Option<Duration>`: a floor on the edit
  interval. Telegram returns `None` and keeps 1 s. Slack returns 1.5 s:
  `chat.update` is Tier 3 (~50/min per workspace), so one busy chat must stay
  under 40/min. Discord's edit bucket is 5 per 5 s per channel, so it keeps
  1 s.
- `polls()`/`last_ok_poll()`/`problem()` are reused for the sockets.
  - `polls()` is true.
  - `last_ok_poll()` is **the last frame received**: a dispatch, a
    heartbeat ACK, a Slack ping or envelope. Discord ACKs every ~41 s, and
    Slack pings every ~30 s, so M19b's 300-second stale rule, the
    `/status` line, the watchdog and the dashboard's `stale` flag all work
    unchanged. A silent socket goes stale, and so does a reconnect loop.
  - `problem()` says what's wrong in words, for example "disconnected
    (gateway closed 4000); reconnecting, attempt 3, next in 8s" or "the
    MESSAGE_CONTENT intent isn't enabled…". It appears under the channel in
    `/status` and on the dashboard.

## 2. Chats, ids and sessions

| | `chat_id` | lane / transcript |
|---|---|---|
| Telegram (as today) | the chat id | `telegram__<chat>` |
| Discord DM | the **user id** (snowflake) | `discord__<user>` |
| Discord guild channel or thread | the channel id | `discord__<channel>` |
| Slack DM | the **user id** (`U…`) | `slack__<user>` |
| Slack channel mention | `<channel>/<thread_ts>` | `slack__<channel>_<ts>` |

A DM is keyed by the user, not by the DM channel. This mirrors Telegram,
where a private chat's id is the user's id, and it lets the owner be named
by one id that is both "who" and "where". The adapter maps user → DM
channel from the inbound message, and otherwise opens one:
- Discord: `POST /users/@me/channels`;
- Slack: `conversations.open`.

The mapping is cached, so a warning, an approval or a scheduled task's
result can reach a user who hasn't written since the restart.

`sender_id` is the author's user id everywhere. It is what the owner doors
compare in a shared channel, as they do in a Telegram group.

**Slack threads.** In a channel, Ferrule **replies in a thread** under the
mention, and each thread is its own lane. Why:
- a bot that answers at the top level floods a shared channel;
- Slack's own guidance for apps is to answer in threads;
- a thread is how Slack users already scope a conversation;
- a per-thread lane means two people asking in one channel don't share one
  transcript and queue behind each other.

A mention inside an existing thread continues that thread's lane. DMs don't
use threads: a DM is already a private, linear conversation, and threading
it would hide the answers.

**Discord doesn't thread.** It replies in the channel as a Discord *reply*
(`message_reference`) to the message that asked. Discord users don't expect
auto-threads, and creating threads needs the Create Public Threads
permission.

## 3. Who gets in: allowlist, mentions, pairing

The same rules as Telegram (M2/M19c), adapted:

```toml
[gateway]
discord_token_env = "DISCORD_BOT_TOKEN"
discord_allowed_users = ["123456789012345678"]   # DMs from these users
discord_allowed_channels = ["234567890123456789"] # guild channels (and their threads)

slack_bot_token_env = "SLACK_BOT_TOKEN"   # xoxb-
slack_app_token_env = "SLACK_APP_TOKEN"   # xapp-
slack_allowed_users = ["U0123ABCD"]
slack_allowed_channels = ["C0123ABCD"]
```

**The rule:**
- A **DM** is admitted iff its author is in `*_allowed_users`.
- A **shared channel** message is admitted iff the channel is in
  `*_allowed_channels` **and** the bot is addressed:
  - Discord: the bot is @mentioned, or the message is a reply to one of
    the bot's messages;
  - Slack: `app_mention`.

  Then anyone in that channel may talk to it. This is Telegram's rule,
  where a group id admits the whole group and group privacy mode means the
  bot sees only what's addressed to it.
- A Discord **thread** is admitted when it, or its parent channel, is
  allow-listed. The parent comes from the channel cache, filled by
  `GUILD_CREATE`/`THREAD_CREATE`, falling back to `GET /channels/{id}`.
- **Bots, webhooks and the bot itself are never admitted.** Discord drops a
  message with `author.bot`. Slack drops a message with `bot_id` or with a
  subtype other than `file_share`.
- **Strangers** are handled as on Telegram:
  - While the allowlist is empty, a stranger's DM is told its id once:
    "This bot is private. Your Discord user id is …, add it to
    discord_allowed_users…". This is what setup's manual path relies on.
  - Once the list is in use, strangers get silence, and the log gets one
    warning per chat per hour.
  - In a channel that isn't allow-listed, the bot never speaks, not even to
    say it's private, because shared channels aren't the place.

**Why mention-only in channels, even allow-listed ones.** An agent that
reads every message in a busy channel spends the owner's money on chatter
and answers things nobody asked it. It also needs Discord's privileged
MESSAGE_CONTENT intent for every message. With mentions and replies, the
bot works even **without** that intent: Discord still delivers the content
of DMs and of messages that mention the app. So:
- Ferrule asks for MESSAGE_CONTENT, and gets the reply-without-ping case
  when it's granted.
- If the portal toggle is off, Discord closes the socket with 4014.
  Ferrule then re-identifies without the intent, keeps working for mentions
  and DMs, and `problem()`/`doctor` say what is lost and where to turn it
  on.

**Stripping the address.** The leading `<@bot>` (Discord, also `<@!bot>`)
or `<@Ubot>` (Slack) is removed from the text. `@Ferrule /status` is
therefore `/status` to the doors and to `is_command`.

**Pairing (setup).** Telegram's setup watches `getUpdates` and asks "let
this chat use the bot?". For Discord and Slack, setup does the analogue
with a **one-time code**:
1. `ferrule setup` shows a code (`ferrule-4827`) and runs the adapter itself
   for up to two minutes, with an empty allowlist and that code.
2. The first DM whose text is exactly the code is admitted once and
   answered "Paired. …". Its author's id is saved to `*_allowed_users`.
3. That user also becomes the channel's owner (§4) unless one is set.

The code is new each run. It's only accepted in a DM, and only during
setup: the daemon has no pairing mode, so a leaked code is worthless after
two minutes. The fallback is typing the id, which the bot tells a
stranger. The code keeps a stranger who happens to DM the bot during
setup from being paired by accident, which Telegram guards against with a
y/n question.

## 4. The owner, in every channel

Today the owner is a Telegram chat id (`Hub::owner() -> Option<i64>`), and
four doors hard-gate on `msg.channel == "telegram"`. M31 generalizes this.

- `ferrule_trust::ChatRef { channel, chat }` names a chat. `From<i64>` is a
  Telegram chat, so every existing call with an `i64` still compiles and
  means the same.
- The hub holds **owners**: at most one per channel, the first being
  **primary**. `owner()` returns the primary, and `is_owner(&ChatRef)`
  checks all of them.
- Per channel, the owner is:
  - Telegram: `[trust] owner_chat`, else the first private chat in
    `telegram_allowed_chats` (unchanged);
  - Discord: `[trust] discord_owner`, else the first
    `discord_allowed_users`;
  - Slack: `[trust] slack_owner`, else the first `slack_allowed_users`.
- **Primary** is `[trust] owner_channel`, else Telegram, else Discord, else
  Slack, among the channels that run and have an owner. With Telegram
  configured, nothing moves.

What the primary gets: approvals, cap warnings, the restart notice, the
connections' OAuth buttons, `/plan` approvals and the dashboard relink. Any
owner chat may use the owner commands.

- `Notifier::send(&ChatRef, text)`. The CLI's `ChannelNotifier` holds every
  channel and routes by `chat.channel`.
- `Approvals` is keyed by `ChatRef`. An answer counts only from the chat
  that was asked, as today.
- The audit's `"route"` is the owner's channel (`"telegram"` stays
  `"telegram"`), and `"chat"` stays a number for Telegram.
- `/stop`'s `by` is `"{channel} chat {chat}"`, which is the old string for
  Telegram.
- The doors (`OwnerDoor`, `ModelDoor`, `SettingsDoor`, `DashboardDoor`,
  `ConnectionsDoor`) drop `channel != "telegram"` for one helper:
  - `owner_in(msg)` is `Some(true)` in an owner's chat;
  - it is `Some(false)` when an owner is the sender in a shared chat;
  - otherwise it is `None`.

  Their per-chat keys (`/model use`, sessions) use `msg.channel` where they
  said `"telegram"`.
- `/plan` runs from any owner chat and reports back to the chat it was
  asked in.

**`/undo`** (M29) exists only in `ferrule chat` today. M31 adds it as an
owner-only command in every channel, since the brief lists it among the
owner's commands. It reverts the latest agent commit in the workspace, as
`ferrule undo` does. This is the one visible addition on Telegram: `/undo`
used to reach the model as text.

## 5. Discord

**Gateway (outbound WebSocket, v10, JSON).**
- The URL comes from `GET /gateway/bot`, which also reports the
  `session_start_limit` that doctor prints. `?v=10&encoding=json` is
  appended; no compression.
- HELLO → heartbeat every `heartbeat_interval`, the first after
  `interval × jitter`, with jitter ∈ [0,1) as Discord requires.
- IDENTIFY with intents `GUILDS | GUILD_MESSAGES | DIRECT_MESSAGES |
  MESSAGE_CONTENT`, and after 4014 without the last one.
- READY stores `session_id`, `resume_gateway_url` and the bot's user id.
- Every dispatch stores `s`.
- **A missed heartbeat ACK** (no op 11 since the previous heartbeat when
  the next is due) means a zombie connection. It is closed with 4000 and
  resumed.
- op 7 RECONNECT → resume.
- op 9 INVALID_SESSION: `d=true` → resume after 1–5 s; `d=false` → a fresh
  IDENTIFY after 1–5 s.
- Close codes:
  - 4004 (bad token) and 4010–4013 are fatal, and `run` returns the error,
    as Telegram's 401 does;
  - 4014 drops MESSAGE_CONTENT and re-identifies;
  - 4007 and 4009 re-identify;
  - anything else, or a TCP/TLS error, resumes when there's a session and
    identifies when there isn't.
- Backoff is 1 s → 60 s, doubled, and reset by READY/RESUMED.
- Every state change goes to `problem()` and the log, never silently.

**Events used:**
- `MESSAGE_CREATE`: DMs, and guild messages under §3.
- `INTERACTION_CREATE`, of two types:
  - button taps (type 3);
  - slash commands (type 2).
- `GUILD_CREATE`/`THREAD_CREATE`/`THREAD_UPDATE`/`CHANNEL_CREATE`, used
  only for the thread → parent cache.

**REST** (`https://discord.com/api/v10`, `Authorization: Bot <token>`).
Rate limits follow Discord's docs:
- The route key is the method plus the major parameter (the channel id).
  Each response's `X-RateLimit-Bucket`, `-Remaining` and `-Reset-After`
  update that bucket.
- A request whose bucket is at 0 waits for the reset. The wait is bounded
  (≤ 60 s). An edit instead gets `RateLimited` straight away, so M27's
  editor paces itself.
- A **429** reads the body's `retry_after` (fractional seconds). If
  `global: true`, or the scope header says so, **every** request waits it
  out. Otherwise only the bucket waits.
- `send` retries a 429 up to three times, and `edit` returns `RateLimited`.

**Sending.**
- Replies go out as plain Markdown, which Discord renders natively, so
  there's no conversion.
- A reply longer than 2000 characters is split with M27's `chunks()` (at a
  newline, else a space, in the last fifth). The first chunk replies to the
  message that asked.
- `allowed_mentions: {parse: []}`, so the model can never ping @everyone or
  a role.

**Streaming.** Through `post`/`edit` (`PATCH
/channels/{c}/messages/{m}`), throttled by the editor at 1/s, with a
rollover at 2000.

**Buttons and approvals.**
- `send_buttons` sends up to 5 buttons per action row:
  - a command button is `custom_id = "cmd:<text>"`;
  - a URL button is style 5.
- A tap is an `INTERACTION_CREATE` type 3. The adapter **acks at once**
  with `POST /interactions/{id}/{token}/callback` type 7 (UPDATE_MESSAGE).
  That removes the buttons and appends "→ <label>" to the message, well
  inside Discord's 3-second window, because the ack happens in the socket
  task before the message is queued.
- The tap then arrives as the command's text from the tapping user in that
  chat. It passes the same allowlist, so a stranger's tap goes nowhere, as
  on Telegram.
- M19 approvals get two buttons, **Allow** (`yes <code>`) and **Refuse**
  (`no <code>`), through `Notifier::ask`, which the CLI's notifier
  implements with buttons for Discord and Slack. Typing `yes` still works.
- Telegram keeps its text approvals, since its behaviour mustn't change.
  Buttons there are a follow-up.

**The receipt.** 👀 via `PUT
/channels/{c}/messages/{m}/reactions/%F0%9F%91%80/@me`.

**Slash commands (included).**
- `ferrule setup` registers global commands: `/status /stop /resume /model
  /dashboard /skills /undo /plan /caps /mcp /connections`, each with an
  optional `args` string. Registration is one `PUT
  /applications/{app}/commands`.
- It happens at setup and not on every daemon start: a daemon should never
  make a mutating call, and re-registering on each start is rate-limited.
- An invocation is acked at once with type 5 (DEFERRED, "thinking…"). It
  then arrives as the text `/<name> <args>`.
- The next reply to that chat within 15 minutes goes out as the
  interaction's response (`PATCH
  /webhooks/{app}/{token}/messages/@original`), so Discord's "thinking…"
  resolves into the answer.
- Plain-text commands work regardless: in a DM `/status` is typed as text,
  and in a channel it's `@Ferrule /status`.

## 6. Slack

**Socket Mode.**
- `apps.connections.open` (with the `xapp-` app token) → a `wss://` URL.
- The server sends `hello`.
- Every envelope that has an `envelope_id` is **acked at once**
  (`{"envelope_id": …}`), before any processing, which keeps us inside
  Slack's 3-second window. Slack redelivers what isn't acked, with
  `retry_attempt`, so events are also de-duplicated by `event_id` (a bounded
  set).
- `disconnect`:
  - `refresh_requested` or `warning` → open a new connection and close the
    old one;
  - `link_disabled` → Socket Mode was turned off in the app config, which
    goes in `problem()`, with backoff.
- Socket errors, closes and missing pings reconnect with backoff (1 s → 60
  s). Ferrule sends a WebSocket ping every 30 s. No frame for 90 s means a
  dead socket, which is closed and reopened.

**Web API** (`https://slack.com/api`, `Authorization: Bearer xoxb-…`).
- `auth.test` at start gives the bot's user id.
- Also used: `chat.postMessage`, `chat.update`, `reactions.add`,
  `conversations.open`.
- A 429 reads `Retry-After` (seconds) and blocks that method until then:
  - `send` waits and retries up to three times;
  - `edit` returns `RateLimited`.

  Slack publishes no per-method remaining headers, so the tier is honoured
  by pacing: `stream_every` is 1.5 s, and posts to one channel are spaced
  at least 1 s apart (the `chat.postMessage` special tier).
- `{"ok": false, "error": "ratelimited"}` is treated like a 429.

**Events.**
- `message` with `channel_type: "im"` (DMs) and `app_mention` (channels).
- Messages from bots (`bot_id`) are dropped, and so are subtypes other than
  `file_share`.
- A message with files and no text gets the "I can only read text" reply.
  One with text is read as its text.

**Formatting: Markdown → mrkdwn.** Models write CommonMark. Slack renders
its own `mrkdwn`, and CommonMark shows up raw (`**bold**` shows its
asterisks). The adapter converts every outgoing text:

| Markdown | mrkdwn | why |
|---|---|---|
| `**b**`, `__b__` | `*b*` | Slack bold is single `*` |
| `*i*`, `_i_` | `_i_` | Slack italic is `_` |
| `~~s~~` | `~s~` | |
| `[t](u)` | `<u\|t>` | Slack's link syntax |
| `# Heading` (any level) | `*Heading*` | no headings in mrkdwn |
| `- item` / `* item` / `+ item` | `• item` | no list syntax; keeps the indent |
| `` `code` ``, fenced blocks | unchanged (the language tag dropped) | Slack has both |
| `&`, `<`, `>` | `&amp;`, `&lt;`, `&gt;` | Slack's control characters, escaped everywhere |
| `> quote`, numbered lists | unchanged | both already render |

- Text inside code spans and fences is never restyled, only escaped.
- A link's URL is not escaped inside `<…>`.
- Tables stay as text.

Why convert rather than use Slack's newer `markdown` block? The block
still needs a `text` fallback for notifications, and that fallback shows
raw Markdown unless converted anyway. It also makes `chat.update` carry
blocks for every streamed edit.

A mid-stream edit may briefly show an unclosed `*`, as any streamed
Markdown does.

**Streaming.** `chat.postMessage` for the first chunk, then `chat.update`
with `ts`, throttled at 1.5 s, with a rollover at 3900.

**Buttons and approvals (Block Kit).**
- `send_buttons` sends a `section` with the text, plus an `actions` block
  of buttons:
  - a command button carries `value = <text>` and `action_id = cmd_<n>`;
  - a URL button is a `url` button.
- A tap is an `interactive` envelope with `block_actions`. It is acked at
  once like any envelope. Then `chat.update` replaces the blocks with the
  text plus "→ <label>". The tap arrives as the command's text from the
  tapping user, and passes the allowlist.
- Approvals work as on Discord: **Allow** / **Refuse**.

**The receipt.** `reactions.add` with `name = "eyes"` (Slack takes names,
so the adapter maps 👀 → `eyes`).

**Slash command.**
- One command, `/ferrule <command> [args]`, declared in the app manifest
  that `docs/slack.md` gives. Slack command names are workspace-global, so
  a bare `/status` would collide with other apps.
- It arrives as a `slash_commands` envelope and is acked at once. It then
  arrives as `/<command> <args>` from that user, in that channel.
- A command from a channel is answered in the channel (not a thread) only
  if the channel is allow-listed, and in the user's DM otherwise, as long
  as the user is allowed. Otherwise it gets nothing.

## 7. Tokens stay out of the model's reach

This works exactly as for the Telegram token:
- The daemon reads the env var the config names (`discord_token_env`,
  `slack_bot_token_env`, `slack_app_token_env`). `ferrule setup` saves the
  values to the sealed `private/secrets.env` (0600, hidden from the
  sandbox).
- The sandbox scrubs `*TOKEN*` variables from every command, as M6 already
  does.
- The redactor hides the configured values in `/status`, the busy notice,
  the heartbeat and logs. It also hides anything shaped like a Slack token
  (`xox[abpr]-…`, `xapp-…`) or a Discord bot token (three dot-separated
  base64url parts) as defence in depth, since the value list only knows
  the configured names.
- Error strings never include a URL or header, which is the same care as
  Telegram's `without_url()`. Tests assert that no log line carries the
  token.
- The config template's `[secrets]` examples bind them:
  - `DISCORD_BOT_TOKEN = { hosts = ["discord.com"] }`
  - `SLACK_BOT_TOKEN = { hosts = ["slack.com"] }`

  Then a skill that needs the token for a shell command gets a placeholder
  the proxy swaps only for those hosts. They are commented out, as the
  Telegram one is.

## 8. Health, doctor, setup, dashboard

- **`/status`** lists each channel with its last event and `problem()`.
  The `/status` reply itself goes through the channel's split. Its 3800
  cap already fits Slack, and Discord sends it as two messages.
- **The watchdog and heartbeat** see a channel with no frame for
  `poll_stale_secs` (300) as stale and say so, as for Telegram today.
- **Dashboard**: the channels card already lists name, last ok poll and
  stale. M31 adds `problem`, one line in `api.rs` and in the page's table
  cell.
- **`ferrule doctor`** makes only reads:
  - Discord:
    - `GET /users/@me`: the token, and the bot's name;
    - `GET /gateway/bot`: the gateway, the shards and the identify budget
      left;
    - `GET /applications/@me`: the message-content intent flag
      (`1<<18`/`1<<19`), explained when it's off;
    - the allowlists (empty = only pairing works).
  - Slack:
    - `auth.test`: the bot token, team and bot user. The `x-oauth-scopes`
      header is checked against the scopes the guide lists, naming any
      that are missing;
    - `apps.connections.open`: the app token and that Socket Mode is on
      (the URL is thrown away);
    - the allowlists.
- **`ferrule setup`** gets a Discord step and a Slack step, each in the
  guided path and the menu:
  1. token(s), saved sealed, checked with the doctor calls;
  2. pairing by code (§3), or typing ids by hand;
  3. for Discord, registering the slash commands and printing the
     invite URL (`client_id` from `/applications/@me`, permissions
     `274877975552`: View Channels, Send Messages, Send Messages in
     Threads, Read Message History, Add Reactions; scopes `bot
     applications.commands`).

  Background service is offered when any chat channel is on.

## 9. WhatsApp: the options (not built)

1. **WhatsApp Business Cloud API (Meta).**
   - It's official and stable, and has no ban risk.
   - But it needs a **public HTTPS webhook** for inbound messages. Ferrule
     is outbound-only by design, so that means a tunnel (Cloudflare Tunnel,
     ngrok) or a relay.
   - It needs a Meta Business account with **business verification**
     beyond the test number, and a dedicated phone number.
   - Business-initiated messages outside the 24-hour window need
     pre-approved templates and cost money.
   - For a personal agent, the owner always writes first, so the 24-hour
     rule mostly doesn't bite. Unprompted warnings or scheduled results
     after a quiet day do.
2. **Unofficial multi-device libraries** (Baileys in Node, whatsmeow in Go;
   no mature Rust one).
   - They pair as a linked device by QR, and need no webhook and no Meta
     account. NanoClaw and OpenClaw use this route.
   - They break WhatsApp's terms, and accounts get **banned**, most often
     new numbers and bot-like traffic patterns. The protocol changes
     without notice.
   - We would have to run a Node/Go sidecar, or port the Signal-protocol
     stack.
3. **A bridge** (Matrix mautrix-whatsapp, or a hosted service such as
   Twilio's WhatsApp API).
   - Twilio is option 1 with a middleman that hosts the webhook, and it
     costs per message.
   - A Matrix bridge is option 2's ban risk plus a homeserver.

**Recommendation: option 1, the Cloud API.** Build it as an opt-in adapter
whose webhook arrives through a tunnel Ferrule doesn't run, and document
the business verification step honestly. The ban risk of option 2 falls on
the owner's own phone number, which is a bad default for a product whose
selling point is trust. Revisit if a well-maintained Rust multi-device
crate appears and the owner explicitly accepts the risk on a spare number.

## 10. Tests (hermetic, 127.0.0.1)

All mocks are in-process: a `tokio-tungstenite` WebSocket server plus a
small HTTP server on 127.0.0.1, scripted per test.

**Discord:**
- HELLO → IDENTIFY (the intents checked) → READY → a heartbeat with the
  sequence;
- RESUME after the server closes 4000;
- INVALID_SESSION `d=false` → a fresh IDENTIFY;
- op 7 → resume;
- **a missed heartbeat ACK → the client closes and resumes on a new
  socket**;
- 4014 → re-identify without MESSAGE_CONTENT, with `problem()` saying so;
- 4004 → `run` returns the error;
- REST 429 with a bucket → only that route waits;
- REST 429 `global` → every route waits;
- the `X-RateLimit-Remaining: 0` pre-wait;
- a 3000-character reply → two messages under 2000, the first one a reply;
- streaming through the router (`post`, then `edit`s, then the final text);
- a button approval: an interaction arrives → a type 7 callback is acked
  before the command is queued → the tap reaches `Approvals` → the approval
  resolves;
- DM from an allowed user → admitted;
- guild channel without a mention → dropped;
- with a mention → admitted, the mention stripped;
- a thread under an allowed parent → admitted;
- an unknown user → blocked and told their id once (empty list), or
  silence (list in use);
- a bot author → dropped;
- no log line carries the token.

**Slack:**
- every envelope acked with its `envelope_id`, before the message reaches
  the router;
- a redelivery with the same `event_id` is dropped;
- `disconnect` `refresh_requested` → a new `apps.connections.open` and a
  new socket;
- a closed socket → reconnect with backoff;
- `app_mention` in an allowed channel → admitted with the thread chat id,
  and the reply carries `thread_ts`;
- DM `message.im` → admitted, the reply is not threaded;
- a `chat.update` stream stays at ≥1.5 s between edits;
- `chat.postMessage` 429 `Retry-After: 1` → waits and retries;
- a Block Kit approval, from the `block_actions` envelope to the resolved
  approval;
- the mrkdwn conversion table, code untouched, escaping;
- an unknown user → blocked;
- `bot_id` → dropped;
- no log line carries the tokens.

**Across channels:**
- Telegram's tests, unmodified and green;
- `tests/channels.rs`: one gateway running the Telegram, Discord and Slack
  mocks at once, with a message from each reaching its own lane and each
  answer going out through its own mock;
- the owner generalization: the hub with a Discord primary gets the
  approval there, and `/stop` from a Slack owner chat engages with
  `by = "slack chat U…"`.

**Live tests** are `#[ignore]`d:
- `discord_live_round_trip` needs `FERRULE_LIVE_DISCORD_TOKEN` and
  `FERRULE_LIVE_DISCORD_USER`;
- `slack_live_round_trip` needs `FERRULE_LIVE_SLACK_BOT_TOKEN`,
  `FERRULE_LIVE_SLACK_APP_TOKEN` and `FERRULE_LIVE_SLACK_USER`.

Each connects, DMs the user "ferrule live test", and waits for the
user's reply. The exact commands are in the user guides.

## 11. Parts

1. This doc.
2. The core seams:
   - the `Channel` additions and the editor honouring them;
   - `ChatRef` and owners in `ferrule-trust`, and the doors generalized
     (Telegram behaviour pinned by its unchanged tests);
   - `/undo` in the gateway;
   - the WebSocket dependency and `ws.rs`.
3. Discord, with its mock tests.
4. Slack, with mrkdwn and its mock tests.
5. The CLI:
   - config;
   - daemon wiring (every channel, the notifier routing, plan/connections);
   - redaction, health/status and the dashboard `problem`;
   - doctor and setup (pairing, slash-command registration);
   - the three-channel test.
6. The user guides, then PLAN/roadmap.
